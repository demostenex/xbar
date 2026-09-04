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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Notification {
    pub id: NotificationId,
    pub source: NotificationSource,
    pub window_id: Option<WindowId>,
    pub app_name: String,
    pub summary: String,
    pub body: String,
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct AboutToShowPending {
    pub window_id: WindowId,
    pub endpoint: MenuSource,
    pub item_id: super::MenuItemId,
    pub request_id: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub outputs: Vec<OutputState>,
    pub workspaces: Vec<WorkspaceState>,
    pub focused_workspace: Option<String>,
    pub focused_window: Option<WindowId>,
    pub focused_app_name: Option<String>,
    pub menu: MenuState,
    pub global_menu_model: Option<(WindowId, MenuSource, MenuModel)>,
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
        registry.active(self.focused_window)
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
