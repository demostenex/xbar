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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MenuEndpoint {
    pub service: String,
    pub object_path: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct GtkMenuEndpoint {
    pub bus_name: String,
    pub menu_object_path: String,
    pub actions_object_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum MenuSource {
    DbusMenu(MenuEndpoint),
    GtkGMenu(GtkMenuEndpoint),
    Tray(MenuEndpoint),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayoutConvergencePhase {
    FollowUpNeeded,
    FollowUpInFlight,
    RetryAvailable,
    RetryInFlight,
    AwaitingNewInvalidation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingLayoutReload {
    endpoint: MenuSource,
    numeric_watermark: Option<u32>,
    numeric_phase: Option<LayoutConvergencePhase>,
    revisionless_epoch: u64,
    revisionless_satisfied_epoch: u64,
}

/// Tracks structural invalidations which arrive while a layout request is in flight.
///
/// The tracker is deliberately endpoint-local.  The main loop still applies its
/// window/endpoint/request-id authority checks before a model can be committed.
/// Numeric invalidations retain their highest watermark and permit one immediate
/// convergence retry. Revision-less invalidations advance an independent epoch;
/// a load satisfies only the epoch captured when that load began, so invalidations
/// observed during it coalesce into one deterministic follow-up.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MenuLayoutReloadTracker {
    pending: Option<PendingLayoutReload>,
    active_endpoint: Option<MenuSource>,
    watcher_generations: HashMap<MenuSource, u64>,
    in_flight_request: Option<u64>,
    in_flight_revisionless_epoch: Option<u64>,
}

impl MenuLayoutReloadTracker {
    pub fn watcher_ready(
        &mut self,
        endpoint: MenuSource,
        watcher_generation: u64,
        request_id: u64,
    ) -> bool {
        if self.active_endpoint.as_ref() != Some(&endpoint)
            || self.in_flight_request != Some(request_id)
        {
            return false;
        }
        let previous = self
            .watcher_generations
            .insert(endpoint.clone(), watcher_generation);
        if previous.is_some_and(|previous| previous != watcher_generation)
            && self.active_endpoint.as_ref() == Some(&endpoint)
        {
            self.pending = None;
            self.in_flight_revisionless_epoch = Some(0);
        }
        true
    }

    pub fn remove_watcher(&mut self, endpoint: &MenuSource) {
        self.watcher_generations.remove(endpoint);
        self.clear_for(endpoint);
    }

    pub fn remove_watchers_for_owner(&mut self, owner: &str) {
        self.watcher_generations.retain(|endpoint, _| {
            !matches!(endpoint, MenuSource::DbusMenu(endpoint) if endpoint.service == owner)
        });
        if matches!(
            self.active_endpoint.as_ref(),
            Some(MenuSource::DbusMenu(endpoint)) if endpoint.service == owner
        ) {
            self.clear_active();
        }
    }

    pub fn begin_load(&mut self, endpoint: MenuSource, request_id: u64) {
        if self.active_endpoint.as_ref() != Some(&endpoint) {
            self.pending = None;
            self.active_endpoint = Some(endpoint);
        }
        if let Some(pending) = &mut self.pending {
            pending.numeric_phase = pending.numeric_phase.map(|phase| match phase {
                LayoutConvergencePhase::FollowUpNeeded => LayoutConvergencePhase::FollowUpInFlight,
                LayoutConvergencePhase::RetryAvailable => LayoutConvergencePhase::RetryInFlight,
                phase => phase,
            });
        }
        self.in_flight_revisionless_epoch = Some(
            self.pending
                .as_ref()
                .map_or(0, |pending| pending.revisionless_epoch),
        );
        self.in_flight_request = Some(request_id);
    }

    pub fn record(
        &mut self,
        endpoint: MenuSource,
        watcher_generation: u64,
        revision: Option<u32>,
    ) -> bool {
        if self.active_endpoint.as_ref() != Some(&endpoint)
            || self.watcher_generations.get(&endpoint) != Some(&watcher_generation)
        {
            return false;
        }
        match &mut self.pending {
            Some(pending) if pending.endpoint == endpoint => {
                match revision {
                    Some(next) => {
                        let is_higher = pending
                            .numeric_watermark
                            .is_none_or(|current| next > current);
                        if !is_higher {
                            return false;
                        }
                        pending.numeric_watermark = Some(next);
                        pending.numeric_phase = Some(match pending.numeric_phase {
                            Some(LayoutConvergencePhase::RetryInFlight) => {
                                LayoutConvergencePhase::FollowUpInFlight
                            }
                            Some(LayoutConvergencePhase::AwaitingNewInvalidation) | None => {
                                LayoutConvergencePhase::FollowUpNeeded
                            }
                            Some(phase) => phase,
                        });
                    }
                    None => {
                        pending.revisionless_epoch = pending.revisionless_epoch.saturating_add(1);
                        pending.numeric_phase = pending.numeric_phase.map(|phase| match phase {
                            LayoutConvergencePhase::RetryInFlight => {
                                LayoutConvergencePhase::FollowUpInFlight
                            }
                            LayoutConvergencePhase::AwaitingNewInvalidation => {
                                LayoutConvergencePhase::FollowUpNeeded
                            }
                            phase => phase,
                        });
                    }
                }
                true
            }
            _ => {
                self.pending = Some(PendingLayoutReload {
                    endpoint,
                    numeric_watermark: revision,
                    numeric_phase: revision.map(|_| LayoutConvergencePhase::FollowUpNeeded),
                    revisionless_epoch: u64::from(revision.is_none()),
                    revisionless_satisfied_epoch: 0,
                });
                true
            }
        }
    }

    pub fn clear_active(&mut self) {
        self.pending = None;
        self.active_endpoint = None;
        self.in_flight_request = None;
        self.in_flight_revisionless_epoch = None;
    }

    pub fn clear_for(&mut self, endpoint: &MenuSource) {
        if self.active_endpoint.as_ref() == Some(endpoint) {
            self.clear_active();
        }
    }

    pub fn accepts_watcher(&self, endpoint: &MenuSource, watcher_generation: u64) -> bool {
        self.active_endpoint.as_ref() == Some(endpoint)
            && self.watcher_generations.get(endpoint) == Some(&watcher_generation)
    }

    pub fn complete_load(
        &mut self,
        endpoint: &MenuSource,
        request_id: u64,
        model_revision: u32,
    ) -> bool {
        if self.active_endpoint.as_ref() != Some(endpoint)
            || self.in_flight_request != Some(request_id)
        {
            return false;
        }
        self.in_flight_request = None;
        let in_flight_revisionless_epoch = self.in_flight_revisionless_epoch.take().unwrap_or(0);
        let Some(pending) = &mut self.pending else {
            return false;
        };
        if pending.endpoint != *endpoint {
            return false;
        }
        pending.revisionless_satisfied_epoch = pending
            .revisionless_satisfied_epoch
            .max(in_flight_revisionless_epoch);
        let revisionless_follow_up_needed =
            pending.revisionless_epoch > pending.revisionless_satisfied_epoch;
        if pending
            .numeric_watermark
            .is_some_and(|watermark| model_revision >= watermark)
        {
            pending.numeric_watermark = None;
            pending.numeric_phase = None;
        }
        let numeric_follow_up_needed = match (pending.numeric_watermark, pending.numeric_phase) {
            (Some(_), Some(LayoutConvergencePhase::FollowUpNeeded)) => true,
            (Some(_), Some(LayoutConvergencePhase::FollowUpInFlight)) => {
                pending.numeric_phase = Some(LayoutConvergencePhase::RetryAvailable);
                true
            }
            (Some(_), Some(LayoutConvergencePhase::RetryInFlight)) => {
                pending.numeric_phase = Some(LayoutConvergencePhase::AwaitingNewInvalidation);
                false
            }
            _ => false,
        };
        let follow_up_needed = revisionless_follow_up_needed || numeric_follow_up_needed;
        if pending.numeric_watermark.is_none()
            && pending.revisionless_epoch <= pending.revisionless_satisfied_epoch
        {
            self.pending = None;
        }
        follow_up_needed
    }

    #[cfg(test)]
    pub fn has_pending(&self, endpoint: &MenuSource) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| &pending.endpoint == endpoint)
    }

    #[cfg(test)]
    fn convergence_phase(&self, endpoint: &MenuSource) -> Option<LayoutConvergencePhase> {
        self.pending
            .as_ref()
            .filter(|pending| &pending.endpoint == endpoint)
            .and_then(|pending| pending.numeric_phase)
    }

    #[cfg(test)]
    fn numeric_watermark(&self, endpoint: &MenuSource) -> Option<u32> {
        self.pending
            .as_ref()
            .filter(|pending| &pending.endpoint == endpoint)
            .and_then(|pending| pending.numeric_watermark)
    }

    #[cfg(test)]
    fn revisionless_epochs(&self, endpoint: &MenuSource) -> Option<(u64, u64)> {
        self.pending
            .as_ref()
            .filter(|pending| &pending.endpoint == endpoint)
            .map(|pending| {
                (
                    pending.revisionless_epoch,
                    pending.revisionless_satisfied_epoch,
                )
            })
    }
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

    fn dbus_source(service: &str, path: &str) -> MenuSource {
        MenuSource::DbusMenu(MenuEndpoint {
            service: service.into(),
            object_path: path.into(),
        })
    }

    fn begin_initial_load(
        tracker: &mut MenuLayoutReloadTracker,
        endpoint: &MenuSource,
        watcher_generation: u64,
        request_id: u64,
    ) {
        tracker.begin_load(endpoint.clone(), request_id);
        assert!(tracker.watcher_ready(endpoint.clone(), watcher_generation, request_id));
    }

    #[test]
    fn layout_invalidations_coalesce_to_the_highest_revision() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(5)));
        assert!(tracker.record(endpoint.clone(), 10, Some(8)));
        assert!(!tracker.record(endpoint.clone(), 10, Some(6)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
    }

    #[test]
    fn a_signal_without_revision_remains_authoritative_until_reloaded() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
    }

    #[test]
    fn invalidation_for_old_endpoint_cannot_schedule_new_endpoint() {
        let old = dbus_source(":1.9", "/old");
        let current = dbus_source(":1.10", "/new");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &old, 10, 1);
        assert!(!tracker.record(old, 99, Some(8)));
        begin_initial_load(&mut tracker, &current, 11, 2);
        assert!(!tracker.has_pending(&current));
    }

    #[test]
    fn invalidation_during_initial_load_requires_one_follow_up() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(8)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
    }

    #[test]
    fn newer_accepted_layout_satisfies_an_older_invalidation() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(3)));
        assert!(!tracker.complete_load(&endpoint, 1, 8));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn repeated_invalidations_during_one_load_stay_bounded() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(5)));
        assert!(tracker.record(endpoint.clone(), 10, Some(6)));
        assert!(tracker.record(endpoint.clone(), 10, Some(8)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
    }

    #[test]
    fn unregister_clears_pending_reload_before_a_new_endpoint_can_use_it() {
        let endpoint_a = dbus_source(":1.9", "/a");
        let endpoint_b = dbus_source(":1.10", "/b");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint_a, 10, 1);
        assert!(tracker.record(endpoint_a.clone(), 10, Some(8)));
        tracker.remove_watcher(&endpoint_a);
        begin_initial_load(&mut tracker, &endpoint_b, 11, 2);
        assert!(!tracker.has_pending(&endpoint_b));
    }

    #[test]
    fn older_follow_up_requests_exactly_one_immediate_convergence_retry() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryAvailable)
        );
        tracker.begin_load(endpoint.clone(), 3);
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryInFlight)
        );
    }

    #[test]
    fn convergence_retry_equal_to_watermark_clears_pending_state() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 9));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn convergence_retry_newer_than_watermark_clears_pending_state() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 12));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn second_older_result_waits_for_a_new_invalidation_without_a_third_load() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 8));
        assert!(tracker.has_pending(&endpoint));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::AwaitingNewInvalidation)
        );
    }

    #[test]
    fn higher_invalidation_rearms_convergence_while_awaiting() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 8));
        assert!(tracker.record(endpoint.clone(), 10, Some(10)));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::FollowUpNeeded)
        );
        tracker.begin_load(endpoint.clone(), 4);
        assert!(tracker.complete_load(&endpoint, 4, 9));
        tracker.begin_load(endpoint.clone(), 5);
        assert!(!tracker.complete_load(&endpoint, 5, 10));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn duplicate_or_lower_invalidation_does_not_rearm_awaiting_convergence() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 8));
        assert!(!tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(!tracker.record(endpoint.clone(), 10, Some(8)));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::AwaitingNewInvalidation)
        );
    }

    #[test]
    fn revisionless_invalidation_preserves_existing_numeric_watermark() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);

        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(9));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
    }

    #[test]
    fn numeric_watermark_is_recorded_after_revisionless_invalidation() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);

        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(9));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
    }

    #[test]
    fn mixed_result_below_watermark_keeps_numeric_obligation() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint.clone(), 10, None));

        assert!(tracker.complete_load(&endpoint, 1, 8));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(9));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
    }

    #[test]
    fn mixed_result_at_watermark_keeps_only_later_revisionless_follow_up() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint.clone(), 10, None));

        assert!(tracker.complete_load(&endpoint, 1, 9));
        assert_eq!(tracker.numeric_watermark(&endpoint), None);
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 9));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn revisionless_invalidation_before_load_is_satisfied_by_that_load() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(!tracker.complete_load(&endpoint, 1, 8));

        assert!(tracker.record(endpoint.clone(), 10, None));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn revisionless_invalidation_during_load_requires_exactly_one_follow_up() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);

        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn multiple_revisionless_invalidations_during_load_coalesce() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);

        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(!tracker.complete_load(&endpoint, 2, 8));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn revisionless_invalidation_during_follow_up_requires_one_more_follow_up() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 1, 8));

        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 2, 8));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((2, 1)));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 8));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn revisionless_during_numeric_follow_up_survives_numeric_satisfaction() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.record(endpoint.clone(), 10, None));

        assert!(tracker.complete_load(&endpoint, 2, 9));
        assert_eq!(tracker.numeric_watermark(&endpoint), None);
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 9));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn revisionless_during_older_numeric_follow_up_preserves_both_obligations() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.record(endpoint.clone(), 10, None));

        assert!(tracker.complete_load(&endpoint, 2, 8));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(9));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryAvailable)
        );
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
        tracker.begin_load(endpoint.clone(), 3);
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryInFlight)
        );
    }

    #[test]
    fn numeric_invalidation_during_revisionless_load_preserves_both_obligations() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(!tracker.complete_load(&endpoint, 1, 8));
        assert!(tracker.record(endpoint.clone(), 10, None));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.record(endpoint.clone(), 10, Some(12)));

        assert!(tracker.complete_load(&endpoint, 2, 9));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(12));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 1)));
        tracker.begin_load(endpoint.clone(), 3);
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::FollowUpInFlight)
        );
        assert!(!tracker.complete_load(&endpoint, 3, 12));
        assert!(!tracker.has_pending(&endpoint));
    }

    #[test]
    fn higher_numeric_invalidation_during_follow_up_in_flight_is_bounded() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);

        assert!(tracker.record(endpoint.clone(), 10, Some(12)));
        assert!(tracker.complete_load(&endpoint, 2, 9));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(12));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryAvailable)
        );
    }

    #[test]
    fn higher_numeric_invalidation_during_retry_starts_new_bounded_epoch() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);

        assert!(tracker.record(endpoint.clone(), 10, Some(12)));
        assert!(tracker.complete_load(&endpoint, 3, 9));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(12));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::RetryAvailable)
        );
        tracker.begin_load(endpoint.clone(), 4);
        assert!(!tracker.complete_load(&endpoint, 4, 8));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::AwaitingNewInvalidation)
        );
    }

    #[test]
    fn duplicate_or_lower_numeric_during_retry_does_not_replenish_budget() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);

        assert!(!tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(!tracker.record(endpoint.clone(), 10, Some(8)));
        assert!(!tracker.complete_load(&endpoint, 3, 8));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::AwaitingNewInvalidation)
        );
    }

    #[test]
    fn revisionless_invalidation_rearms_awaiting_numeric_epoch_boundedly() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.complete_load(&endpoint, 1, 3));
        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.complete_load(&endpoint, 2, 8));
        tracker.begin_load(endpoint.clone(), 3);
        assert!(!tracker.complete_load(&endpoint, 3, 8));

        assert!(tracker.record(endpoint.clone(), 10, None));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(9));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::FollowUpNeeded)
        );
        tracker.begin_load(endpoint.clone(), 4);
        assert!(tracker.complete_load(&endpoint, 4, 8));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 1)));
        tracker.begin_load(endpoint.clone(), 5);
        assert!(!tracker.complete_load(&endpoint, 5, 8));
        assert_eq!(
            tracker.convergence_phase(&endpoint),
            Some(LayoutConvergencePhase::AwaitingNewInvalidation)
        );
    }

    #[test]
    fn endpoint_transition_discards_mixed_obligations() {
        let endpoint_a = dbus_source(":1.9", "/a");
        let endpoint_b = dbus_source(":1.9", "/b");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint_a, 10, 1);
        assert!(tracker.record(endpoint_a.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint_a.clone(), 10, None));

        begin_initial_load(&mut tracker, &endpoint_b, 11, 2);
        assert!(!tracker.has_pending(&endpoint_b));
        assert_eq!(tracker.numeric_watermark(&endpoint_b), None);
        assert!(!tracker.record(endpoint_a, 10, Some(12)));
    }

    #[test]
    fn watcher_recreation_keeps_mixed_obligations_generation_fenced() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint.clone(), 10, None));
        tracker.remove_watcher(&endpoint);

        tracker.begin_load(endpoint.clone(), 2);
        assert!(tracker.watcher_ready(endpoint.clone(), 11, 2));
        assert!(!tracker.record(endpoint.clone(), 10, Some(12)));
        assert!(tracker.record(endpoint.clone(), 11, Some(12)));
        assert_eq!(tracker.numeric_watermark(&endpoint), Some(12));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((0, 0)));
    }

    #[test]
    fn generation_replacement_resets_revisionless_load_capture() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        assert!(tracker.record(endpoint.clone(), 10, None));
        assert!(tracker.complete_load(&endpoint, 1, 8));
        tracker.begin_load(endpoint.clone(), 2);

        assert!(tracker.watcher_ready(endpoint.clone(), 11, 2));
        assert!(tracker.record(endpoint.clone(), 11, None));
        assert!(tracker.complete_load(&endpoint, 2, 8));
        assert_eq!(tracker.revisionless_epochs(&endpoint), Some((1, 0)));
    }

    #[test]
    fn old_watermark_cannot_affect_a_new_endpoint() {
        let old = dbus_source(":1.9", "/old");
        let current = dbus_source(":1.10", "/new");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &old, 10, 1);
        assert!(tracker.record(old.clone(), 10, Some(9)));
        begin_initial_load(&mut tracker, &current, 11, 2);
        assert!(!tracker.accepts_watcher(&old, 10));
        assert!(!tracker.has_pending(&current));
    }

    #[test]
    fn live_watcher_generation_survives_focus_away_and_back() {
        let endpoint_a = dbus_source(":1.9", "/a");
        let endpoint_b = dbus_source(":1.9", "/b");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint_a, 10, 1);
        begin_initial_load(&mut tracker, &endpoint_b, 11, 2);
        tracker.begin_load(endpoint_a.clone(), 25);
        assert!(tracker.watcher_ready(endpoint_a.clone(), 10, 25));
        assert!(tracker.accepts_watcher(&endpoint_a, 10));
        assert!(tracker.record(endpoint_a, 10, Some(9)));
    }

    #[test]
    fn recreated_watcher_replaces_generation_and_rejects_queued_old_signal() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        tracker.remove_watcher(&endpoint);
        tracker.begin_load(endpoint.clone(), 25);
        assert!(tracker.watcher_ready(endpoint.clone(), 12, 25));
        assert!(!tracker.record(endpoint.clone(), 10, Some(9)));
        assert!(tracker.record(endpoint, 12, Some(9)));
    }

    #[test]
    fn delayed_ready_for_an_old_request_cannot_restore_a_dead_generation() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        begin_initial_load(&mut tracker, &endpoint, 10, 1);
        tracker.remove_watcher(&endpoint);
        tracker.begin_load(endpoint.clone(), 2);

        assert!(!tracker.watcher_ready(endpoint.clone(), 10, 1));
        assert!(!tracker.accepts_watcher(&endpoint, 10));
        assert!(tracker.watcher_ready(endpoint.clone(), 11, 2));
        assert!(tracker.accepts_watcher(&endpoint, 11));
    }

    #[test]
    fn same_owner_endpoints_keep_independent_watcher_generations() {
        let endpoint_a = dbus_source(":1.9", "/a");
        let endpoint_b = dbus_source(":1.9", "/b");
        let mut tracker = MenuLayoutReloadTracker::default();
        tracker.begin_load(endpoint_a.clone(), 25);
        assert!(tracker.watcher_ready(endpoint_a.clone(), 10, 25));
        assert!(tracker.accepts_watcher(&endpoint_a, 10));
        assert!(!tracker.accepts_watcher(&endpoint_a, 11));
        tracker.begin_load(endpoint_b.clone(), 26);
        assert!(tracker.watcher_ready(endpoint_b.clone(), 11, 26));
        assert!(tracker.accepts_watcher(&endpoint_b, 11));
        assert!(!tracker.accepts_watcher(&endpoint_b, 10));
    }

    #[test]
    fn request_ids_advance_independently_of_live_watcher_generation() {
        let endpoint = dbus_source(":1.9", "/menu");
        let mut tracker = MenuLayoutReloadTracker::default();
        tracker.begin_load(endpoint.clone(), 25);
        assert!(tracker.watcher_ready(endpoint.clone(), 10, 25));
        for request_id in [25, 26, 27] {
            tracker.begin_load(endpoint.clone(), request_id);
            assert!(tracker.accepts_watcher(&endpoint, 10));
        }
    }

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
