use std::backtrace::Backtrace;
use std::cell::Cell;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::panic::{self, PanicHookInfo};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, TryLockError};
use std::time::{SystemTime, UNIX_EPOCH};

const LOG_PATH: &str = "/tmp/xbar.log";
pub const ROTATION_BYTES: u64 = 10 * 1024 * 1024;

static LOGGER: OnceLock<Logger> = OnceLock::new();

thread_local! {
    static PANIC_HOOK_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

struct PanicHookGuard;

impl PanicHookGuard {
    fn enter() -> Option<Self> {
        PANIC_HOOK_ACTIVE
            .try_with(|active| (!active.replace(true)).then_some(Self))
            .ok()
            .flatten()
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        let _ = PANIC_HOOK_ACTIVE.try_with(|active| active.set(false));
    }
}

#[derive(Clone, Copy)]
enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

struct Logger {
    path: PathBuf,
    file: Mutex<File>,
}

impl Logger {
    fn open(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Err(error) = rotate_if_needed(&path) {
            eprintln!("xbar: log rotation failed path={}: {error}", path.display());
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    fn write(&self, level: Level, marker: &str, message: &str) -> io::Result<()> {
        let line = format_line(level, marker, message);
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write_all(line.as_bytes())?;
        file.flush()
    }

    fn write_panic(&self, message: &str) {
        let line = format_line(Level::Error, "PANIC", message);
        match self.file.try_lock() {
            Ok(mut file) => {
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
            Err(TryLockError::Poisoned(poisoned)) => {
                let mut file = poisoned.into_inner();
                let _ = file.write_all(line.as_bytes());
                let _ = file.flush();
            }
            Err(TryLockError::WouldBlock) => append_direct(&self.path, &line),
        }
    }
}

pub fn init() {
    if let Some(logger) = initialize(Path::new(LOG_PATH)) {
        let _ = LOGGER.set(logger);
    }
}

pub fn info(marker: &str, message: &str) {
    write(Level::Info, marker, message);
}

#[allow(dead_code)]
pub fn warn(marker: &str, message: &str) {
    write(Level::Warn, marker, message);
}

pub fn error(marker: &str, message: &str) {
    write(Level::Error, marker, message);
}

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Some(_guard) = PanicHookGuard::enter() {
            log_panic(info);
        }
        previous(info);
    }));
}

fn write(level: Level, marker: &str, message: &str) {
    if let Some(logger) = LOGGER.get() {
        if let Err(error) = logger.write(level, marker, message) {
            eprintln!("xbar: log write failed: {error}");
        }
    }
}

fn log_panic(info: &PanicHookInfo<'_>) {
    let message = panic_message(info);
    let location = info.location().map(|location| {
        format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        )
    });
    let backtrace = Backtrace::force_capture();
    let record = format_panic_record(&message, location.as_deref(), &backtrace.to_string());
    if let Some(logger) = LOGGER.get() {
        logger.write_panic(&record);
    } else {
        append_direct(
            Path::new(LOG_PATH),
            &format_line(Level::Error, "PANIC", &record),
        );
    }
}

fn rotate_if_needed(path: &Path) -> io::Result<()> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() >= ROTATION_BYTES => {
            fs::rename(path, path.with_extension("log.old"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn initialize(path: &Path) -> Option<Logger> {
    match Logger::open(path) {
        Ok(logger) => Some(logger),
        Err(error) => {
            eprintln!("xbar: logging unavailable path={}: {error}", path.display());
            None
        }
    }
}

fn append_direct(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
        let _ = file.flush();
    }
}

fn format_line(level: Level, marker: &str, message: &str) -> String {
    format!(
        "{} {} {} {}\n",
        timestamp(),
        level.as_str(),
        marker,
        escape(message)
    )
}

fn format_panic_record(message: &str, location: Option<&str>, backtrace: &str) -> String {
    let location = location.unwrap_or("unknown");
    format!("message={message} location={location} backtrace={backtrace}")
}

fn panic_message(info: &PanicHookInfo<'_>) -> String {
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

fn timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => format!("{}.{:03}Z", duration.as_secs(), duration.subsec_millis()),
        Err(_) => "time-before-unix-epoch".to_owned(),
    }
}

fn escape(value: &str) -> String {
    value.replace('\n', "\\n").replace('\r', "\\r")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn test_dir(name: &str) -> PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("xbar-logging-{name}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn absent_log_initializes_and_writes_start() {
        let dir = test_dir("absent");
        let path = dir.join("xbar.log");
        let logger = Logger::open(&path).unwrap();
        logger.write(Level::Info, "START", "pid=1").unwrap();
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("INFO START pid=1"));
    }

    #[test]
    fn small_log_is_preserved_and_appended() {
        let dir = test_dir("append");
        let path = dir.join("xbar.log");
        fs::write(&path, "existing\n").unwrap();
        let logger = Logger::open(&path).unwrap();
        logger.write(Level::Warn, "TEST", "continued").unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.starts_with("existing\n"));
        assert!(contents.contains("WARN TEST continued"));
    }

    #[test]
    fn threshold_rotates_and_replaces_old_file() {
        let dir = test_dir("rotate");
        let path = dir.join("xbar.log");
        let old_path = dir.join("xbar.log.old");
        File::create(&path)
            .unwrap()
            .set_len(ROTATION_BYTES)
            .unwrap();
        fs::write(&old_path, "stale old\n").unwrap();
        let logger = Logger::open(&path).unwrap();
        logger.write(Level::Info, "START", "pid=1").unwrap();
        assert_eq!(fs::metadata(old_path).unwrap().len(), ROTATION_BYTES);
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("INFO START pid=1"));
    }

    #[test]
    fn open_failure_is_nonfatal_to_initializer() {
        let dir = test_dir("failure");
        assert!(initialize(&dir).is_none());
    }

    #[test]
    fn levels_and_exit_are_grep_friendly() {
        let dir = test_dir("levels");
        let path = dir.join("xbar.log");
        let logger = Logger::open(&path).unwrap();
        logger.write(Level::Info, "START", "pid=1").unwrap();
        logger.write(Level::Warn, "WARN_TEST", "warning").unwrap();
        logger.write(Level::Error, "ERROR_TEST", "failure").unwrap();
        logger.write(Level::Info, "EXIT", "normal").unwrap();
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("INFO START"));
        assert!(contents.contains("WARN WARN_TEST"));
        assert!(contents.contains("ERROR ERROR_TEST"));
        assert!(contents.contains("INFO EXIT normal"));
    }

    #[test]
    fn panic_record_contains_message_location_and_backtrace() {
        let record = format_panic_record("boom", Some("src/main.rs:10:2"), "forced backtrace");
        assert!(record.contains("message=boom"));
        assert!(record.contains("location=src/main.rs:10:2"));
        assert!(record.contains("backtrace=forced backtrace"));
    }

    #[test]
    fn panic_write_uses_direct_append_when_logger_lock_is_held() {
        let dir = test_dir("panic-lock");
        let path = dir.join("xbar.log");
        let logger = Logger::open(&path).unwrap();
        let _guard = logger.file.lock().unwrap();
        logger.write_panic("message=boom backtrace=forced");
        drop(_guard);
        assert!(fs::read_to_string(path)
            .unwrap()
            .contains("PANIC message=boom"));
    }

    #[test]
    fn panic_guard_releases_after_nested_entry_is_rejected() {
        let guard = PanicHookGuard::enter().expect("first guard enters");
        assert!(PanicHookGuard::enter().is_none());
        drop(guard);
        assert!(PanicHookGuard::enter().is_some());
    }

    #[test]
    fn panic_guard_is_independent_per_thread() {
        let guard = PanicHookGuard::enter().expect("main thread guard enters");
        let other_thread_enters = std::thread::spawn(|| PanicHookGuard::enter().is_some())
            .join()
            .expect("test thread joins");
        drop(guard);
        assert!(other_thread_enters);
    }
}
