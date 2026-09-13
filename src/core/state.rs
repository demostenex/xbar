#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct WindowId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OutputId(pub u32);

#[derive(Clone, Debug, PartialEq)]
pub struct OutputState {
    pub id: OutputId,
    pub name: String,
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceState {
    pub name: String,
    pub output: Option<String>,
    pub focused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClockState {
    pub hour: u8,
    pub minute: u8,
    pub day: u8,
    pub month: u8,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioDevice {
    pub name: String,
    pub display_name: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioState {
    pub available: bool,
    pub default_output: Option<String>,
    pub volume_percent: u32,
    pub muted: bool,
    pub default_input: Option<String>,
    pub input_description: Option<String>,
    pub input_volume_percent: u32,
    pub input_muted: bool,
    pub output_description: Option<String>,
    pub outputs: Vec<AudioDevice>,
    pub inputs: Vec<AudioDevice>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkAccessPoint {
    pub path: String,
    pub device_path: String,
    pub interface: String,
    pub ssid: String,
    pub strength: u8,
    pub frequency: u32,
    pub is_active: bool,
    pub saved_profile: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkWifiTarget {
    pub interface: String,
    pub ssid: String,
    pub band: String,
    pub saved: bool,
    pub active: bool,
}

pub fn wifi_band(frequency: u32) -> &'static str {
    match frequency {
        2400..=2500 => "2.4 GHz",
        4900..=6000 => "5 GHz",
        _ => "unknown band",
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WifiDevice {
    pub path: String,
    pub interface: String,
    pub driver: Option<String>,
    pub state: u32,
    pub raw_access_points: usize,
    pub named_access_points: usize,
    pub active_connection: Option<String>,
    pub active_ap: Option<String>,
    pub access_points: Vec<NetworkAccessPoint>,
}

#[allow(dead_code)] // Kept as a domain mapping for a future network details presentation.
pub fn wifi_device_state_label(state: u32) -> &'static str {
    match state {
        10 => "Não gerenciada",
        20 => "Indisponível",
        30 => "Desconectada",
        40..=90 => "Conectando",
        100 => "Conectada",
        110 => "Desconectando",
        120 => "Falha",
        _ => "Estado desconhecido",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkPendingAction {
    SetWireless(bool),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkState {
    pub available: bool,
    pub wireless_enabled: bool,
    pub connectivity: NetworkConnectivity,
    pub link_kind: NetworkLinkKind,
    pub interface: Option<String>,
    pub display_name: Option<String>,
    pub signal_percent: Option<u8>,
    pub access_points: Vec<NetworkAccessPoint>,
    pub wifi_devices: Vec<WifiDevice>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkStatus {
    pub available: bool,
    pub connected: bool,
    pub interface: Option<String>,
    pub ssid: Option<String>,
    pub frequency: Option<u32>,
    pub strength: Option<u8>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PluginId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PluginStatus {
    Ready,
    Stale,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginSummary {
    pub id: PluginId,
    pub display_name: String,
    pub text: String,
    pub status: PluginStatus,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PluginZoneState {
    pub plugins: Vec<PluginSummary>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[allow(dead_code)]
pub enum AccountIdentity {
    Default,
    Named(String),
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum UsageStatus {
    Fresh,
    Stale,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub enum UsageValue {
    Percentage {
        remaining_pct: Option<u16>,
        used_pct: Option<u16>,
    },
    Amount {
        value: String,
        unit: Option<String>,
    },
    Count {
        value: u64,
        unit: Option<String>,
    },
    Text {
        value: String,
        unit: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageMeter {
    pub id: String,
    pub label: String,
    pub remaining_pct: Option<u16>,
    pub used_pct: Option<u16>,
    pub value: Option<UsageValue>,
    pub reset_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSummary {
    pub label: String,
    pub remaining_pct: Option<u16>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActiveAgentUsage {
    pub agent_id: String,
    pub provider_id: String,
    pub account_id: AccountIdentity,
    pub display_name: String,
    pub active_instances: u32,
    pub meters: Vec<UsageMeter>,
    pub summary: UsageSummary,
    pub status: UsageStatus,
    pub fetched_at: Option<u64>,
    pub cache_age_secs: Option<u64>,
}

const AI_USAGE_GLYPH: &str = "\u{f06a9}";

impl ActiveAgentUsage {
    pub fn plugin_summary(&self) -> PluginSummary {
        let account = match &self.account_id {
            AccountIdentity::Default => "default".to_owned(),
            AccountIdentity::Named(value) => format!("named:{value}"),
            AccountIdentity::Unknown => "unknown".to_owned(),
        };
        let text = match self.status {
            UsageStatus::Fresh | UsageStatus::Stale => self
                .summary
                .remaining_pct
                .map(|percent| format!("{AI_USAGE_GLYPH} {} {}%", self.display_name, percent))
                .unwrap_or_else(|| format!("{AI_USAGE_GLYPH} {} ?", self.display_name)),
            UsageStatus::Unavailable | UsageStatus::Unknown => {
                format!("{AI_USAGE_GLYPH} {} ?", self.display_name)
            }
        };
        PluginSummary {
            id: PluginId(format!(
                "ai-usage:{}:{}:{account}",
                self.provider_id, self.agent_id
            )),
            display_name: self.display_name.clone(),
            text,
            status: match self.status {
                UsageStatus::Fresh => PluginStatus::Ready,
                UsageStatus::Stale => PluginStatus::Stale,
                UsageStatus::Unavailable => PluginStatus::Unavailable,
                UsageStatus::Unknown => PluginStatus::Unknown,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BluetoothDevice {
    pub path: String,
    pub address: String,
    pub alias: String,
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BluetoothState {
    pub available: bool,
    pub powered: bool,
    pub devices: Vec<BluetoothDevice>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BluetoothPendingAction {
    SetPowered(bool),
    ConnectDevice(String),
    DisconnectDevice(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NotificationId(pub u32);

#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HistoryEntryId(pub u64);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Notification {
    pub id: NotificationId,
    pub source: NotificationSource,
    pub window_id: Option<WindowId>,
    pub app_name: String,
    pub summary: String,
    pub body: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationActionView {
    pub key: String,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationActionProjection {
    pub history_id: HistoryEntryId,
    pub actions: Vec<NotificationActionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationHistoryEntry {
    pub id: HistoryEntryId,
    pub live_notification_id: Option<NotificationId>,
    pub source: NotificationSource,
    pub app_name: String,
    pub summary: String,
    pub body: String,
    pub order: u64,
    pub received_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum GroupKey {
    ApplicationName(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationCenterItem {
    Single {
        member: HistoryEntryId,
    },
    Group {
        key: GroupKey,
        members: Vec<HistoryEntryId>,
        expanded: bool,
    },
}

impl NotificationCenterItem {
    pub fn members(&self) -> &[HistoryEntryId] {
        match self {
            Self::Single { member } => std::slice::from_ref(member),
            Self::Group { members, .. } => members,
        }
    }

    pub fn group_key(&self) -> Option<&GroupKey> {
        match self {
            Self::Single { .. } => None,
            Self::Group { key, .. } => Some(key),
        }
    }

    #[allow(dead_code)]
    pub fn front_member(&self) -> HistoryEntryId {
        self.members()[0]
    }

    #[allow(dead_code)]
    pub fn is_expanded(&self) -> bool {
        matches!(self, Self::Group { expanded: true, .. })
    }
}

#[cfg(test)]
mod notification_group_tests {
    use super::*;

    fn entry(id: u64, app_name: &str) -> NotificationHistoryEntry {
        NotificationHistoryEntry {
            id: HistoryEntryId(id),
            live_notification_id: None,
            source: NotificationSource::Freedesktop,
            app_name: app_name.into(),
            summary: id.to_string(),
            body: String::new(),
            order: id,
            received_at: id,
            updated_at: id,
        }
    }

    fn group_items(entries: &[NotificationHistoryEntry]) -> Vec<NotificationCenterItem> {
        notification_center_items(entries, &Default::default())
    }

    #[test]
    fn same_app_forms_one_newest_first_group() {
        let items = group_items(&[
            entry(3, "Discord"),
            entry(2, "Discord"),
            entry(1, "Discord"),
        ]);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].group_key(),
            Some(&GroupKey::ApplicationName("Discord".into()))
        );
        assert_eq!(
            items[0].members(),
            &[HistoryEntryId(3), HistoryEntryId(2), HistoryEntryId(1)]
        );
        assert_eq!(items[0].front_member(), HistoryEntryId(3));
        assert!(!items[0].is_expanded());
    }

    #[test]
    fn different_apps_keep_first_newest_occurrence_order() {
        let items = group_items(&[
            entry(2, "Discord"),
            entry(4, "Slack"),
            entry(1, "Discord"),
            entry(3, "Slack"),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].group_key(),
            Some(&GroupKey::ApplicationName("Discord".into()))
        );
        assert_eq!(items[0].members(), &[HistoryEntryId(2), HistoryEntryId(1)]);
        assert_eq!(
            items[1].group_key(),
            Some(&GroupKey::ApplicationName("Slack".into()))
        );
        assert_eq!(items[1].members(), &[HistoryEntryId(4), HistoryEntryId(3)]);
    }

    #[test]
    fn empty_names_are_independent_singles_and_names_are_exact() {
        let items = group_items(&[
            entry(4, ""),
            entry(3, ""),
            entry(2, "Discord"),
            entry(1, "discord"),
        ]);
        assert!(matches!(
            items[0],
            NotificationCenterItem::Single {
                member: HistoryEntryId(4)
            }
        ));
        assert!(matches!(
            items[1],
            NotificationCenterItem::Single {
                member: HistoryEntryId(3)
            }
        ));
        assert_eq!(items[2].group_key(), None);
        assert_eq!(items[3].group_key(), None);
    }

    #[test]
    fn expansion_is_derived_without_redundant_member_count() {
        let mut expanded = std::collections::HashSet::new();
        expanded.insert(GroupKey::ApplicationName("Discord".into()));
        let items = notification_center_items(
            &[entry(2, "Discord"), entry(1, "Discord"), entry(9, "Slack")],
            &expanded,
        );
        assert!(items[0].is_expanded());
        assert_eq!(items[0].members().len(), 2);
        assert_eq!(items[1].members().len(), 1);
    }

    #[test]
    fn group_transitions_follow_authoritative_members() {
        let key = GroupKey::ApplicationName("Discord".into());
        let expanded = std::collections::HashSet::from([key.clone()]);
        let group =
            notification_center_items(&[entry(2, "Discord"), entry(1, "Discord")], &expanded);
        assert!(group[0].is_expanded());
        let singleton = notification_center_items(&[entry(1, "Discord")], &expanded);
        assert!(matches!(
            singleton[0],
            NotificationCenterItem::Single { .. }
        ));
        assert!(!singleton[0].is_expanded());
        assert!(notification_center_items(&[], &expanded).is_empty());
    }

    #[test]
    fn target_lookup_returns_only_real_group_members() {
        let history = [entry(3, "Discord"), entry(2, "Discord"), entry(1, "Slack")];
        assert_eq!(
            notification_group_for_history_id(&history, HistoryEntryId(2)),
            Some(GroupKey::ApplicationName("Discord".into()))
        );
        assert_eq!(
            notification_group_for_history_id(&history, HistoryEntryId(1)),
            None
        );
        assert_eq!(
            notification_group_for_history_id(&history, HistoryEntryId(99)),
            None
        );
    }
}

pub fn notification_center_items(
    history: &[NotificationHistoryEntry],
    expanded_groups: &std::collections::HashSet<GroupKey>,
) -> Vec<NotificationCenterItem> {
    let mut items = Vec::new();
    let mut group_positions = std::collections::HashMap::new();

    for entry in history {
        let Some(key) =
            (!entry.app_name.is_empty()).then(|| GroupKey::ApplicationName(entry.app_name.clone()))
        else {
            items.push(NotificationCenterItem::Single { member: entry.id });
            continue;
        };

        if let Some(&position) = group_positions.get(&key) {
            match &mut items[position] {
                NotificationCenterItem::Single { member } => {
                    let first = *member;
                    items[position] = NotificationCenterItem::Group {
                        expanded: expanded_groups.contains(&key),
                        key: key.clone(),
                        members: vec![first, entry.id],
                    };
                }
                NotificationCenterItem::Group { members, .. } => members.push(entry.id),
            }
        } else {
            group_positions.insert(key, items.len());
            items.push(NotificationCenterItem::Single { member: entry.id });
        }
    }

    items
}

pub fn notification_group_keys(
    history: &[NotificationHistoryEntry],
) -> std::collections::HashSet<GroupKey> {
    notification_center_items(history, &Default::default())
        .into_iter()
        .filter_map(|item| item.group_key().cloned())
        .collect()
}

pub fn notification_group_for_history_id(
    history: &[NotificationHistoryEntry],
    history_id: HistoryEntryId,
) -> Option<GroupKey> {
    notification_center_items(history, &Default::default())
        .into_iter()
        .find(|item| item.members().contains(&history_id))
        .and_then(|item| item.group_key().cloned())
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum NotificationSource {
    #[default]
    Freedesktop,
    WindowAttention,
    #[allow(dead_code)]
    Internal,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub enum NetworkConnectivity {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Limited,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum NetworkLinkKind {
    #[default]
    Other,
    Ethernet,
    Wifi,
}

use super::{MenuModel, MenuSource};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuInteractionState {
    pub open_root: Option<super::MenuItemId>,
    pub open_path: Vec<super::MenuItemId>,
    pub hovered_path: Vec<super::MenuItemId>,
    pub about_to_show_item: Option<super::MenuItemId>,
    pub pending_about_to_show: Option<AboutToShowPending>,
    pub pending_lazy_root: Option<LazyRootOpenPending>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AboutToShowPending {
    pub window_id: WindowId,
    pub endpoint: MenuSource,
    pub item_id: super::MenuItemId,
    pub request_id: u64,
    pub lazy_root: bool,
    pub intent_id: Option<u64>,
    pub watcher_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LazyRootOpenPending {
    pub window_id: WindowId,
    pub endpoint: MenuSource,
    pub item_id: super::MenuItemId,
    pub intent_id: u64,
    pub watcher_generation: u64,
    pub layout_request_id: Option<u64>,
}

/// Identifies the remote source currently allowed to present and update the
/// canonical global-menu model.  It is deliberately independent from real
/// X11 focus: K0 still follows focus, while a later interaction session can
/// retain this identity without freezing `focused_window`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuPresentation {
    pub window_id: WindowId,
    pub endpoint: MenuSource,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum MenuPresentationPolicy {
    #[default]
    FollowFocus,
    Pinned {
        workspace: String,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeyboardGrabState {
    #[default]
    Requested,
    Active,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MenuNavigationSession {
    pub id: u64,
    pub source_window: WindowId,
    pub endpoint: MenuSource,
    pub selected_path: Option<Vec<super::MenuItemId>>,
    pub grab_state: KeyboardGrabState,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub outputs: Vec<OutputState>,
    pub workspaces: Vec<WorkspaceState>,
    pub focused_workspace: Option<String>,
    pub focused_window: Option<WindowId>,
    pub focused_app_name: Option<String>,
    pub menu_presentation: Option<MenuPresentation>,
    pub menu_presentation_policy: MenuPresentationPolicy,
    pub menu_presentation_needs_focus_reconciliation: bool,
    pub menu_navigation: Option<MenuNavigationSession>,
    pub next_menu_navigation_session: u64,
    pub menu: MenuState,
    pub global_menu_model: Option<(WindowId, MenuSource, MenuModel)>,
    pub watcher_generations: HashMap<MenuSource, u64>,
    pub next_lazy_root_intent: u64,
    pub menu_interaction: MenuInteractionState,
    pub clock: Option<ClockState>,
    pub audio: AudioState,
    pub network: NetworkState,
    pub network_status: NetworkStatus,
    pub network_status_authoritative: bool,
    pub network_pending: Vec<NetworkPendingAction>,
    pub bluetooth: BluetoothState,
    pub bluetooth_pending: Vec<BluetoothPendingAction>,
    pub bluetooth_popup_open: bool,
    pub network_popup_open: bool,
    pub network_popup_open_pending: bool,
    pub ai_usage: Vec<ActiveAgentUsage>,
    pub plugin_zone: PluginZoneState,
    pub audio_popup_open: bool,
    pub notifications: Vec<Notification>,
    pub notification_history: Vec<NotificationHistoryEntry>,
    pub notification_action_projections: Vec<NotificationActionProjection>,
    pub notification_center_open: Option<OutputId>,
    pub expanded_notification_groups: std::collections::HashSet<GroupKey>,
    pub audio_dragging: bool,
    pub audio_drag_input: bool,
    pub status_notifiers: super::StatusNotifierRegistry,
    pub status_notifier_items: super::StatusNotifierItemRegistry,
    pub status_notifier_host_registered: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum MenuState {
    #[default]
    NoMenu,
    Loading {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
    },
    Loaded {
        window_id: WindowId,
        endpoint: MenuSource,
        model: MenuModel,
    },
    Error {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
        error: String,
    },
    TrayLoading {
        endpoint: super::MenuEndpoint,
        request_id: u64,
    },
    TrayLoaded {
        endpoint: super::MenuEndpoint,
        model: MenuModel,
    },
    TrayError {
        endpoint: super::MenuEndpoint,
        request_id: u64,
        error: String,
    },
}

impl State {
    pub fn active_menu_model(&self) -> Option<&MenuModel> {
        match &self.menu {
            MenuState::Loaded { model, .. } | MenuState::TrayLoaded { model, .. } => Some(model),
            MenuState::NoMenu
            | MenuState::Loading { .. }
            | MenuState::Error { .. }
            | MenuState::TrayLoading { .. }
            | MenuState::TrayError { .. } => None,
        }
    }

    pub fn active_menu_endpoint(&self, registry: &super::MenuRegistry) -> Option<MenuSource> {
        self.menu_presentation
            .as_ref()
            .map(|presentation| presentation.endpoint.clone())
            .or_else(|| registry.active(self.focused_window))
    }

    pub fn menu_presentation_matches(&self, window_id: WindowId, endpoint: &MenuSource) -> bool {
        self.menu_presentation.as_ref().map_or_else(
            || self.focused_window == Some(window_id),
            |presentation| {
                presentation.window_id == window_id && presentation.endpoint == *endpoint
            },
        )
    }

    pub fn menu_presentation_window(&self) -> Option<WindowId> {
        self.menu_presentation
            .as_ref()
            .map(|presentation| presentation.window_id)
            .or(self.focused_window)
    }

    pub fn current_menu_source(&self, registry: &super::MenuRegistry) -> Option<MenuSource> {
        match &self.menu {
            MenuState::Loading { endpoint, .. }
            | MenuState::Loaded { endpoint, .. }
            | MenuState::Error { endpoint, .. } => Some(endpoint.clone()),
            MenuState::TrayLoading { endpoint, .. }
            | MenuState::TrayLoaded { endpoint, .. }
            | MenuState::TrayError { endpoint, .. } => Some(MenuSource::Tray(endpoint.clone())),
            MenuState::NoMenu => self.active_menu_endpoint(registry),
        }
    }
}
use std::collections::HashMap;
