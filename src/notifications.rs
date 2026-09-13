use crate::core::{
    Event, HistoryEntryId, Notification, NotificationHistoryEntry, NotificationId,
    NotificationSource, WindowId,
};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use zbus::zvariant::OwnedValue;

pub const DEFAULT_EXPIRE: Duration = Duration::from_secs(5);
pub const REASON_EXPIRED: u32 = 1;
pub const REASON_DISMISSED: u32 = 2;
pub const REASON_CLOSED: u32 = 3;
pub const MAX_SOUND_HINT_LENGTH: usize = 256;

pub fn unix_epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationSoundRequest {
    Default,
    Named(String),
    File(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SoundDecision {
    Silent,
    Play(NotificationSoundRequest),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedSoundHints {
    pub sound_name: Option<String>,
    pub sound_file: Option<String>,
    pub suppress_sound: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryKind {
    New,
    Replacement,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationAction {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionInvocation {
    pub notification_id: NotificationId,
    pub action_key: String,
    pub close_reason: Option<u32>,
    pub pending_changed: bool,
}

pub fn parse_notification_actions(values: Vec<String>) -> Vec<NotificationAction> {
    values
        .chunks_exact(2)
        .map(|pair| NotificationAction {
            key: pair[0].clone(),
            label: pair[1].clone(),
        })
        .collect()
}

pub fn parse_resident_hint(hints: &std::collections::HashMap<String, OwnedValue>) -> bool {
    hints
        .get("resident")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}

fn bounded_string(value: &OwnedValue) -> Option<String> {
    let value: &str = value.downcast_ref().ok()?;
    (value.len() <= MAX_SOUND_HINT_LENGTH).then(|| value.to_owned())
}

pub fn parse_sound_hints(
    hints: &std::collections::HashMap<String, OwnedValue>,
) -> ParsedSoundHints {
    ParsedSoundHints {
        sound_name: hints.get("sound-name").and_then(bounded_string),
        sound_file: hints.get("sound-file").and_then(bounded_string),
        suppress_sound: hints
            .get("suppress-sound")
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(false),
    }
}

pub fn decide_notification_sound(
    delivery: DeliveryKind,
    hints: &ParsedSoundHints,
) -> SoundDecision {
    if delivery == DeliveryKind::Replacement || hints.suppress_sound {
        return SoundDecision::Silent;
    }
    if let Some(file) = &hints.sound_file {
        return SoundDecision::Play(NotificationSoundRequest::File(file.clone()));
    }
    if let Some(name) = &hints.sound_name {
        return SoundDecision::Play(NotificationSoundRequest::Named(name.clone()));
    }
    SoundDecision::Play(NotificationSoundRequest::Default)
}

struct Record {
    notification: Notification,
    deadline: Option<Instant>,
    pending_history_id: Option<HistoryEntryId>,
    actions: Vec<NotificationAction>,
    resident: bool,
}

#[derive(Default)]
pub struct Store {
    next_id: u32,
    next_history_id: u64,
    next_order: u64,
    records: BTreeMap<NotificationId, Record>,
    history: Vec<NotificationHistoryEntry>,
}

impl Store {
    #[allow(dead_code)]
    pub fn notify(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
    ) -> NotificationId {
        self.notify_with_disposition(replaces_id, app_name, summary, body, expire_timeout)
            .0
    }

    pub fn notify_with_disposition(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
    ) -> (NotificationId, DeliveryKind) {
        self.notify_with_disposition_at(
            replaces_id,
            app_name,
            summary,
            body,
            expire_timeout,
            unix_epoch_millis(),
        )
    }

    pub fn notify_with_disposition_at(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
        now: u64,
    ) -> (NotificationId, DeliveryKind) {
        self.notify_with_disposition_at_with_actions(
            replaces_id,
            app_name,
            summary,
            body,
            expire_timeout,
            now,
            Vec::new(),
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn notify_with_disposition_at_with_actions(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
        now: u64,
        actions: Vec<NotificationAction>,
        resident: bool,
    ) -> (NotificationId, DeliveryKind) {
        let replacing = replaces_id != 0 && self.records.contains_key(&NotificationId(replaces_id));
        let id = if replaces_id != 0 && self.records.contains_key(&NotificationId(replaces_id)) {
            NotificationId(replaces_id)
        } else {
            self.allocate_id()
        };
        let pending_history_id = self
            .records
            .get(&id)
            .and_then(|record| record.pending_history_id);
        let deadline = if expire_timeout == 0 {
            None
        } else {
            let duration = if expire_timeout < 0 {
                DEFAULT_EXPIRE
            } else {
                Duration::from_millis(expire_timeout as u64)
            };
            Some(Instant::now() + duration)
        };
        self.records.insert(
            id,
            Record {
                notification: Notification {
                    id,
                    source: NotificationSource::Freedesktop,
                    window_id: None,
                    app_name,
                    summary,
                    body,
                },
                deadline,
                pending_history_id,
                actions,
                resident,
            },
        );
        self.next_order = self.next_order.wrapping_add(1);
        if let Some(history_id) = pending_history_id {
            if let Some(entry) = self.history.iter_mut().find(|entry| entry.id == history_id) {
                entry.live_notification_id = Some(id);
                entry.app_name = self.records[&id].notification.app_name.clone();
                entry.summary = self.records[&id].notification.summary.clone();
                entry.body = self.records[&id].notification.body.clone();
                entry.order = self.next_order;
                entry.updated_at = now;
                self.history
                    .sort_by_key(|entry| std::cmp::Reverse(entry.order));
            } else {
                self.create_pending_history(id, now);
            }
        } else {
            self.create_pending_history(id, now);
        }
        (
            id,
            if replacing {
                DeliveryKind::Replacement
            } else {
                DeliveryKind::New
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn notify_with_actions(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
        actions: Vec<NotificationAction>,
        resident: bool,
    ) -> (NotificationId, DeliveryKind) {
        self.notify_with_disposition_at_with_actions(
            replaces_id,
            app_name,
            summary,
            body,
            expire_timeout,
            unix_epoch_millis(),
            actions,
            resident,
        )
    }

    pub fn attention(&mut self, window: WindowId, app_name: String, active: bool) {
        let existing = self.records.iter().find_map(|(id, record)| {
            (record.notification.source == NotificationSource::WindowAttention
                && record.notification.window_id == Some(window))
            .then_some(*id)
        });
        match (active, existing) {
            (true, Some(id)) => {
                if let Some(record) = self.records.get_mut(&id) {
                    record.notification.app_name = app_name.clone();
                    record.notification.summary = format!("{app_name} needs attention");
                }
            }
            (true, None) => {
                let id = self.allocate_id();
                self.records.insert(
                    id,
                    Record {
                        notification: Notification {
                            id,
                            source: NotificationSource::WindowAttention,
                            window_id: Some(window),
                            app_name: app_name.clone(),
                            summary: format!("{app_name} needs attention"),
                            body: "A window is requesting attention".into(),
                        },
                        deadline: None,
                        pending_history_id: None,
                        actions: Vec::new(),
                        resident: false,
                    },
                );
            }
            (false, Some(id)) => {
                self.records.remove(&id);
            }
            (false, None) => {}
        }
    }

    fn allocate_id(&mut self) -> NotificationId {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id != 0 && !self.records.contains_key(&NotificationId(self.next_id)) {
                return NotificationId(self.next_id);
            }
        }
    }

    fn allocate_history_id(&mut self) -> HistoryEntryId {
        loop {
            self.next_history_id = self.next_history_id.wrapping_add(1);
            if self.next_history_id != 0
                && !self
                    .history
                    .iter()
                    .any(|entry| entry.id == HistoryEntryId(self.next_history_id))
            {
                return HistoryEntryId(self.next_history_id);
            }
        }
    }

    fn create_pending_history(&mut self, id: NotificationId, now: u64) {
        let history_id = self.allocate_history_id();
        let notification = &self.records[&id].notification;
        self.history.insert(
            0,
            NotificationHistoryEntry {
                id: history_id,
                live_notification_id: Some(id),
                source: notification.source.clone(),
                app_name: notification.app_name.clone(),
                summary: notification.summary.clone(),
                body: notification.body.clone(),
                order: self.next_order,
                received_at: now,
                updated_at: now,
            },
        );
        if let Some(record) = self.records.get_mut(&id) {
            record.pending_history_id = Some(history_id);
        }
        self.evict_history_overflow();
    }

    fn evict_history_overflow(&mut self) {
        while self.history.len() > 50 {
            if let Some(evicted) = self.history.pop() {
                if let Some(notification_id) = evicted.live_notification_id {
                    if let Some(record) = self.records.get_mut(&notification_id) {
                        if record.pending_history_id == Some(evicted.id) {
                            record.pending_history_id = None;
                        }
                    }
                }
            }
        }
    }

    pub fn dismiss_history_entry(&mut self, id: HistoryEntryId) -> bool {
        let Some(index) = self.history.iter().position(|entry| entry.id == id) else {
            return false;
        };
        let entry = self.history.remove(index);
        if let Some(notification_id) = entry.live_notification_id {
            if let Some(record) = self.records.get_mut(&notification_id) {
                if record.pending_history_id == Some(id) {
                    record.pending_history_id = None;
                }
            }
        }
        true
    }

    pub fn restore_pending(&mut self, entries: Vec<NotificationHistoryEntry>) {
        self.next_order = entries.iter().map(|entry| entry.order).max().unwrap_or(0);
        self.history = entries;
        self.next_history_id = self
            .history
            .iter()
            .map(|entry| entry.id.0)
            .max()
            .unwrap_or(0);
    }

    pub fn invoke_default_action(
        &mut self,
        history_id: HistoryEntryId,
    ) -> Option<ActionInvocation> {
        let entry = self.history.iter().find(|entry| entry.id == history_id)?;
        let notification_id = entry.live_notification_id?;
        let record = self.records.get(&notification_id)?;
        if !record.actions.iter().any(|action| action.key == "default") {
            return None;
        }
        let resident = record.resident;
        if !resident {
            self.records.remove(&notification_id);
            self.history.retain(|entry| entry.id != history_id);
        }
        Some(ActionInvocation {
            notification_id,
            action_key: "default".into(),
            close_reason: (!resident).then_some(REASON_DISMISSED),
            pending_changed: !resident,
        })
    }

    pub fn clear_history(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        for entry in &self.history {
            if let Some(notification_id) = entry.live_notification_id {
                if let Some(record) = self.records.get_mut(&notification_id) {
                    if record.pending_history_id == Some(entry.id) {
                        record.pending_history_id = None;
                    }
                }
            }
        }
        self.history.clear();
        true
    }

    pub fn close(&mut self, id: NotificationId) -> bool {
        let record = self.records.remove(&id);
        if let Some(record) = record {
            if let Some(history_id) = record.pending_history_id {
                self.history.retain(|entry| entry.id != history_id);
            }
            true
        } else {
            false
        }
    }

    pub fn expired(&mut self, now: Instant) -> Vec<NotificationId> {
        let ids = self
            .records
            .iter()
            .filter_map(|(id, record)| {
                record
                    .deadline
                    .is_some_and(|deadline| deadline <= now)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in &ids {
            if let Some(record) = self.records.remove(id) {
                if let Some(history_id) = record.pending_history_id {
                    if let Some(entry) =
                        self.history.iter_mut().find(|entry| entry.id == history_id)
                    {
                        entry.live_notification_id = None;
                    }
                }
            }
        }
        ids
    }

    pub fn snapshot(&self) -> Vec<Notification> {
        self.records
            .values()
            .map(|record| record.notification.clone())
            .collect()
    }

    pub fn history_snapshot(&self) -> Vec<NotificationHistoryEntry> {
        self.history.clone()
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.records
            .values()
            .filter_map(|record| record.deadline)
            .min()
    }
}

pub struct DeadlineTimer {
    fd: RawFd,
}

impl DeadlineTimer {
    pub fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::timerfd_create(
                libc::CLOCK_MONOTONIC,
                libc::TFD_CLOEXEC | libc::TFD_NONBLOCK,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self { fd })
        }
    }

    pub fn rearm(&self, deadline: Option<Instant>) -> io::Result<()> {
        let value = deadline.map_or(
            libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
                it_value: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
            },
            |deadline| {
                let duration = deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_nanos(1));
                libc::itimerspec {
                    it_interval: libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    },
                    it_value: libc::timespec {
                        tv_sec: duration.as_secs() as i64,
                        tv_nsec: duration.subsec_nanos() as i64,
                    },
                }
            },
        );
        if unsafe { libc::timerfd_settime(self.fd, 0, &value, std::ptr::null_mut()) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn consume(&self) -> io::Result<()> {
        let mut expirations = 0_u64;
        let result = unsafe {
            libc::read(
                self.fd,
                (&mut expirations as *mut u64).cast::<libc::c_void>(),
                8,
            )
        };
        if result == 8
            || (result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock)
        {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl AsRawFd for DeadlineTimer {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for DeadlineTimer {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

pub type SharedStore = Arc<Mutex<Store>>;
pub type SharedTimer = Arc<Mutex<DeadlineTimer>>;

pub fn publish(
    store: &SharedStore,
    timer: &SharedTimer,
    events: &Arc<Mutex<std::collections::VecDeque<Event>>>,
    wake: &Arc<Mutex<UnixStream>>,
) {
    let deadline = store
        .lock()
        .expect("notification store poisoned")
        .next_deadline();
    let _ = timer
        .lock()
        .expect("notification timer poisoned")
        .rearm(deadline);
    let (snapshot, history) = {
        let store = store.lock().expect("notification store poisoned");
        (store.snapshot(), store.history_snapshot())
    };
    crate::dbus::push_event(
        events,
        wake,
        Event::NotificationsState {
            active: snapshot,
            history,
        },
    );
}

pub fn expire(
    store: &SharedStore,
    timer: &SharedTimer,
    events: &Arc<Mutex<std::collections::VecDeque<Event>>>,
    wake: &Arc<Mutex<UnixStream>>,
) -> Vec<NotificationId> {
    let _ = timer.lock().expect("notification timer poisoned").consume();
    let ids = store
        .lock()
        .expect("notification store poisoned")
        .expired(Instant::now());
    let deadline = store
        .lock()
        .expect("notification store poisoned")
        .next_deadline();
    let _ = timer
        .lock()
        .expect("notification timer poisoned")
        .rearm(deadline);
    let (snapshot, history) = {
        let store = store.lock().expect("notification store poisoned");
        (store.snapshot(), store.history_snapshot())
    };
    crate::dbus::push_event(
        events,
        wake,
        Event::NotificationsState {
            active: snapshot,
            history,
        },
    );
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(values: &[(&str, OwnedValue)]) -> std::collections::HashMap<String, OwnedValue> {
        values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    fn string_value(value: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(value.to_owned())).unwrap()
    }

    fn bool_value(value: bool) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(value)).unwrap()
    }

    fn sound(delivery: DeliveryKind, values: &[(&str, OwnedValue)]) -> SoundDecision {
        let parsed = parse_sound_hints(&hints(values));
        decide_notification_sound(delivery, &parsed)
    }

    #[test]
    fn sound_policy_defaults_and_selects_name_or_file() {
        assert_eq!(
            sound(DeliveryKind::New, &[]),
            SoundDecision::Play(NotificationSoundRequest::Default)
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[("sound-name", string_value("message-new"))]
            ),
            SoundDecision::Play(NotificationSoundRequest::Named("message-new".into()))
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[("sound-file", string_value("/tmp/notify.wav"))]
            ),
            SoundDecision::Play(NotificationSoundRequest::File("/tmp/notify.wav".into()))
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[
                    ("sound-name", string_value("message-new")),
                    ("sound-file", string_value("/tmp/notify.wav")),
                ],
            ),
            SoundDecision::Play(NotificationSoundRequest::File("/tmp/notify.wav".into()))
        );
    }

    #[test]
    fn sound_policy_suppression_overrides_every_request() {
        for values in [
            vec![("suppress-sound", bool_value(true))],
            vec![
                ("suppress-sound", bool_value(true)),
                ("sound-name", string_value("message-new")),
            ],
            vec![
                ("suppress-sound", bool_value(true)),
                ("sound-file", string_value("/tmp/notify.wav")),
            ],
        ] {
            assert_eq!(sound(DeliveryKind::New, &values), SoundDecision::Silent);
        }
        assert_eq!(
            sound(DeliveryKind::New, &[("suppress-sound", bool_value(false))]),
            SoundDecision::Play(NotificationSoundRequest::Default)
        );
    }

    #[test]
    fn sound_policy_replacement_is_always_silent() {
        assert_eq!(
            sound(
                DeliveryKind::Replacement,
                &[
                    ("sound-name", string_value("message-new")),
                    ("sound-file", string_value("/tmp/notify.wav")),
                ],
            ),
            SoundDecision::Silent
        );
    }

    #[test]
    fn malformed_and_oversized_sound_hints_are_ignored() {
        let malformed = hints(&[
            ("sound-name", bool_value(true)),
            ("sound-file", bool_value(true)),
            ("suppress-sound", string_value("true")),
        ]);
        assert_eq!(
            decide_notification_sound(DeliveryKind::New, &parse_sound_hints(&malformed)),
            SoundDecision::Play(NotificationSoundRequest::Default)
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[
                    ("sound-file", bool_value(true)),
                    ("sound-name", string_value("message-new")),
                ],
            ),
            SoundDecision::Play(NotificationSoundRequest::Named("message-new".into()))
        );
        let oversized = "x".repeat(MAX_SOUND_HINT_LENGTH + 1);
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[("sound-name", string_value(&oversized))]
            ),
            SoundDecision::Play(NotificationSoundRequest::Default)
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[("sound-file", string_value(&oversized))]
            ),
            SoundDecision::Play(NotificationSoundRequest::Default)
        );
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[
                    ("sound-file", string_value(&oversized)),
                    ("sound-name", string_value("message-new")),
                ],
            ),
            SoundDecision::Play(NotificationSoundRequest::Named("message-new".into()))
        );
    }

    #[test]
    fn unknown_replacement_is_new_delivery_for_sound_policy() {
        let mut store = Store::default();
        let (_, first_kind) =
            store.notify_with_disposition(99, "app".into(), "one".into(), String::new(), 0);
        assert_eq!(first_kind, DeliveryKind::New);
        assert_eq!(
            sound(
                DeliveryKind::New,
                &[("sound-name", string_value("message-new"))]
            ),
            SoundDecision::Play(NotificationSoundRequest::Named("message-new".into()))
        );
        let first = store.snapshot()[0].id;
        let (_, replacement_kind) =
            store.notify_with_disposition(first.0, "app".into(), "two".into(), String::new(), 0);
        assert_eq!(replacement_kind, DeliveryKind::Replacement);
    }

    #[test]
    fn ids_are_nonzero_and_replacement_keeps_id() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), "body".into(), 0);
        assert_ne!(first.0, 0);
        assert_eq!(
            store.notify(first.0, "app".into(), "two".into(), "body".into(), 0),
            first
        );
        assert_eq!(store.snapshot()[0].summary, "two");
    }

    #[test]
    fn unknown_replacement_allocates_new_id() {
        let mut store = Store::default();
        let id = store.notify(42, "app".into(), "one".into(), String::new(), 0);
        assert_ne!(id.0, 42);
    }

    #[test]
    fn zero_timeout_has_no_deadline() {
        let mut store = Store::default();
        store.notify(0, "app".into(), "one".into(), String::new(), 0);
        assert!(store.next_deadline().is_none());
    }

    #[test]
    fn positive_timeout_expires_and_removes_notification() {
        let mut store = Store::default();
        store.notify(0, "app".into(), "one".into(), String::new(), 1);
        let expired = store.expired(Instant::now() + Duration::from_secs(1));
        assert_eq!(expired.len(), 1);
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn close_removes_only_existing_notification() {
        let mut store = Store::default();
        let id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        assert!(store.close(id));
        assert!(!store.close(id));
    }

    #[test]
    fn attention_is_deduplicated_by_window_and_cleared() {
        let mut store = Store::default();
        let window = WindowId(77);
        store.attention(window, "Editor".into(), true);
        store.attention(window, "Editor".into(), true);
        assert_eq!(store.snapshot().len(), 1);
        assert_eq!(
            store.snapshot()[0].source,
            NotificationSource::WindowAttention
        );
        store.attention(window, "Editor".into(), false);
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn attention_windows_have_distinct_notifications() {
        let mut store = Store::default();
        store.attention(WindowId(1), "One".into(), true);
        store.attention(WindowId(2), "Two".into(), true);
        assert_eq!(store.snapshot().len(), 2);
    }

    #[test]
    fn history_is_newest_first_and_replacement_moves_without_duplicate() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), "body".into(), 0);
        let second = store.notify(0, "app".into(), "two".into(), "body".into(), 0);
        assert_eq!(
            store
                .history_snapshot()
                .iter()
                .map(|e| e.live_notification_id)
                .collect::<Vec<_>>(),
            vec![Some(second), Some(first)]
        );
        let replaced = store.notify(first.0, "app".into(), "updated".into(), "body".into(), 0);
        let history = store.history_snapshot();
        assert_eq!(replaced, first);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].live_notification_id, Some(first));
        assert_eq!(history[0].summary, "updated");
    }

    #[test]
    fn history_is_bounded_and_expiry_retains_entry() {
        let mut store = Store::default();
        for index in 0..51 {
            store.notify(0, "app".into(), index.to_string(), String::new(), 1);
        }
        assert_eq!(store.history_snapshot().len(), 50);
        let id = store.history_snapshot()[0].id;
        store.expired(Instant::now() + Duration::from_secs(2));
        assert!(store.snapshot().is_empty());
        assert!(store.history_snapshot().iter().any(|entry| entry.id == id));
    }

    #[test]
    fn close_removes_history_but_attention_never_enters_it() {
        let mut store = Store::default();
        let id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        assert_eq!(store.history_snapshot().len(), 1);
        assert!(store.close(id));
        assert!(store.history_snapshot().is_empty());
        store.attention(WindowId(1), "Editor".into(), true);
        assert!(store.history_snapshot().is_empty());
    }

    #[test]
    fn pending_entries_have_bidirectional_local_identity_links() {
        let mut store = Store::default();
        let (notification_id, disposition) =
            store.notify_with_disposition(0, "app".into(), "one".into(), String::new(), 0);
        assert_eq!(disposition, DeliveryKind::New);
        let entry = &store.history[0];
        assert_eq!(entry.live_notification_id, Some(notification_id));
        assert_eq!(
            store.records[&notification_id].pending_history_id,
            Some(entry.id)
        );
    }

    #[test]
    fn separate_deliveries_have_distinct_protocol_and_history_ids() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let second = store.notify(0, "app".into(), "two".into(), String::new(), 0);
        assert_ne!(first, second);
        assert_ne!(store.history[0].id, store.history[1].id);
    }

    #[test]
    fn replacement_pending_preserves_history_entry_id() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let history_id = store.history[0].id;
        let (replaced, disposition) =
            store.notify_with_disposition(first.0, "app".into(), "two".into(), String::new(), 0);
        assert_eq!(replaced, first);
        assert_eq!(disposition, DeliveryKind::Replacement);
        assert_eq!(store.history.len(), 1);
        assert_eq!(store.history[0].id, history_id);
    }

    #[test]
    fn timestamps_new_and_pending_replacement_follow_the_local_history_contract() {
        let mut store = Store::default();
        let first =
            store.notify_with_disposition_at(0, "app".into(), "one".into(), String::new(), 0, 100);
        let original = store.history[0].clone();
        assert_eq!(first.1, DeliveryKind::New);
        assert_eq!((original.received_at, original.updated_at), (100, 100));
        store.notify_with_disposition_at(
            first.0 .0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
            200,
        );
        assert_eq!(store.history[0].id, original.id);
        assert_eq!(store.history[0].received_at, 100);
        assert_eq!(store.history[0].updated_at, 200);
    }

    #[test]
    fn replacement_after_dismiss_and_clear_gets_new_timestamped_history_entries() {
        let mut store = Store::default();
        let notification_id = store
            .notify_with_disposition_at(0, "app".into(), "one".into(), String::new(), 0, 100)
            .0;
        let dismissed_id = store.history[0].id;
        assert!(store.dismiss_history_entry(dismissed_id));
        store.notify_with_disposition_at(
            notification_id.0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
            300,
        );
        let replacement_id = store.history[0].id;
        assert_ne!(replacement_id, dismissed_id);
        assert_eq!(
            (store.history[0].received_at, store.history[0].updated_at),
            (300, 300)
        );

        assert!(store.clear_history());
        store.notify_with_disposition_at(
            notification_id.0,
            "app".into(),
            "three".into(),
            String::new(),
            0,
            400,
        );
        assert_ne!(store.history[0].id, replacement_id);
        assert_eq!(
            (store.history[0].received_at, store.history[0].updated_at),
            (400, 400)
        );
    }

    #[test]
    fn notification_actions_parse_complete_pairs_and_ignore_odd_tail() {
        assert!(parse_notification_actions(Vec::new()).is_empty());
        assert_eq!(
            parse_notification_actions(vec![
                "default".into(),
                "Open".into(),
                "reply".into(),
                "Reply".into(),
                "orphan".into(),
            ]),
            vec![
                NotificationAction {
                    key: "default".into(),
                    label: "Open".into(),
                },
                NotificationAction {
                    key: "reply".into(),
                    label: "Reply".into(),
                },
            ]
        );
    }

    #[test]
    fn resident_hint_accepts_only_a_boolean_true() {
        assert!(parse_resident_hint(&hints(&[(
            "resident",
            bool_value(true)
        )])));
        assert!(!parse_resident_hint(&hints(&[])));
        assert!(!parse_resident_hint(&hints(&[(
            "resident",
            string_value("true")
        )])));
    }

    #[test]
    fn replacement_replaces_the_complete_live_action_set() {
        let mut unknown_replacement = Store::default();
        let (unknown_id, disposition) = unknown_replacement
            .notify_with_disposition_at_with_actions(
                999,
                "app".into(),
                "new".into(),
                String::new(),
                0,
                50,
                vec![NotificationAction {
                    key: "default".into(),
                    label: "Open".into(),
                }],
                false,
            );
        assert_eq!(disposition, DeliveryKind::New);
        assert_eq!(unknown_replacement.records[&unknown_id].actions.len(), 1);

        let mut store = Store::default();
        let id = store
            .notify_with_disposition_at_with_actions(
                0,
                "app".into(),
                "one".into(),
                String::new(),
                0,
                100,
                vec![
                    NotificationAction {
                        key: "default".into(),
                        label: "Open".into(),
                    },
                    NotificationAction {
                        key: "reply".into(),
                        label: "Reply".into(),
                    },
                ],
                false,
            )
            .0;
        store.notify_with_disposition_at_with_actions(
            id.0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
            200,
            vec![NotificationAction {
                key: "archive".into(),
                label: "Archive".into(),
            }],
            false,
        );
        assert_eq!(
            store.records[&id].actions,
            vec![NotificationAction {
                key: "archive".into(),
                label: "Archive".into(),
            }]
        );
        assert!(store.invoke_default_action(store.history[0].id).is_none());

        let mut removed_default = Store::default();
        let id = removed_default
            .notify_with_disposition_at_with_actions(
                0,
                "app".into(),
                "one".into(),
                String::new(),
                0,
                100,
                vec![NotificationAction {
                    key: "default".into(),
                    label: "Open".into(),
                }],
                false,
            )
            .0;
        let history_id = removed_default.history[0].id;
        removed_default.notify_with_disposition_at_with_actions(
            id.0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
            200,
            Vec::new(),
            false,
        );
        assert!(removed_default.records[&id].actions.is_empty());
        assert!(removed_default.invoke_default_action(history_id).is_none());
    }

    #[test]
    fn default_action_invocation_has_resident_and_non_resident_effects() {
        let mut non_resident = Store::default();
        let id = non_resident
            .notify_with_disposition_at_with_actions(
                0,
                "app".into(),
                "one".into(),
                String::new(),
                0,
                100,
                vec![NotificationAction {
                    key: "default".into(),
                    label: "Open".into(),
                }],
                false,
            )
            .0;
        let history_id = non_resident.history[0].id;
        let effect = non_resident
            .invoke_default_action(history_id)
            .expect("default action should resolve");
        assert_eq!(effect.notification_id, id);
        assert_eq!(effect.action_key, "default");
        assert_eq!(effect.close_reason, Some(REASON_DISMISSED));
        assert!(effect.pending_changed);
        assert!(non_resident.records.is_empty());
        assert!(non_resident.history.is_empty());
        assert!(non_resident.invoke_default_action(history_id).is_none());

        let (replacement_id, disposition) = non_resident.notify_with_disposition_at_with_actions(
            id.0,
            "app".into(),
            "replacement".into(),
            String::new(),
            0,
            200,
            vec![NotificationAction {
                key: "default".into(),
                label: "Open again".into(),
            }],
            false,
        );
        assert_ne!(replacement_id, id);
        assert_eq!(disposition, DeliveryKind::New);
        assert_ne!(non_resident.history[0].id, history_id);

        let mut resident = Store::default();
        let id = resident
            .notify_with_disposition_at_with_actions(
                0,
                "app".into(),
                "one".into(),
                String::new(),
                0,
                100,
                vec![NotificationAction {
                    key: "default".into(),
                    label: "Open".into(),
                }],
                true,
            )
            .0;
        let history_id = resident.history[0].id;
        let timestamps = (
            resident.history[0].received_at,
            resident.history[0].updated_at,
        );
        for _ in 0..2 {
            let effect = resident
                .invoke_default_action(history_id)
                .expect("resident action should remain invocable");
            assert_eq!(
                effect,
                ActionInvocation {
                    notification_id: id,
                    action_key: "default".into(),
                    close_reason: None,
                    pending_changed: false,
                }
            );
        }
        assert!(resident.records.contains_key(&id));
        assert_eq!(resident.history.len(), 1);
        assert_eq!(
            (
                resident.history[0].received_at,
                resident.history[0].updated_at
            ),
            timestamps
        );
    }

    #[test]
    fn default_action_requires_live_pending_identity_and_active_record() {
        let mut restored = Store::default();
        restored.restore_pending(vec![NotificationHistoryEntry {
            id: HistoryEntryId(7),
            live_notification_id: None,
            source: NotificationSource::Freedesktop,
            app_name: "app".into(),
            summary: "restored".into(),
            body: String::new(),
            order: 1,
            received_at: 10,
            updated_at: 10,
        }]);
        assert!(restored.invoke_default_action(HistoryEntryId(7)).is_none());
        assert!(restored.invoke_default_action(HistoryEntryId(99)).is_none());

        let mut stale_link = Store::default();
        stale_link.restore_pending(vec![NotificationHistoryEntry {
            id: HistoryEntryId(8),
            live_notification_id: Some(NotificationId(123)),
            source: NotificationSource::Freedesktop,
            app_name: "app".into(),
            summary: "stale".into(),
            body: String::new(),
            order: 1,
            received_at: 10,
            updated_at: 10,
        }]);
        assert!(stale_link
            .invoke_default_action(HistoryEntryId(8))
            .is_none());
        assert_eq!(stale_link.history.len(), 1);

        let mut expired = Store::default();
        expired.notify_with_disposition_at_with_actions(
            0,
            "app".into(),
            "one".into(),
            String::new(),
            1,
            10,
            vec![NotificationAction {
                key: "default".into(),
                label: "Open".into(),
            }],
            true,
        );
        let history_id = expired.history[0].id;
        expired.expired(Instant::now() + Duration::from_secs(1));
        assert!(expired.invoke_default_action(history_id).is_none());
    }

    #[test]
    fn restoring_pending_history_keeps_ids_timestamps_and_has_no_active_records() {
        let mut store = Store::default();
        store.restore_pending(vec![
            NotificationHistoryEntry {
                id: HistoryEntryId(3),
                live_notification_id: None,
                source: NotificationSource::Freedesktop,
                app_name: "app".into(),
                summary: "restored".into(),
                body: "body".into(),
                order: 9,
                received_at: 100,
                updated_at: 200,
            },
            NotificationHistoryEntry {
                id: HistoryEntryId(42),
                live_notification_id: None,
                source: NotificationSource::Freedesktop,
                app_name: "app".into(),
                summary: "second".into(),
                body: "body".into(),
                order: 8,
                received_at: 300,
                updated_at: 400,
            },
        ]);
        assert!(store.records.is_empty());
        assert!(store
            .history
            .iter()
            .all(|entry| entry.live_notification_id.is_none()));
        assert_eq!(store.history[0].id, HistoryEntryId(3));
        assert_eq!(
            (store.history[0].received_at, store.history[0].updated_at),
            (100, 200)
        );
        let next = store
            .notify_with_disposition_at(0, "app".into(), "new".into(), String::new(), 0, 500)
            .0;
        assert_eq!(store.history[0].id, HistoryEntryId(43));
        assert_eq!(store.history[0].order, 10);
        assert_ne!(next, NotificationId(0));
    }

    #[test]
    fn dismiss_removes_pending_but_keeps_active_without_protocol_close() {
        let mut store = Store::default();
        let notification_id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let history_id = store.history[0].id;
        assert!(store.dismiss_history_entry(history_id));
        assert!(store
            .snapshot()
            .iter()
            .any(|item| item.id == notification_id));
        assert!(store.history.is_empty());
        assert_eq!(store.records[&notification_id].pending_history_id, None);
        assert!(!store.dismiss_history_entry(history_id));
    }

    #[test]
    fn replacement_after_dismiss_creates_new_pending_identity() {
        let mut store = Store::default();
        let notification_id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let old_history_id = store.history[0].id;
        assert!(store.dismiss_history_entry(old_history_id));
        let (replaced, disposition) = store.notify_with_disposition(
            notification_id.0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
        );
        assert_eq!(replaced, notification_id);
        assert_eq!(disposition, DeliveryKind::Replacement);
        assert_eq!(store.history.len(), 1);
        assert_ne!(store.history[0].id, old_history_id);
        assert_eq!(store.history[0].live_notification_id, Some(notification_id));
    }

    #[test]
    fn replacement_after_clear_history_reappears_with_new_identity() {
        let mut store = Store::default();
        let notification_id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let old_history_id = store.history[0].id;
        assert!(store.clear_history());
        let (replaced, disposition) = store.notify_with_disposition(
            notification_id.0,
            "app".into(),
            "two".into(),
            String::new(),
            0,
        );
        assert_eq!(replaced, notification_id);
        assert_eq!(disposition, DeliveryKind::Replacement);
        assert_ne!(store.history[0].id, old_history_id);
    }

    #[test]
    fn close_after_local_dismiss_removes_active_record_safely() {
        let mut store = Store::default();
        let notification_id = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let history_id = store.history[0].id;
        assert!(store.dismiss_history_entry(history_id));
        assert!(store.close(notification_id));
        assert!(store.snapshot().is_empty());
        assert!(store.history.is_empty());
    }

    #[test]
    fn unknown_replacement_is_new_with_new_notification_and_history_ids() {
        let mut store = Store::default();
        let (id, disposition) =
            store.notify_with_disposition(999, "app".into(), "one".into(), String::new(), 0);
        assert_eq!(disposition, DeliveryKind::New);
        assert_ne!(id, NotificationId(999));
        assert_eq!(store.history.len(), 1);
        assert_eq!(store.history[0].live_notification_id, Some(id));
    }

    #[test]
    fn clear_history_keeps_active_records_and_clears_links() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        let second = store.notify(0, "app".into(), "two".into(), String::new(), 0);
        assert!(store.clear_history());
        assert!(store.history.is_empty());
        assert_eq!(store.snapshot().len(), 2);
        assert_eq!(store.records[&first].pending_history_id, None);
        assert_eq!(store.records[&second].pending_history_id, None);
        assert!(!store.clear_history());
    }

    #[test]
    fn expiry_detaches_live_identity_but_retains_history_identity() {
        let mut store = Store::default();
        let id = store.notify(0, "app".into(), "one".into(), String::new(), 1);
        let history_id = store.history[0].id;
        let expired = store.expired(Instant::now() + Duration::from_secs(2));
        assert_eq!(expired, vec![id]);
        assert!(store.snapshot().is_empty());
        assert_eq!(store.history[0].id, history_id);
        assert_eq!(store.history[0].live_notification_id, None);
    }

    #[test]
    fn eviction_clears_evicted_active_record_link() {
        let mut store = Store::default();
        let first = store.notify(0, "app".into(), "one".into(), String::new(), 0);
        for index in 1..51 {
            store.notify(0, "app".into(), index.to_string(), String::new(), 0);
        }
        assert_eq!(store.history.len(), 50);
        assert_eq!(store.records[&first].pending_history_id, None);
        let (replacement, disposition) =
            store.notify_with_disposition(first.0, "app".into(), "again".into(), String::new(), 0);
        assert_eq!(replacement, first);
        assert_eq!(disposition, DeliveryKind::Replacement);
        assert_eq!(store.history.len(), 50);
        assert!(store
            .history
            .iter()
            .any(|entry| entry.live_notification_id == Some(first)));
    }
}
