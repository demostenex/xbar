use crate::core::{AgendaItem, CalendarSnapshot, ClockState, Event, TodayAgenda};
use serde::Deserialize;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const MAX_ADAPTER_OUTPUT: usize = 512 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug)]
struct CalendarConfig {
    python: String,
    adapter: String,
    credentials: String,
    token: String,
}

impl CalendarConfig {
    fn from_environment() -> Option<Self> {
        Some(Self {
            python: std::env::var("XBAR_CALENDAR_PYTHON").ok()?,
            adapter: std::env::var("XBAR_CALENDAR_ADAPTER").ok()?,
            credentials: std::env::var("XBAR_CALENDAR_CREDENTIALS").ok()?,
            token: std::env::var("XBAR_CALENDAR_TOKEN").ok()?,
        })
        .filter(|config| {
            !config.python.is_empty()
                && !config.adapter.is_empty()
                && !config.credentials.is_empty()
                && !config.token.is_empty()
        })
    }
}

#[derive(Debug)]
pub struct CalendarSensor {
    config: Option<CalendarConfig>,
    child: Option<Child>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    stdout_data: Vec<u8>,
    stdout_eof: bool,
    stderr_eof: bool,
    last_requested: Option<Instant>,
    last_success: Option<Instant>,
    last_success_date: Option<String>,
}

impl CalendarSensor {
    pub fn from_environment() -> Self {
        Self {
            config: CalendarConfig::from_environment(),
            child: None,
            stdout: None,
            stderr: None,
            stdout_data: Vec::new(),
            stdout_eof: false,
            stderr_eof: false,
            last_requested: None,
            last_success: None,
            last_success_date: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.is_some()
    }

    pub fn stdout_fd(&self) -> RawFd {
        self.stdout.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    pub fn stderr_fd(&self) -> RawFd {
        self.stderr.as_ref().map_or(-1, AsRawFd::as_raw_fd)
    }

    pub fn in_flight(&self) -> bool {
        self.child.is_some()
    }

    pub fn should_refresh(&self, clock: Option<ClockState>, _calendar_open: bool) -> bool {
        if !self.enabled() || self.in_flight() {
            return false;
        }
        let Some(clock) = clock else {
            return self.last_requested.is_none();
        };
        let local_date = format!("{:04}-{:02}-{:02}", clock.year, clock.month, clock.day);
        if self.last_success_date.as_deref() != Some(&local_date) {
            return true;
        }
        let age = self
            .last_success
            .or(self.last_requested)
            .map(|instant| instant.elapsed())
            .unwrap_or(REFRESH_INTERVAL);
        age >= REFRESH_INTERVAL
    }

    pub fn request(&mut self) -> Result<bool, String> {
        if !self.enabled() || self.in_flight() {
            return Ok(false);
        }
        let config = self.config.as_ref().expect("enabled calendar config");
        let mut command = Command::new(&config.python);
        command
            .arg(&config.adapter)
            .arg("today")
            .arg("--credentials")
            .arg(&config.credentials)
            .arg("--token")
            .arg(&config.token)
            .arg("--no-browser")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn failed ({})", error.kind()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "adapter stdout unavailable".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "adapter stderr unavailable".to_owned())?;
        set_nonblocking(stdout.as_raw_fd())
            .map_err(|error| format!("stdout setup failed ({error})"))?;
        set_nonblocking(stderr.as_raw_fd())
            .map_err(|error| format!("stderr setup failed ({error})"))?;
        self.child = Some(child);
        self.stdout = Some(stdout);
        self.stderr = Some(stderr);
        self.stdout_data.clear();
        self.stdout_eof = false;
        self.stderr_eof = false;
        self.last_requested = Some(Instant::now());
        Ok(true)
    }

    pub fn drain_events(&mut self, stdout_revents: i16, stderr_revents: i16) -> Vec<Event> {
        if self.child.is_none() {
            return Vec::new();
        }
        let readable = libc::POLLIN | libc::POLLHUP | libc::POLLERR;
        if stdout_revents & readable != 0
            || self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.read_stdout();
        }
        if stderr_revents & readable != 0
            || self
                .child
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
        {
            self.read_stderr();
        }
        let finished = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_some();
        if !finished || !self.stdout_eof {
            return Vec::new();
        }
        let success = self
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_some_and(|status| status.success());
        self.child.take();
        self.stdout.take();
        self.stderr.take();
        self.stderr_eof = true;
        if !success {
            self.stdout_data.clear();
            return vec![Event::CalendarSnapshotFailed(
                "adapter exited unsuccessfully".to_owned(),
            )];
        }
        match parse_snapshot(&self.stdout_data) {
            Ok(snapshot) => {
                self.last_success = Some(Instant::now());
                self.last_success_date = Some(snapshot.agenda.local_date.clone());
                self.stdout_data.clear();
                vec![Event::CalendarSnapshotUpdated(snapshot.agenda)]
            }
            Err(error) => {
                self.stdout_data.clear();
                vec![Event::CalendarSnapshotFailed(error)]
            }
        }
    }

    fn read_stdout(&mut self) {
        let Some(stdout) = self.stdout.as_mut() else {
            return;
        };
        let (bytes, eof, oversized) = read_pipe(stdout.as_raw_fd(), &mut self.stdout_data, true);
        self.stdout_eof |= eof;
        if oversized {
            self.stdout_eof = true;
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
            }
        }
        if bytes == 0 && eof {
            self.stdout_eof = true;
        }
    }

    fn read_stderr(&mut self) {
        let Some(stderr) = self.stderr.as_mut() else {
            return;
        };
        let mut discarded = Vec::new();
        let (_, eof, _) = read_pipe(stderr.as_raw_fd(), &mut discarded, false);
        self.stderr_eof |= eof;
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_pipe(fd: RawFd, output: &mut Vec<u8>, retain: bool) -> (usize, bool, bool) {
    let mut buffer = [0_u8; 8192];
    let mut total = 0;
    let mut eof = false;
    let mut oversized = false;
    loop {
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read > 0 {
            total += read as usize;
            if retain {
                if output.len().saturating_add(read as usize) > MAX_ADAPTER_OUTPUT {
                    oversized = true;
                    break;
                }
                output.extend_from_slice(&buffer[..read as usize]);
            }
        } else if read == 0 {
            eof = true;
            break;
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                break;
            }
            eof = true;
            break;
        }
    }
    (total, eof, oversized)
}

#[derive(Debug, Deserialize)]
struct AdapterDocument {
    generated_at: String,
    timezone: String,
    local_date: String,
    mode: String,
    events: Vec<AdapterEvent>,
}

#[derive(Debug, Deserialize)]
struct AdapterEvent {
    calendar_id: String,
    id: String,
    recurring_event_id: Option<String>,
    original_start_time: Option<serde_json::Value>,
    original_start_time_utc: Option<String>,
    summary: String,
    start: Option<String>,
    end: Option<String>,
    start_utc: Option<String>,
    end_utc: Option<String>,
    timezone: Option<String>,
    start_date: Option<String>,
    end_date: Option<String>,
    all_day: bool,
    status: String,
}

pub fn parse_snapshot(bytes: &[u8]) -> Result<CalendarSnapshot, String> {
    let document: AdapterDocument =
        serde_json::from_slice(bytes).map_err(|_| "adapter returned malformed JSON".to_owned())?;
    if document.mode != "today" || !valid_date(&document.local_date) {
        return Err("adapter returned an invalid today snapshot".to_owned());
    }
    let mut items = Vec::with_capacity(document.events.len());
    for event in document.events {
        let start_epoch = event.start_utc.as_deref().map(parse_rfc3339).transpose()?;
        let end_epoch = event.end_utc.as_deref().map(parse_rfc3339).transpose()?;
        if event.all_day {
            if event.start_date.is_none() || event.end_date.is_none() {
                return Err("adapter returned an invalid all-day event".to_owned());
            }
        } else if event.start.is_none()
            || event.end.is_none()
            || start_epoch.is_none()
            || end_epoch.is_none()
        {
            return Err("adapter returned an invalid timed event".to_owned());
        }
        let occurrence_id = occurrence_id(&event, start_epoch);
        let (local_hour, local_minute) = start_epoch.map(local_time).unzip();
        items.push(AgendaItem {
            occurrence_id,
            calendar_id: event.calendar_id,
            event_id: event.id,
            recurring_event_id: event.recurring_event_id,
            original_start: event.original_start_time_utc.or_else(|| {
                event.original_start_time.and_then(|value| {
                    value
                        .get("dateTime")
                        .and_then(|value| value.as_str())
                        .map(str::to_owned)
                })
            }),
            start_epoch,
            end_epoch,
            local_hour,
            local_minute,
            all_day: event.all_day,
            title: if event.summary.trim().is_empty() {
                "(Sem título)".to_owned()
            } else {
                event.summary
            },
            status: event.status,
            timezone: event.timezone,
        });
    }
    Ok(CalendarSnapshot {
        agenda: TodayAgenda {
            local_date: document.local_date,
            generated_at: document.generated_at,
            source_timezone: document.timezone,
            items,
            status: crate::core::AgendaStatus::Fresh,
        },
    })
}

fn occurrence_id(event: &AdapterEvent, start_epoch: Option<i64>) -> String {
    let identity = event
        .recurring_event_id
        .as_deref()
        .zip(event.original_start_time_utc.as_deref())
        .map(|(series, original)| format!("recurring:{series}:{original}"))
        .unwrap_or_else(|| format!("event:{}:{}", event.id, start_epoch.unwrap_or_default()));
    format!("{}:{identity}", event.calendar_id)
}

fn valid_date(value: &str) -> bool {
    let mut parts = value.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    year.parse::<i32>().is_ok()
        && month
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
        && day.parse::<u8>().is_ok_and(|day| (1..=31).contains(&day))
}

fn parse_rfc3339(value: &str) -> Result<i64, String> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err("adapter returned an invalid RFC3339 time".to_owned());
    }
    let number = |start: usize, end: usize| {
        value[start..end]
            .parse::<i64>()
            .map_err(|_| "adapter returned an invalid RFC3339 time".to_owned())
    };
    let year = number(0, 4)? as i32;
    let month = number(5, 7)? as u8;
    let day = number(8, 10)? as u8;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let tail = &value[19..];
    let (offset_minutes, _) = if tail.starts_with('Z') {
        (0_i64, 1)
    } else {
        if tail.len() < 6 || !matches!(&tail[0..1], "+" | "-") || &tail[3..4] != ":" {
            return Err("adapter returned an invalid RFC3339 offset".to_owned());
        }
        let sign = if &tail[0..1] == "-" { -1 } else { 1 };
        let hours = tail[1..3]
            .parse::<i64>()
            .map_err(|_| "adapter returned an invalid RFC3339 offset".to_owned())?;
        let minutes = tail[4..6]
            .parse::<i64>()
            .map_err(|_| "adapter returned an invalid RFC3339 offset".to_owned())?;
        (sign * (hours * 60 + minutes), 6)
    };
    if month == 0
        || month > 12
        || day == 0
        || day > crate::calendar::days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err("adapter returned an out-of-range RFC3339 time".to_owned());
    }
    Ok(
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
            - offset_minutes * 60,
    )
}

fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = i64::from(year).div_euclid(400);
    let year_of_era = i64::from(year).rem_euclid(400);
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn local_time(epoch: i64) -> (u8, u8) {
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    unsafe {
        libc::localtime_r(&epoch, &mut local);
    }
    (local.tm_hour as u8, local.tm_min as u8)
}

#[cfg(test)]
mod tests {
    use super::parse_snapshot;

    fn document(event: &str) -> Vec<u8> {
        format!(
            r#"{{"generated_at":"2026-09-21T17:00:00Z","timezone":"America/Sao_Paulo","local_date":"2026-09-21","mode":"today","events":[{event}]}}"#
        )
        .into_bytes()
    }

    #[test]
    fn parses_synthetic_snapshot_and_occurrence_identity() {
        let snapshot = parse_snapshot(br#"{"generated_at":"2026-09-21T17:00:00Z","timezone":"America/Sao_Paulo","local_date":"2026-09-21","mode":"today","events":[{"calendar_id":"calendar-test","id":"event-123","recurring_event_id":"series-1","original_start_time":{"dateTime":"2026-09-14T14:30:00-03:00"},"original_start_time_utc":"2026-09-14T17:30:00Z","summary":"Evento recorrente","start":"2026-09-21T14:30:00-03:00","end":"2026-09-21T15:00:00-03:00","start_utc":"2026-09-21T17:30:00Z","end_utc":"2026-09-21T18:00:00Z","timezone":"America/Sao_Paulo","start_date":null,"end_date":null,"all_day":false,"status":"confirmed"}]}"#).unwrap();
        assert_eq!(
            snapshot.agenda.items[0].occurrence_id,
            "calendar-test:recurring:series-1:2026-09-14T17:30:00Z"
        );
    }

    #[test]
    fn rejects_malformed_or_invalid_snapshots() {
        assert!(parse_snapshot(b"not-json").is_err());
        assert!(parse_snapshot(
            br#"{"generated_at":"x","timezone":"x","local_date":"bad","mode":"today","events":[]}"#
        )
        .is_err());
    }

    #[test]
    fn preserves_all_day_dates_and_normalizes_empty_title() {
        let snapshot = parse_snapshot(&document(
            r#"{"calendar_id":"calendar-test","id":"event-all-day","recurring_event_id":null,"original_start_time":null,"original_start_time_utc":null,"summary":"","start":null,"end":null,"start_utc":null,"end_utc":null,"timezone":null,"start_date":"2026-09-21","end_date":"2026-09-22","all_day":true,"status":"cancelled"}"#,
        ))
        .unwrap();
        let item = &snapshot.agenda.items[0];
        assert!(item.all_day);
        assert_eq!(item.title, "(Sem título)");
        assert_eq!(item.status, "cancelled");
    }

    #[test]
    fn rejects_missing_timed_fields() {
        assert!(parse_snapshot(&document(
            r#"{"calendar_id":"calendar-test","id":"event-missing","summary":"Evento A","all_day":false,"status":"confirmed","start":null,"end":null,"start_utc":null,"end_utc":null}"#,
        ))
        .is_err());
    }
}
