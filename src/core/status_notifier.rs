#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StatusNotifierEndpoint {
    pub service: String,
    pub object_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusNotifierStatus {
    Passive,
    Active,
    NeedsAttention,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusNotifierAction {
    Activate,
    SecondaryActivate,
    ContextMenu,
    Scroll {
        delta: i32,
        orientation: &'static str,
    },
}

pub const SCROLL_STEP: i32 = 1;

impl StatusNotifierAction {
    pub fn for_button(button: u8, item_is_menu: bool) -> Option<Self> {
        match button {
            1 if item_is_menu => Some(Self::ContextMenu),
            1 => Some(Self::Activate),
            2 => Some(Self::SecondaryActivate),
            3 => Some(Self::ContextMenu),
            4 => Some(Self::Scroll {
                delta: SCROLL_STEP,
                orientation: "vertical",
            }),
            5 => Some(Self::Scroll {
                delta: -SCROLL_STEP,
                orientation: "vertical",
            }),
            6 => Some(Self::Scroll {
                delta: -SCROLL_STEP,
                orientation: "horizontal",
            }),
            7 => Some(Self::Scroll {
                delta: SCROLL_STEP,
                orientation: "horizontal",
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StatusNotifierIcon {
    Pixmap {
        width: u16,
        height: u16,
        argb: Vec<u32>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusNotifierItem {
    pub endpoint: StatusNotifierEndpoint,
    pub status: StatusNotifierStatus,
    pub icon: Option<StatusNotifierIcon>,
    pub item_is_menu: bool,
    pub menu: Option<super::MenuEndpoint>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusNotifierItemRegistry {
    items: Vec<StatusNotifierItem>,
}

impl StatusNotifierItemRegistry {
    pub fn upsert(&mut self, item: StatusNotifierItem) -> bool {
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|existing| existing.endpoint == item.endpoint)
        {
            if *existing == item {
                false
            } else {
                *existing = item;
                true
            }
        } else {
            self.items.push(item);
            true
        }
    }

    pub fn remove(&mut self, endpoint: &StatusNotifierEndpoint) -> bool {
        let Some(index) = self
            .items
            .iter()
            .position(|item| &item.endpoint == endpoint)
        else {
            return false;
        };
        self.items.remove(index);
        true
    }

    pub fn remove_service(&mut self, service: &str) -> usize {
        let before = self.items.len();
        self.items.retain(|item| item.endpoint.service != service);
        before - self.items.len()
    }

    pub fn items(&self) -> &[StatusNotifierItem] {
        &self.items
    }
}

pub fn format_notifier_item_id(endpoint: &StatusNotifierEndpoint) -> String {
    format!("{}{}", endpoint.service, endpoint.object_path)
}

pub fn parse_notifier_item_id(item: &str) -> Option<StatusNotifierEndpoint> {
    let (service, object_path) = item.split_once('/')?;
    if service.is_empty() || object_path.is_empty() {
        return None;
    }
    Some(StatusNotifierEndpoint {
        service: service.into(),
        object_path: format!("/{object_path}"),
    })
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StatusNotifierRegistry {
    endpoints: Vec<StatusNotifierEndpoint>,
}

impl StatusNotifierRegistry {
    pub fn register(&mut self, endpoint: StatusNotifierEndpoint) -> bool {
        if self.endpoints.contains(&endpoint) {
            false
        } else {
            self.endpoints.push(endpoint);
            true
        }
    }
    pub fn unregister(&mut self, endpoint: &StatusNotifierEndpoint) -> bool {
        let Some(index) = self.endpoints.iter().position(|item| item == endpoint) else {
            return false;
        };
        self.endpoints.remove(index);
        true
    }
    pub fn remove_service(&mut self, service: &str) -> Vec<StatusNotifierEndpoint> {
        let mut removed = Vec::new();
        self.endpoints.retain(|endpoint| {
            if endpoint.service == service {
                removed.push(endpoint.clone());
                false
            } else {
                true
            }
        });
        removed
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }
    #[cfg(test)]
    pub fn endpoints(&self) -> &[StatusNotifierEndpoint] {
        &self.endpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn endpoint(service: &str, path: &str) -> StatusNotifierEndpoint {
        StatusNotifierEndpoint {
            service: service.into(),
            object_path: path.into(),
        }
    }
    #[test]
    fn identity_is_service_and_path_and_duplicates_are_ignored() {
        let mut registry = StatusNotifierRegistry::default();
        assert!(registry.register(endpoint(":1.50", "/a")));
        assert!(!registry.register(endpoint(":1.50", "/a")));
        assert!(registry.register(endpoint(":1.50", "/b")));
        assert_eq!(registry.len(), 2);
    }
    #[test]
    fn owner_vanished_removes_only_that_service() {
        let mut registry = StatusNotifierRegistry::default();
        registry.register(endpoint(":1.50", "/a"));
        registry.register(endpoint(":1.50", "/b"));
        registry.register(endpoint(":1.51", "/c"));
        assert_eq!(registry.remove_service(":1.50").len(), 2);
        assert_eq!(registry.endpoints(), &[endpoint(":1.51", "/c")]);
    }

    #[test]
    fn canonical_item_id_roundtrips_service_and_path() {
        let endpoint = endpoint(":1.141", "/StatusNotifierItem/2");
        assert_eq!(
            format_notifier_item_id(&endpoint),
            ":1.141/StatusNotifierItem/2"
        );
        assert_eq!(
            parse_notifier_item_id(":1.141/StatusNotifierItem/2"),
            Some(endpoint)
        );
        assert!(parse_notifier_item_id("/StatusNotifierItem").is_none());
    }

    fn tray_item(
        status: StatusNotifierStatus,
        icon: Option<StatusNotifierIcon>,
    ) -> StatusNotifierItem {
        StatusNotifierItem {
            endpoint: endpoint(":1.50", "/StatusNotifierItem"),
            status,
            icon,
            item_is_menu: false,
            menu: None,
        }
    }

    #[test]
    fn item_registry_ignores_identical_updates_and_replaces_changed_state() {
        let icon = Some(StatusNotifierIcon::Pixmap {
            width: 16,
            height: 16,
            argb: vec![0xff00_0000],
        });
        let mut registry = StatusNotifierItemRegistry::default();
        let item = tray_item(StatusNotifierStatus::Active, icon.clone());
        assert!(registry.upsert(item.clone()));
        assert!(!registry.upsert(item));
        assert!(registry.upsert(tray_item(StatusNotifierStatus::Passive, icon)));
    }

    #[test]
    fn mouse_buttons_map_to_sni_actions() {
        assert_eq!(
            StatusNotifierAction::for_button(1, false),
            Some(StatusNotifierAction::Activate)
        );
        assert_eq!(
            StatusNotifierAction::for_button(2, false),
            Some(StatusNotifierAction::SecondaryActivate)
        );
        assert_eq!(
            StatusNotifierAction::for_button(3, false),
            Some(StatusNotifierAction::ContextMenu)
        );
        assert_eq!(
            StatusNotifierAction::for_button(1, true),
            Some(StatusNotifierAction::ContextMenu)
        );
        assert_eq!(
            StatusNotifierAction::for_button(4, false),
            Some(StatusNotifierAction::Scroll {
                delta: SCROLL_STEP,
                orientation: "vertical"
            })
        );
        assert_eq!(
            StatusNotifierAction::for_button(5, false),
            Some(StatusNotifierAction::Scroll {
                delta: -SCROLL_STEP,
                orientation: "vertical"
            })
        );
        assert_eq!(
            StatusNotifierAction::for_button(6, false),
            Some(StatusNotifierAction::Scroll {
                delta: -SCROLL_STEP,
                orientation: "horizontal"
            })
        );
        assert_eq!(
            StatusNotifierAction::for_button(7, false),
            Some(StatusNotifierAction::Scroll {
                delta: SCROLL_STEP,
                orientation: "horizontal"
            })
        );
    }
}
