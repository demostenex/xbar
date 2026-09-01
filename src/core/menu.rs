use std::collections::HashMap;

use super::WindowId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MenuItemId(pub i32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuItemType {
    Standard,
    Separator,
    Unknown(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildrenDisplay {
    Submenu,
    Unknown(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuShortcut {
    pub keys: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuActionTarget {
    String(String),
    Boolean(bool),
    Int32(i32),
    Uint32(u32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuAction {
    pub name: String,
    pub target: Option<MenuActionTarget>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuItem {
    pub id: MenuItemId,
    pub label: Option<String>,
    pub enabled: bool,
    pub visible: bool,
    pub item_type: MenuItemType,
    pub children_display: Option<ChildrenDisplay>,
    pub shortcut: Option<MenuShortcut>,
    pub icon_name: Option<String>,
    pub action: Option<MenuAction>,
    pub children: Vec<MenuItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuModel {
    pub revision: u32,
    pub root: MenuItem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuPropertyUpdate {
    Label(Option<String>),
    Enabled(bool),
    Visible(bool),
    ItemType(MenuItemType),
    ChildrenDisplay(Option<ChildrenDisplay>),
    Shortcut(Option<MenuShortcut>),
    IconName(Option<String>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuItemPropertiesUpdate {
    pub item_id: MenuItemId,
    pub properties: Vec<MenuPropertyUpdate>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuEndpoint {
    pub service: String,
    pub object_path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GtkMenuEndpoint {
    pub bus_name: String,
    pub menu_object_path: String,
    pub actions_object_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MenuSource {
    DbusMenu(MenuEndpoint),
    GtkGMenu(GtkMenuEndpoint),
    Tray(MenuEndpoint),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Registration {
    sender: String,
    endpoint: MenuEndpoint,
}

#[derive(Clone, Debug, Eq, PartialEq, Default)]
struct WindowMenuSources {
    dbus: Option<Registration>,
    gtk: Option<GtkMenuEndpoint>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MenuRegistry {
    by_window: HashMap<WindowId, WindowMenuSources>,
}

impl MenuRegistry {
    pub fn register(&mut self, window_id: WindowId, sender: String, object_path: String) {
        self.by_window.entry(window_id).or_default().dbus = Some(Registration {
            sender: sender.clone(),
            endpoint: MenuEndpoint {
                service: sender,
                object_path,
            },
        });
    }

    pub fn register_gtk(&mut self, window_id: WindowId, endpoint: GtkMenuEndpoint) {
        self.by_window.entry(window_id).or_default().gtk = Some(endpoint);
    }

    pub fn unregister(&mut self, window_id: WindowId) -> Option<MenuEndpoint> {
        let sources = self.by_window.get_mut(&window_id)?;
        let endpoint = sources
            .dbus
            .take()
            .map(|registration| registration.endpoint);
        if sources.dbus.is_none() && sources.gtk.is_none() {
            self.by_window.remove(&window_id);
        }
        endpoint
    }

    pub fn get(&self, window_id: WindowId) -> Option<&MenuEndpoint> {
        self.by_window.get(&window_id).and_then(|sources| {
            sources
                .dbus
                .as_ref()
                .map(|registration| &registration.endpoint)
        })
    }

    pub fn gtk(&self, window_id: WindowId) -> Option<&GtkMenuEndpoint> {
        self.by_window
            .get(&window_id)
            .and_then(|sources| sources.gtk.as_ref())
    }

    pub fn remove_gtk(&mut self, window_id: WindowId) -> bool {
        let Some(sources) = self.by_window.get_mut(&window_id) else {
            return false;
        };
        let removed = sources.gtk.take().is_some();
        if sources.dbus.is_none() && sources.gtk.is_none() {
            self.by_window.remove(&window_id);
        }
        removed
    }

    pub fn remove_sender(&mut self, sender: &str) -> Vec<WindowId> {
        let mut removed = Vec::new();
        let ids: Vec<_> = self.by_window.keys().copied().collect();
        for window_id in ids {
            let Some(sources) = self.by_window.get_mut(&window_id) else {
                continue;
            };
            let dbus_removed = sources
                .dbus
                .as_ref()
                .is_some_and(|registration| registration.sender == sender);
            let gtk_removed = sources
                .gtk
                .as_ref()
                .is_some_and(|endpoint| endpoint.bus_name == sender);
            if dbus_removed {
                sources.dbus = None;
            }
            if gtk_removed {
                sources.gtk = None;
            }
            if dbus_removed || gtk_removed {
                removed.push(window_id);
            }
            if sources.dbus.is_none() && sources.gtk.is_none() {
                self.by_window.remove(&window_id);
            }
        }
        removed.sort_by_key(|window_id| window_id.0);
        removed
    }

    pub fn active(&self, focused_window: Option<WindowId>) -> Option<MenuSource> {
        let sources = focused_window.and_then(|window_id| self.by_window.get(&window_id))?;
        sources
            .dbus
            .as_ref()
            .map(|registration| MenuSource::DbusMenu(registration.endpoint.clone()))
            .or_else(|| sources.gtk.clone().map(MenuSource::GtkGMenu))
    }

    pub fn source_matches(&self, window_id: WindowId, source: &MenuSource) -> bool {
        self.active(Some(window_id)).as_ref() == Some(source)
    }

    pub fn remove_gtk_if_matches(
        &mut self,
        window_id: WindowId,
        endpoint: &GtkMenuEndpoint,
    ) -> bool {
        let matches = self.gtk(window_id) == Some(endpoint);
        matches && self.remove_gtk(window_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(service: &str, path: &str) -> MenuEndpoint {
        MenuEndpoint {
            service: service.into(),
            object_path: path.into(),
        }
    }

    #[test]
    fn register_get_and_unregister() {
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(10), ":1.1".into(), "/menu/a".into());
        assert_eq!(
            registry.get(WindowId(10)),
            Some(&endpoint(":1.1", "/menu/a"))
        );
        assert_eq!(
            registry.unregister(WindowId(10)),
            Some(endpoint(":1.1", "/menu/a"))
        );
        assert_eq!(registry.get(WindowId(10)), None);
    }

    #[test]
    fn replacement_cleans_previous_sender() {
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(10), ":1.1".into(), "/old".into());
        registry.register(WindowId(10), ":1.2".into(), "/new".into());
        assert_eq!(registry.get(WindowId(10)), Some(&endpoint(":1.2", "/new")));
        assert!(registry.remove_sender(":1.1").is_empty());
    }

    #[test]
    fn old_owner_vanishing_does_not_remove_replacement() {
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(10), ":1.old".into(), "/menu/a".into());
        registry.register(WindowId(10), ":1.new".into(), "/menu/b".into());
        assert_eq!(
            registry.get(WindowId(10)),
            Some(&endpoint(":1.new", "/menu/b"))
        );
        assert!(registry.remove_sender(":1.old").is_empty());
        assert_eq!(
            registry.get(WindowId(10)),
            Some(&endpoint(":1.new", "/menu/b"))
        );
    }

    #[test]
    fn sender_cleanup_removes_all_owned_windows() {
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(2), ":1.1".into(), "/two".into());
        registry.register(WindowId(1), ":1.1".into(), "/one".into());
        registry.register(WindowId(3), ":1.2".into(), "/three".into());
        assert_eq!(
            registry.remove_sender(":1.1"),
            vec![WindowId(1), WindowId(2)]
        );
        assert_eq!(registry.get(WindowId(3)), Some(&endpoint(":1.2", "/three")));
    }

    #[test]
    fn focused_lookup_is_derived_from_registry() {
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(10), ":1.1".into(), "/menu".into());
        assert_eq!(
            registry.active(Some(WindowId(10))),
            Some(MenuSource::DbusMenu(endpoint(":1.1", "/menu")))
        );
        assert_eq!(registry.active(Some(WindowId(11))), None);
        assert_eq!(registry.active(None), None);
    }

    #[test]
    fn dbus_menu_has_precedence_and_gtk_falls_back_after_unregister() {
        let mut registry = MenuRegistry::default();
        let gtk = GtkMenuEndpoint {
            bus_name: ":1.gtk".into(),
            menu_object_path: "/gtk/menu".into(),
            actions_object_paths: vec!["/gtk/actions".into()],
        };
        registry.register_gtk(WindowId(10), gtk.clone());
        assert_eq!(
            registry.active(Some(WindowId(10))),
            Some(MenuSource::GtkGMenu(gtk.clone()))
        );
        registry.register(WindowId(10), ":1.dbus".into(), "/dbus/menu".into());
        assert!(matches!(
            registry.active(Some(WindowId(10))),
            Some(MenuSource::DbusMenu(_))
        ));
        registry.unregister(WindowId(10));
        assert_eq!(
            registry.active(Some(WindowId(10))),
            Some(MenuSource::GtkGMenu(gtk))
        );
    }

    #[test]
    fn gtk_replacement_is_not_removed_by_old_owner() {
        let mut registry = MenuRegistry::default();
        let old = GtkMenuEndpoint {
            bus_name: ":1.old".into(),
            menu_object_path: "/gtk/old".into(),
            actions_object_paths: vec![],
        };
        let current = GtkMenuEndpoint {
            bus_name: ":1.current".into(),
            menu_object_path: "/gtk/current".into(),
            actions_object_paths: vec![],
        };
        registry.register_gtk(WindowId(10), old);
        registry.register_gtk(WindowId(10), current.clone());
        assert!(registry.remove_sender(":1.old").is_empty());
        assert_eq!(registry.gtk(WindowId(10)), Some(&current));
    }
}
