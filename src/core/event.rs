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
    MenuLoadRequested {
        window_id: WindowId,
        endpoint: MenuSource,
        request_id: u64,
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
    },
    MenuAboutToShowCompleted {
        window_id: WindowId,
        endpoint: MenuSource,
        item_id: MenuItemId,
        request_id: u64,
        need_update: bool,
        model: Option<MenuModel>,
        error: Option<String>,
    },
    MenuLayoutInvalidated {
        endpoint: MenuSource,
        revision: Option<u32>,
    },
    MenuPropertiesUpdated {
        endpoint: MenuSource,
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
    NetworkPopupOpenRequested,
    #[allow(dead_code)]
    NetworkPopupSnapshotReceived(NetworkState),
    #[allow(dead_code)]
    NetworkPopupSnapshotFailed,
    NetworkPopupToggled,
    NetworkSetWireless(bool),
    NetworkActionFinished(NetworkPendingAction),
    BluetoothSnapshotReceived(super::BluetoothState),
    BluetoothUnavailable,
    BluetoothPopupToggled,
    BluetoothSetPowered(bool),
    BluetoothConnectDevice(String),
    BluetoothDisconnectDevice(String),
    BluetoothActionFinished(BluetoothPendingAction),
    NotificationsSnapshot(Vec<super::Notification>),
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
