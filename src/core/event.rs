use super::{
    AudioDevice, AudioState, BluetoothPendingAction, ClockState, MenuItemId,
    MenuItemPropertiesUpdate, MenuModel, MenuSource, NetworkPendingAction, NetworkState,
    OutputState, StatusNotifierAction, StatusNotifierEndpoint, StatusNotifierItem, WindowId,
    WorkspaceState,
};
use crate::platform::x11::X11Event;

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    WorkspacesSnapshot(Vec<WorkspaceState>),
    WorkspaceFocused {
        name: Option<String>,
    },
    #[allow(dead_code)]
    WindowFocused(Option<WindowId>),
    WindowFocusedWithApp {
        window: Option<WindowId>,
        app_name: Option<String>,
    },
    MenuRegistered {
        window_id: WindowId,
        endpoint: MenuSource,
    },
    GtkMenuDiscovered {
        window_id: WindowId,
        endpoint: super::GtkMenuEndpoint,
    },
    GtkMenuRemoved {
        window_id: WindowId,
        endpoint: super::GtkMenuEndpoint,
    },
    MenuUnregistered {
        window_id: WindowId,
    },
    MenuOwnerVanished {
        sender: String,
    },
    MenuWatcherReady {
        endpoint: MenuSource,
        watcher_generation: u64,
        request_id: u64,
    },
    MenuLoadRequested {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
    },
    MenuLazyRootLayoutRequested {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
        intent_id: u64,
        watcher_generation: u64,
    },
    MenuLazyRootLoadConvergence {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
        follow_up_request_id: Option<u64>,
    },
    MenuLoaded {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
        model: MenuModel,
    },
    MenuLoadFailed {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
        error: String,
    },
    MenuRootClicked(MenuItemId),
    MenuItemActivateRequested {
        window_id: WindowId,
        endpoint: MenuSource,
        item_id: MenuItemId,
        timestamp: u32,
    },
    MenuItemHovered {
        path: Vec<MenuItemId>,
    },
    MenuClickedOutside,
    PassivePopupDismissRequested {
        ai_usage: bool,
        notification_center: bool,
    },
    PinCurrentMenuPresentation,
    UnpinMenuPresentation,
    #[allow(dead_code)] // Reserved for the future native global shortcut.
    ToggleMenuPresentationPin,
    MenuNavigationStarted,
    MenuNavigateLeft,
    MenuNavigateRight,
    MenuNavigateUp,
    MenuNavigateDown,
    MenuNavigateEnter,
    MenuNavigateEscape,
    KeyboardGrabAcquired {
        session_id: u64,
    },
    KeyboardGrabFailed {
        session_id: u64,
    },
    TrayMenuOpenRequested {
        endpoint: super::MenuEndpoint,
    },
    TrayMenuLoaded {
        endpoint: super::MenuEndpoint,
        request_id: u64,
        model: MenuModel,
    },
    TrayMenuLoadFailed {
        endpoint: super::MenuEndpoint,
        request_id: u64,
        error: String,
    },
    MenuAboutToShowRequested {
        window_id: WindowId,
        endpoint: MenuSource,
        item_id: MenuItemId,
        request_id: u64,
        lazy_root: bool,
        intent_id: Option<u64>,
        watcher_generation: Option<u64>,
    },
    MenuAboutToShowCompleted {
        window_id: WindowId,
        endpoint: MenuSource,
        item_id: MenuItemId,
        request_id: u64,
        lazy_root: bool,
        intent_id: Option<u64>,
        watcher_generation: Option<u64>,
        need_update: bool,
        model: Option<MenuModel>,
        error: Option<String>,
    },
    MenuLayoutInvalidated {
        endpoint: MenuSource,
        watcher_generation: Option<u64>,
        revision: Option<u32>,
    },
    MenuPropertiesUpdated {
        endpoint: MenuSource,
        watcher_generation: Option<u64>,
        updates: Vec<MenuItemPropertiesUpdate>,
    },
    OutputsChanged(Vec<OutputState>),
    ClockUpdated(ClockState),
    AudioSnapshotReceived(AudioState),
    AudioInventoryReceived {
        outputs: Vec<AudioDevice>,
        inputs: Vec<AudioDevice>,
    },
    AudioSelectOutput(String),
    AudioSelectInput(String),
    AudioUnavailable,
    #[allow(dead_code)]
    NetworkSnapshotReceived(NetworkState),
    NetworkStatusChanged(super::NetworkStatus),
    NetworkPopupProjectionChanged(NetworkState),
    NetworkConnectSavedWifi(super::NetworkWifiTarget),
    NetworkPopupOpenRequested,
    #[allow(dead_code)]
    NetworkPopupSnapshotReceived(NetworkState),
    #[allow(dead_code)]
    NetworkPopupSnapshotFailed,
    NetworkPopupToggled,
    NetworkSetWireless(bool),
    NetworkActionFinished(NetworkPendingAction),
    #[allow(dead_code)]
    ActiveAiUsageChanged(Vec<super::ActiveAgentUsage>),
    AiUsagePopupToggled {
        plugin: super::PluginId,
        output: super::OutputId,
    },
    BluetoothSnapshotReceived(super::BluetoothState),
    BluetoothUnavailable,
    BluetoothPopupToggled,
    BluetoothSetPowered(bool),
    BluetoothConnectDevice(String),
    BluetoothDisconnectDevice(String),
    BluetoothActionFinished(BluetoothPendingAction),
    #[allow(dead_code)]
    NotificationsSnapshot(Vec<super::Notification>),
    NotificationsState {
        active: Vec<super::Notification>,
        history: Vec<super::NotificationHistoryEntry>,
        action_projections: Vec<super::state::NotificationActionProjection>,
    },
    #[allow(dead_code)]
    ToggleNotificationCenter(super::OutputId),
    EnsureNotificationCenterOpen {
        output: super::OutputId,
        target: Option<super::HistoryEntryId>,
    },
    #[allow(dead_code)]
    ExpandNotificationGroup(super::GroupKey),
    #[allow(dead_code)]
    CollapseNotificationGroup(super::GroupKey),
    NotificationToastConsumed,
    WindowAttentionChanged {
        window: WindowId,
        app_name: String,
        attention: bool,
    },
    AudioPopupToggled,
    AudioTrackChanged {
        input: bool,
        percent: u32,
    },
    AudioDragReleased,
    AudioMuteToggled {
        input: bool,
    },
    StatusNotifierRegistered(StatusNotifierEndpoint),
    StatusNotifierUnregistered(StatusNotifierEndpoint),
    StatusNotifierOwnerVanished(String),
    StatusNotifierWatcherUnavailable,
    StatusNotifierItemUpdated(StatusNotifierItem),
    StatusNotifierHostRegistered,
    StatusNotifierActionRequested {
        endpoint: StatusNotifierEndpoint,
        action: StatusNotifierAction,
        root_x: i32,
        root_y: i32,
    },
    X11(X11Event),
}
