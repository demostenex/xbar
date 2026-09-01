use crate::core::{Event, Notification, NotificationId};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_EXPIRE: Duration = Duration::from_secs(5);
pub const REASON_EXPIRED: u32 = 1;
pub const REASON_CLOSED: u32 = 3;

struct Record {
    notification: Notification,
    deadline: Option<Instant>,
}

#[derive(Default)]
pub struct Store {
    next_id: u32,
    records: BTreeMap<NotificationId, Record>,
}

impl Store {
    pub fn notify(
        &mut self,
        replaces_id: u32,
        app_name: String,
        summary: String,
        body: String,
        expire_timeout: i32,
    ) -> NotificationId {
        let id = if replaces_id != 0 && self.records.contains_key(&NotificationId(replaces_id)) {
            NotificationId(replaces_id)
        } else {
            self.allocate_id()
        };
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
                    app_name,
                    summary,
                    body,
                },
                deadline,
            },
        );
        id
    }

    fn allocate_id(&mut self) -> NotificationId {
        loop {
            self.next_id = self.next_id.wrapping_add(1);
            if self.next_id != 0 && !self.records.contains_key(&NotificationId(self.next_id)) {
                return NotificationId(self.next_id);
            }
        }
    }

    pub fn close(&mut self, id: NotificationId) -> bool {
        self.records.remove(&id).is_some()
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
            self.records.remove(id);
        }
        ids
    }

    pub fn snapshot(&self) -> Vec<Notification> {
        self.records
            .values()
            .map(|record| record.notification.clone())
            .collect()
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
    let snapshot = store
        .lock()
        .expect("notification store poisoned")
        .snapshot();
    crate::dbus::push_event(events, wake, Event::NotificationsSnapshot(snapshot));
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
    let snapshot = store
        .lock()
        .expect("notification store poisoned")
        .snapshot();
    crate::dbus::push_event(events, wake, Event::NotificationsSnapshot(snapshot));
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
