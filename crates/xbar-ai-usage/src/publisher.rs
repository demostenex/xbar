//! Session-D-Bus publication of the collector's latest encoded snapshot.

use std::sync::{Arc, RwLock};

use tokio::sync::watch;

pub const DBUS_NAME: &str = "org.xbar.AiUsage1";
pub const DBUS_PATH: &str = "/org/xbar/AiUsage1";
pub const DBUS_INTERFACE: &str = "org.xbar.AiUsage1";

#[derive(Debug)]
pub enum PublisherError {
    Dbus(zbus::Error),
    StatePoisoned,
    SignalFailed,
    NameNotPrimary(&'static str),
}

impl std::fmt::Display for PublisherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dbus(error) => write!(f, "D-Bus publisher: {error}"),
            Self::StatePoisoned => write!(f, "D-Bus publisher state lock poisoned"),
            Self::SignalFailed => write!(f, "D-Bus StateChanged publisher stopped"),
            Self::NameNotPrimary(reply) => {
                write!(f, "D-Bus name was not acquired as primary owner: {reply}")
            }
        }
    }
}

impl std::error::Error for PublisherError {}

struct AiUsageService {
    state: Arc<RwLock<Vec<u8>>>,
}

#[zbus::interface(name = "org.xbar.AiUsage1")]
impl AiUsageService {
    fn get_state(&self) -> zbus::fdo::Result<Vec<u8>> {
        self.state
            .read()
            .map(|state| state.clone())
            .map_err(|_| zbus::fdo::Error::Failed("state lock poisoned".into()))
    }

    #[zbus(signal)]
    async fn state_changed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        payload: Vec<u8>,
    ) -> zbus::Result<()>;
}

pub struct Publisher {
    state: Arc<RwLock<Vec<u8>>>,
    updates: watch::Sender<Vec<u8>>,
    errors: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl Publisher {
    pub async fn start(initial: Vec<u8>) -> Result<Self, PublisherError> {
        let state = Arc::new(RwLock::new(initial.clone()));
        let service = AiUsageService {
            state: Arc::clone(&state),
        };
        let connection = zbus::connection::Builder::session()
            .map_err(PublisherError::Dbus)?
            .allow_name_replacements(false)
            .replace_existing_names(false)
            .name(DBUS_NAME)
            .map_err(PublisherError::Dbus)?
            .serve_at(DBUS_PATH, service)
            .map_err(PublisherError::Dbus)?
            .build()
            .await
            .map_err(PublisherError::Dbus)?;
        let (updates, receiver) = watch::channel(initial);
        let (error_sender, errors) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(publish_updates(connection, receiver, error_sender));
        Ok(Self {
            state,
            updates,
            errors,
        })
    }

    pub fn take_errors(&mut self) -> tokio::sync::mpsc::UnboundedReceiver<()> {
        std::mem::replace(&mut self.errors, tokio::sync::mpsc::unbounded_channel().1)
    }

    pub fn update(&self, encoded: Vec<u8>) -> Result<(), PublisherError> {
        self.state
            .write()
            .map_err(|_| PublisherError::StatePoisoned)?
            .clone_from(&encoded);
        self.updates.send_replace(encoded);
        Ok(())
    }
}

#[cfg(test)]
fn accept_primary_owner(reply: zbus::fdo::RequestNameReply) -> Result<(), PublisherError> {
    if reply == zbus::fdo::RequestNameReply::PrimaryOwner {
        Ok(())
    } else {
        Err(PublisherError::NameNotPrimary("not-primary"))
    }
}

async fn publish_updates(
    connection: zbus::Connection,
    mut updates: watch::Receiver<Vec<u8>>,
    errors: tokio::sync::mpsc::UnboundedSender<()>,
) {
    while updates.changed().await.is_ok() {
        let payload = updates.borrow_and_update().clone();
        let Ok(emitter) = zbus::object_server::SignalEmitter::new(&connection, DBUS_PATH) else {
            eprintln!("xbar-ai-usage: D-Bus signal path unavailable");
            let _ = errors.send(());
            return;
        };
        if let Err(error) = AiUsageService::state_changed(&emitter, payload).await {
            eprintln!("xbar-ai-usage: D-Bus StateChanged failed: {error}");
            let _ = errors.send(());
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_state_replaces_pending_update() {
        let state = Arc::new(RwLock::new(vec![1]));
        let (updates, mut receiver) = watch::channel(vec![1]);
        let publisher = Publisher {
            state,
            updates,
            errors: tokio::sync::mpsc::unbounded_channel().1,
        };
        publisher.update(vec![2]).unwrap();
        publisher.update(vec![3]).unwrap();
        assert!(receiver.has_changed().unwrap());
        assert_eq!(receiver.borrow_and_update().as_slice(), &[3]);
    }

    #[test]
    fn get_state_returns_the_current_encoded_snapshot() {
        let state = Arc::new(RwLock::new(vec![4, 5]));
        let service = AiUsageService {
            state: Arc::clone(&state),
        };
        assert_eq!(service.get_state().unwrap(), vec![4, 5]);
    }

    #[test]
    fn repeated_get_state_does_not_create_publication_demand() {
        let state = Arc::new(RwLock::new(vec![4, 5]));
        let service = AiUsageService {
            state: Arc::clone(&state),
        };
        let (updates, receiver) = watch::channel(vec![4, 5]);
        let publisher = Publisher {
            state,
            updates,
            errors: tokio::sync::mpsc::unbounded_channel().1,
        };
        assert_eq!(service.get_state().unwrap(), vec![4, 5]);
        assert_eq!(service.get_state().unwrap(), vec![4, 5]);
        assert!(!receiver.has_changed().unwrap());
        assert!(publisher.errors.is_empty());
    }

    #[test]
    fn empty_state_is_a_valid_current_snapshot() {
        let service = AiUsageService {
            state: Arc::new(RwLock::new(Vec::new())),
        };
        assert!(service.get_state().unwrap().is_empty());
    }

    #[test]
    fn only_primary_owner_is_accepted() {
        assert!(accept_primary_owner(zbus::fdo::RequestNameReply::PrimaryOwner).is_ok());
        assert!(accept_primary_owner(zbus::fdo::RequestNameReply::Exists).is_err());
        assert!(accept_primary_owner(zbus::fdo::RequestNameReply::InQueue).is_err());
    }

    #[test]
    fn get_state_and_signal_share_committed_revision() {
        let state = Arc::new(RwLock::new(vec![1]));
        let service = AiUsageService {
            state: Arc::clone(&state),
        };
        let (updates, mut receiver) = watch::channel(vec![1]);
        let publisher = Publisher {
            state,
            updates,
            errors: tokio::sync::mpsc::unbounded_channel().1,
        };
        publisher.update(vec![2, 3]).unwrap();
        let pending_signal = receiver.borrow_and_update().clone();
        assert_eq!(service.get_state().unwrap(), pending_signal);
    }
}
