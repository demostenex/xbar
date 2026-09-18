use crate::core::{NotificationHistoryEntry, NotificationIconMetadata, NotificationSource};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const FORMAT_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 50;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct FileFormat {
    version: u32,
    entries: Vec<PersistedEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PersistedEntry {
    id: u64,
    source: String,
    app_name: String,
    summary: String,
    body: String,
    #[serde(default)]
    icon_metadata: NotificationIconMetadata,
    order: u64,
    received_at: u64,
    updated_at: u64,
}

#[derive(Debug)]
pub enum LoadResult {
    Disabled,
    Empty,
    Loaded(Vec<NotificationHistoryEntry>),
    Invalid(String),
}

pub struct Persistence {
    path: Option<PathBuf>,
    writes_enabled: bool,
}

impl Persistence {
    pub fn from_environment() -> Self {
        let path = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("xbar/notifications.json"))
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|path| path.join(".local/state/xbar/notifications.json"))
            });
        Self {
            writes_enabled: path.is_some(),
            path,
        }
    }

    #[cfg(test)]
    fn at_path(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            writes_enabled: true,
        }
    }

    pub fn load(&mut self) -> LoadResult {
        let Some(path) = &self.path else {
            self.writes_enabled = false;
            return LoadResult::Disabled;
        };
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return LoadResult::Empty,
            Err(error) => {
                self.writes_enabled = false;
                return LoadResult::Invalid(format!("read failed: {error}"));
            }
        };
        let file: FileFormat = match serde_json::from_slice(&bytes) {
            Ok(file) => file,
            Err(error) => {
                self.writes_enabled = false;
                return LoadResult::Invalid(format!("invalid JSON: {error}"));
            }
        };
        if file.version != FORMAT_VERSION {
            self.writes_enabled = false;
            return LoadResult::Invalid(format!("unsupported version {}", file.version));
        }
        let mut ids = HashSet::with_capacity(file.entries.len());
        let mut entries = Vec::with_capacity(file.entries.len().min(MAX_ENTRIES));
        for persisted in file.entries.into_iter().take(MAX_ENTRIES) {
            if persisted.id == 0 || !ids.insert(persisted.id) {
                self.writes_enabled = false;
                return LoadResult::Invalid("invalid or duplicate HistoryEntryId".into());
            }
            let source = match persisted.source.as_str() {
                "freedesktop" => NotificationSource::Freedesktop,
                "window-attention" | "internal" => {
                    self.writes_enabled = false;
                    return LoadResult::Invalid("non-pending notification source".into());
                }
                _ => {
                    self.writes_enabled = false;
                    return LoadResult::Invalid("unknown notification source".into());
                }
            };
            entries.push(NotificationHistoryEntry {
                id: crate::core::HistoryEntryId(persisted.id),
                live_notification_id: None,
                source,
                app_name: persisted.app_name,
                summary: persisted.summary,
                body: persisted.body,
                icon_metadata: persisted.icon_metadata,
                order: persisted.order,
                received_at: persisted.received_at,
                updated_at: persisted.updated_at,
            });
        }
        LoadResult::Loaded(entries)
    }

    pub fn save(&self, entries: &[NotificationHistoryEntry]) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if !self.writes_enabled {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "notification persistence disabled",
            ));
        }
        let directory = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "notification path has no directory",
            )
        })?;
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let file = FileFormat {
            version: FORMAT_VERSION,
            entries: entries
                .iter()
                .take(MAX_ENTRIES)
                .map(|entry| PersistedEntry {
                    id: entry.id.0,
                    source: match entry.source {
                        NotificationSource::Freedesktop => "freedesktop".into(),
                        NotificationSource::WindowAttention => "window-attention".into(),
                        NotificationSource::Internal => "internal".into(),
                    },
                    app_name: entry.app_name.clone(),
                    summary: entry.summary.clone(),
                    body: entry.body.clone(),
                    icon_metadata: entry.icon_metadata.clone(),
                    order: entry.order,
                    received_at: entry.received_at,
                    updated_at: entry.updated_at,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp = directory.join(format!(
            ".notifications.json.tmp.{}.{}",
            std::process::id(),
            sequence
        ));
        let result = (|| {
            let mut handle = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            handle.write_all(&bytes)?;
            handle.sync_all()?;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
            fs::rename(&temp, path)?;
            Ok::<(), io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn test_path() -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "xbar-notification-persistence-test-{}-{sequence}",
            std::process::id()
        ))
    }

    fn entry(id: u64, order: u64, received_at: u64, updated_at: u64) -> NotificationHistoryEntry {
        NotificationHistoryEntry {
            id: crate::core::HistoryEntryId(id),
            live_notification_id: Some(crate::core::NotificationId(88)),
            source: NotificationSource::Freedesktop,
            app_name: "app".into(),
            summary: format!("summary-{id}"),
            body: "body".into(),
            icon_metadata: Default::default(),
            order,
            received_at,
            updated_at,
        }
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn round_trip_preserves_local_content_order_ids_and_timestamps_without_live_linkage() {
        let path = test_path().join("notifications.json");
        let persistence = Persistence::at_path(path.clone());
        let entries = vec![entry(42, 9, 100, 200), entry(3, 8, 300, 400)];
        persistence.save(&entries).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("NotificationId"));
        assert!(!raw.contains("live_notification_id"));

        let mut loaded = Persistence::at_path(path.clone());
        let LoadResult::Loaded(restored) = loaded.load() else {
            panic!("expected valid persisted entries")
        };
        assert_eq!(
            restored,
            vec![
                NotificationHistoryEntry {
                    live_notification_id: None,
                    ..entries[0].clone()
                },
                NotificationHistoryEntry {
                    live_notification_id: None,
                    ..entries[1].clone()
                },
            ]
        );
        cleanup(&path);
    }

    #[test]
    fn missing_file_is_empty_and_save_uses_private_atomic_state_file() {
        let path = test_path().join("notifications.json");
        cleanup(&path);
        let persistence = Persistence::at_path(path.clone());
        assert!(matches!(
            Persistence::at_path(path.clone()).load(),
            LoadResult::Empty
        ));
        persistence.save(&[entry(1, 1, 10, 10)]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(!path.with_file_name(".notifications.json.tmp").exists());
        cleanup(&path);
    }

    #[test]
    fn missing_state_environment_disables_persistence_without_affecting_runtime() {
        let persistence = Persistence {
            path: None,
            writes_enabled: false,
        };
        assert!(persistence.save(&[]).is_ok());
    }

    #[test]
    fn corrupt_and_future_files_are_non_destructive_and_disable_writes() {
        for contents in [
            "not json".to_owned(),
            serde_json::to_string(&FileFormat {
                version: 2,
                entries: Vec::new(),
            })
            .unwrap(),
        ] {
            let path = test_path().join("notifications.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &contents).unwrap();
            let mut persistence = Persistence::at_path(path.clone());
            assert!(matches!(persistence.load(), LoadResult::Invalid(_)));
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            assert!(persistence.save(&[]).is_err());
            cleanup(&path);
        }
    }

    #[test]
    fn invalid_zero_duplicate_and_non_pending_entries_are_rejected() {
        let invalids = [
            vec![entry(0, 1, 1, 1)],
            vec![entry(1, 1, 1, 1), entry(1, 2, 2, 2)],
            vec![NotificationHistoryEntry {
                source: NotificationSource::WindowAttention,
                ..entry(1, 1, 1, 1)
            }],
        ];
        for entries in invalids {
            let path = test_path().join("notifications.json");
            let file = FileFormat {
                version: FORMAT_VERSION,
                entries: entries
                    .iter()
                    .map(|entry| PersistedEntry {
                        id: entry.id.0,
                        source: match entry.source {
                            NotificationSource::Freedesktop => "freedesktop".into(),
                            _ => "window-attention".into(),
                        },
                        app_name: entry.app_name.clone(),
                        summary: entry.summary.clone(),
                        body: entry.body.clone(),
                        icon_metadata: entry.icon_metadata.clone(),
                        order: entry.order,
                        received_at: entry.received_at,
                        updated_at: entry.updated_at,
                    })
                    .collect(),
            };
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
            assert!(matches!(
                Persistence::at_path(path.clone()).load(),
                LoadResult::Invalid(_)
            ));
            cleanup(&path);
        }
    }

    #[test]
    fn restore_normalizes_to_newest_first_order_without_exceeding_fifty() {
        let path = test_path().join("notifications.json");
        let persistence = Persistence::at_path(path.clone());
        let entries = (1..=55)
            .map(|id| entry(id, 56 - id, id, id))
            .collect::<Vec<_>>();
        persistence.save(&entries).unwrap();
        let mut loaded = Persistence::at_path(path.clone());
        let LoadResult::Loaded(restored) = loaded.load() else {
            panic!("expected entries")
        };
        assert_eq!(restored.len(), 50);
        assert_eq!(restored[0].id.0, 1);
        assert_eq!(restored[49].id.0, 50);
        cleanup(&path);
    }
}
