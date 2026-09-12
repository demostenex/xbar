use crate::core::{
    GtkMenuEndpoint, MenuItemId, NetworkWifiTarget, OutputId, OutputState, State,
    StatusNotifierEndpoint, WindowId,
};
use crate::ui::style::{self, TextMeasurer, BAR_STYLE, POPUP_STYLE};
use crate::ui::{layout, view};
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::render::{self, ConnectionExt as RenderExt};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ButtonIndex, ConnectionExt as XprotoExt, EventMask, GrabMode, ModMask,
    WindowClass,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as WrapperExt;
use x11rb::xcb_ffi::XCBConnection;

use super::surface::{
    select_argb_visual, DirectPixelFormat, SurfaceEffect, SurfaceRole, SurfaceVisual,
    VisualCandidate,
};
use super::x11_text::X11Text;

fn trace_x11_resource(event: &str, role: &str, xid: u32) {
    if std::env::var_os("XBAR_TRACE_XFT").is_some() {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(stderr, "xbar xft: {event} role={role} xid=0x{xid:x}");
        let _ = stderr.flush();
    }
}

fn notification_scroll_trace_enabled() -> bool {
    std::env::var_os("XBAR_TRACE_NOTIFICATION_SCROLL").is_some()
}

/// Render an eligible template pixel while retaining its source alpha as
/// antialiasing. Color tray pixmaps bypass this function entirely.
fn template_icon_pixel(pixel: u32, foreground: u32, background: u32) -> Option<u32> {
    let alpha = (pixel >> 24) as u8;
    if alpha == 0 {
        return None;
    }
    if alpha == u8::MAX {
        return Some(foreground);
    }
    let blend = |foreground: u8, background: u8| {
        ((u16::from(foreground) * u16::from(alpha)
            + u16::from(background) * u16::from(u8::MAX - alpha)
            + 127)
            / 255) as u8
    };
    Some(
        (u32::from(blend(
            ((foreground >> 16) & 0xff) as u8,
            ((background >> 16) & 0xff) as u8,
        )) << 16)
            | (u32::from(blend(
                ((foreground >> 8) & 0xff) as u8,
                ((background >> 8) & 0xff) as u8,
            )) << 8)
            | u32::from(blend((foreground & 0xff) as u8, (background & 0xff) as u8)),
    )
}

fn preserve_color_pixel(pixel: u32) -> Option<u32> {
    ((pixel >> 24) as u8 != 0).then_some(pixel & 0x00ff_ffff)
}

const TRAY_ICON_MAX_SIZE: u16 = 14;

fn tray_draw_size(width: u16, height: u16) -> (u16, u16) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    if width >= height {
        (
            TRAY_ICON_MAX_SIZE.min(width),
            (u32::from(height) * u32::from(TRAY_ICON_MAX_SIZE.min(width)) / u32::from(width)).max(1)
                as u16,
        )
    } else {
        (
            (u32::from(width) * u32::from(TRAY_ICON_MAX_SIZE.min(height)) / u32::from(height))
                .max(1) as u16,
            TRAY_ICON_MAX_SIZE.min(height),
        )
    }
}

struct PopupMeasurer<'a>(&'a X11Text);
impl TextMeasurer for PopupMeasurer<'_> {
    fn measure_width(&self, text: &str) -> u16 {
        self.0.measure_popup_width(text)
    }
    fn metrics(&self) -> style::FontMetrics {
        self.0.popup_metrics()
    }
}

const BAR_HEIGHT: u16 = 26;
const XK_G: u32 = 0x0067;
const XK_M: u32 = 0x006d;
const XK_NUM_LOCK: u32 = 0xff7f;

/// The process-global passive shortcut is intentionally platform-owned.  It
/// has no relationship to the temporary active keyboard grab used by menu
/// navigation.
#[derive(Clone, Debug)]
struct GlobalPinShortcut {
    keycode: u8,
    num_lock_mask: Option<ModMask>,
    grab_modifiers: Vec<ModMask>,
    event: crate::core::Event,
    down: bool,
    pending_release_timestamp: Option<u32>,
}

impl GlobalPinShortcut {
    fn from_keycode(keycode: Option<u8>, num_lock_mask: Option<ModMask>) -> Option<Self> {
        keycode.map(|keycode| Self::new(keycode, num_lock_mask))
    }

    fn new(keycode: u8, num_lock_mask: Option<ModMask>) -> Self {
        Self::for_event(
            keycode,
            num_lock_mask,
            crate::core::Event::ToggleMenuPresentationPin,
        )
    }

    fn for_event(keycode: u8, num_lock_mask: Option<ModMask>, event: crate::core::Event) -> Self {
        let base = ModMask::M4 | ModMask::SHIFT;
        let mut variants = vec![base, base | ModMask::LOCK];
        if let Some(num_lock_mask) = num_lock_mask {
            variants.push(base | num_lock_mask);
            variants.push(base | ModMask::LOCK | num_lock_mask);
        }
        variants.sort();
        variants.dedup();
        Self {
            keycode,
            num_lock_mask,
            grab_modifiers: variants,
            event,
            down: false,
            pending_release_timestamp: None,
        }
    }

    fn modifier_variants(&self) -> &[ModMask] {
        &self.grab_modifiers
    }

    fn matches_press(&self, keycode: u8, state: u16) -> bool {
        if keycode != self.keycode {
            return false;
        }
        let required = u16::from(ModMask::M4 | ModMask::SHIFT);
        let ignored = u16::from(ModMask::LOCK) | self.num_lock_mask.map(u16::from).unwrap_or(0);
        state & required == required && state & !(required | ignored) == 0
    }

    fn event(&mut self, event: &X11Event) -> Option<crate::core::Event> {
        // Traditional X11 autorepeat is represented as a KeyRelease followed
        // by a KeyPress for the same keycode and timestamp.  Defer rearming
        // until the next event so that synthetic release does not toggle a
        // second time while the physical key is still held.
        if let Some(timestamp) = self.pending_release_timestamp.take() {
            if matches!(
                event,
                X11Event::KeyPress {
                    keycode,
                    timestamp: next_timestamp,
                    ..
                } if *keycode == self.keycode && *next_timestamp == timestamp
            ) {
                return None;
            }
            self.down = false;
        }
        match event {
            X11Event::KeyPress { keycode, state, .. } if self.matches_press(*keycode, *state) => {
                if self.down {
                    None
                } else {
                    self.down = true;
                    Some(self.event.clone())
                }
            }
            X11Event::KeyRelease {
                keycode, timestamp, ..
            } if *keycode == self.keycode && self.down => {
                self.pending_release_timestamp = Some(*timestamp);
                None
            }
            _ => None,
        }
    }
}

fn install_passive_grabs<E>(
    modifiers: &[ModMask],
    mut grab: impl FnMut(ModMask) -> Result<(), E>,
    mut ungrab: impl FnMut(ModMask),
) -> Result<Vec<ModMask>, E> {
    let mut installed = Vec::with_capacity(modifiers.len());
    for modifier in modifiers.iter().copied() {
        if let Err(error) = grab(modifier) {
            for installed_modifier in installed.iter().copied() {
                ungrab(installed_modifier);
            }
            return Err(error);
        }
        installed.push(modifier);
    }
    Ok(installed)
}

#[derive(Clone, Debug, PartialEq)]
pub enum X11Event {
    RandrChanged,
    InstanceLost,
    Expose(u32),
    ButtonPress {
        window: u32,
        x: i16,
        y: i16,
        root_x: i32,
        root_y: i32,
        button: u8,
        timestamp: u32,
    },
    ButtonRelease {
        window: u32,
        x: i16,
        y: i16,
        button: u8,
    },
    KeyPress {
        keycode: u8,
        state: u16,
        timestamp: u32,
    },
    KeyRelease {
        keycode: u8,
        state: u16,
        timestamp: u32,
    },
    MotionNotify {
        window: u32,
        x: i16,
        y: i16,
    },
    GtkWindowChanged(WindowId),
    GtkWindowsChanged,
    GtkWindowDestroyed(WindowId),
    WindowAttentionChanged {
        window: WindowId,
        app_name: String,
        attention: bool,
    },
    Close,
}
pub struct X11Platform {
    // X11Text must drop before the XCB connection: its XftDraw resources
    // reference drawables owned by this connection.
    text: X11Text,
    conn: XCBConnection,
    root: u32,
    default_surface: SurfaceVisual,
    // One platform-owned colormap is intentionally shared by every xbar-owned
    // glass window using this visual. It is released only after all such
    // windows and their Xft drawables are gone.
    glass_surface: SurfaceVisual,
    atoms: Atoms,
    instance_window: Option<u32>,
    windows: Vec<BarWindow>,
    popups: Vec<PopupWindow>,
    audio_popup: Option<AudioPopupWindow>,
    audio_backing: Option<PopupBacking>,
    bluetooth_popup: Option<BluetoothPopupWindow>,
    bluetooth_backing: Option<PopupBacking>,
    network_popup: Option<NetworkPopupWindow>,
    network_backing: Option<PopupBacking>,
    popup_hover: Option<PopupHover>,
    popup_hover_changed: bool,
    menu_popup_dirty: MenuPopupDirty,
    hover_repaint_active: bool,
    notification: Option<NotificationWindow>,
    notification_center: Option<NotificationCenterWindow>,
    pointer_grabbed: bool,
    keyboard_grab_session: Option<u64>,
    global_pin_shortcut: Option<GlobalPinShortcut>,
    global_navigation_shortcut: Option<GlobalPinShortcut>,
    bar_hits: Vec<BarHitMap>,
    notification_hits: Vec<(u32, OutputId, layout::MenuRect)>,
    previous_contexts: HashMap<u32, view::ContextView>,
}
struct Atoms {
    instance: Atom,
    window_type: Atom,
    dock: Atom,
    strut: Atom,
    strut_partial: Atom,
    state: Atom,
    above: Atom,
    notification: Atom,
    wm_protocols: Atom,
    wm_delete: Atom,
    net_wm_state: Atom,
    demands_attention: Atom,
    wm_hints: Atom,
    net_wm_name: Atom,
    wm_name: Atom,
    gtk_unique_bus_name: Atom,
    gtk_menubar_object_path: Atom,
    gtk_app_menu_object_path: Atom,
    gtk_application_object_path: Atom,
    gtk_window_object_path: Atom,
    unity_object_path: Atom,
    net_client_list: Atom,
    net_wm_window_opacity: Atom,
    blur_behind_region: Atom,
    xomposite_effect_owner: Atom,
}

#[derive(Debug, PartialEq, Eq)]
enum AttentionPropertyRead<T> {
    Value(T),
    WindowGone,
}

fn classify_attention_property_reply<T>(
    window: u32,
    reply: Result<T, x11rb::errors::ReplyError>,
) -> Result<AttentionPropertyRead<T>, x11rb::errors::ReplyError> {
    match reply {
        Ok(value) => Ok(AttentionPropertyRead::Value(value)),
        Err(x11rb::errors::ReplyError::X11Error(error))
            if error.error_kind == x11rb::protocol::ErrorKind::Window
                && error.bad_value == window =>
        {
            Ok(AttentionPropertyRead::WindowGone)
        }
        Err(error) => Err(error),
    }
}

fn classify_property_string_reply(
    window: u32,
    reply: Result<xproto::GetPropertyReply, x11rb::errors::ReplyError>,
) -> Result<Option<xproto::GetPropertyReply>, x11rb::errors::ReplyError> {
    match reply {
        Ok(reply) if reply.value.is_empty() => Ok(None),
        Ok(reply) => Ok(Some(reply)),
        Err(x11rb::errors::ReplyError::X11Error(error))
            if error.error_kind == x11rb::protocol::ErrorKind::Window
                && error.bad_value == window =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
enum AttentionProperty {
    NetWmState,
    WmHints,
}
struct BarWindow {
    output: OutputId,
    window: u32,
    backing: Option<BarBacking>,
}

#[derive(Clone, Copy)]
struct BarBacking {
    pixmap: u32,
    gc: u32,
    width: u16,
    height: u16,
    depth: u8,
}

#[derive(Clone, Copy)]
enum BarTextKind {
    Bar,
    StatusIcon,
}

struct BarText {
    kind: BarTextKind,
    text: String,
    x: i32,
    y: i32,
    color: u32,
}
struct PopupWindow {
    window: u32,
    layout: layout::PopupLayout,
    backing: Option<PopupBacking>,
}
struct AudioPopupWindow {
    window: u32,
    rect: layout::MenuRect,
    track: layout::MenuRect,
    mute: layout::MenuRect,
    input_track: layout::MenuRect,
    input_mute: layout::MenuRect,
    output_devices: Vec<layout::AudioDeviceRow>,
    input_devices: Vec<layout::AudioDeviceRow>,
}

#[derive(Clone, Copy)]
struct PopupBacking {
    pixmap: u32,
    gc: u32,
    width: u16,
    height: u16,
    depth: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum MenuPopupDirty {
    #[default]
    None,
    Specific(HashSet<PopupSlot>),
    All,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct PopupSlot(usize);

impl MenuPopupDirty {
    fn mark(&mut self, slot: PopupSlot) {
        match self {
            Self::None => {
                *self = Self::Specific(HashSet::from([slot]));
            }
            Self::Specific(slots) => {
                slots.insert(slot);
            }
            Self::All => {}
        }
    }

    fn mark_all(&mut self) {
        *self = Self::All;
    }

    fn merge(&mut self, other: Self) {
        match other {
            Self::None => {}
            Self::All => self.mark_all(),
            Self::Specific(slots) => {
                for slot in slots {
                    self.mark(slot);
                }
            }
        }
    }

    fn renders(&self, slot: PopupSlot) -> bool {
        matches!(self, Self::All) || matches!(self, Self::Specific(slots) if slots.contains(&slot))
    }

    fn is_pending(&self) -> bool {
        !matches!(self, Self::None)
    }
}

fn menu_popup_slots_for_item(popups: &[PopupWindow], item_id: MenuItemId) -> Vec<PopupSlot> {
    popups
        .iter()
        .enumerate()
        .filter_map(|(index, popup)| {
            popup
                .layout
                .items
                .iter()
                .any(|item| item.id == item_id)
                .then_some(PopupSlot(index))
        })
        .collect()
}

fn menu_popup_slot_for_window(popups: &[PopupWindow], window: u32) -> Option<PopupSlot> {
    popups
        .iter()
        .enumerate()
        .find(|(_, popup)| popup.window == window)
        .map(|(index, _)| PopupSlot(index))
}

fn popup_slot_is_selected(dirty: &MenuPopupDirty, slot: PopupSlot) -> bool {
    dirty.renders(slot)
}

fn menu_popup_dirty_for_interaction_change(
    popups: &[PopupWindow],
    old_root: Option<MenuItemId>,
    old_open_path: &[MenuItemId],
    old_hovered_path: &[MenuItemId],
    new_root: Option<MenuItemId>,
    new_open_path: &[MenuItemId],
    new_hovered_path: &[MenuItemId],
) -> MenuPopupDirty {
    if old_root != new_root || old_open_path != new_open_path {
        return MenuPopupDirty::All;
    }
    if old_hovered_path == new_hovered_path {
        return MenuPopupDirty::None;
    }
    let mut dirty = MenuPopupDirty::None;
    for item_id in [
        old_hovered_path.last().copied(),
        new_hovered_path.last().copied(),
    ]
    .into_iter()
    .flatten()
    {
        for slot in menu_popup_slots_for_item(popups, item_id) {
            dirty.mark(slot);
        }
    }
    dirty
}

fn backing_matches(backing: Option<PopupBacking>, width: u16, height: u16, depth: u8) -> bool {
    backing.is_some_and(|backing| {
        backing.width == width && backing.height == height && backing.depth == depth
    })
}

fn bar_backing_matches(backing: Option<BarBacking>, width: u16, height: u16, depth: u8) -> bool {
    backing.is_some_and(|backing| {
        backing.width == width && backing.height == height && backing.depth == depth
    })
}
struct BluetoothPopupWindow {
    window: u32,
    rect: layout::MenuRect,
    power: layout::MenuRect,
    devices: Vec<(String, layout::MenuRect)>,
}
struct NetworkPopupWindow {
    window: u32,
    rect: layout::MenuRect,
    wireless: layout::MenuRect,
    access_points: Vec<(NetworkWifiTarget, layout::MenuRect)>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum PopupHover {
    MenuItem(crate::core::MenuItemId),
    AudioOutputDevice(String),
    AudioInputDevice(String),
    BluetoothPower,
    BluetoothDevice(String),
    NetworkWifi(NetworkWifiTarget),
    NetworkWireless,
}
struct NotificationWindow {
    window: u32,
    width: u16,
    height: u16,
    backing: Option<PopupBacking>,
}

struct NotificationCenterWindow {
    window: u32,
    output: OutputId,
    width: u16,
    height: u16,
    backing: Option<PopupBacking>,
    card_hits: Vec<(crate::core::HistoryEntryId, layout::MenuRect)>,
    hover: Option<crate::core::HistoryEntryId>,
    scroll: usize,
    scroll_changed: bool,
    dirty: bool,
}

fn notification_visible_capacity(output_height: u16) -> usize {
    usize::from(output_height.saturating_sub(BAR_HEIGHT + 12) / 62).max(1)
}

fn notification_max_scroll(history_len: usize, visible_capacity: usize) -> usize {
    history_len.saturating_sub(visible_capacity)
}

fn notification_previous_scroll(same_output: bool, previous: usize) -> usize {
    if same_output {
        previous
    } else {
        0
    }
}

fn notification_scroll_target(
    current: usize,
    button: u8,
    history_len: usize,
    visible_capacity: usize,
) -> usize {
    let maximum = notification_max_scroll(history_len, visible_capacity);
    match button {
        4 => current.saturating_sub(1),
        5 => current.saturating_add(1).min(maximum),
        _ => current.min(maximum),
    }
}

fn notification_wheel_direction(button: u8) -> Option<i8> {
    match button {
        4 => Some(-1),
        5 => Some(1),
        _ => None,
    }
}

fn reconcile_notification_scroll(
    previous: usize,
    anchor: Option<crate::core::HistoryEntryId>,
    history: &[crate::core::NotificationHistoryEntry],
    visible_capacity: usize,
) -> usize {
    let scroll = anchor
        .and_then(|anchor| history.iter().position(|entry| entry.id == anchor))
        .unwrap_or(previous);
    scroll.min(notification_max_scroll(history.len(), visible_capacity))
}

fn notification_hover_transition(
    old: Option<crate::core::HistoryEntryId>,
    next: Option<crate::core::HistoryEntryId>,
) -> (Option<crate::core::HistoryEntryId>, bool) {
    (next, old != next)
}

#[cfg(test)]
fn notification_indicator_rect(output: &OutputState) -> layout::MenuRect {
    layout::MenuRect {
        x: (output.x as i32 + output.width as i32 - 8 - 28).max(output.x as i32) as i16,
        y: output.y,
        width: 28.min(output.width),
        height: BAR_HEIGHT,
    }
}

fn notification_indicator_hit(
    indicators: &[(u32, OutputId, layout::MenuRect)],
    window: u32,
    x: i16,
    y: i16,
) -> Option<HitTarget> {
    indicators
        .iter()
        .find(|(bar, _, rect)| {
            *bar == window
                && x >= rect.x
                && x < rect.x + rect.width as i16
                && y >= rect.y
                && y < rect.y + rect.height as i16
        })
        .map(|(_, output, _)| HitTarget::NotificationCenter(*output))
}
type BarHitMap = (
    u32,
    OutputId,
    i16,
    i16,
    Vec<view::MenuVisualItem>,
    Vec<view::TrayVisualItem>,
    Option<view::NetworkVisual>,
    Option<view::AudioVisual>,
    Option<view::BluetoothVisual>,
);

#[derive(Clone, Debug, PartialEq)]
pub enum HitTarget {
    #[allow(dead_code)]
    NotificationCenter(OutputId),
    NotificationCenterCard(crate::core::HistoryEntryId),
    NotificationCenterEmpty,
    TopLevel(crate::core::MenuItemId),
    Item(Vec<crate::core::MenuItemId>),
    Tray(StatusNotifierEndpoint),
    Audio,
    AudioMute,
    AudioTrack,
    AudioInputMute,
    AudioInputTrack,
    AudioOutputDevice(String),
    AudioInputDevice(String),
    AudioInside,
    Bluetooth,
    BluetoothPower,
    BluetoothDevice(String),
    BluetoothInside,
    Network,
    NetworkWifi(NetworkWifiTarget),
    NetworkWireless,
    NetworkInside,
    Outside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderTarget(u16);

impl RenderTarget {
    const WORKSPACES: u16 = 1 << 0;
    const CONTEXT: u16 = 1 << 1;
    const PLUGIN_ZONE: u16 = 1 << 2;
    const TRAY: u16 = 1 << 3;
    const NETWORK: u16 = 1 << 4;
    const BLUETOOTH: u16 = 1 << 5;
    const AUDIO: u16 = 1 << 6;
    const DATETIME: u16 = 1 << 7;
    const POPUP: u16 = 1 << 8;
    const NOTIFICATION: u16 = 1 << 9;
    const DOCK: u16 = Self::WORKSPACES
        | Self::CONTEXT
        | Self::PLUGIN_ZONE
        | Self::TRAY
        | Self::NETWORK
        | Self::BLUETOOTH
        | Self::AUDIO
        | Self::DATETIME;

    #[allow(non_upper_case_globals)]
    pub const Dock: Self = Self(Self::DOCK);
    #[allow(non_upper_case_globals)]
    pub const DockContext: Self = Self(Self::CONTEXT);
    #[allow(non_upper_case_globals)]
    #[allow(dead_code)]
    pub const DockRight: Self = Self(
        Self::PLUGIN_ZONE
            | Self::TRAY
            | Self::NETWORK
            | Self::BLUETOOTH
            | Self::AUDIO
            | Self::DATETIME,
    );
    #[allow(non_upper_case_globals)]
    #[allow(dead_code)]
    pub const DockRightPopup: Self = Self(Self::DockRight.0 | Self::POPUP);
    #[allow(non_upper_case_globals)]
    pub const Popup: Self = Self(Self::POPUP);
    #[allow(non_upper_case_globals)]
    pub const Notification: Self = Self(Self::NOTIFICATION);
    #[allow(non_upper_case_globals)]
    pub const All: Self = Self(Self::DOCK | Self::POPUP | Self::NOTIFICATION);
    #[allow(non_upper_case_globals)]
    pub const Workspaces: Self = Self(Self::WORKSPACES);
    #[allow(non_upper_case_globals)]
    pub const PluginZone: Self = Self(Self::PLUGIN_ZONE);
    #[allow(non_upper_case_globals)]
    pub const Tray: Self = Self(Self::TRAY);
    #[allow(non_upper_case_globals)]
    pub const Network: Self = Self(Self::NETWORK);
    #[allow(non_upper_case_globals)]
    pub const Bluetooth: Self = Self(Self::BLUETOOTH);
    #[allow(non_upper_case_globals)]
    pub const Audio: Self = Self(Self::AUDIO);
    #[allow(non_upper_case_globals)]
    pub const DateTime: Self = Self(Self::DATETIME);

    pub fn merge(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    fn contains(self, region: u16) -> bool {
        self.0 & region != 0
    }
    fn includes_dock(self) -> bool {
        self.contains(Self::DOCK)
    }
    fn is_full_dock(self) -> bool {
        self.0 & Self::DOCK == Self::DOCK
    }

    pub fn debug_regions(self) -> String {
        let regions = [
            (Self::WORKSPACES, "WORKSPACES"),
            (Self::CONTEXT, "CONTEXT"),
            (Self::PLUGIN_ZONE, "PLUGIN_ZONE"),
            (Self::TRAY, "TRAY"),
            (Self::NETWORK, "NETWORK"),
            (Self::BLUETOOTH, "BLUETOOTH"),
            (Self::AUDIO, "AUDIO"),
            (Self::DATETIME, "DATETIME"),
            (Self::POPUP, "POPUP"),
            (Self::NOTIFICATION, "NOTIFICATION"),
        ];
        let names = regions
            .into_iter()
            .filter_map(|(region, name)| self.contains(region).then_some(name))
            .collect::<Vec<_>>();
        format!("[{}]", names.join(","))
    }
}

fn context_bounds(context: &view::ContextView, output: &OutputState) -> layout::MenuRect {
    let left = context
        .workspaces
        .last()
        .map(|rect| rect.x + rect.width as i16)
        .unwrap_or(output.x);
    let right = context
        .plugins
        .first()
        .map(|item| item.rect.x)
        .or_else(|| context.tray.first().map(|item| item.rect.x))
        .or_else(|| context.network.as_ref().map(|item| item.rect.x))
        .or_else(|| context.bluetooth.as_ref().map(|item| item.rect.x))
        .or_else(|| context.audio.as_ref().map(|item| item.rect.x))
        .unwrap_or(output.x + output.width as i16);
    layout::MenuRect {
        x: left,
        y: output.y,
        width: right.saturating_sub(left) as u16,
        height: BAR_HEIGHT,
    }
}

fn x11_rect(rect: layout::MenuRect, output: &OutputState) -> xproto::Rectangle {
    xproto::Rectangle {
        x: rect.x.saturating_sub(output.x),
        y: rect.y.saturating_sub(output.y),
        width: rect.width,
        height: rect.height,
    }
}

fn union_menu_rects(rects: &[layout::MenuRect]) -> Option<layout::MenuRect> {
    let mut rects = rects
        .iter()
        .copied()
        .filter(|rect| rect.width > 0 && rect.height > 0);
    let first = rects.next()?;
    Some(rects.fold(first, |union, rect| {
        let left = union.x.min(rect.x);
        let top = union.y.min(rect.y);
        let right = (union.x as i32 + union.width as i32).max(rect.x as i32 + rect.width as i32);
        let bottom = (union.y as i32 + union.height as i32).max(rect.y as i32 + rect.height as i32);
        layout::MenuRect {
            x: left,
            y: top,
            width: (right - left as i32) as u16,
            height: (bottom - top as i32) as u16,
        }
    }))
}

fn workspace_as_menu(rect: layout::WorkspaceRect) -> layout::MenuRect {
    layout::MenuRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

fn render_direct_format(
    formats: Option<&render::QueryPictFormatsReply>,
    screen: usize,
    visual: u32,
    depth: u8,
) -> Option<DirectPixelFormat> {
    let formats = formats?;
    let visual_format = formats.screens.get(screen).and_then(|screen| {
        screen
            .depths
            .iter()
            .find(|candidate| candidate.depth == depth)
            .and_then(|depth| {
                depth
                    .visuals
                    .iter()
                    .find(|candidate| candidate.visual == visual)
            })
    })?;
    formats
        .formats
        .iter()
        .find(|format| {
            format.id == visual_format.format
                && format.type_ == render::PictType::DIRECT
                && format.depth == depth
        })
        .map(|format| DirectPixelFormat {
            red_shift: format.direct.red_shift,
            red_mask: format.direct.red_mask,
            green_shift: format.direct.green_shift,
            green_mask: format.direct.green_mask,
            blue_shift: format.direct.blue_shift,
            blue_mask: format.direct.blue_mask,
            alpha_shift: format.direct.alpha_shift,
            alpha_mask: format.direct.alpha_mask,
        })
}

#[derive(Clone, Copy)]
struct SurfaceWindowGeometry {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
}

const fn blur_behind_rect(geometry: SurfaceWindowGeometry) -> [u32; 4] {
    [0, 0, geometry.width as u32, geometry.height as u32]
}

fn popup_effect_owner(windows: &[BarWindow], output: OutputId) -> Option<u32> {
    windows
        .iter()
        .find(|bar| bar.output == output)
        .map(|bar| bar.window)
}

const fn effect_owner_property_value(dock: u32) -> [u32; 1] {
    [dock]
}

fn with_default_border_pixel(attributes: xproto::CreateWindowAux) -> xproto::CreateWindowAux {
    if attributes.border_pixel.is_none() && attributes.border_pixmap.is_none() {
        attributes.border_pixel(0)
    } else {
        attributes
    }
}

impl X11Platform {
    fn create_surface_window(
        &self,
        surface: SurfaceVisual,
        role: SurfaceRole,
        window: u32,
        geometry: SurfaceWindowGeometry,
        background: style::Rgba,
        attributes: xproto::CreateWindowAux,
    ) -> Result<(), Box<dyn Error>> {
        let attributes = with_default_border_pixel(attributes)
            .colormap(surface.colormap)
            .background_pixel(surface.background_pixel(background));
        self.conn
            .create_window(
                surface.depth,
                window,
                self.root,
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
                geometry.border_width,
                WindowClass::INPUT_OUTPUT,
                surface.visual,
                &attributes,
            )?
            .check()?;
        self.apply_surface_effect(surface, role, window, geometry)?;
        Ok(())
    }

    fn apply_surface_effect(
        &self,
        surface: SurfaceVisual,
        role: SurfaceRole,
        window: u32,
        geometry: SurfaceWindowGeometry,
    ) -> Result<(), Box<dyn Error>> {
        if matches!(role.effect(surface), Some(SurfaceEffect::BlurBehind)) {
            // The compositor's positive control requests its local surface
            // rectangle explicitly; keep the property synchronized with the
            // real client geometry rather than emitting a degenerate rect.
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.blur_behind_region,
                    AtomEnum::CARDINAL,
                    &blur_behind_rect(geometry),
                )?
                .check()?;
        }
        Ok(())
    }

    fn configure_auxiliary_effect_surface(
        &self,
        role: SurfaceRole,
        popup: u32,
        dock: u32,
    ) -> Result<(), Box<dyn Error>> {
        debug_assert!(role.uses_effect_owner());
        self.conn
            .change_property32(
                xproto::PropMode::REPLACE,
                popup,
                self.atoms.xomposite_effect_owner,
                AtomEnum::WINDOW,
                &effect_owner_property_value(dock),
            )?
            .check()?;
        Ok(())
    }

    fn create_glass_popup_window(
        &self,
        role: SurfaceRole,
        window: u32,
        rect: layout::MenuRect,
        border_width: u16,
        event_mask: EventMask,
    ) -> Result<(), Box<dyn Error>> {
        debug_assert!(role.uses_override_redirect());
        self.create_surface_window(
            self.glass_surface,
            role,
            window,
            SurfaceWindowGeometry {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                border_width,
            },
            POPUP_STYLE.material.background,
            xproto::CreateWindowAux::new()
                .override_redirect(1)
                .border_pixel(self.glass_surface.opaque_pixel(POPUP_STYLE.border))
                .event_mask(event_mask),
        )
    }

    fn draw_popup_frame(
        &self,
        window: u32,
        gc: u32,
        width: u16,
        height: u16,
    ) -> Result<(), Box<dyn Error>> {
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new()
                    .foreground(self.glass_surface.opaque_pixel(POPUP_STYLE.border)),
            )?
            .check()?;
        self.conn
            .poly_rectangle(
                window,
                gc,
                &[xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }],
            )?
            .check()?;
        Ok(())
    }

    fn draw_popup_card(
        &self,
        window: u32,
        gc: u32,
        popup: layout::MenuRect,
        card: layout::MenuRect,
    ) -> Result<(), Box<dyn Error>> {
        let x = card.x - popup.x;
        let y = card.y - popup.y;
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(
                    self.glass_surface
                        .background_pixel(POPUP_STYLE.card_background),
                ),
            )?
            .check()?;
        self.fill_rounded_popup_card(window, gc, x, y, card.width, card.height)?;
        Ok(())
    }

    fn fill_rounded_popup_card(
        &self,
        window: u32,
        gc: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    ) -> Result<(), Box<dyn Error>> {
        let radius = POPUP_STYLE.card_radius.min(width / 2).min(height / 2);
        if radius == 0 {
            self.conn.poly_fill_rectangle(
                window,
                gc,
                &[xproto::Rectangle {
                    x,
                    y,
                    width,
                    height,
                }],
            )?;
            return Ok(());
        }
        let mut strips = Vec::with_capacity(radius as usize * 2 + 1);
        for offset in 0..radius {
            let remaining = radius - offset - 1;
            let inset = remaining.saturating_mul(remaining) / radius;
            let strip_width = width.saturating_sub(inset.saturating_mul(2));
            strips.push(xproto::Rectangle {
                x: x + inset as i16,
                y: y + offset as i16,
                width: strip_width,
                height: 1,
            });
            strips.push(xproto::Rectangle {
                x: x + inset as i16,
                y: y + height as i16 - offset as i16 - 1,
                width: strip_width,
                height: 1,
            });
        }
        strips.push(xproto::Rectangle {
            x,
            y: y + radius as i16,
            width,
            height: height.saturating_sub(radius.saturating_mul(2)),
        });
        self.conn.poly_fill_rectangle(window, gc, &strips)?;
        Ok(())
    }

    fn draw_popup_hover(
        &self,
        window: u32,
        gc: u32,
        popup: layout::MenuRect,
        row: layout::MenuRect,
    ) -> Result<(), Box<dyn Error>> {
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(
                    self.glass_surface
                        .background_pixel(POPUP_STYLE.hover_background),
                ),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: row.x - popup.x,
                y: row.y - popup.y,
                width: row.width,
                height: row.height,
            }],
        )?;
        Ok(())
    }

    fn switch_thumb_rect(rect: layout::MenuRect, enabled: bool) -> layout::MenuRect {
        let thumb = rect.height.saturating_sub(6);
        let x_offset = if enabled {
            rect.width.saturating_sub(thumb.saturating_add(3))
        } else {
            3
        };
        layout::MenuRect {
            x: rect.x.saturating_add(x_offset as i16),
            y: rect.y.saturating_add(3),
            width: thumb,
            height: thumb,
        }
    }

    fn draw_switch(
        &self,
        window: u32,
        gc: u32,
        popup: layout::MenuRect,
        rect: layout::MenuRect,
        enabled: bool,
    ) -> Result<(), Box<dyn Error>> {
        let track = if enabled { 0x61718a } else { 0x47515f };
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(self.glass_surface.opaque_pixel(track)),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: rect.x - popup.x,
                y: rect.y - popup.y,
                width: rect.width,
                height: rect.height,
            }],
        )?;
        let thumb = Self::switch_thumb_rect(rect, enabled);
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(
                    self.glass_surface
                        .opaque_pixel(BAR_STYLE.material.foreground),
                ),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: thumb.x - popup.x,
                y: thumb.y - popup.y,
                width: thumb.width,
                height: thumb.height,
            }],
        )?;
        Ok(())
    }

    fn fill_glass_background(
        &self,
        window: u32,
        gc: u32,
        width: u16,
        height: u16,
    ) -> Result<(), Box<dyn Error>> {
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(
                    self.glass_surface
                        .background_pixel(POPUP_STYLE.material.background),
                ),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        Ok(())
    }

    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (conn, screen) = XCBConnection::connect(None)?;
        let root_screen = &conn.setup().roots[screen];
        let root = root_screen.root;
        let default_surface = SurfaceVisual::default(
            root_screen.root_visual,
            root_screen.root_depth,
            root_screen.default_colormap,
        );
        let intern = |n: &[u8]| -> Result<Atom, Box<dyn Error>> {
            Ok(conn.intern_atom(false, n)?.reply()?.atom)
        };
        let atoms = Atoms {
            window_type: intern(b"_NET_WM_WINDOW_TYPE")?,
            dock: intern(b"_NET_WM_WINDOW_TYPE_DOCK")?,
            strut: intern(b"_NET_WM_STRUT")?,
            strut_partial: intern(b"_NET_WM_STRUT_PARTIAL")?,
            state: intern(b"_NET_WM_STATE")?,
            above: intern(b"_NET_WM_STATE_ABOVE")?,
            notification: intern(b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?,
            wm_protocols: intern(b"WM_PROTOCOLS")?,
            wm_delete: intern(b"WM_DELETE_WINDOW")?,
            net_wm_state: intern(b"_NET_WM_STATE")?,
            demands_attention: intern(b"_NET_WM_STATE_DEMANDS_ATTENTION")?,
            wm_hints: intern(b"WM_HINTS")?,
            net_wm_name: intern(b"_NET_WM_NAME")?,
            wm_name: intern(b"WM_NAME")?,
            instance: intern(b"_XBAR_INSTANCE")?,
            gtk_unique_bus_name: intern(b"_GTK_UNIQUE_BUS_NAME")?,
            gtk_menubar_object_path: intern(b"_GTK_MENUBAR_OBJECT_PATH")?,
            gtk_app_menu_object_path: intern(b"_GTK_APP_MENU_OBJECT_PATH")?,
            gtk_application_object_path: intern(b"_GTK_APPLICATION_OBJECT_PATH")?,
            gtk_window_object_path: intern(b"_GTK_WINDOW_OBJECT_PATH")?,
            unity_object_path: intern(b"_UNITY_OBJECT_PATH")?,
            net_client_list: intern(b"_NET_CLIENT_LIST")?,
            net_wm_window_opacity: intern(b"_NET_WM_WINDOW_OPACITY")?,
            blur_behind_region: intern(b"_KDE_NET_WM_BLUR_BEHIND_REGION")?,
            xomposite_effect_owner: intern(b"_XOMPOSITE_EFFECT_OWNER")?,
        };
        let render_formats = conn
            .render_query_pict_formats()
            .ok()
            .and_then(|reply| reply.reply().ok());
        let text = X11Text::open()?;
        let mut candidates = Vec::new();
        for depth in root_screen
            .allowed_depths
            .iter()
            .filter(|depth| depth.depth == 32)
        {
            for visual in &depth.visuals {
                if let Some(pixel_format) = render_direct_format(
                    render_formats.as_ref(),
                    screen,
                    visual.visual_id,
                    depth.depth,
                ) {
                    candidates.push(VisualCandidate {
                        visual: visual.visual_id,
                        depth: depth.depth,
                        true_color: visual.class == xproto::VisualClass::TRUE_COLOR,
                        pixel_format,
                    });
                }
            }
        }
        let glass_surface = select_argb_visual(&candidates)
            .and_then(|candidate| {
                let colormap = conn.generate_id().ok()?;
                conn.create_colormap(
                    xproto::ColormapAlloc::NONE,
                    colormap,
                    root,
                    candidate.visual,
                )
                .ok()?
                .check()
                .ok()?;
                Some(SurfaceVisual::argb(
                    candidate.visual,
                    colormap,
                    candidate.pixel_format,
                ))
            })
            .unwrap_or(default_surface);
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!(
                "xbar trace: glass surface kind={:?} visual=0x{:x} depth={} colormap=0x{:x} alpha_mask=0x{:x} pixel_format={:?} background_pixel=0x{:x}",
                glass_surface.kind,
                glass_surface.visual,
                glass_surface.depth,
                glass_surface.colormap,
                glass_surface.alpha_mask,
                glass_surface.pixel_format,
                glass_surface.background_pixel(BAR_STYLE.material.background),
            );
        }
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
        )?
        .check()?;
        conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE,
        )?
        .check()?;
        let mut platform = Self {
            conn,
            root,
            default_surface,
            glass_surface,
            atoms,
            text,
            instance_window: None,
            windows: Vec::new(),
            popups: Vec::new(),
            audio_popup: None,
            audio_backing: None,
            bluetooth_popup: None,
            bluetooth_backing: None,
            network_popup: None,
            network_backing: None,
            popup_hover: None,
            popup_hover_changed: false,
            menu_popup_dirty: MenuPopupDirty::None,
            hover_repaint_active: false,
            notification: None,
            notification_center: None,
            pointer_grabbed: false,
            keyboard_grab_session: None,
            global_pin_shortcut: None,
            global_navigation_shortcut: None,
            bar_hits: Vec::new(),
            notification_hits: Vec::new(),
            previous_contexts: HashMap::new(),
        };
        platform.install_global_pin_shortcut();
        platform.install_global_navigation_shortcut();
        Ok(platform)
    }
    pub fn connection(&self) -> &XCBConnection {
        &self.conn
    }
    pub fn root(&self) -> u32 {
        self.root
    }
    pub fn raw_fd(&self) -> RawFd {
        self.conn.as_raw_fd()
    }
    pub fn pointer_grabbed(&self) -> bool {
        self.pointer_grabbed
    }

    pub fn acquire_keyboard_grab(&mut self, session_id: u64) -> Result<bool, Box<dyn Error>> {
        if self.keyboard_grab_session == Some(session_id) {
            return Ok(true);
        }
        if self.keyboard_grab_session.is_some() {
            self.conn.ungrab_keyboard(x11rb::CURRENT_TIME)?.check()?;
            self.keyboard_grab_session = None;
        }
        let reply = self
            .conn
            .grab_keyboard(
                false,
                self.root,
                x11rb::CURRENT_TIME,
                xproto::GrabMode::ASYNC,
                xproto::GrabMode::ASYNC,
            )?
            .reply()?;
        if reply.status == xproto::GrabStatus::SUCCESS {
            self.keyboard_grab_session = Some(session_id);
            self.conn.flush()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn release_keyboard_grab(&mut self, session_id: Option<u64>) -> Result<(), Box<dyn Error>> {
        if session_id.is_none() || self.keyboard_grab_session == session_id {
            if self.keyboard_grab_session.is_some() {
                self.conn.ungrab_keyboard(x11rb::CURRENT_TIME)?.check()?;
                self.conn.flush()?;
            }
            self.keyboard_grab_session = None;
        }
        Ok(())
    }

    pub fn keyboard_grab_session(&self) -> Option<u64> {
        self.keyboard_grab_session
    }

    fn num_lock_mask(&self) -> Result<Option<ModMask>, Box<dyn Error>> {
        let mapping = self.conn.get_modifier_mapping()?.reply()?;
        let keycodes_per_modifier = mapping.keycodes.len() / 8;
        for modifier_index in 0..8 {
            let start = modifier_index * keycodes_per_modifier;
            let end = start + keycodes_per_modifier;
            if mapping.keycodes[start..end]
                .iter()
                .copied()
                .any(|keycode| self.text.lookup_keysym(keycode, 0) == Some(XK_NUM_LOCK))
            {
                return Ok(Some(ModMask::from(1_u8 << modifier_index)));
            }
        }
        Ok(None)
    }

    fn unregister_global_pin_shortcut(&self, shortcut: &GlobalPinShortcut) {
        for modifiers in shortcut.modifier_variants().iter().copied() {
            let _ = self.conn.ungrab_key(shortcut.keycode, self.root, modifiers);
        }
        let _ = self.conn.flush();
    }

    fn install_global_pin_shortcut(&mut self) {
        if self.global_pin_shortcut.is_some() {
            return;
        }
        let result = (|| -> Result<GlobalPinShortcut, Box<dyn Error>> {
            let keycode = self.text.keycode_for_keysym(XK_M);
            let shortcut = GlobalPinShortcut::from_keycode(keycode, self.num_lock_mask()?)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the current X11 keyboard map has no M keysym",
                    )
                })?;
            let installed = install_passive_grabs(
                shortcut.modifier_variants(),
                |modifiers| {
                    self.conn
                        .grab_key(
                            false,
                            self.root,
                            modifiers,
                            shortcut.keycode,
                            xproto::GrabMode::ASYNC,
                            xproto::GrabMode::ASYNC,
                        )?
                        .check()?;
                    Ok::<_, Box<dyn Error>>(())
                },
                |modifiers| {
                    let _ = self.conn.ungrab_key(shortcut.keycode, self.root, modifiers);
                },
            )?;
            if let Err(error) = self.conn.flush() {
                for modifiers in installed {
                    let _ = self.conn.ungrab_key(shortcut.keycode, self.root, modifiers);
                }
                let _ = self.conn.flush();
                return Err(Box::new(error));
            }
            Ok(shortcut)
        })();
        match result {
            Ok(shortcut) => self.global_pin_shortcut = Some(shortcut),
            Err(error) => eprintln!("xbar: global menu pin shortcut unavailable: {error}"),
        }
    }

    fn install_global_navigation_shortcut(&mut self) {
        if self.global_navigation_shortcut.is_some() {
            return;
        }
        let result = (|| -> Result<GlobalPinShortcut, Box<dyn Error>> {
            let num_lock_mask = self.num_lock_mask()?;
            let keycode = self.text.keycode_for_keysym(XK_G);
            let shortcut = keycode
                .map(|keycode| {
                    GlobalPinShortcut::for_event(
                        keycode,
                        num_lock_mask,
                        crate::core::Event::MenuNavigationStarted,
                    )
                })
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "the current X11 keyboard map has no G keysym",
                    )
                })?;
            let installed = install_passive_grabs(
                shortcut.modifier_variants(),
                |modifiers| {
                    self.conn
                        .grab_key(
                            false,
                            self.root,
                            modifiers,
                            shortcut.keycode,
                            xproto::GrabMode::ASYNC,
                            xproto::GrabMode::ASYNC,
                        )?
                        .check()?;
                    Ok::<_, Box<dyn Error>>(())
                },
                |modifiers| {
                    let _ = self.conn.ungrab_key(shortcut.keycode, self.root, modifiers);
                },
            )?;
            if let Err(error) = self.conn.flush() {
                for modifiers in installed {
                    let _ = self.conn.ungrab_key(shortcut.keycode, self.root, modifiers);
                }
                let _ = self.conn.flush();
                return Err(Box::new(error));
            }
            Ok(shortcut)
        })();
        match result {
            Ok(shortcut) => self.global_navigation_shortcut = Some(shortcut),
            Err(error) => eprintln!("xbar: global menu keyboard shortcut unavailable: {error}"),
        }
    }

    /// Translate the passive Super+Shift+M grab before navigation filtering.
    /// Repeated KeyPress events are latched until the matching KeyRelease.
    pub fn global_pin_shortcut_event(&mut self, event: &X11Event) -> Option<crate::core::Event> {
        self.global_pin_shortcut.as_mut()?.event(event)
    }

    /// Translate the passive Super+Shift+G grab before navigation filtering.
    /// Repeated KeyPress events are latched until the matching KeyRelease.
    pub fn global_navigation_shortcut_event(
        &mut self,
        event: &X11Event,
    ) -> Option<crate::core::Event> {
        self.global_navigation_shortcut.as_mut()?.event(event)
    }

    pub fn navigation_event(
        &self,
        event: &X11Event,
    ) -> Result<Option<crate::core::Event>, Box<dyn Error>> {
        let X11Event::KeyPress { keycode, state, .. } = event else {
            return Ok(None);
        };
        let Some(keysym) = self.text.lookup_keysym(*keycode, *state) else {
            return Ok(None);
        };
        Ok(Self::navigation_event_for_keysym(keysym))
    }

    pub(crate) fn navigation_event_for_keysym(keysym: u32) -> Option<crate::core::Event> {
        Some(match keysym {
            0xff51 => crate::core::Event::MenuNavigateLeft,
            0xff52 => crate::core::Event::MenuNavigateUp,
            0xff53 => crate::core::Event::MenuNavigateRight,
            0xff54 => crate::core::Event::MenuNavigateDown,
            0xff0d | 0xff8d => crate::core::Event::MenuNavigateEnter,
            0xff1b => crate::core::Event::MenuNavigateEscape,
            _ => return None,
        })
    }

    /// Hover is renderer-local presentation state. It never changes a domain
    /// model or action; a changed target asks only the mapped popup to redraw.
    pub fn update_popup_hover(&mut self, target: Option<&HitTarget>) -> bool {
        let (next, changed) = popup_hover_transition(&self.popup_hover, target);
        if changed {
            let old_item = match self.popup_hover {
                Some(PopupHover::MenuItem(item_id)) => Some(item_id),
                _ => None,
            };
            let new_item = match next {
                Some(PopupHover::MenuItem(item_id)) => Some(item_id),
                _ => None,
            };
            for item_id in [old_item, new_item].into_iter().flatten() {
                for slot in menu_popup_slots_for_item(&self.popups, item_id) {
                    self.menu_popup_dirty.mark(slot);
                }
            }
            self.popup_hover = next;
            self.popup_hover_changed = true;
        }
        changed
    }

    pub fn note_menu_interaction_change(
        &mut self,
        old_root: Option<MenuItemId>,
        old_open_path: &[MenuItemId],
        old_hovered_path: &[MenuItemId],
        new_root: Option<MenuItemId>,
        new_open_path: &[MenuItemId],
        new_hovered_path: &[MenuItemId],
    ) {
        self.menu_popup_dirty
            .merge(menu_popup_dirty_for_interaction_change(
                &self.popups,
                old_root,
                old_open_path,
                old_hovered_path,
                new_root,
                new_open_path,
                new_hovered_path,
            ));
    }

    pub fn note_menu_popup_exposed(&mut self, window: u32) -> bool {
        if let Some(slot) = menu_popup_slot_for_window(&self.popups, window) {
            self.menu_popup_dirty.mark(slot);
            true
        } else {
            false
        }
    }

    pub fn mark_all_menu_popups_dirty(&mut self) {
        self.menu_popup_dirty.mark_all();
    }

    pub fn has_menu_popup_dirty(&self) -> bool {
        self.menu_popup_dirty.is_pending()
    }

    pub fn audio_track_percent(&self, event: &X11Event) -> Option<u32> {
        let (X11Event::ButtonPress { window, x, .. } | X11Event::MotionNotify { window, x, .. }) =
            event
        else {
            return None;
        };
        let popup = self
            .audio_popup
            .as_ref()
            .filter(|popup| popup.window == *window || self.root == *window)?;
        let root_x = if self.root == *window {
            *x
        } else {
            *x + popup.rect.x
        };
        let relative = root_x
            .saturating_sub(popup.track.x)
            .clamp(0, popup.track.width as i16);
        Some((relative as u32 * 100 / popup.track.width.max(1) as u32).min(100))
    }
    pub fn audio_input_track_percent(&self, event: &X11Event) -> Option<u32> {
        let (X11Event::ButtonPress { window, x, .. } | X11Event::MotionNotify { window, x, .. }) =
            event
        else {
            return None;
        };
        let popup = self
            .audio_popup
            .as_ref()
            .filter(|popup| popup.window == *window || self.root == *window)?;
        let root_x = if self.root == *window {
            *x
        } else {
            *x + popup.rect.x
        };
        let relative = root_x
            .saturating_sub(popup.input_track.x)
            .clamp(0, popup.input_track.width as i16);
        Some((relative as u32 * 100 / popup.input_track.width.max(1) as u32).min(100))
    }
    pub fn popup_count(&self) -> usize {
        self.popups.len() + usize::from(self.audio_popup.is_some())
    }

    pub fn text_raw_fd(&self) -> RawFd {
        self.text.raw_fd()
    }

    pub fn text_font_name(&self) -> &str {
        self.text.font_name()
    }

    pub fn popup_font_name(&self) -> &str {
        self.text.popup_font_name()
    }
    pub fn status_icon_font_name(&self) -> &str {
        self.text.status_icon_font_name()
    }

    pub fn text_metrics(&self) -> crate::ui::style::FontMetrics {
        self.text.metrics()
    }

    pub fn is_dock_window(&self, window: u32) -> bool {
        self.windows.iter().any(|bar| bar.window == window)
    }

    pub fn is_popup_window(&self, window: u32) -> bool {
        self.popups.iter().any(|popup| popup.window == window)
            || self
                .audio_popup
                .as_ref()
                .is_some_and(|popup| popup.window == window)
            || self
                .bluetooth_popup
                .as_ref()
                .is_some_and(|popup| popup.window == window)
            || self
                .network_popup
                .as_ref()
                .is_some_and(|popup| popup.window == window)
            || self
                .notification_center
                .as_ref()
                .is_some_and(|center| center.window == window)
    }

    pub fn is_notification_center_window(&self, window: u32) -> bool {
        self.notification_center
            .as_ref()
            .is_some_and(|center| center.window == window)
    }

    pub fn update_notification_center_hover(&mut self, target: Option<&HitTarget>) -> bool {
        let Some(center) = self.notification_center.as_mut() else {
            return false;
        };
        let next = match target {
            Some(HitTarget::NotificationCenterCard(id))
                if center
                    .card_hits
                    .iter()
                    .any(|(candidate, _)| candidate == id) =>
            {
                Some(*id)
            }
            _ => None,
        };
        let (hover, changed) = notification_hover_transition(center.hover, next);
        if changed {
            center.hover = hover;
            center.dirty = true;
        }
        changed
    }

    pub fn scroll_notification_center(&mut self, button: u8, state: &State) -> bool {
        let Some(center) = self.notification_center.as_mut() else {
            return false;
        };
        let Some(output) = state
            .outputs
            .iter()
            .find(|output| output.id == center.output)
        else {
            return false;
        };
        let capacity = notification_visible_capacity(output.height);
        let Some(_) = notification_wheel_direction(button) else {
            return false;
        };
        let next = notification_scroll_target(
            center.scroll,
            button,
            state.notification_history.len(),
            capacity,
        );
        let before = center.scroll;
        let dirty_before = center.dirty;
        if next == center.scroll {
            if notification_scroll_trace_enabled() {
                eprintln!(
                    "notification-center scroll: button={} scroll_before={} history_len={} visible_capacity={} max_scroll={} scroll_after={} changed=false dirty_before={} dirty_after={}",
                    button,
                    before,
                    state.notification_history.len(),
                    capacity,
                    notification_max_scroll(state.notification_history.len(), capacity),
                    next,
                    dirty_before,
                    center.dirty
                );
            }
            return false;
        }
        center.scroll = next;
        center.scroll_changed = true;
        center.dirty = true;
        if notification_scroll_trace_enabled() {
            eprintln!(
                "notification-center scroll: button={} scroll_before={} history_len={} visible_capacity={} max_scroll={} scroll_after={} changed=true dirty_before={} dirty_after={}",
                button,
                before,
                state.notification_history.len(),
                capacity,
                notification_max_scroll(state.notification_history.len(), capacity),
                center.scroll,
                dirty_before,
                center.dirty
            );
        }
        true
    }

    fn install_notification_center_wheel_grabs(&self, window: u32) -> (String, String) {
        let mut outcomes = Vec::with_capacity(2);
        for (button, label) in [(ButtonIndex::M4, "button4"), (ButtonIndex::M5, "button5")] {
            let result = self.conn.grab_button(
                true,
                window,
                EventMask::BUTTON_PRESS,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                x11rb::NONE,
                x11rb::NONE,
                button,
                ModMask::ANY,
            );
            let outcome = match result {
                Ok(cookie) => match cookie.check() {
                    Ok(()) => "SUCCESS".to_owned(),
                    Err(error) => format!("ERROR:{error}"),
                },
                Err(error) => format!("ERROR:{error}"),
            };
            if notification_scroll_trace_enabled() {
                eprintln!(
                    "notification-center grab {}: xid={} result={}",
                    label, window, outcome
                );
            }
            outcomes.push(outcome);
        }
        (outcomes.remove(0), outcomes.remove(0))
    }
    pub fn acquire_instance(&mut self) -> Result<bool, Box<dyn Error>> {
        let window = self.conn.generate_id()?;
        trace_x11_resource("WINDOW_CREATE", "instance-candidate", window);
        self.conn
            .create_window(
                0,
                window,
                self.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new(),
            )?
            .check()?;

        let owner = self
            .conn
            .get_selection_owner(self.atoms.instance)?
            .reply()?
            .owner;
        if owner != x11rb::NONE {
            trace_x11_resource("WINDOW_DESTROY", "instance-candidate", window);
            self.conn.destroy_window(window)?.check()?;
            return Ok(false);
        }

        self.conn
            .set_selection_owner(window, self.atoms.instance, x11rb::CURRENT_TIME)?
            .check()?;
        let acquired = self
            .conn
            .get_selection_owner(self.atoms.instance)?
            .reply()?
            .owner
            == window;
        if acquired {
            self.instance_window = Some(window);
            self.conn.flush()?;
        } else {
            trace_x11_resource("WINDOW_DESTROY", "instance-candidate", window);
            self.conn.destroy_window(window)?.check()?;
        }
        Ok(acquired)
    }
    pub fn next_event(&mut self) -> Result<Option<X11Event>, Box<dyn Error>> {
        Ok(match self.conn.poll_for_event()? {
            Some(Event::RandrNotify(_)) => Some(X11Event::RandrChanged),
            Some(Event::Expose(e)) => Some(X11Event::Expose(e.window)),
            Some(Event::ButtonPress(e)) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!(
                        "xbar trace: raw ButtonPress event={} root={} child={} event_x={} event_y={} root_x={} root_y={} detail={} state={}",
                        e.event,
                        e.root,
                        e.child,
                        e.event_x,
                        e.event_y,
                        e.root_x,
                        e.root_y,
                        e.detail,
                        e.state.bits()
                    );
                }
                if notification_scroll_trace_enabled() {
                    eprintln!(
                        "notification-center button-press raw: event={} root={} child={} event_x={} event_y={} root_x={} root_y={} detail={} center_xid={}",
                        e.event,
                        e.root,
                        e.child,
                        e.event_x,
                        e.event_y,
                        e.root_x,
                        e.root_y,
                        e.detail,
                        self.notification_center
                            .as_ref()
                            .map_or(0, |center| center.window)
                    );
                }
                Some(X11Event::ButtonPress {
                    window: e.event,
                    x: e.event_x,
                    y: e.event_y,
                    root_x: e.root_x as i32,
                    root_y: e.root_y as i32,
                    button: e.detail,
                    timestamp: e.time,
                })
            }
            Some(Event::MotionNotify(e)) => Some(X11Event::MotionNotify {
                window: e.event,
                x: e.event_x,
                y: e.event_y,
            }),
            Some(Event::ButtonRelease(e)) => Some(X11Event::ButtonRelease {
                window: e.event,
                x: e.event_x,
                y: e.event_y,
                button: e.detail,
            }),
            Some(Event::KeyPress(e)) => Some(X11Event::KeyPress {
                keycode: e.detail,
                state: e.state.bits(),
                timestamp: e.time,
            }),
            Some(Event::KeyRelease(e)) => Some(X11Event::KeyRelease {
                keycode: e.detail,
                state: e.state.bits(),
                timestamp: e.time,
            }),
            Some(Event::SelectionClear(_)) => Some(X11Event::InstanceLost),
            Some(Event::CreateNotify(event)) => {
                if self.is_xbar_owned_window(event.window) {
                    return Ok(None);
                }
                self.select_property_events(event.window, "create")?
                    .then_some(X11Event::GtkWindowChanged(WindowId(event.window)))
            }
            Some(Event::MapNotify(event)) => {
                if self.is_xbar_owned_window(event.window) {
                    return Ok(None);
                }
                self.select_property_events(event.window, "map")?
                    .then_some(X11Event::GtkWindowChanged(WindowId(event.window)))
            }
            Some(Event::DestroyNotify(event)) => {
                Some(X11Event::GtkWindowDestroyed(WindowId(event.window)))
            }
            Some(Event::PropertyNotify(event))
                if event.window == self.root && event.atom == self.atoms.net_client_list =>
            {
                Some(X11Event::GtkWindowsChanged)
            }
            Some(Event::PropertyNotify(event))
                if event.atom == self.atoms.net_wm_state || event.atom == self.atoms.wm_hints =>
            {
                if self.is_xbar_owned_window(event.window) {
                    None
                } else {
                    match self.attention_event(event.window)? {
                        AttentionPropertyRead::Value(event) => Some(event),
                        AttentionPropertyRead::WindowGone => None,
                    }
                }
            }
            Some(Event::PropertyNotify(event)) if self.is_gtk_atom(event.atom) => {
                Some(X11Event::GtkWindowChanged(WindowId(event.window)))
            }
            Some(Event::ClientMessage(event))
                if event.type_ == self.atoms.wm_protocols
                    && event.data.as_data32()[0] == self.atoms.wm_delete =>
            {
                Some(X11Event::Close)
            }
            Some(_) => None,
            None => None,
        })
    }

    fn attention_event(
        &self,
        window: u32,
    ) -> Result<AttentionPropertyRead<X11Event>, Box<dyn Error>> {
        let AttentionPropertyRead::Value(states) =
            self.read_attention_property(window, AttentionProperty::NetWmState)?
        else {
            return Ok(AttentionPropertyRead::WindowGone);
        };
        let states = states
            .value32()
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default();
        let AttentionPropertyRead::Value(hints) =
            self.read_attention_property(window, AttentionProperty::WmHints)?
        else {
            return Ok(AttentionPropertyRead::WindowGone);
        };
        let urgency = hints
            .value32()
            .and_then(|mut values| values.next())
            .is_some_and(|flags| flags & (1 << 8) != 0);
        Ok(AttentionPropertyRead::Value(
            X11Event::WindowAttentionChanged {
                window: WindowId(window),
                app_name: self.window_name(window),
                attention: states.contains(&self.atoms.demands_attention) || urgency,
            },
        ))
    }

    fn read_attention_property(
        &self,
        window: u32,
        property: AttentionProperty,
    ) -> Result<AttentionPropertyRead<xproto::GetPropertyReply>, Box<dyn Error>> {
        let (atom, type_, long_length) = match property {
            AttentionProperty::NetWmState => (self.atoms.net_wm_state, AtomEnum::ATOM, u32::MAX),
            AttentionProperty::WmHints => (self.atoms.wm_hints, AtomEnum::ANY, 9),
        };
        let reply = self
            .conn
            .get_property(false, window, atom, type_, 0, long_length)?
            .reply();
        match classify_attention_property_reply(window, reply) {
            Ok(AttentionPropertyRead::WindowGone) => Ok(AttentionPropertyRead::WindowGone),
            Ok(AttentionPropertyRead::Value(reply)) => Ok(AttentionPropertyRead::Value(reply)),
            Err(error) => Err(error.into()),
        }
    }

    fn window_name(&self, window: u32) -> String {
        for atom in [self.atoms.net_wm_name, self.atoms.wm_name] {
            if let Ok(cookie) =
                self.conn
                    .get_property(false, window, atom, AtomEnum::ANY, 0, u32::MAX)
            {
                if let Ok(reply) = cookie.reply() {
                    let name = String::from_utf8_lossy(&reply.value)
                        .trim_end_matches('\0')
                        .trim()
                        .to_string();
                    if !name.is_empty() {
                        return name;
                    }
                }
            }
        }
        format!("Window {window}")
    }

    fn select_property_events(&self, window: u32, reason: &str) -> Result<bool, Box<dyn Error>> {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!(
                "xbar trace: x11 watch request window=0x{window:08x} reason={reason} mask=PROPERTY_CHANGE"
            );
        }
        let cookie = self.conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        match cookie.check() {
            Ok(()) => {
                self.trace_your_event_mask(window, "after-property-watch")?;
                Ok(true)
            }
            Err(x11rb::errors::ReplyError::X11Error(error))
                if error.error_kind == x11rb::protocol::ErrorKind::Window =>
            {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!(
                        "xbar trace: gmenu discovery stale window=0x{window:08x} reason={reason}"
                    );
                }
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    fn trace_your_event_mask(&self, window: u32, phase: &str) -> Result<(), Box<dyn Error>> {
        if std::env::var_os("XBAR_TRACE").is_none() {
            return Ok(());
        }
        let attributes = self.conn.get_window_attributes(window)?.reply()?;
        eprintln!(
            "xbar trace: window event mask phase={phase} window=0x{window:08x} your_event_mask={:?}",
            attributes.your_event_mask
        );
        Ok(())
    }

    fn is_gtk_atom(&self, atom: Atom) -> bool {
        [
            self.atoms.gtk_unique_bus_name,
            self.atoms.gtk_menubar_object_path,
            self.atoms.gtk_app_menu_object_path,
            self.atoms.gtk_application_object_path,
            self.atoms.gtk_window_object_path,
            self.atoms.unity_object_path,
        ]
        .contains(&atom)
    }

    fn is_xbar_owned_window(&self, window: u32) -> bool {
        is_xbar_owned_window(
            window,
            self.root,
            self.instance_window,
            self.windows.iter().map(|bar| bar.window),
            self.popups.iter().map(|popup| popup.window),
        ) || self
            .notification
            .as_ref()
            .is_some_and(|notification| notification.window == window)
            || self
                .network_popup
                .as_ref()
                .is_some_and(|popup| popup.window == window)
    }

    pub fn discover_gmenu_windows(
        &mut self,
    ) -> Result<Vec<(WindowId, GtkMenuEndpoint)>, Box<dyn Error>> {
        let client_list = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )?
            .reply()?
            .value32()
            .map(|values| values.collect::<Vec<_>>());
        let children = match client_list {
            Some(children) if !children.is_empty() => children,
            _ => self.conn.query_tree(self.root)?.reply()?.children,
        };
        let mut discovered = Vec::new();
        for window in children {
            if self.is_xbar_owned_window(window) {
                continue;
            }
            if self.select_property_events(window, "startup")? {
                if let Some(endpoint) = self.discover_gmenu_window(window)? {
                    discovered.push((WindowId(window), endpoint));
                }
            }
        }
        self.conn.flush()?;
        Ok(discovered)
    }

    pub fn discover_attention_windows(&mut self) -> Result<Vec<X11Event>, Box<dyn Error>> {
        let client_list = self
            .conn
            .get_property(
                false,
                self.root,
                self.atoms.net_client_list,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )?
            .reply()?
            .value32()
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default();
        let mut events = Vec::new();
        for window in client_list {
            if self.is_xbar_owned_window(window) {
                continue;
            }
            if self.select_property_events(window, "attention-startup")? {
                match self.attention_event(window)? {
                    AttentionPropertyRead::Value(event) => events.push(event),
                    AttentionPropertyRead::WindowGone => {}
                }
            }
        }
        self.conn.flush()?;
        Ok(events)
    }

    pub fn discover_gmenu_window(
        &self,
        window: u32,
    ) -> Result<Option<GtkMenuEndpoint>, Box<dyn Error>> {
        let Some(bus_name) = self.property_string(window, self.atoms.gtk_unique_bus_name)? else {
            return Ok(None);
        };
        let menu_object_path = self
            .property_string(window, self.atoms.gtk_menubar_object_path)?
            .or(self.property_string(window, self.atoms.gtk_app_menu_object_path)?)
            .or(self.property_string(window, self.atoms.unity_object_path)?);
        let Some(menu_object_path) = menu_object_path else {
            return Ok(None);
        };
        let mut actions_object_paths = Vec::new();
        for atom in [
            self.atoms.gtk_window_object_path,
            self.atoms.gtk_application_object_path,
        ] {
            if let Some(path) = self.property_string(window, atom)? {
                if !actions_object_paths.contains(&path) {
                    actions_object_paths.push(path);
                }
            }
        }
        if !actions_object_paths.contains(&menu_object_path) {
            actions_object_paths.push(menu_object_path.clone());
        }
        Ok(Some(GtkMenuEndpoint {
            bus_name,
            menu_object_path,
            actions_object_paths,
        }))
    }

    fn property_string(&self, window: u32, atom: Atom) -> Result<Option<String>, Box<dyn Error>> {
        let reply = self
            .conn
            .get_property(false, window, atom, AtomEnum::ANY, 0, u32::MAX)?
            .reply();
        let Some(reply) = classify_property_string_reply(window, reply)? else {
            return Ok(None);
        };
        if reply.value.is_empty() {
            return Ok(None);
        }
        let value = reply
            .value
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        let value = String::from_utf8(value.to_vec())?.trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }
    pub fn outputs(&self) -> Result<Vec<OutputState>, Box<dyn Error>> {
        let resources = self
            .conn
            .randr_get_screen_resources_current(self.root)?
            .reply()?;
        let mut result = Vec::new();
        for output in resources.outputs {
            let info = self
                .conn
                .randr_get_output_info(output, resources.config_timestamp)?
                .reply()?;
            if info.connection != randr::Connection::CONNECTED || info.crtc == 0 {
                continue;
            }
            let crtc = self
                .conn
                .randr_get_crtc_info(info.crtc, resources.config_timestamp)?
                .reply()?;
            if crtc.width == 0 || crtc.height == 0 {
                continue;
            }
            let name = String::from_utf8_lossy(&info.name).into_owned();
            result.push(OutputState {
                id: OutputId(output),
                name,
                x: crtc.x,
                y: crtc.y,
                width: crtc.width,
                height: crtc.height,
            });
        }
        Ok(result)
    }

    fn create_bar_backing(
        &self,
        width: u16,
        height: u16,
    ) -> Result<Option<BarBacking>, Box<dyn Error>> {
        let pixmap = self.conn.generate_id()?;
        let create_result = self
            .conn
            .create_pixmap(self.glass_surface.depth, pixmap, self.root, width, height)?
            .check();
        if let Err(error) = create_result {
            if matches!(
                error,
                x11rb::errors::ReplyError::X11Error(ref error)
                    if error.error_kind == x11rb::protocol::ErrorKind::Alloc
            ) {
                return Ok(None);
            }
            return Err(error.into());
        }
        let gc = self.conn.generate_id()?;
        let gc_result = self
            .conn
            .create_gc(
                gc,
                pixmap,
                &xproto::CreateGCAux::new().foreground(
                    self.glass_surface
                        .background_pixel(BAR_STYLE.material.background),
                ),
            )?
            .check();
        if let Err(error) = gc_result {
            self.conn.free_pixmap(pixmap)?.check()?;
            if matches!(
                error,
                x11rb::errors::ReplyError::X11Error(ref error)
                    if error.error_kind == x11rb::protocol::ErrorKind::Alloc
            ) {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(BarBacking {
            pixmap,
            gc,
            width,
            height,
            depth: self.glass_surface.depth,
        }))
    }

    fn release_bar_backing(&mut self, backing: BarBacking) -> Result<(), Box<dyn Error>> {
        self.text.release_drawable(backing.pixmap);
        self.conn.free_gc(backing.gc)?.check()?;
        self.conn.free_pixmap(backing.pixmap)?.check()?;
        Ok(())
    }

    pub fn sync_windows(&mut self, outputs: &[OutputState]) -> Result<(), Box<dyn Error>> {
        self.close_popups(None)?;
        self.previous_contexts.clear();
        let old_windows = std::mem::take(&mut self.windows);
        for old in old_windows {
            self.text.release_drawable(old.window);
            if let Some(backing) = old.backing {
                self.release_bar_backing(backing)?;
            }
            trace_x11_resource("WINDOW_DESTROY", "bar", old.window);
            self.conn.destroy_window(old.window)?.check()?;
        }
        for output in outputs {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "bar", window);
            self.create_surface_window(
                self.glass_surface,
                SurfaceRole::Dock,
                window,
                SurfaceWindowGeometry {
                    x: output.x,
                    y: output.y,
                    width: output.width,
                    height: BAR_HEIGHT,
                    border_width: 0,
                },
                BAR_STYLE.material.background,
                xproto::CreateWindowAux::new().event_mask(
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::POINTER_MOTION
                        | EventMask::ENTER_WINDOW
                        | EventMask::LEAVE_WINDOW,
                ),
            )?;
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.window_type,
                    AtomEnum::ATOM,
                    &[self.atoms.dock],
                )?
                .check()?;
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.state,
                    AtomEnum::ATOM,
                    &[self.atoms.above],
                )?
                .check()?;
            if let Some(opacity) = self
                .glass_surface
                .window_opacity(BAR_STYLE.fallback_window_opacity)
            {
                self.conn
                    .change_property32(
                        xproto::PropMode::REPLACE,
                        window,
                        self.atoms.net_wm_window_opacity,
                        AtomEnum::CARDINAL,
                        &[style::opacity_cardinal(opacity)],
                    )?
                    .check()?;
            }
            let end = output
                .x
                .saturating_add(output.width as i16)
                .saturating_sub(1);
            let strut = [0, 0, BAR_HEIGHT as u32, 0];
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.strut,
                    AtomEnum::CARDINAL,
                    &strut,
                )?
                .check()?;
            let strut_partial = [
                0,
                0,
                BAR_HEIGHT as u32,
                0,
                0,
                0,
                output.x.max(0) as u32,
                end.max(0) as u32,
                0,
                0,
                0,
                0,
            ];
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.strut_partial,
                    AtomEnum::CARDINAL,
                    &strut_partial,
                )?
                .check()?;
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.wm_protocols,
                    AtomEnum::ATOM,
                    &[self.atoms.wm_delete],
                )?
                .check()?;
            self.conn.map_window(window)?.check()?;
            self.trace_your_event_mask(window, "after-create")?;
            self.windows.push(BarWindow {
                output: output.id,
                window,
                backing: None,
            });
        }
        self.conn.flush()?;
        Ok(())
    }
    pub fn render(&mut self, state: &State, target: RenderTarget) -> Result<(), Box<dyn Error>> {
        if target.includes_dock() {
            self.render_dock(state, target)?;
        }
        if target.contains(RenderTarget::POPUP) {
            // A hover transition only changes already-mapped popup content.
            // Do not clear the ARGB surface (which briefly reveals its backdrop)
            // and do not run structural popup reconciliation in that path.
            self.hover_repaint_active = std::mem::take(&mut self.popup_hover_changed);
            if !self.hover_repaint_active {
                self.reconcile_interactive_popup_surfaces(state)?;
            }
            if !state.audio_popup_open && !state.bluetooth_popup_open && !state.network_popup_open {
                // RenderTarget::Popup is shared by several popup domains. A
                // None plan means this frame has no Global Menu work; only an
                // explicit plan or topology reconciliation may select windows.
                self.render_popups(state)?;
            } else {
                // A local popup is exclusive with the Global Menu. Its menu
                // windows are destroyed in this frame, so the plan is
                // consumed by teardown rather than deferred to a later menu.
                let _ = std::mem::take(&mut self.menu_popup_dirty);
                self.destroy_popup_suffix(0)?;
            }
            if state.bluetooth_popup_open {
                if self.audio_popup.is_some() {
                    self.close_popups(Some(state))?;
                }
                self.render_bluetooth_popup(state)?;
            } else if state.network_popup_open {
                if self.audio_popup.is_some() || self.bluetooth_popup.is_some() {
                    self.close_popups(Some(state))?;
                }
                self.render_network_popup(state)?;
            } else {
                self.render_audio_popup(state)?;
            }
            self.hover_repaint_active = false;
        }
        if target.contains(RenderTarget::NOTIFICATION) {
            self.render_notification(state)?;
            self.render_notification_center(state)?;
        }
        self.conn.flush()?;
        self.text.flush();
        Ok(())
    }

    fn reconcile_interactive_popup_surfaces(
        &mut self,
        state: &State,
    ) -> Result<(), Box<dyn Error>> {
        let desired = if state.audio_popup_open {
            "Audio"
        } else if state.bluetooth_popup_open {
            "Bluetooth"
        } else if state.network_popup_open {
            "Network"
        } else if state.menu_interaction.open_root.is_some() {
            "Menu"
        } else {
            "None"
        };
        let wrong_surface = match desired {
            "Audio" => {
                self.bluetooth_popup.is_some()
                    || self.network_popup.is_some()
                    || !self.popups.is_empty()
            }
            "Bluetooth" => {
                self.audio_popup.is_some()
                    || self.network_popup.is_some()
                    || !self.popups.is_empty()
            }
            "Network" => {
                self.audio_popup.is_some()
                    || self.bluetooth_popup.is_some()
                    || !self.popups.is_empty()
            }
            "Menu" => {
                self.audio_popup.is_some()
                    || self.bluetooth_popup.is_some()
                    || self.network_popup.is_some()
            }
            "None" => {
                self.audio_popup.is_some()
                    || self.bluetooth_popup.is_some()
                    || self.network_popup.is_some()
                    || !self.popups.is_empty()
            }
            _ => false,
        };
        if wrong_surface {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: popup reconciliation desired={desired} action=close-stale");
            }
            self.close_popups(Some(state))?;
        }
        Ok(())
    }

    fn render_notification(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        let Some(notification) = state.notifications.last() else {
            if let Some(notification) = self.notification.take() {
                self.text.release_drawable(notification.window);
                if let Some(backing) = notification.backing {
                    self.text.release_drawable(backing.pixmap);
                    self.conn.free_gc(backing.gc)?.check()?;
                    self.conn.free_pixmap(backing.pixmap)?.check()?;
                }
                trace_x11_resource("WINDOW_DESTROY", "notification", notification.window);
                self.conn.destroy_window(notification.window)?.check()?;
            }
            return Ok(());
        };
        let output = state.outputs.first().ok_or("no output for notification")?;
        let width = 360_u16.min(output.width.max(1));
        let summary = single_line(&notification.summary);
        let body = single_line(&notification.body);
        let height = if body.is_empty() { 64 } else { 88 };
        let x =
            (output.x as i32 + output.width as i32 - width as i32 - 10).max(output.x as i32) as i16;
        let y = output.y + BAR_HEIGHT as i16 + 8;
        let window = if let Some(window) = &self.notification {
            window.window
        } else {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "notification", window);
            self.create_surface_window(
                self.default_surface,
                SurfaceRole::Notification,
                window,
                SurfaceWindowGeometry {
                    x,
                    y,
                    width,
                    height,
                    border_width: 1,
                },
                BAR_STYLE.material.background,
                xproto::CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::EXPOSURE),
            )?;
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.window_type,
                    AtomEnum::ATOM,
                    &[self.atoms.notification],
                )?
                .check()?;
            self.conn.map_window(window)?.check()?;
            window
        };
        if self
            .notification
            .as_ref()
            .is_some_and(|old| old.width != width || old.height != height)
        {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(x as i32)
                        .y(y as i32)
                        .width(width as u32)
                        .height(height as u32),
                )?
                .check()?;
        }
        let backing_replaced = !backing_matches(
            self.notification.as_ref().and_then(|n| n.backing),
            width,
            height,
            self.default_surface.depth,
        );
        let backing = if backing_replaced {
            let pixmap = self.conn.generate_id()?;
            self.conn
                .create_pixmap(self.default_surface.depth, pixmap, self.root, width, height)?
                .check()?;
            let gc = self.conn.generate_id()?;
            self.conn
                .create_gc(
                    gc,
                    pixmap,
                    &xproto::CreateGCAux::new().foreground(
                        self.default_surface
                            .background_pixel(BAR_STYLE.material.background),
                    ),
                )?
                .check()?;
            if let Some(old) = self.notification.as_mut().and_then(|n| n.backing.take()) {
                self.text.release_drawable(old.pixmap);
                self.conn.free_gc(old.gc)?.check()?;
                self.conn.free_pixmap(old.pixmap)?.check()?;
            }
            PopupBacking {
                pixmap,
                gc,
                width,
                height,
                depth: self.default_surface.depth,
            }
        } else {
            self.notification
                .as_ref()
                .and_then(|n| n.backing)
                .expect("notification backing")
        };
        self.conn.poly_fill_rectangle(
            backing.pixmap,
            backing.gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        self.conn.poly_rectangle(
            backing.pixmap,
            backing.gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        self.conn.flush()?;
        self.conn.get_input_focus()?.reply()?;
        self.text
            .prepare_drawable("notification", backing.pixmap, self.default_surface)?;
        self.text
            .draw_popup_utf8(&summary, 12, 25, BAR_STYLE.material.foreground)?;
        if !body.is_empty() {
            self.text
                .draw_popup_utf8(&body, 12, 52, BAR_STYLE.material.foreground)?;
        }
        self.text.release_drawable(backing.pixmap);
        self.conn
            .copy_area(
                backing.pixmap,
                window,
                backing.gc,
                0,
                0,
                0,
                0,
                width,
                height,
            )?
            .check()?;
        self.notification = Some(NotificationWindow {
            window,
            width,
            height,
            backing: Some(backing),
        });
        Ok(())
    }

    fn render_notification_center(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        let Some(output_id) = state.notification_center_open else {
            if let Some(center) = self.notification_center.take() {
                if let Some(backing) = center.backing {
                    self.text.release_drawable(backing.pixmap);
                    self.conn.free_gc(backing.gc)?.check()?;
                    self.conn.free_pixmap(backing.pixmap)?.check()?;
                }
                self.conn.destroy_window(center.window)?.check()?;
            }
            return Ok(());
        };
        let Some(output) = state.outputs.iter().find(|output| output.id == output_id) else {
            return Ok(());
        };
        let width = 360_u16.min(output.width.max(1));
        let available = output.height.saturating_sub(BAR_HEIGHT + 12);
        let visible_capacity = notification_visible_capacity(output.height);
        let previous = self.notification_center.as_ref();
        let same_output = previous.is_some_and(|center| center.output == output_id);
        let previous_scroll =
            notification_previous_scroll(same_output, previous.map_or(0, |center| center.scroll));
        let user_scrolled = same_output && previous.is_some_and(|center| center.scroll_changed);
        let previous_anchor = (!user_scrolled && previous_scroll > 0)
            .then(|| previous.and_then(|center| center.card_hits.first().map(|(id, _)| *id)));
        let scroll = reconcile_notification_scroll(
            previous_scroll,
            previous_anchor.flatten(),
            &state.notification_history,
            visible_capacity,
        );
        let cards = state
            .notification_history
            .iter()
            .skip(scroll)
            .take(visible_capacity)
            .count();
        if notification_scroll_trace_enabled() {
            let first_visible = state
                .notification_history
                .iter()
                .skip(scroll)
                .take(cards)
                .next()
                .map_or_else(|| "NONE".to_owned(), |entry| entry.id.0.to_string());
            eprintln!(
                "notification-center render: scroll={} history_len={} capacity={} first_visible_id={} visible_count={}",
                scroll,
                state.notification_history.len(),
                visible_capacity,
                first_visible,
                cards
            );
        }
        let height = if state.notification_history.is_empty() {
            62
        } else {
            (visible_capacity as u16)
                .saturating_mul(62)
                .min(available.max(62))
        };
        let x =
            (output.x as i32 + output.width as i32 - width as i32 - 8).max(output.x as i32) as i16;
        let y = output.y.saturating_add(BAR_HEIGHT as i16 + 4);
        let existing = self
            .notification_center
            .as_ref()
            .map(|center| center.window);
        let window = if let Some(window) = existing {
            window
        } else {
            let window = self.conn.generate_id()?;
            self.create_surface_window(
                self.glass_surface,
                SurfaceRole::Notification,
                window,
                SurfaceWindowGeometry {
                    x,
                    y,
                    width,
                    height,
                    border_width: 1,
                },
                crate::ui::style::Rgba {
                    red: 0x20,
                    green: 0x24,
                    blue: 0x2b,
                    alpha: 0xb8,
                },
                xproto::CreateWindowAux::new()
                    .override_redirect(1)
                    .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
            )?;
            self.conn.map_window(window)?.check()?;
            let (grab_button4, grab_button5) = self.install_notification_center_wheel_grabs(window);
            if notification_scroll_trace_enabled() {
                eprintln!(
                    "notification-center create: xid={} output={} geometry={}x{}+{}+{} event_mask=EXPOSURE|BUTTON_PRESS grab_button4={} grab_button5={}",
                    window, output_id.0, width, height, x, y, grab_button4, grab_button5
                );
            }
            window
        };
        if self.notification_center.as_ref().is_some_and(|center| {
            center.width != width || center.height != height || center.output != output_id
        }) {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(x as i32)
                        .y(y as i32)
                        .width(width as u32)
                        .height(height as u32),
                )?
                .check()?;
        }
        let backing_replaced = !backing_matches(
            self.notification_center
                .as_ref()
                .and_then(|center| center.backing),
            width,
            height,
            self.glass_surface.depth,
        );
        let backing = if backing_replaced {
            let pixmap = self.conn.generate_id()?;
            self.conn
                .create_pixmap(self.glass_surface.depth, pixmap, self.root, width, height)?
                .check()?;
            let gc = self.conn.generate_id()?;
            self.conn
                .create_gc(
                    gc,
                    pixmap,
                    &xproto::CreateGCAux::new().foreground(self.glass_surface.background_pixel(
                        crate::ui::style::Rgba {
                            red: 0x20,
                            green: 0x24,
                            blue: 0x2b,
                            alpha: 0xb8,
                        },
                    )),
                )?
                .check()?;
            if let Some(old) = self
                .notification_center
                .as_mut()
                .and_then(|center| center.backing.take())
            {
                self.text.release_drawable(old.pixmap);
                self.conn.free_gc(old.gc)?.check()?;
                self.conn.free_pixmap(old.pixmap)?.check()?;
            }
            PopupBacking {
                pixmap,
                gc,
                width,
                height,
                depth: self.glass_surface.depth,
            }
        } else {
            self.notification_center
                .as_ref()
                .and_then(|center| center.backing)
                .expect("center backing")
        };
        self.conn
            .change_gc(
                backing.gc,
                &xproto::ChangeGCAux::new().foreground(self.glass_surface.background_pixel(
                    crate::ui::style::Rgba {
                        red: 0x20,
                        green: 0x24,
                        blue: 0x2b,
                        alpha: 0xb8,
                    },
                )),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            backing.pixmap,
            backing.gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        let mut card_hits = Vec::with_capacity(cards);
        for (index, entry) in state
            .notification_history
            .iter()
            .skip(scroll)
            .take(cards)
            .enumerate()
        {
            let top = (index as u16).saturating_mul(62);
            let card_rect = layout::MenuRect {
                x: 4,
                y: (top + 4) as i16,
                width: width.saturating_sub(8),
                height: 54,
            };
            card_hits.push((entry.id, card_rect));
            let hovered = self
                .notification_center
                .as_ref()
                .and_then(|center| center.hover)
                == Some(entry.id);
            self.conn
                .change_gc(
                    backing.gc,
                    &xproto::ChangeGCAux::new().foreground(
                        self.glass_surface
                            .opaque_pixel(if hovered { 0x354052 } else { 0x2b3340 }),
                    ),
                )?
                .check()?;
            self.conn.poly_fill_rectangle(
                backing.pixmap,
                backing.gc,
                &[xproto::Rectangle {
                    x: card_rect.x,
                    y: card_rect.y,
                    width: card_rect.width,
                    height: card_rect.height,
                }],
            )?;
            self.conn.flush()?;
            self.conn.get_input_focus()?.reply()?;
            self.text.prepare_drawable(
                "notification-center",
                backing.pixmap,
                self.glass_surface,
            )?;
            let title = if entry.app_name.is_empty() {
                "Notification"
            } else {
                &entry.app_name
            };
            self.text.draw_popup_utf8(
                title,
                12,
                i32::from(top) + 20,
                BAR_STYLE.material.foreground,
            )?;
            self.text.draw_popup_utf8(
                &single_line(&entry.summary),
                12,
                i32::from(top) + 38,
                BAR_STYLE.material.foreground,
            )?;
            self.text.draw_popup_utf8(
                &single_line(&entry.body),
                12,
                i32::from(top) + 54,
                BAR_STYLE.material.foreground,
            )?;
            self.text.release_drawable(backing.pixmap);
        }
        if state.notification_history.is_empty() {
            self.conn.flush()?;
            self.conn.get_input_focus()?.reply()?;
            self.text.prepare_drawable(
                "notification-center",
                backing.pixmap,
                self.glass_surface,
            )?;
            self.text
                .draw_popup_utf8("No notifications", 12, 34, BAR_STYLE.material.foreground)?;
            self.text.release_drawable(backing.pixmap);
        }
        self.conn
            .copy_area(
                backing.pixmap,
                window,
                backing.gc,
                0,
                0,
                0,
                0,
                width,
                height,
            )?
            .check()?;
        let hover = self
            .notification_center
            .as_ref()
            .and_then(|center| center.hover)
            .filter(|id| card_hits.iter().any(|(candidate, _)| candidate == id));
        self.notification_center = Some(NotificationCenterWindow {
            window,
            output: output_id,
            width,
            height,
            backing: Some(backing),
            card_hits,
            hover,
            scroll,
            scroll_changed: false,
            dirty: false,
        });
        Ok(())
    }

    fn render_dock(&mut self, state: &State, target: RenderTarget) -> Result<(), Box<dyn Error>> {
        self.bar_hits.clear();
        self.notification_hits.clear();
        let requested_full = target.is_full_dock();
        let draw_workspaces = requested_full || target.contains(RenderTarget::WORKSPACES);
        let draw_context = requested_full || target.contains(RenderTarget::CONTEXT);
        let draw_plugins = requested_full || target.contains(RenderTarget::PLUGIN_ZONE);
        let draw_tray = requested_full || target.contains(RenderTarget::TRAY);
        let draw_network = requested_full || target.contains(RenderTarget::NETWORK);
        let draw_bluetooth = requested_full || target.contains(RenderTarget::BLUETOOTH);
        let draw_audio = requested_full || target.contains(RenderTarget::AUDIO);
        let draw_datetime = requested_full || target.contains(RenderTarget::DATETIME);
        let trace = std::env::var_os("XBAR_TRACE").is_some();
        if trace {
            for (draw, name) in [
                (draw_workspaces, "WORKSPACES"),
                (draw_context, "CONTEXT"),
                (draw_plugins, "PLUGIN_ZONE"),
                (draw_tray, "TRAY"),
                (draw_network, "NETWORK"),
                (draw_bluetooth, "BLUETOOTH"),
                (draw_audio, "AUDIO"),
                (draw_datetime, "DATETIME"),
            ] {
                if draw {
                    eprintln!("xbar trace: DRAW region={name}");
                }
            }
        }
        for bar_index in 0..self.windows.len() {
            let (bar_output, bar_window, backing) = {
                let bar = &self.windows[bar_index];
                (bar.output, bar.window, bar.backing)
            };
            let Some(output) = state.outputs.iter().find(|output| output.id == bar_output) else {
                continue;
            };
            let backing_replaced =
                !bar_backing_matches(backing, output.width, BAR_HEIGHT, self.glass_surface.depth);
            if backing_replaced {
                let Some(new_backing) = self.create_bar_backing(output.width, BAR_HEIGHT)? else {
                    continue;
                };
                if let Some(old_backing) = self.windows[bar_index].backing.replace(new_backing) {
                    self.release_bar_backing(old_backing)?;
                }
            }
            let backing = self.windows[bar_index]
                .backing
                .expect("bar backing created");
            let full = requested_full || backing_replaced;
            let draw_workspaces = full || draw_workspaces;
            let draw_context = full || draw_context;
            let draw_plugins = full || draw_plugins;
            let draw_tray = full || draw_tray;
            let draw_network = full || draw_network;
            let draw_bluetooth = full || draw_bluetooth;
            let draw_audio = full || draw_audio;
            let draw_datetime = full || draw_datetime;
            let gc = backing.gc;
            self.conn
                .change_gc(
                    gc,
                    &xproto::ChangeGCAux::new().foreground(
                        self.glass_surface
                            .background_pixel(BAR_STYLE.material.background),
                    ),
                )?
                .check()?;
            let previous_context = self.previous_contexts.get(&bar_window).cloned();
            let workspaces: Vec<_> = state
                .workspaces
                .iter()
                .filter(|w| {
                    w.focused
                        && w.output
                            .as_deref()
                            .map(|n| {
                                state
                                    .outputs
                                    .iter()
                                    .any(|o| o.id == bar_output && o.name == n)
                            })
                            .unwrap_or(true)
                })
                .collect();
            let workspace_values: Vec<_> = workspaces.into_iter().cloned().collect();
            let active_output = state
                .focused_workspace
                .as_ref()
                .and_then(|name| {
                    state
                        .workspaces
                        .iter()
                        .find(|workspace| &workspace.name == name)
                })
                .and_then(|workspace| workspace.output.as_ref())
                .is_some_and(|name| {
                    state
                        .outputs
                        .iter()
                        .any(|candidate| candidate.id == output.id && candidate.name == *name)
                });
            let context = view::context_view_with_app_name_and_audio_and_bluetooth_and_plugins(
                output,
                &workspace_values,
                if active_output {
                    match state.menu {
                        crate::core::MenuState::TrayLoading { .. }
                        | crate::core::MenuState::TrayLoaded { .. }
                        | crate::core::MenuState::TrayError { .. } => {
                            state.global_menu_model.as_ref().map(|(_, _, model)| model)
                        }
                        _ => state.active_menu_model(),
                    }
                } else {
                    None
                },
                state.clock.as_ref(),
                state.status_notifier_items.items(),
                state.focused_app_name.as_deref(),
                Some(&state.audio),
                Some(&state.network),
                Some(&state.bluetooth),
                &state.plugin_zone.plugins,
                &self.text,
            );
            let old_context = previous_context.as_ref().unwrap_or(&context);
            let draw_context = draw_context
                || old_context.menu != context.menu
                || old_context.app_name != context.app_name;
            let draw_plugins = draw_plugins || old_context.plugins != context.plugins;
            let draw_tray = draw_tray || old_context.tray != context.tray;
            let draw_network = draw_network || old_context.network != context.network;
            let draw_bluetooth = draw_bluetooth || old_context.bluetooth != context.bluetooth;
            let draw_audio = draw_audio || old_context.audio != context.audio;
            let draw_datetime = draw_datetime || old_context.datetime != context.datetime;
            let draw_notification =
                full || draw_context || old_context.notification != context.notification;
            self.bar_hits.push((
                bar_window,
                bar_output,
                output.x,
                output.y,
                context.menu.clone(),
                context.tray.clone(),
                context.network.clone(),
                context.audio.clone(),
                context.bluetooth.clone(),
            ));
            self.notification_hits
                .push((bar_window, bar_output, context.notification.rect));
            let present_region = if full {
                self.conn.poly_fill_rectangle(
                    backing.pixmap,
                    gc,
                    &[xproto::Rectangle {
                        x: 0,
                        y: 0,
                        width: output.width,
                        height: BAR_HEIGHT,
                    }],
                )?;
                layout::MenuRect {
                    x: output.x,
                    y: output.y,
                    width: output.width,
                    height: BAR_HEIGHT,
                }
            } else {
                let old = old_context;
                let mut clear: Vec<layout::MenuRect> = Vec::new();
                if draw_workspaces {
                    clear.extend(old.workspaces.iter().map(|rect| workspace_as_menu(*rect)));
                    clear.extend(
                        context
                            .workspaces
                            .iter()
                            .map(|rect| workspace_as_menu(*rect)),
                    );
                }
                if draw_context {
                    clear.push(context_bounds(old, output));
                    clear.push(context_bounds(&context, output));
                }
                if draw_plugins {
                    clear.extend(old.plugins.iter().map(|item| item.rect));
                    clear.extend(context.plugins.iter().map(|item| item.rect));
                }
                if draw_tray {
                    clear.extend(old.tray.iter().map(|item| item.rect));
                    clear.extend(context.tray.iter().map(|item| item.rect));
                }
                if draw_network {
                    if let Some(item) = &old.network {
                        clear.push(item.rect);
                    }
                    if let Some(item) = &context.network {
                        clear.push(item.rect);
                    }
                }
                if draw_bluetooth {
                    if let Some(item) = &old.bluetooth {
                        clear.push(item.rect);
                    }
                    if let Some(item) = &context.bluetooth {
                        clear.push(item.rect);
                    }
                }
                if draw_audio {
                    if let Some(item) = &old.audio {
                        clear.push(item.rect);
                    }
                    if let Some(item) = &context.audio {
                        clear.push(item.rect);
                    }
                }
                if draw_datetime {
                    if let Some(item) = &old.datetime {
                        clear.push(item.rect);
                    }
                    if let Some(item) = &context.datetime {
                        clear.push(item.rect);
                    }
                }
                let Some(present_region) = union_menu_rects(&clear) else {
                    self.previous_contexts.insert(bar_window, context);
                    continue;
                };
                for rect in &clear {
                    self.conn.poly_fill_rectangle(
                        backing.pixmap,
                        gc,
                        &[x11_rect(*rect, output)],
                    )?;
                }
                present_region
            };
            let mut text = Vec::new();
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: PLUGINZONE_VIEW items={}",
                    context.plugins.len()
                );
                eprintln!(
                    "xbar trace: PLUGINZONE_LAYOUT items={} rects={:?}",
                    context.plugins.len(),
                    context
                        .plugins
                        .iter()
                        .map(|plugin| (plugin.rect.x, plugin.rect.width))
                        .collect::<Vec<_>>()
                );
                eprintln!(
                    "xbar trace: context output={} workspaces={:?} menu={:?} bluetooth={:?} audio={:?} tray={:?} datetime={:?}",
                    output.name, context.workspaces, context.menu, context.bluetooth, context.audio, context.tray, context.datetime
                );
            }
            let rects = &context.workspaces;
            if draw_context && std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: CONTEXT_DRAW");
            }
            for (workspace, rect) in workspace_values.iter().zip(rects) {
                if !draw_workspaces {
                    break;
                }
                let x = rect.x.saturating_sub(output.x).saturating_add(4);
                let width = rect.width.saturating_sub(8).max(1);
                let color = if workspace.focused {
                    self.glass_surface
                        .opaque_pixel(BAR_STYLE.workspace_background)
                } else {
                    self.glass_surface
                        .background_pixel(BAR_STYLE.material.background)
                };
                self.conn
                    .change_gc(gc, &xproto::ChangeGCAux::new().foreground(color))?
                    .check()?;
                self.conn.poly_fill_rectangle(
                    backing.pixmap,
                    gc,
                    &[xproto::Rectangle {
                        x,
                        y: 4,
                        width,
                        height: 18,
                    }],
                )?;
                self.conn
                    .change_gc(
                        gc,
                        &xproto::ChangeGCAux::new().foreground(
                            self.glass_surface
                                .opaque_pixel(BAR_STYLE.workspace_foreground),
                        ),
                    )?
                    .check()?;
                text.push(BarText {
                    kind: BarTextKind::Bar,
                    text: layout::truncate_text_to_width(
                        &workspace.name,
                        rect.width.saturating_sub(12),
                        &self.text,
                    )
                    .unwrap_or_default(),
                    x: x as i32 + BAR_STYLE.horizontal_padding as i32,
                    y: self.text.baseline(BAR_HEIGHT) as i32,
                    color: BAR_STYLE.workspace_foreground,
                });
            }
            for item in &context.menu {
                if !draw_context {
                    break;
                }
                let x = item.rect.x.saturating_sub(output.x).saturating_add(8);
                self.conn
                    .change_gc(
                        gc,
                        &xproto::ChangeGCAux::new().foreground(self.glass_surface.opaque_pixel(
                            if state.menu_interaction.hovered_path.last() == Some(&item.id) {
                                BAR_STYLE.menu_hover_foreground
                            } else if item.enabled {
                                BAR_STYLE.material.foreground
                            } else {
                                BAR_STYLE.menu_disabled_foreground
                            },
                        )),
                    )?
                    .check()?;
                let color = if state.menu_interaction.hovered_path.last() == Some(&item.id) {
                    BAR_STYLE.menu_hover_foreground
                } else if item.enabled {
                    BAR_STYLE.material.foreground
                } else {
                    BAR_STYLE.menu_disabled_foreground
                };
                text.push(BarText {
                    kind: BarTextKind::Bar,
                    text: layout::truncate_text_to_width(
                        &item.label,
                        item.rect.width.saturating_sub(16),
                        &self.text,
                    )
                    .unwrap_or_default(),
                    x: x as i32,
                    y: self.text.baseline(BAR_HEIGHT) as i32,
                    color,
                });
            }
            if draw_context {
                if let Some(title) = &context.app_name {
                    let x = title.rect.x.saturating_sub(output.x) as i32;
                    text.push(BarText {
                        kind: BarTextKind::Bar,
                        text: layout::truncate_text_to_width(
                            &title.text,
                            title.rect.width,
                            &self.text,
                        )
                        .unwrap_or_default(),
                        x,
                        y: self.text.baseline(BAR_HEIGHT) as i32,
                        color: BAR_STYLE.material.foreground,
                    });
                }
            }
            if draw_network || draw_audio || draw_bluetooth {
                if draw_network {
                    if let Some(network) = &context.network {
                        let width = self.text.measure_status_icon_width(&network.text);
                        let x = network.rect.x.saturating_sub(output.x) as i32
                            + (network.rect.width.saturating_sub(width) / 2) as i32;
                        let baseline = self.text.status_icon_baseline(BAR_HEIGHT) as i32;
                        text.push(BarText {
                            kind: BarTextKind::StatusIcon,
                            text: network.text.clone(),
                            x,
                            y: baseline,
                            color: BAR_STYLE.material.foreground,
                        });
                    }
                }
                if draw_audio {
                    if let Some(audio) = &context.audio {
                        let width = self.text.measure_status_icon_width(&audio.text);
                        let x = audio.rect.x.saturating_sub(output.x) as i32
                            + (audio.rect.width.saturating_sub(width) / 2) as i32;
                        let baseline = self.text.status_icon_baseline(BAR_HEIGHT) as i32;
                        text.push(BarText {
                            kind: BarTextKind::StatusIcon,
                            text: audio.text.clone(),
                            x,
                            y: baseline,
                            color: BAR_STYLE.material.foreground,
                        });
                    }
                }
                if draw_bluetooth {
                    if let Some(bluetooth) = &context.bluetooth {
                        let width = self.text.measure_status_icon_width(&bluetooth.text);
                        let x = bluetooth.rect.x.saturating_sub(output.x) as i32
                            + (bluetooth.rect.width.saturating_sub(width) / 2) as i32;
                        let baseline = self.text.status_icon_baseline(BAR_HEIGHT) as i32;
                        text.push(BarText {
                            kind: BarTextKind::StatusIcon,
                            text: bluetooth.text.clone(),
                            x,
                            y: baseline,
                            color: BAR_STYLE.material.foreground,
                        });
                    }
                }
            }
            if draw_notification {
                let indicator = &context.notification;
                let width = self.text.measure_status_icon_width(&indicator.text);
                let x = indicator.rect.x.saturating_sub(output.x) as i32
                    + (indicator.rect.width.saturating_sub(width) / 2) as i32;
                text.push(BarText {
                    kind: BarTextKind::StatusIcon,
                    text: indicator.text.clone(),
                    x,
                    y: self.text.status_icon_baseline(BAR_HEIGHT) as i32,
                    color: BAR_STYLE.material.foreground,
                });
            }
            for tray in &context.tray {
                if !draw_tray {
                    continue;
                }
                let crate::core::StatusNotifierIcon::Pixmap {
                    width,
                    height,
                    argb,
                } = &tray.icon;
                let (draw_width, draw_height) = tray_draw_size(*width, *height);
                let x0 = tray.rect.x.saturating_sub(output.x)
                    + ((tray.rect.width - draw_width) / 2) as i16;
                let y0 = tray.rect.y.saturating_sub(output.y)
                    + ((tray.rect.height - draw_height) / 2) as i16;
                for py in 0..draw_height {
                    for px in 0..draw_width {
                        let source_x = px * *width / draw_width.max(1);
                        let source_y = py * *height / draw_height.max(1);
                        let index = (source_y * *width + source_x) as usize;
                        let pixel = argb[index];
                        let rendered_pixel = match tray.render_mode {
                            view::TrayIconRenderMode::Template => {
                                let Some(template_pixel) = template_icon_pixel(
                                    pixel,
                                    BAR_STYLE.material.foreground,
                                    BAR_STYLE.material.background.rgb(),
                                ) else {
                                    continue;
                                };
                                template_pixel
                            }
                            view::TrayIconRenderMode::PreserveColor => {
                                let Some(color_pixel) = preserve_color_pixel(pixel) else {
                                    continue;
                                };
                                color_pixel
                            }
                        };
                        self.conn
                            .change_gc(
                                gc,
                                &xproto::ChangeGCAux::new()
                                    .foreground(self.glass_surface.opaque_pixel(rendered_pixel)),
                            )?
                            .check()?;
                        self.conn.poly_fill_rectangle(
                            backing.pixmap,
                            gc,
                            &[xproto::Rectangle {
                                x: x0 + px as i16,
                                y: y0 + py as i16,
                                width: 1,
                                height: 1,
                            }],
                        )?;
                    }
                }
            }
            for plugin in &context.plugins {
                if !draw_plugins {
                    continue;
                }
                let x = plugin.rect.x.saturating_sub(output.x) as i32 + 6;
                text.push(BarText {
                    kind: BarTextKind::Bar,
                    text: layout::truncate_text_to_width(
                        &plugin.text,
                        plugin.rect.width.saturating_sub(12),
                        &self.text,
                    )
                    .unwrap_or_default(),
                    x,
                    y: self.text.baseline(BAR_HEIGHT) as i32,
                    color: BAR_STYLE.material.foreground,
                });
            }
            if trace && draw_plugins {
                eprintln!(
                    "xbar trace: PLUGINZONE_DRAW items={}",
                    context.plugins.len()
                );
            }
            if trace {
                for (draw, name) in [
                    (draw_tray, "TRAY"),
                    (draw_network, "NETWORK"),
                    (draw_bluetooth, "BLUETOOTH"),
                    (draw_audio, "AUDIO"),
                    (draw_datetime, "DATETIME"),
                ] {
                    if draw {
                        eprintln!("xbar trace: RIGHT_STATUS_DRAW region={name}");
                    }
                }
            }
            if let Some(datetime) = &context.datetime {
                if draw_datetime {
                    let x = datetime.rect.x.saturating_sub(output.x).saturating_add(8);
                    text.push(BarText {
                        kind: BarTextKind::Bar,
                        text: layout::truncate_text_to_width(
                            &datetime.text,
                            datetime.rect.width.saturating_sub(8),
                            &self.text,
                        )
                        .unwrap_or_default(),
                        x: x as i32,
                        y: self.text.baseline(BAR_HEIGHT) as i32,
                        color: BAR_STYLE.material.foreground,
                    });
                }
            }
            self.conn.flush()?;
            self.conn.get_input_focus()?.reply()?;
            self.text
                .prepare_drawable("bar", backing.pixmap, self.glass_surface)?;
            for text in text {
                match text.kind {
                    BarTextKind::Bar => {
                        self.text
                            .draw_utf8(&text.text, text.x, text.y, text.color)?;
                    }
                    BarTextKind::StatusIcon => {
                        self.text
                            .draw_status_icon_utf8(&text.text, text.x, text.y, text.color)?;
                    }
                }
            }
            self.text.release_drawable(backing.pixmap);
            let present = x11_rect(present_region, output);
            self.conn
                .copy_area(
                    backing.pixmap,
                    bar_window,
                    gc,
                    present.x,
                    present.y,
                    present.x,
                    present.y,
                    present.width,
                    present.height,
                )?
                .check()?;
            self.previous_contexts.insert(bar_window, context);
        }
        Ok(())
    }

    fn close_popups(&mut self, state: Option<&State>) -> Result<(), Box<dyn Error>> {
        self.destroy_popup_suffix(0)?;
        if let Some(popup) = self.audio_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "audio-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
            if let Some(backing) = self.audio_backing.take() {
                self.text.release_drawable(backing.pixmap);
                self.conn.free_gc(backing.gc)?.check()?;
                self.conn.free_pixmap(backing.pixmap)?.check()?;
            }
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: audio popup destroyed xid={}", popup.window);
            }
        }
        if let Some(popup) = self.bluetooth_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "bluetooth-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
            if let Some(backing) = self.bluetooth_backing.take() {
                self.text.release_drawable(backing.pixmap);
                self.conn.free_gc(backing.gc)?.check()?;
                self.conn.free_pixmap(backing.pixmap)?.check()?;
            }
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: UNMAP popup=Bluetooth xid={}", popup.window);
            }
        }
        if let Some(popup) = self.network_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "network-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
            if let Some(backing) = self.network_backing.take() {
                self.text.release_drawable(backing.pixmap);
                self.conn.free_gc(backing.gc)?.check()?;
                self.conn.free_pixmap(backing.pixmap)?.check()?;
            }
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: UNMAP popup=Network xid={}", popup.window);
            }
        }
        if self.pointer_grabbed {
            if std::env::var_os("XBAR_TRACE").is_some() {
                if let Some(state) = state {
                    eprintln!(
                        "xbar trace: pointer grab release reason=close_popups open_root={:?} open_path={:?} popup_count={} focused_window={:?} focused_workspace={:?}",
                        state.menu_interaction.open_root,
                        state.menu_interaction.open_path,
                        self.popups.len(),
                        state.focused_window,
                        state.focused_workspace
                    );
                } else {
                    eprintln!(
                        "xbar trace: pointer grab release reason=close_popups open_root=unknown open_path=unknown popup_count={} focused_window=unknown focused_workspace=unknown",
                        self.popups.len()
                    );
                }
            }
            self.conn.ungrab_pointer(0_u32)?.check()?;
            self.pointer_grabbed = false;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: pointer grab released pointer_grabbed=false");
            }
        }
        Ok(())
    }

    fn render_audio_popup(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        if !state.audio_popup_open || !state.audio.available {
            if self.audio_popup.is_some() {
                self.close_popups(Some(state))?;
            }
            return Ok(());
        }
        let output = state.outputs.first().ok_or("no output for audio popup")?;
        let output_count = state.audio.outputs.len().min(8);
        let input_count = state.audio.inputs.len().min(8);
        let content_height = 316 + output_count as u16 * 24 + input_count as u16 * 24;
        let popup_width = 340_u16.min(output.width.max(1));
        let popup_height = content_height
            .max(280)
            .min(output.height.saturating_sub(26).max(1));
        let rect = layout::MenuRect {
            x: (output.x as i32 + output.width as i32 - popup_width as i32) as i16,
            y: output.y + 26,
            width: popup_width,
            height: popup_height,
        };
        let card_x = rect.x + POPUP_STYLE.outer_padding as i16;
        let card_width = rect.width.saturating_sub(POPUP_STYLE.outer_padding * 2);
        let master_card = layout::MenuRect {
            x: card_x,
            y: rect.y + POPUP_STYLE.outer_padding as i16,
            width: card_width,
            height: 92,
        };
        let input_control_card = layout::MenuRect {
            x: card_x,
            y: rect.y + 112,
            width: card_width,
            height: 104,
        };
        let output_card = layout::MenuRect {
            x: card_x,
            y: rect.y + 220,
            width: card_width,
            height: 29 + output_count as u16 * 24,
        };
        let input_card = layout::MenuRect {
            x: card_x,
            y: output_card.y + output_card.height as i16 + POPUP_STYLE.card_row_gap as i16,
            width: card_width,
            height: 29 + input_count as u16 * 24,
        };
        let master_content = layout::popup_card_content_rect(master_card);
        let input_content = layout::popup_card_content_rect(input_control_card);
        let audio_content_x = master_content.x - rect.x - layout::AUDIO_POPUP_BORDER as i16;
        let track = layout::MenuRect {
            x: master_content.x + 38,
            y: master_content.y + 34,
            width: 240,
            height: 22,
        };
        let mute = layout::MenuRect {
            x: master_content.x,
            y: master_content.y,
            width: 46,
            height: 48,
        };
        let input_track = layout::MenuRect {
            x: input_content.x + 38,
            y: input_content.y + 34,
            width: 240,
            height: 22,
        };
        let input_mute = layout::MenuRect {
            x: input_content.x,
            y: input_content.y,
            width: 46,
            height: 48,
        };
        let output_label_y = (output_card.y - rect.y - layout::AUDIO_POPUP_BORDER as i16
            + POPUP_STYLE.card_padding as i16
            + 14) as i32;
        let input_label_y = output_label_y + 22 + output_count as i32 * 24;
        let output_devices = layout::audio_device_rows(
            rect,
            &state.audio.outputs,
            (output_label_y + 22) as i16,
            &PopupMeasurer(&self.text),
        );
        let input_devices = layout::audio_device_rows(
            rect,
            &state.audio.inputs,
            (input_label_y + 22) as i16,
            &PopupMeasurer(&self.text),
        );
        let effect_owner = popup_effect_owner(&self.windows, output.id)
            .ok_or("no dock window for audio popup effect owner")?;
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: audio device layout outputs={output_devices:?} inputs={input_devices:?}");
        }
        let window = if let Some(popup) = &self.audio_popup {
            popup.window
        } else {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "audio-popup", window);
            self.create_glass_popup_window(
                SurfaceRole::AudioPopup,
                window,
                rect,
                layout::AUDIO_POPUP_BORDER,
                EventMask::EXPOSURE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION,
            )?;
            self.configure_auxiliary_effect_surface(SurfaceRole::AudioPopup, window, effect_owner)?;
            self.conn.map_window(window)?.check()?;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: audio popup created xid={} geometry=x{} y{} w{} h{}",
                    window, rect.x, rect.y, rect.width, rect.height
                );
            }
            window
        };
        let needs_resize = self.audio_popup.as_ref().is_some_and(|popup| {
            popup.rect.width != rect.width || popup.rect.height != rect.height
        });
        if let Some(popup) = &mut self.audio_popup {
            popup.rect = rect;
            popup.track = track;
            popup.mute = mute;
            popup.input_track = input_track;
            popup.input_mute = input_mute;
            popup.output_devices = output_devices.clone();
            popup.input_devices = input_devices.clone();
        } else {
            self.audio_popup = Some(AudioPopupWindow {
                window,
                rect,
                track,
                mute,
                input_track,
                input_mute,
                output_devices: output_devices.clone(),
                input_devices: input_devices.clone(),
            });
        }
        let backing_replaced = !backing_matches(
            self.audio_backing,
            rect.width,
            rect.height,
            self.glass_surface.depth,
        );
        if backing_replaced {
            let pixmap = self.conn.generate_id()?;
            let create_result = self
                .conn
                .create_pixmap(
                    self.glass_surface.depth,
                    pixmap,
                    self.root,
                    rect.width,
                    rect.height,
                )?
                .check();
            if let Err(error) = create_result {
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            let gc = self.conn.generate_id()?;
            let gc_result = self
                .conn
                .create_gc(
                    gc,
                    pixmap,
                    &xproto::CreateGCAux::new().foreground(
                        self.glass_surface
                            .background_pixel(POPUP_STYLE.material.background),
                    ),
                )?
                .check();
            if let Err(error) = gc_result {
                self.conn.free_pixmap(pixmap)?.check()?;
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            if let Some(old) = self.audio_backing.replace(PopupBacking {
                pixmap,
                gc,
                width: rect.width,
                height: rect.height,
                depth: self.glass_surface.depth,
            }) {
                self.conn.free_gc(old.gc)?.check()?;
                self.conn.free_pixmap(old.pixmap)?.check()?;
            }
        }
        let backing = self.audio_backing.expect("audio backing created");
        if self.audio_popup.is_some() && needs_resize {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .width(rect.width as u32)
                        .height(rect.height as u32),
                )?
                .check()?;
            self.apply_surface_effect(
                self.glass_surface,
                SurfaceRole::AudioPopup,
                window,
                SurfaceWindowGeometry {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    border_width: layout::AUDIO_POPUP_BORDER,
                },
            )?;
        }
        self.text
            .prepare_drawable("audio-popup", backing.pixmap, self.glass_surface)?;
        let gc = backing.gc;
        self.fill_glass_background(backing.pixmap, gc, rect.width, rect.height)?;
        self.draw_popup_frame(backing.pixmap, gc, rect.width, rect.height)?;
        for card in [master_card, input_control_card, output_card, input_card] {
            self.draw_popup_card(backing.pixmap, gc, rect, card)?;
        }
        for device in &output_devices {
            if matches!(
                self.popup_hover,
                Some(PopupHover::AudioOutputDevice(ref name)) if name == &device.name
            ) {
                self.draw_popup_hover(backing.pixmap, gc, rect, device.rect)?;
            }
        }
        for device in &input_devices {
            if matches!(
                self.popup_hover,
                Some(PopupHover::AudioInputDevice(ref name)) if name == &device.name
            ) {
                self.draw_popup_hover(backing.pixmap, gc, rect, device.rect)?;
            }
        }
        self.conn.flush()?;
        self.conn.get_input_focus()?.reply()?;
        self.text.draw_popup_utf8(
            "Som",
            audio_content_x as i32,
            (master_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 14) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            &format!(
                "{}   {}%",
                view::audio_glyph(&state.audio),
                state.audio.volume_percent
            ),
            audio_content_x as i32,
            (master_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 40) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            "Saída",
            audio_content_x as i32,
            output_label_y,
            BAR_STYLE.material.foreground,
        )?;
        for device in &output_devices {
            let marker = if state.audio.default_output.as_deref() == Some(device.name.as_str()) {
                "✓"
            } else {
                " "
            };
            let (x, y) = device.label_position(rect);
            self.text.draw_popup_utf8(
                &format!(
                    "{marker} {}",
                    layout::truncate_text_to_width(
                        &device.display_name,
                        device.rect.width.saturating_sub(8),
                        &PopupMeasurer(&self.text),
                    )
                    .unwrap_or_default()
                ),
                x,
                y,
                BAR_STYLE.material.foreground,
            )?;
        }
        self.text.draw_popup_utf8(
            "Entrada",
            audio_content_x as i32,
            input_label_y,
            BAR_STYLE.material.foreground,
        )?;
        for device in &input_devices {
            let marker = if state.audio.default_input.as_deref() == Some(device.name.as_str()) {
                "✓"
            } else {
                " "
            };
            let (x, y) = device.label_position(rect);
            self.text.draw_popup_utf8(
                &format!("{marker} {}", device.display_name),
                x,
                y,
                BAR_STYLE.material.foreground,
            )?;
        }
        self.draw_audio_slider(backing.pixmap, gc, track, state.audio.volume_percent)?;
        self.text.draw_popup_utf8(
            "Mudo",
            audio_content_x as i32,
            (master_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 82) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            "Microfone",
            audio_content_x as i32,
            (input_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 14) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            &format!(
                "{}   {}%",
                view::microphone_glyph(&state.audio),
                state.audio.input_volume_percent
            ),
            audio_content_x as i32,
            (input_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 40) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.draw_audio_slider(
            backing.pixmap,
            gc,
            input_track,
            state.audio.input_volume_percent,
        )?;
        self.text.draw_popup_utf8(
            "Mudo",
            audio_content_x as i32,
            (input_content.y - rect.y - layout::AUDIO_POPUP_BORDER as i16 + 82) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.release_drawable(backing.pixmap);
        self.conn
            .copy_area(
                backing.pixmap,
                window,
                gc,
                0,
                0,
                0,
                0,
                rect.width,
                rect.height,
            )?
            .check()?;
        if !self.pointer_grabbed {
            let grab = self
                .conn
                .grab_pointer(
                    false,
                    self.root,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    0_u32,
                )?
                .reply()?;
            self.pointer_grabbed = grab.status == xproto::GrabStatus::SUCCESS;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: audio pointer grab acquired status={:?} pointer_grabbed={}",
                    grab.status, self.pointer_grabbed
                );
            }
        }
        Ok(())
    }

    fn render_bluetooth_popup(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        if !state.bluetooth_popup_open || !state.bluetooth.available {
            if self.bluetooth_popup.is_some() {
                self.close_popups(Some(state))?;
            }
            return Ok(());
        }
        let output = state
            .outputs
            .first()
            .ok_or("no output for bluetooth popup")?;
        let mut devices: Vec<_> = state
            .bluetooth
            .devices
            .iter()
            .filter(|d| d.connected || d.paired)
            .collect();
        let popup_width = 330_u16.min(output.width.max(1));
        let popup_height = (72_u32
            .saturating_add(
                u32::try_from(devices.len())
                    .unwrap_or(u32::MAX)
                    .saturating_mul(30),
            )
            .min(u32::from(u16::MAX)) as u16)
            .min(output.height.saturating_sub(26).max(1));
        devices.truncate(usize::from(popup_height.saturating_sub(72) / 30));
        let rect = layout::MenuRect {
            x: (output.x as i32 + output.width as i32 - popup_width as i32) as i16,
            y: output.y + 26,
            width: popup_width,
            height: popup_height,
        };
        let power = layout::MenuRect {
            x: rect.x + rect.width as i16
                - POPUP_STYLE.outer_padding as i16
                - POPUP_STYLE.card_padding as i16
                - 46,
            y: rect.y + POPUP_STYLE.outer_padding as i16 + POPUP_STYLE.card_padding as i16,
            width: 46,
            height: 22,
        };
        let rows: Vec<_> = devices
            .iter()
            .enumerate()
            .map(|(i, d)| (d.path.clone(), layout::bluetooth_device_row(rect, i)))
            .collect();
        let effect_owner = popup_effect_owner(&self.windows, output.id)
            .ok_or("no dock window for bluetooth popup effect owner")?;
        let window = if let Some(p) = &self.bluetooth_popup {
            p.window
        } else {
            let w = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "bluetooth-popup", w);
            self.create_glass_popup_window(
                SurfaceRole::BluetoothPopup,
                w,
                rect,
                POPUP_STYLE.border_width,
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION,
            )?;
            self.configure_auxiliary_effect_surface(SurfaceRole::BluetoothPopup, w, effect_owner)?;
            self.conn.map_window(w)?.check()?;
            w
        };
        let resize = self
            .bluetooth_popup
            .as_ref()
            .is_some_and(|p| p.rect != rect);
        self.bluetooth_popup = Some(BluetoothPopupWindow {
            window,
            rect,
            power,
            devices: rows.clone(),
        });
        let backing_replaced = !backing_matches(
            self.bluetooth_backing,
            rect.width,
            rect.height,
            self.glass_surface.depth,
        );
        if backing_replaced {
            let pixmap = self.conn.generate_id()?;
            let create_result = self
                .conn
                .create_pixmap(
                    self.glass_surface.depth,
                    pixmap,
                    self.root,
                    rect.width,
                    rect.height,
                )?
                .check();
            if let Err(error) = create_result {
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            let gc = self.conn.generate_id()?;
            let gc_result = self
                .conn
                .create_gc(
                    gc,
                    pixmap,
                    &xproto::CreateGCAux::new().foreground(
                        self.glass_surface
                            .background_pixel(POPUP_STYLE.material.background),
                    ),
                )?
                .check();
            if let Err(error) = gc_result {
                self.conn.free_pixmap(pixmap)?.check()?;
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            if let Some(old) = self.bluetooth_backing.replace(PopupBacking {
                pixmap,
                gc,
                width: rect.width,
                height: rect.height,
                depth: self.glass_surface.depth,
            }) {
                self.conn.free_gc(old.gc)?.check()?;
                self.conn.free_pixmap(old.pixmap)?.check()?;
            }
        }
        let backing = self.bluetooth_backing.expect("bluetooth backing created");
        if resize || backing_replaced {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(rect.x as i32)
                        .y(rect.y as i32)
                        .width(rect.width as u32)
                        .height(rect.height as u32),
                )?
                .check()?;
            self.apply_surface_effect(
                self.glass_surface,
                SurfaceRole::BluetoothPopup,
                window,
                SurfaceWindowGeometry {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    border_width: POPUP_STYLE.border_width,
                },
            )?;
        }
        self.text
            .prepare_drawable("bluetooth-popup", backing.pixmap, self.glass_surface)?;
        let gc = backing.gc;
        self.fill_glass_background(backing.pixmap, gc, rect.width, rect.height)?;
        self.draw_popup_frame(backing.pixmap, gc, rect.width, rect.height)?;
        self.draw_popup_card(
            backing.pixmap,
            gc,
            rect,
            layout::MenuRect {
                x: rect.x + POPUP_STYLE.outer_padding as i16,
                y: rect.y + POPUP_STYLE.outer_padding as i16,
                width: rect.width.saturating_sub(POPUP_STYLE.outer_padding * 2),
                height: rect.height.saturating_sub(POPUP_STYLE.outer_padding * 2),
            },
        )?;
        if matches!(self.popup_hover, Some(PopupHover::BluetoothPower)) {
            self.draw_popup_hover(backing.pixmap, gc, rect, power)?;
        }
        for (path, device) in &rows {
            if matches!(
                self.popup_hover,
                Some(PopupHover::BluetoothDevice(ref hovered)) if hovered == path
            ) {
                self.draw_popup_hover(backing.pixmap, gc, rect, *device)?;
            }
        }
        self.draw_switch(backing.pixmap, gc, rect, power, state.bluetooth.powered)?;
        self.conn.flush()?;
        self.conn.get_input_focus()?.reply()?;
        self.text.draw_popup_utf8(
            "Bluetooth",
            (POPUP_STYLE.outer_padding + POPUP_STYLE.card_padding - POPUP_STYLE.border_width)
                as i32,
            35,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            "Dispositivos",
            (POPUP_STYLE.outer_padding + POPUP_STYLE.card_padding - POPUP_STYLE.border_width)
                as i32,
            59,
            BAR_STYLE.material.foreground,
        )?;
        for (i, d) in devices.iter().enumerate() {
            let name = if !d.alias.is_empty() {
                &d.alias
            } else if !d.name.is_empty() {
                &d.name
            } else {
                &d.address
            };
            let marker = if d.connected { "✓" } else { " " };
            let status = state
                .bluetooth_pending
                .iter()
                .find_map(|pending| match pending {
                    crate::core::BluetoothPendingAction::ConnectDevice(path) if path == &d.path => {
                        Some("Conectando...")
                    }
                    crate::core::BluetoothPendingAction::DisconnectDevice(path)
                        if path == &d.path =>
                    {
                        Some("Desconectando...")
                    }
                    _ => None,
                })
                .unwrap_or(if d.connected { "Connected" } else { "Paired" });
            let status_width = self.text.measure_popup_width(status);
            let name_width = popup_width
                .saturating_sub(
                    POPUP_STYLE
                        .outer_padding
                        .saturating_add(POPUP_STYLE.card_padding)
                        .saturating_mul(2),
                )
                .saturating_sub(status_width)
                .saturating_sub(24);
            let name = layout::truncate_text_to_width(name, name_width, &PopupMeasurer(&self.text))
                .unwrap_or_default();
            self.text.draw_popup_utf8(
                &format!("{marker} {name} {status}"),
                (POPUP_STYLE.outer_padding + POPUP_STYLE.card_padding - POPUP_STYLE.border_width)
                    as i32,
                81 + i as i32 * 30,
                BAR_STYLE.material.foreground,
            )?;
        }
        self.text.release_drawable(backing.pixmap);
        self.conn
            .copy_area(
                backing.pixmap,
                window,
                gc,
                0,
                0,
                0,
                0,
                rect.width,
                rect.height,
            )?
            .check()?;
        if !self.pointer_grabbed {
            let grab = self
                .conn
                .grab_pointer(
                    false,
                    self.root,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    0_u32,
                )?
                .reply()?;
            self.pointer_grabbed = grab.status == xproto::GrabStatus::SUCCESS;
        }
        Ok(())
    }

    fn render_network_popup(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        if !state.network_popup_open || !state.network.available {
            if self.network_popup.is_some() {
                self.close_popups(Some(state))?;
            }
            return Ok(());
        }
        let output = state.outputs.first().ok_or("no output for network popup")?;
        let popup_width = 380_u16.min(output.width.max(1));
        let card_padding = POPUP_STYLE.card_padding as i16;
        let interface_targets: Vec<_> = state
            .network
            .wifi_devices
            .iter()
            .map(|device| {
                let targets = device
                    .access_points
                    .iter()
                    .map(|access_point| {
                        debug_assert_eq!(access_point.interface, device.interface);
                        NetworkWifiTarget {
                            interface: device.interface.clone(),
                            ssid: access_point.ssid.clone(),
                            band: crate::core::wifi_band(access_point.frequency).into(),
                            saved: access_point.saved_profile.is_some(),
                            active: access_point.is_active,
                        }
                    })
                    .collect::<Vec<_>>();
                (device.interface.clone(), targets)
            })
            .collect();
        let interface_row_counts: Vec<_> = interface_targets
            .iter()
            .map(|(_, targets)| targets.len())
            .collect();
        let popup_height = layout::network_popup_content_height(&interface_row_counts)
            .min(output.height.saturating_sub(26).max(1));
        let rect = layout::MenuRect {
            x: (output.x as i32 + output.width as i32 - popup_width as i32) as i16,
            y: output.y + 26,
            width: popup_width,
            height: popup_height,
        };
        let network_layout = layout::network_popup_layout(rect, &interface_row_counts);
        let status_card = network_layout.status_card;
        let available_section = network_layout.available_section;
        let rows: Vec<_> = interface_targets
            .iter()
            .zip(&network_layout.interfaces)
            .flat_map(|((_, targets), card)| targets.iter().cloned().zip(card.rows.iter().copied()))
            .collect();
        let wireless = layout::MenuRect {
            x: status_card.x + status_card.width as i16 - card_padding - 46,
            y: status_card.y + card_padding,
            width: 46,
            height: 22,
        };
        let window = if let Some(popup) = &self.network_popup {
            popup.window
        } else {
            let window = self.conn.generate_id()?;
            let effect_owner = popup_effect_owner(&self.windows, output.id)
                .ok_or("no dock window for network popup effect owner")?;
            trace_x11_resource("WINDOW_CREATE", "network-popup", window);
            self.create_glass_popup_window(
                SurfaceRole::NetworkPopup,
                window,
                rect,
                POPUP_STYLE.border_width,
                EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::POINTER_MOTION,
            )?;
            self.configure_auxiliary_effect_surface(
                SurfaceRole::NetworkPopup,
                window,
                effect_owner,
            )?;
            self.conn.map_window(window)?.check()?;
            window
        };
        let resize = self
            .network_popup
            .as_ref()
            .is_some_and(|popup| popup.rect != rect);
        self.network_popup = Some(NetworkPopupWindow {
            window,
            rect,
            wireless,
            access_points: rows.clone(),
        });
        let backing_replaced = !backing_matches(
            self.network_backing,
            rect.width,
            rect.height,
            self.glass_surface.depth,
        );
        if backing_replaced {
            let pixmap = self.conn.generate_id()?;
            let create_result = self
                .conn
                .create_pixmap(
                    self.glass_surface.depth,
                    pixmap,
                    self.root,
                    rect.width,
                    rect.height,
                )?
                .check();
            if let Err(error) = create_result {
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            let gc = self.conn.generate_id()?;
            let gc_result = self
                .conn
                .create_gc(
                    gc,
                    pixmap,
                    &xproto::CreateGCAux::new().foreground(
                        self.glass_surface
                            .background_pixel(POPUP_STYLE.material.background),
                    ),
                )?
                .check();
            if let Err(error) = gc_result {
                self.conn.free_pixmap(pixmap)?.check()?;
                if matches!(
                    error,
                    x11rb::errors::ReplyError::X11Error(ref error)
                        if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                ) {
                    return Ok(());
                }
                return Err(error.into());
            }
            if let Some(old) = self.network_backing.replace(PopupBacking {
                pixmap,
                gc,
                width: rect.width,
                height: rect.height,
                depth: self.glass_surface.depth,
            }) {
                self.conn.free_gc(old.gc)?.check()?;
                self.conn.free_pixmap(old.pixmap)?.check()?;
            }
        }
        let backing = self.network_backing.expect("network backing created");
        if resize || backing_replaced {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .x(rect.x as i32)
                        .y(rect.y as i32)
                        .width(rect.width as u32)
                        .height(rect.height as u32),
                )?
                .check()?;
            self.apply_surface_effect(
                self.glass_surface,
                SurfaceRole::NetworkPopup,
                window,
                SurfaceWindowGeometry {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    border_width: POPUP_STYLE.border_width,
                },
            )?;
        }
        self.text
            .prepare_drawable("network-popup", backing.pixmap, self.glass_surface)?;
        let gc = backing.gc;
        self.fill_glass_background(backing.pixmap, gc, rect.width, rect.height)?;
        self.draw_popup_frame(backing.pixmap, gc, rect.width, rect.height)?;
        self.draw_popup_card(backing.pixmap, gc, rect, status_card)?;
        for interface in &network_layout.interfaces {
            self.draw_popup_card(backing.pixmap, gc, rect, interface.card)?;
        }
        let wireless_label = state
            .network_pending
            .iter()
            .map(|pending| match pending {
                crate::core::NetworkPendingAction::SetWireless(enabled) => {
                    if *enabled {
                        "Ligando..."
                    } else {
                        "Desligando..."
                    }
                }
            })
            .next()
            .unwrap_or(if state.network.wireless_enabled {
                "ON"
            } else {
                "OFF"
            });
        if matches!(self.popup_hover, Some(PopupHover::NetworkWireless)) {
            self.draw_popup_hover(backing.pixmap, gc, rect, wireless)?;
        }
        self.draw_switch(
            backing.pixmap,
            gc,
            rect,
            wireless,
            state.network.wireless_enabled,
        )?;
        for (target, row) in &rows {
            if matches!(
                self.popup_hover,
                Some(PopupHover::NetworkWifi(ref hovered)) if hovered == target
            ) {
                self.draw_popup_hover(backing.pixmap, gc, rect, *row)?;
            }
            let button = layout::MenuRect {
                x: row.x + row.width as i16 - 78,
                y: row.y + 4,
                width: 68,
                height: row.height.saturating_sub(8),
            };
            self.draw_popup_card(backing.pixmap, gc, rect, button)?;
        }
        self.conn.flush()?;
        self.conn.get_input_focus()?.reply()?;
        self.text.draw_popup_utf8(
            "Wi-Fi",
            (status_card.x - rect.x + card_padding) as i32,
            (status_card.y - rect.y + card_padding + 16) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            wireless_label,
            (wireless.x - rect.x - 42) as i32,
            (wireless.y - rect.y + 16) as i32,
            POPUP_STYLE.muted_foreground,
        )?;
        self.text.draw_popup_utf8(
            &if state.network.link_kind == crate::core::NetworkLinkKind::Ethernet {
                "Ethernet                 Connected".to_string()
            } else if state.network.connectivity == crate::core::NetworkConnectivity::Connected {
                format!(
                    "Conectado: {}",
                    state.network.display_name.as_deref().unwrap_or("Wi-Fi")
                )
            } else {
                "Desconectado".to_string()
            },
            (status_card.x - rect.x + card_padding) as i32,
            (status_card.y - rect.y + 54) as i32,
            BAR_STYLE.material.foreground,
        )?;
        self.text.draw_popup_utf8(
            "Redes disponíveis",
            (available_section.x - rect.x) as i32,
            (available_section.y - rect.y + self.text.popup_baseline(available_section.height))
                as i32,
            BAR_STYLE.material.foreground,
        )?;
        for ((interface, _), card) in interface_targets.iter().zip(&network_layout.interfaces) {
            self.text.draw_popup_utf8(
                &layout::truncate_text_to_width(
                    interface,
                    card.header.width.saturating_sub(4),
                    &PopupMeasurer(&self.text),
                )
                .unwrap_or_default(),
                (card.header.x - rect.x) as i32,
                (card.header.y - rect.y + self.text.popup_baseline(card.header.height)) as i32,
                POPUP_STYLE.muted_foreground,
            )?;
        }
        for (target, row) in &rows {
            let access_point = state
                .network
                .access_points
                .iter()
                .find(|access_point| {
                    access_point.interface == target.interface
                        && access_point.ssid == target.ssid
                        && crate::core::wifi_band(access_point.frequency) == target.band
                })
                .expect("network popup row has matching access point");
            let button = layout::MenuRect {
                x: row.x + row.width as i16 - 78,
                y: row.y + 4,
                width: 68,
                height: row.height.saturating_sub(8),
            };
            let label_width = button
                .x
                .saturating_sub(row.x)
                .saturating_sub(card_padding)
                .saturating_sub(4) as u16;
            let label = layout::truncate_text_to_width(
                &network_primary_row_label(&access_point.ssid, access_point.is_active),
                label_width,
                &PopupMeasurer(&self.text),
            )
            .unwrap_or_default();
            self.text.draw_popup_utf8(
                &label,
                (row.x - rect.x + card_padding) as i32,
                (row.y - rect.y + self.text.popup_baseline(row.height)) as i32,
                BAR_STYLE.material.foreground,
            )?;
            self.text.draw_popup_utf8(
                if access_point.is_active { "✓" } else { "↗" },
                (button.x - rect.x + 25) as i32,
                (button.y - rect.y + self.text.popup_baseline(button.height)) as i32,
                BAR_STYLE.material.foreground,
            )?;
        }
        self.text.release_drawable(backing.pixmap);
        self.conn
            .copy_area(
                backing.pixmap,
                window,
                gc,
                0,
                0,
                0,
                0,
                rect.width,
                rect.height,
            )?
            .check()?;
        if !self.pointer_grabbed {
            let grab = self
                .conn
                .grab_pointer(
                    false,
                    self.root,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    0_u32,
                )?
                .reply()?;
            self.pointer_grabbed = grab.status == xproto::GrabStatus::SUCCESS;
        }
        Ok(())
    }

    fn draw_audio_slider(
        &self,
        window: u32,
        gc: u32,
        hit: layout::MenuRect,
        percent: u32,
    ) -> Result<(), Box<dyn Error>> {
        let x = hit.x - self.audio_popup.as_ref().map_or(hit.x, |p| p.rect.x);
        let y = hit.y - self.audio_popup.as_ref().map_or(hit.y, |p| p.rect.y) + 9;
        let width = hit.width as i16;
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(self.glass_surface.opaque_pixel(0x596273)),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x,
                y,
                width: width as u16,
                height: 4,
            }],
        )?;
        self.conn
            .change_gc(
                gc,
                &xproto::ChangeGCAux::new().foreground(
                    self.glass_surface
                        .opaque_pixel(BAR_STYLE.menu_hover_foreground),
                ),
            )?
            .check()?;
        let fill = (width as u32 * percent.min(100) / 100) as u16;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x,
                y,
                width: fill,
                height: 4,
            }],
        )?;
        let thumb_x = x + fill as i16 - 4;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: thumb_x,
                y: y - 4,
                width: 8,
                height: 12,
            }],
        )?;
        Ok(())
    }

    fn destroy_popup_suffix(&mut self, index: usize) -> Result<(), Box<dyn Error>> {
        let trace = std::env::var_os("XBAR_TRACE").is_some();
        for popup in self.popups.split_off(index) {
            self.text.release_drawable(popup.window);
            if let Some(backing) = popup.backing {
                self.text.release_drawable(backing.pixmap);
                self.conn.free_gc(backing.gc)?.check()?;
                self.conn.free_pixmap(backing.pixmap)?.check()?;
            }
            if trace {
                eprintln!("xbar trace: popup destroyed xid={}", popup.window);
            }
            trace_x11_resource("WINDOW_DESTROY", "menu-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
        }
        Ok(())
    }

    fn render_popups(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
        let mut dirty = std::mem::take(&mut self.menu_popup_dirty);
        let Some(root_id) = state.menu_interaction.open_root else {
            self.close_popups(Some(state))?;
            return Ok(());
        };
        let Some(model) = state.active_menu_model() else {
            self.close_popups(Some(state))?;
            return Ok(());
        };
        let Some(root_item) = layout::find_item(&model.root, root_id) else {
            return Ok(());
        };
        let tray_endpoint = match &state.menu {
            crate::core::MenuState::TrayLoaded { endpoint, .. } => Some(endpoint),
            _ => None,
        };
        let popup_role = if tray_endpoint.is_some() {
            SurfaceRole::TrayPopup
        } else {
            SurfaceRole::GlobalMenuPopup
        };
        let anchor = self
            .bar_hits
            .iter()
            .find_map(|(_, output_id, _, _, items, tray, _, _, _)| {
                if let Some(endpoint) = tray_endpoint {
                    tray.iter()
                        .find(|item| item.endpoint.service == endpoint.service)
                        .map(|item| (*output_id, item.rect))
                } else {
                    items
                        .iter()
                        .find(|item| item.id == root_id)
                        .map(|item| (*output_id, item.rect))
                }
            });
        let Some((output_id, top_rect)) = anchor else {
            return Ok(());
        };
        let Some(output) = state.outputs.iter().find(|output| output.id == output_id) else {
            return Ok(());
        };
        let effect_owner = popup_effect_owner(&self.windows, output_id)
            .ok_or("no dock window for menu popup effect owner")?;
        let structure_changed = self.popups.len() != state.menu_interaction.open_path.len()
            || self
                .popups
                .iter()
                .zip(state.menu_interaction.open_path.iter())
                .any(|(popup, id)| popup.layout.parent_id != *id);
        if structure_changed {
            // The path owns popup topology. Reconciliation may create or
            // destroy a suffix, so a stale per-window plan cannot be used.
            dirty.mark_all();
        }
        let first_mismatch = self
            .popups
            .iter()
            .zip(state.menu_interaction.open_path.iter())
            .position(|(popup, id)| popup.layout.parent_id != *id)
            .unwrap_or_else(|| {
                self.popups
                    .len()
                    .min(state.menu_interaction.open_path.len())
            });
        if first_mismatch < self.popups.len() {
            self.destroy_popup_suffix(first_mismatch)?;
        }
        let mut parent = root_item;
        let mut anchor = top_rect;
        for (level, id) in state.menu_interaction.open_path.iter().enumerate() {
            if level > 0 {
                let Some(item) = layout::find_item(parent, *id) else {
                    break;
                };
                parent = item;
                let Some(previous) = self
                    .popups
                    .last()
                    .and_then(|p| p.layout.items.iter().find(|i| i.id == *id))
                else {
                    break;
                };
                anchor = previous.rect;
            }
            let popup_layout = layout::popup_layout_with_measurer(
                output,
                parent,
                anchor,
                level > 0,
                &PopupMeasurer(&self.text),
            );
            let reuse = self
                .popups
                .get(level)
                .is_some_and(|popup| popup.layout.parent_id == parent.id);
            if !reuse && level < self.popups.len() {
                self.destroy_popup_suffix(level)?;
            }
            let window = if reuse {
                self.popups[level].window
            } else {
                let window = self.conn.generate_id()?;
                trace_x11_resource("WINDOW_CREATE", "menu-popup", window);
                window
            };
            if !reuse {
                self.create_glass_popup_window(
                    popup_role,
                    window,
                    popup_layout.rect,
                    POPUP_STYLE.border_width,
                    EventMask::EXPOSURE
                        | EventMask::BUTTON_PRESS
                        | EventMask::POINTER_MOTION
                        | EventMask::ENTER_WINDOW
                        | EventMask::LEAVE_WINDOW,
                )?;
                self.configure_auxiliary_effect_surface(popup_role, window, effect_owner)?;
                self.conn.map_window(window)?.check()?;
                self.popups.push(PopupWindow {
                    window,
                    layout: popup_layout.clone(),
                    backing: None,
                });
            }
            let resize = self.popups[level].layout.rect != popup_layout.rect;
            let backing_replaced = !backing_matches(
                self.popups[level].backing,
                popup_layout.rect.width,
                popup_layout.rect.height,
                self.glass_surface.depth,
            );
            let render_popup = popup_slot_is_selected(&dirty, PopupSlot(level))
                || !reuse
                || resize
                || backing_replaced;
            if !render_popup {
                // A specific dirty plan only skips a popup whose existing
                // geometry and backing are still valid. Its buffered frame is
                // already complete and remains mapped unchanged.
                self.popups[level].layout = popup_layout;
                continue;
            }
            if backing_replaced {
                let pixmap = self.conn.generate_id()?;
                let create_result = self
                    .conn
                    .create_pixmap(
                        self.glass_surface.depth,
                        pixmap,
                        self.root,
                        popup_layout.rect.width,
                        popup_layout.rect.height,
                    )?
                    .check();
                if let Err(error) = create_result {
                    if matches!(
                        error,
                        x11rb::errors::ReplyError::X11Error(ref error)
                            if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                    ) {
                        return Ok(());
                    }
                    return Err(error.into());
                }
                let gc = self.conn.generate_id()?;
                let gc_result = self
                    .conn
                    .create_gc(
                        gc,
                        pixmap,
                        &xproto::CreateGCAux::new().foreground(
                            self.glass_surface
                                .background_pixel(POPUP_STYLE.material.background),
                        ),
                    )?
                    .check();
                if let Err(error) = gc_result {
                    self.conn.free_pixmap(pixmap)?.check()?;
                    if matches!(
                        error,
                        x11rb::errors::ReplyError::X11Error(ref error)
                            if error.error_kind == x11rb::protocol::ErrorKind::Alloc
                    ) {
                        return Ok(());
                    }
                    return Err(error.into());
                }
                if let Some(old) = self.popups[level].backing.replace(PopupBacking {
                    pixmap,
                    gc,
                    width: popup_layout.rect.width,
                    height: popup_layout.rect.height,
                    depth: self.glass_surface.depth,
                }) {
                    self.conn.free_gc(old.gc)?.check()?;
                    self.conn.free_pixmap(old.pixmap)?.check()?;
                }
            }
            let backing = self.popups[level]
                .backing
                .expect("menu popup backing created");
            if resize || backing_replaced {
                self.conn
                    .configure_window(
                        window,
                        &xproto::ConfigureWindowAux::new()
                            .x(popup_layout.rect.x as i32)
                            .y(popup_layout.rect.y as i32)
                            .width(popup_layout.rect.width as u32)
                            .height(popup_layout.rect.height as u32),
                    )?
                    .check()?;
                self.apply_surface_effect(
                    self.glass_surface,
                    popup_role,
                    window,
                    SurfaceWindowGeometry {
                        x: popup_layout.rect.x,
                        y: popup_layout.rect.y,
                        width: popup_layout.rect.width,
                        height: popup_layout.rect.height,
                        border_width: POPUP_STYLE.border_width,
                    },
                )?;
            }
            self.text
                .prepare_drawable("menu-popup", backing.pixmap, self.glass_surface)?;
            let gc = backing.gc;
            self.fill_glass_background(
                backing.pixmap,
                gc,
                popup_layout.rect.width,
                popup_layout.rect.height,
            )?;
            self.draw_popup_frame(
                backing.pixmap,
                gc,
                popup_layout.rect.width,
                popup_layout.rect.height,
            )?;
            let card = popup_layout.content_rect();
            self.draw_popup_card(backing.pixmap, gc, popup_layout.rect, card)?;
            for item in &popup_layout.items {
                if item.separator {
                    self.conn.poly_fill_rectangle(
                        backing.pixmap,
                        gc,
                        &[xproto::Rectangle {
                            x: card.x - popup_layout.rect.x + POPUP_STYLE.card_padding as i16,
                            y: item.rect.y - popup_layout.rect.y
                                + (POPUP_STYLE.section_gap / 2) as i16,
                            width: card.width.saturating_sub(POPUP_STYLE.card_padding * 2),
                            height: 1,
                        }],
                    )?;
                    continue;
                }
                let hovered = state.menu_interaction.hovered_path.last() == Some(&item.id);
                if hovered {
                    self.draw_popup_hover(backing.pixmap, gc, popup_layout.rect, item.rect)?;
                    self.conn
                        .change_gc(
                            gc,
                            &xproto::ChangeGCAux::new().foreground(
                                self.glass_surface
                                    .opaque_pixel(BAR_STYLE.menu_hover_foreground),
                            ),
                        )?
                        .check()?;
                }
                let color = if item.enabled {
                    BAR_STYLE.material.foreground
                } else {
                    POPUP_STYLE.muted_foreground
                };
                self.conn
                    .change_gc(
                        gc,
                        &xproto::ChangeGCAux::new()
                            .foreground(self.glass_surface.opaque_pixel(color)),
                    )?
                    .check()?;
            }
            self.conn.flush()?;
            self.conn.get_input_focus()?.reply()?;
            for item in &popup_layout.items {
                if item.separator {
                    continue;
                }
                let color = if item.enabled {
                    BAR_STYLE.material.foreground
                } else {
                    POPUP_STYLE.muted_foreground
                };
                self.text.draw_popup_utf8(
                    &item.label,
                    (item.rect.x - popup_layout.rect.x + POPUP_STYLE.row_horizontal_padding as i16)
                        as i32,
                    (item.rect.y - popup_layout.rect.y) as i32
                        + self.text.popup_baseline(POPUP_STYLE.row_height) as i32,
                    color,
                )?;
                if let Some(shortcut) = &item.shortcut {
                    let width = self.text.measure_popup_width(shortcut);
                    self.text.draw_popup_utf8(
                        shortcut,
                        popup_layout
                            .rect
                            .x
                            .saturating_sub(popup_layout.rect.x)
                            .saturating_add(item.rect.width as i16)
                            .saturating_sub((POPUP_STYLE.row_horizontal_padding + width) as i16)
                            as i32,
                        (item.rect.y - popup_layout.rect.y) as i32
                            + self.text.popup_baseline(POPUP_STYLE.row_height) as i32,
                        color,
                    )?;
                }
                if item.has_submenu {
                    self.text.draw_popup_utf8(
                        ">",
                        (item.rect.x - popup_layout.rect.x + item.rect.width as i16 - 14) as i32,
                        (item.rect.y - popup_layout.rect.y) as i32
                            + self.text.popup_baseline(POPUP_STYLE.row_height) as i32,
                        color,
                    )?;
                }
            }
            self.text.release_drawable(backing.pixmap);
            self.conn
                .copy_area(
                    backing.pixmap,
                    window,
                    gc,
                    0,
                    0,
                    0,
                    0,
                    popup_layout.rect.width,
                    popup_layout.rect.height,
                )?
                .check()?;
            self.popups[level].layout = popup_layout;
            if !reuse && std::env::var_os("XBAR_TRACE").is_some() {
                let popup = self.popups.last().expect("popup just inserted");
                eprintln!(
                    "xbar trace: popup created xid={} parent={} geometry=x{} y{} w{} h{} level={}",
                    popup.window,
                    popup.layout.parent_id.0,
                    popup.layout.rect.x,
                    popup.layout.rect.y,
                    popup.layout.rect.width,
                    popup.layout.rect.height,
                    level
                );
            }
        }
        if !self.pointer_grabbed {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: pointer grab acquire reason=menu_open open_root={:?} open_path={:?} popup_count={} focused_window={:?} focused_workspace={:?}",
                    state.menu_interaction.open_root,
                    state.menu_interaction.open_path,
                    self.popups.len(),
                    state.focused_window,
                    state.focused_workspace
                );
            }
            let grab = self
                .conn
                .grab_pointer(
                    false,
                    self.root,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    0_u32,
                )?
                .reply()?;
            self.pointer_grabbed = grab.status == xproto::GrabStatus::SUCCESS;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: pointer grab acquired status={:?} pointer_grabbed={}",
                    grab.status, self.pointer_grabbed
                );
            }
        }
        Ok(())
    }

    pub fn hit_test(&self, event: &X11Event) -> HitTarget {
        let (window, x, y, root_x, root_y) = match event {
            X11Event::ButtonPress {
                window,
                x,
                y,
                root_x,
                root_y,
                ..
            } => (*window, *x, *y, *root_x as i16, *root_y as i16),
            X11Event::ButtonRelease { window, x, y, .. }
            | X11Event::MotionNotify { window, x, y } => (*window, *x, *y, *x, *y),
            _ => return HitTarget::Outside,
        };
        if let Some(center) = &self.notification_center {
            if notification_scroll_trace_enabled() {
                if let X11Event::ButtonPress { button, .. } = event {
                    eprintln!(
                        "notification-center button-match: event={} center={} matches={} detail={}",
                        window,
                        center.window,
                        center.window == window,
                        button
                    );
                }
            }
            if center.window == window {
                let local_x = x;
                let local_y = y;
                return center
                    .card_hits
                    .iter()
                    .find(|(_, rect)| {
                        local_x >= rect.x
                            && local_x < rect.x + rect.width as i16
                            && local_y >= rect.y
                            && local_y < rect.y + rect.height as i16
                    })
                    .map(|(id, _)| HitTarget::NotificationCenterCard(*id))
                    .unwrap_or(HitTarget::NotificationCenterEmpty);
            }
        }
        if let Some((_bar, _, ox, oy, items, tray, network, audio, bluetooth)) =
            self.bar_hits.iter().find(|(bar, _, _, _, _, _, _, _, _)| {
                *bar == window || (self.root == window && root_y < BAR_HEIGHT as i16)
            })
        {
            if let Some(target) =
                notification_indicator_hit(&self.notification_hits, window, root_x, root_y)
            {
                return target;
            }
            let root_coordinates = self.root == window;
            let bar_x = if root_coordinates { root_x } else { x + *ox };
            let bar_y = if root_coordinates { root_y } else { y + *oy };
            if let Some(item) = items
                .iter()
                .find(|i| {
                    bar_x >= i.rect.x
                        && bar_x < i.rect.x + i.rect.width as i16
                        && bar_y >= i.rect.y
                        && bar_y < i.rect.y + i.rect.height as i16
                })
                .map(|i| HitTarget::TopLevel(i.id))
            {
                return item;
            }
            let root_x = bar_x;
            let root_y = bar_y;
            if let Some(network) = network {
                if root_x >= network.rect.x
                    && root_x < network.rect.x + network.rect.width as i16
                    && root_y >= network.rect.y
                    && root_y < network.rect.y + network.rect.height as i16
                {
                    return HitTarget::Network;
                }
            }
            if let Some(audio) = audio {
                if root_x >= audio.rect.x
                    && root_x < audio.rect.x + audio.rect.width as i16
                    && root_y >= audio.rect.y
                    && root_y < audio.rect.y + audio.rect.height as i16
                {
                    return HitTarget::Audio;
                }
            }
            if let Some(bluetooth) = bluetooth {
                if root_x >= bluetooth.rect.x
                    && root_x < bluetooth.rect.x + bluetooth.rect.width as i16
                    && root_y >= bluetooth.rect.y
                    && root_y < bluetooth.rect.y + bluetooth.rect.height as i16
                {
                    return HitTarget::Bluetooth;
                }
            }
            return tray_hit(tray, root_x, root_y)
                .map(HitTarget::Tray)
                .unwrap_or(HitTarget::Outside);
        }
        if let Some(popup) = &self.audio_popup {
            let root_inside_popup = self.root == window
                && root_x >= popup.rect.x
                && root_x < popup.rect.x + popup.rect.width as i16
                && root_y >= popup.rect.y
                && root_y < popup.rect.y + popup.rect.height as i16;
            if popup.window == window || root_inside_popup {
                let root_coordinates = self.root == window;
                let root_x = if root_coordinates {
                    root_x
                } else {
                    x + popup.rect.x
                };
                let root_y = if root_coordinates {
                    root_y
                } else {
                    y + popup.rect.y
                };
                if root_x >= popup.track.x
                    && root_x < popup.track.x + popup.track.width as i16
                    && root_y >= popup.track.y
                    && root_y < popup.track.y + popup.track.height as i16
                {
                    return HitTarget::AudioTrack;
                }
                if root_x >= popup.input_track.x
                    && root_x < popup.input_track.x + popup.input_track.width as i16
                    && root_y >= popup.input_track.y
                    && root_y < popup.input_track.y + popup.input_track.height as i16
                {
                    return HitTarget::AudioInputTrack;
                }
                if root_x >= popup.mute.x
                    && root_x < popup.mute.x + popup.mute.width as i16
                    && root_y >= popup.mute.y
                    && root_y < popup.mute.y + popup.mute.height as i16
                {
                    return HitTarget::AudioMute;
                }
                if root_x >= popup.input_mute.x
                    && root_x < popup.input_mute.x + popup.input_mute.width as i16
                    && root_y >= popup.input_mute.y
                    && root_y < popup.input_mute.y + popup.input_mute.height as i16
                {
                    return HitTarget::AudioInputMute;
                }
                // Window-local coordinates start inside the X11 border. The device
                // layout uses true root coordinates, as do events from the root grab.
                let (device_x, device_y) = if root_coordinates {
                    (root_x, root_y)
                } else {
                    (
                        root_x + layout::AUDIO_POPUP_BORDER as i16,
                        root_y + layout::AUDIO_POPUP_BORDER as i16,
                    )
                };
                if let Some(row) = popup
                    .output_devices
                    .iter()
                    .find(|row| row.contains(device_x, device_y))
                {
                    return HitTarget::AudioOutputDevice(row.name.clone());
                }
                if let Some(row) = popup
                    .input_devices
                    .iter()
                    .find(|row| row.contains(device_x, device_y))
                {
                    return HitTarget::AudioInputDevice(row.name.clone());
                }
                return HitTarget::AudioInside;
            }
        }
        if let Some(popup) = &self.bluetooth_popup {
            let inside = popup.window == window
                || (self.root == window
                    && root_x >= popup.rect.x
                    && root_x < popup.rect.x + popup.rect.width as i16
                    && root_y >= popup.rect.y
                    && root_y < popup.rect.y + popup.rect.height as i16);
            if inside {
                let rx = if popup.window == window {
                    x + popup.rect.x
                } else {
                    root_x
                };
                let ry = if popup.window == window {
                    y + popup.rect.y
                } else {
                    root_y
                };
                if rx >= popup.power.x
                    && rx < popup.power.x + popup.power.width as i16
                    && ry >= popup.power.y
                    && ry < popup.power.y + popup.power.height as i16
                {
                    return HitTarget::BluetoothPower;
                }
                if let Some((path, _)) = popup.devices.iter().find(|(_, r)| r.contains(rx, ry)) {
                    return HitTarget::BluetoothDevice(path.clone());
                }
                return HitTarget::BluetoothInside;
            }
        }
        if let Some(popup) = &self.network_popup {
            let inside = popup.window == window
                || (self.root == window
                    && root_x >= popup.rect.x
                    && root_x < popup.rect.x + popup.rect.width as i16
                    && root_y >= popup.rect.y
                    && root_y < popup.rect.y + popup.rect.height as i16);
            if inside {
                let rx = if popup.window == window {
                    x + popup.rect.x
                } else {
                    root_x
                };
                let ry = if popup.window == window {
                    y + popup.rect.y
                } else {
                    root_y
                };
                if rx >= popup.wireless.x
                    && rx < popup.wireless.x + popup.wireless.width as i16
                    && ry >= popup.wireless.y
                    && ry < popup.wireless.y + popup.wireless.height as i16
                {
                    return HitTarget::NetworkWireless;
                }
                if let Some((target, _)) = popup
                    .access_points
                    .iter()
                    .find(|(_, rect)| rect.contains(rx, ry))
                {
                    return HitTarget::NetworkWifi(target.clone());
                }
                return HitTarget::NetworkInside;
            }
        }
        for (level, popup) in self.popups.iter().enumerate() {
            if popup.window != window && self.root != window {
                continue;
            }
            let popup_x = if self.root == window {
                root_x - popup.layout.rect.x
            } else {
                x
            };
            let popup_y = if self.root == window {
                root_y - popup.layout.rect.y
            } else {
                y
            };
            if let Some(item) = popup.layout.item_at_local(popup_x, popup_y) {
                let mut path = Vec::new();
                path.extend(self.popups.iter().take(level).map(|p| p.layout.parent_id));
                path.push(item.id);
                return HitTarget::Item(path);
            }
        }
        HitTarget::Outside
    }
}

fn popup_hover_for(target: Option<&HitTarget>) -> Option<PopupHover> {
    match target {
        Some(HitTarget::Item(path)) => path.last().copied().map(PopupHover::MenuItem),
        Some(HitTarget::AudioOutputDevice(name)) => {
            Some(PopupHover::AudioOutputDevice(name.clone()))
        }
        Some(HitTarget::AudioInputDevice(name)) => Some(PopupHover::AudioInputDevice(name.clone())),
        Some(HitTarget::BluetoothPower) => Some(PopupHover::BluetoothPower),
        Some(HitTarget::BluetoothDevice(path)) => Some(PopupHover::BluetoothDevice(path.clone())),
        Some(HitTarget::NetworkWifi(target)) => Some(PopupHover::NetworkWifi(target.clone())),
        Some(HitTarget::NetworkWireless) => Some(PopupHover::NetworkWireless),
        _ => None,
    }
}

fn network_primary_row_label(ssid: &str, active: bool) -> String {
    format!("{} {ssid}", if active { "●" } else { " " })
}

fn popup_hover_transition(
    old: &Option<PopupHover>,
    target: Option<&HitTarget>,
) -> (Option<PopupHover>, bool) {
    let next = popup_hover_for(target);
    let changed = *old != next;
    (next, changed)
}

impl Drop for X11Platform {
    fn drop(&mut self) {
        if let Some(shortcut) = self.global_navigation_shortcut.take() {
            self.unregister_global_pin_shortcut(&shortcut);
        }
        if let Some(shortcut) = self.global_pin_shortcut.take() {
            self.unregister_global_pin_shortcut(&shortcut);
        }
        if self.keyboard_grab_session.is_some() {
            let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
            self.keyboard_grab_session = None;
        }
        self.text.release_active_drawable();

        for backing in [
            self.audio_backing.take(),
            self.bluetooth_backing.take(),
            self.network_backing.take(),
        ]
        .into_iter()
        .flatten()
        {
            self.text.release_drawable(backing.pixmap);
            let _ = self.conn.free_gc(backing.gc);
            let _ = self.conn.free_pixmap(backing.pixmap);
        }

        for popup in self.popups.drain(..) {
            if let Some(backing) = popup.backing {
                self.text.release_drawable(backing.pixmap);
                let _ = self.conn.free_gc(backing.gc);
                let _ = self.conn.free_pixmap(backing.pixmap);
            }
            let _ = self.conn.destroy_window(popup.window);
        }
        if let Some(notification) = self.notification.take() {
            if let Some(backing) = notification.backing {
                self.text.release_drawable(backing.pixmap);
                let _ = self.conn.free_gc(backing.gc);
                let _ = self.conn.free_pixmap(backing.pixmap);
            }
            let _ = self.conn.destroy_window(notification.window);
        }
        if let Some(center) = self.notification_center.take() {
            if let Some(backing) = center.backing {
                self.text.release_drawable(backing.pixmap);
                let _ = self.conn.free_gc(backing.gc);
                let _ = self.conn.free_pixmap(backing.pixmap);
            }
            let _ = self.conn.destroy_window(center.window);
        }
        for window in [
            self.audio_popup.take().map(|popup| popup.window),
            self.bluetooth_popup.take().map(|popup| popup.window),
            self.network_popup.take().map(|popup| popup.window),
            self.instance_window.take(),
        ]
        .into_iter()
        .flatten()
        {
            let _ = self.conn.destroy_window(window);
        }
        for bar in std::mem::take(&mut self.windows) {
            if let Some(backing) = bar.backing {
                self.text.release_drawable(backing.pixmap);
                let _ = self.conn.free_gc(backing.gc);
                let _ = self.conn.free_pixmap(backing.pixmap);
            }
            let _ = self.conn.destroy_window(bar.window);
        }
        if let Some(colormap) = self.glass_surface.owned_colormap {
            let _ = self.conn.free_colormap(colormap);
        }
        let _ = self.conn.flush();
    }
}

fn tray_hit(items: &[view::TrayVisualItem], x: i16, y: i16) -> Option<StatusNotifierEndpoint> {
    items
        .iter()
        .find(|item| {
            x >= item.rect.x
                && x < item.rect.x + item.rect.width as i16
                && y >= item.rect.y
                && y < item.rect.y + item.rect.height as i16
        })
        .map(|item| item.endpoint.clone())
}

fn single_line(text: &str) -> String {
    text.lines()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(42)
        .collect()
}

fn is_xbar_owned_window(
    window: u32,
    root: u32,
    instance_window: Option<u32>,
    dock_windows: impl Iterator<Item = u32>,
    popup_windows: impl Iterator<Item = u32>,
) -> bool {
    window == root
        || instance_window == Some(window)
        || dock_windows.into_iter().any(|dock| dock == window)
        || popup_windows.into_iter().any(|popup| popup == window)
}

#[cfg(test)]
mod tests {
    use super::{
        backing_matches, bar_backing_matches, blur_behind_rect, classify_attention_property_reply,
        classify_property_string_reply, effect_owner_property_value, install_passive_grabs,
        is_xbar_owned_window, menu_popup_dirty_for_interaction_change, menu_popup_slot_for_window,
        menu_popup_slots_for_item, network_primary_row_label, notification_hover_transition,
        notification_indicator_hit, notification_indicator_rect, notification_previous_scroll,
        notification_scroll_target, notification_wheel_direction, popup_effect_owner,
        popup_hover_for, popup_hover_transition, popup_slot_is_selected, preserve_color_pixel,
        reconcile_notification_scroll, template_icon_pixel, tray_draw_size, tray_hit,
        union_menu_rects, AttentionPropertyRead, BarBacking, BarWindow, GlobalPinShortcut,
        HitTarget, MenuPopupDirty, PopupBacking, PopupHover, PopupSlot, PopupWindow, RenderTarget,
        SurfaceWindowGeometry, X11Event, BAR_HEIGHT,
    };
    use crate::core::{
        MenuItemId, OutputId, OutputState, StatusNotifierEndpoint, StatusNotifierIcon,
    };
    use crate::ui::{
        layout::{MenuRect, PopupItemRect, PopupLayout},
        view::TrayIconRenderMode,
        view::TrayVisualItem,
    };
    use x11rb::errors::ReplyError;
    use x11rb::protocol::xproto::{EventMask, ModMask};
    use x11rb::protocol::{xproto, ErrorKind};
    use x11rb::x11_utils::X11Error;

    fn x11_error(kind: ErrorKind, bad_value: u32) -> ReplyError {
        ReplyError::X11Error(X11Error {
            error_kind: kind,
            error_code: 3,
            sequence: 1,
            bad_value,
            minor_opcode: 0,
            major_opcode: 20,
            extension_name: None,
            request_name: Some("GetProperty"),
        })
    }

    fn property_reply(value: &[u8]) -> xproto::GetPropertyReply {
        xproto::GetPropertyReply {
            format: 8,
            sequence: 1,
            length: value.len() as u32,
            type_: 31,
            bytes_after: 0,
            value_len: value.len() as u32,
            value: value.to_vec(),
        }
    }

    #[test]
    fn property_string_classifies_success_and_absence() {
        assert!(
            classify_property_string_reply(7, Ok(property_reply(b"app")))
                .expect("successful property")
                .is_some()
        );
        assert!(classify_property_string_reply(7, Ok(property_reply(b"")))
            .expect("absent property")
            .is_none());
    }

    #[test]
    fn property_string_classifies_stale_window_as_absent() {
        assert!(
            classify_property_string_reply(7, Err(x11_error(ErrorKind::Window, 7)))
                .expect("stale external window")
                .is_none()
        );
    }

    #[test]
    fn property_string_propagates_unrelated_x11_errors() {
        assert!(classify_property_string_reply(7, Err(x11_error(ErrorKind::Match, 7))).is_err());
        assert!(classify_property_string_reply(7, Err(x11_error(ErrorKind::Window, 8))).is_err());
    }

    #[test]
    fn navigation_keysyms_map_only_the_supported_navigation_keys() {
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff51),
            Some(crate::core::Event::MenuNavigateLeft)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff52),
            Some(crate::core::Event::MenuNavigateUp)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff53),
            Some(crate::core::Event::MenuNavigateRight)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff54),
            Some(crate::core::Event::MenuNavigateDown)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff1b),
            Some(crate::core::Event::MenuNavigateEscape)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff0d),
            Some(crate::core::Event::MenuNavigateEnter)
        ));
        assert!(matches!(
            super::X11Platform::navigation_event_for_keysym(0xff8d),
            Some(crate::core::Event::MenuNavigateEnter)
        ));
        assert_eq!(super::X11Platform::navigation_event_for_keysym(0), None);
        assert_eq!(super::X11Platform::navigation_event_for_keysym(0x61), None);
    }

    #[test]
    fn global_pin_shortcut_handles_lock_variants_and_latches_repeat_until_release() {
        let base = u16::from(ModMask::M4 | ModMask::SHIFT);
        let num_lock = ModMask::M2;
        let mut shortcut = GlobalPinShortcut::new(58, Some(num_lock));
        assert_eq!(shortcut.modifier_variants().len(), 4);

        for state in [
            base,
            base | u16::from(ModMask::LOCK),
            base | u16::from(num_lock),
            base | u16::from(ModMask::LOCK) | u16::from(num_lock),
        ] {
            shortcut.down = false;
            let press = X11Event::KeyPress {
                keycode: 58,
                state,
                timestamp: 1,
            };
            assert!(matches!(
                shortcut.event(&press),
                Some(crate::core::Event::ToggleMenuPresentationPin)
            ));
            assert_eq!(shortcut.event(&press), None, "repeat state={state}");
            assert_eq!(
                shortcut.event(&X11Event::KeyRelease {
                    keycode: 58,
                    state,
                    timestamp: 2,
                }),
                None
            );
            assert!(matches!(
                shortcut.event(&press),
                Some(crate::core::Event::ToggleMenuPresentationPin)
            ));
        }
    }

    #[test]
    fn global_navigation_shortcut_has_independent_repeat_state_and_event() {
        let base = u16::from(ModMask::M4 | ModMask::SHIFT);
        let mut pin = GlobalPinShortcut::new(58, Some(ModMask::M2));
        let mut navigation = GlobalPinShortcut::for_event(
            42,
            Some(ModMask::M2),
            crate::core::Event::MenuNavigationStarted,
        );
        let press_g = X11Event::KeyPress {
            keycode: 42,
            state: base,
            timestamp: 10,
        };
        assert_eq!(pin.event(&press_g), None);
        assert!(matches!(
            navigation.event(&press_g),
            Some(crate::core::Event::MenuNavigationStarted)
        ));
        assert_eq!(navigation.event(&press_g), None);
        assert_eq!(
            navigation.event(&X11Event::KeyRelease {
                keycode: 42,
                state: base,
                timestamp: 20,
            }),
            None
        );
        assert_eq!(
            navigation.event(&X11Event::KeyPress {
                keycode: 42,
                state: base,
                timestamp: 20,
            }),
            None
        );
        assert!(matches!(
            pin.event(&X11Event::KeyPress {
                keycode: 58,
                state: base,
                timestamp: 30,
            }),
            Some(crate::core::Event::ToggleMenuPresentationPin)
        ));
    }

    #[test]
    fn global_pin_shortcut_rejects_unrelated_key_or_modifier() {
        let mut shortcut = GlobalPinShortcut::new(58, Some(ModMask::M2));
        let base = u16::from(ModMask::M4 | ModMask::SHIFT);
        assert_eq!(
            shortcut.event(&X11Event::KeyPress {
                keycode: 57,
                state: base,
                timestamp: 1,
            }),
            None
        );
        assert_eq!(
            shortcut.event(&X11Event::KeyPress {
                keycode: 58,
                state: base | u16::from(ModMask::CONTROL),
                timestamp: 1,
            }),
            None
        );
        assert!(!shortcut.down);
    }

    #[test]
    fn global_pin_shortcut_ignores_legacy_autorepeat_release_press_pairs() {
        let base = u16::from(ModMask::M4 | ModMask::SHIFT);
        let mut shortcut = GlobalPinShortcut::new(58, Some(ModMask::M2));
        let press = X11Event::KeyPress {
            keycode: 58,
            state: base,
            timestamp: 10,
        };
        assert!(matches!(
            shortcut.event(&press),
            Some(crate::core::Event::ToggleMenuPresentationPin)
        ));
        assert_eq!(
            shortcut.event(&X11Event::KeyRelease {
                keycode: 58,
                state: base,
                timestamp: 20,
            }),
            None
        );
        assert_eq!(
            shortcut.event(&X11Event::KeyPress {
                keycode: 58,
                state: base,
                timestamp: 20,
            }),
            None
        );
        assert!(shortcut.down);
        assert_eq!(
            shortcut.event(&X11Event::KeyRelease {
                keycode: 58,
                state: 0,
                timestamp: 30,
            }),
            None
        );
        assert!(matches!(
            shortcut.event(&X11Event::KeyPress {
                keycode: 58,
                state: base,
                timestamp: 40,
            }),
            Some(crate::core::Event::ToggleMenuPresentationPin)
        ));
    }

    #[test]
    fn passive_grab_installation_rolls_back_exactly_the_variants_already_owned() {
        let variants = [
            ModMask::M4 | ModMask::SHIFT,
            ModMask::M4 | ModMask::LOCK,
            ModMask::M2,
        ];
        let mut attempted = Vec::new();
        let mut rolled_back = Vec::new();
        let result = install_passive_grabs(
            &variants,
            |modifier| {
                attempted.push(modifier);
                (modifier != ModMask::M2).then_some(()).ok_or("BadAccess")
            },
            |modifier| rolled_back.push(modifier),
        );
        assert_eq!(result, Err("BadAccess"));
        assert_eq!(attempted, variants);
        assert_eq!(rolled_back, variants[..2]);
    }

    #[test]
    fn passive_grab_first_failure_is_non_owning_and_a_missing_keycode_cannot_be_grabbed() {
        let variants = [ModMask::M4 | ModMask::SHIFT];
        let mut rolled_back = Vec::new();
        let result = install_passive_grabs(
            &variants,
            |_| Err::<(), _>("BadAccess"),
            |modifier| rolled_back.push(modifier),
        );
        assert_eq!(result, Err("BadAccess"));
        assert!(rolled_back.is_empty());
        assert!(GlobalPinShortcut::from_keycode(None, Some(ModMask::M2)).is_none());
    }

    #[test]
    fn passive_grab_installation_records_every_variant_only_after_full_success() {
        let variants = [ModMask::M4 | ModMask::SHIFT, ModMask::M4 | ModMask::LOCK];
        let mut rolled_back = Vec::new();
        let result = install_passive_grabs(
            &variants,
            |_| Ok::<_, ()>(()),
            |modifier| rolled_back.push(modifier),
        );
        assert_eq!(result, Ok(variants.to_vec()));
        assert!(rolled_back.is_empty());
    }

    #[test]
    fn blur_behind_rect_uses_the_actual_surface_dimensions() {
        assert_eq!(
            blur_behind_rect(SurfaceWindowGeometry {
                x: 0,
                y: 0,
                width: 1920,
                height: 26,
                border_width: 0,
            }),
            [0, 0, 1920, 26]
        );
        assert_eq!(
            blur_behind_rect(SurfaceWindowGeometry {
                x: -12,
                y: 34,
                width: 517,
                height: 93,
                border_width: 1,
            }),
            [0, 0, 517, 93]
        );
    }

    #[test]
    fn surface_window_default_border_pixel_is_injected_only_when_unspecified() {
        let default_attributes = super::with_default_border_pixel(
            xproto::CreateWindowAux::new()
                .background_pixel(0x1122_3344)
                .event_mask(EventMask::EXPOSURE)
                .colormap(0x55),
        );
        assert_eq!(default_attributes.border_pixel, Some(0));
        assert_eq!(default_attributes.border_pixmap, None);

        let explicit_pixel = super::with_default_border_pixel(
            xproto::CreateWindowAux::new().border_pixel(0x1234_5678),
        );
        assert_eq!(explicit_pixel.border_pixel, Some(0x1234_5678));
        assert_eq!(explicit_pixel.border_pixmap, None);

        let explicit_pixmap =
            super::with_default_border_pixel(xproto::CreateWindowAux::new().border_pixmap(0x77));
        assert_eq!(explicit_pixmap.border_pixel, None);
        assert_eq!(explicit_pixmap.border_pixmap, Some(0x77));
    }

    #[test]
    fn network_effect_owner_is_the_dock_for_its_own_output() {
        let windows = [
            BarWindow {
                output: crate::core::OutputId(1),
                window: 0x400_002,
                backing: None,
            },
            BarWindow {
                output: crate::core::OutputId(2),
                window: 0x400_003,
                backing: None,
            },
        ];
        assert_eq!(
            popup_effect_owner(&windows, crate::core::OutputId(2)),
            Some(0x400_003)
        );
        assert_eq!(popup_effect_owner(&windows, crate::core::OutputId(3)), None);
    }

    #[test]
    fn effect_owner_property_contains_exactly_one_dock_xid() {
        assert_eq!(effect_owner_property_value(0x400_003), [0x400_003]);
        assert_eq!(effect_owner_property_value(0x400_003).len(), 1);
    }

    #[test]
    fn attention_net_wm_state_bad_window_is_window_gone() {
        assert!(matches!(
            classify_attention_property_reply::<()>(
                0x03a0_0006,
                Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
            ),
            Ok(AttentionPropertyRead::WindowGone),
        ));
    }

    #[test]
    fn attention_wm_hints_bad_window_is_window_gone() {
        assert!(matches!(
            classify_attention_property_reply::<()>(
                0x03a0_0006,
                Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
            ),
            Ok(AttentionPropertyRead::WindowGone),
        ));
    }

    #[test]
    fn attention_wm_hints_disappearance_after_state_success_discards_snapshot() {
        assert!(matches!(
            classify_attention_property_reply(0x03a0_0006, Ok(())),
            Ok(AttentionPropertyRead::Value(())),
        ));
        assert!(matches!(
            classify_attention_property_reply::<()>(
                0x03a0_0006,
                Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
            ),
            Ok(AttentionPropertyRead::WindowGone),
        ));
    }

    #[test]
    fn attention_unrelated_x11_error_is_propagated() {
        assert!(classify_attention_property_reply::<()>(
            0x03a0_0006,
            Err(x11_error(ErrorKind::Match, 0x03a0_0006)),
        )
        .is_err());
        assert!(classify_attention_property_reply::<()>(
            0x03a0_0006,
            Err(x11_error(ErrorKind::Window, 0x03a0_0007)),
        )
        .is_err());
    }

    #[test]
    fn property_notify_stale_attention_window_produces_no_attention_value() {
        let result = classify_attention_property_reply::<bool>(
            0x03a0_0006,
            Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
        )
        .expect("a stale attention window is non-fatal");
        assert!(matches!(result, AttentionPropertyRead::WindowGone));
    }

    #[test]
    fn attention_discovery_skips_a_stale_window_and_keeps_later_clients() {
        let reads = [
            classify_attention_property_reply::<u32>(
                0x03a0_0006,
                Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
            )
            .expect("stale client is non-fatal"),
            classify_attention_property_reply(0x03a0_0007, Ok(7_u32))
                .expect("live client remains readable"),
        ];
        let values = reads
            .into_iter()
            .filter_map(|read| match read {
                AttentionPropertyRead::Value(value) => Some(value),
                AttentionPropertyRead::WindowGone => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(values, vec![7]);
    }

    #[test]
    fn attention_window_gone_does_not_blacklist_a_reused_xid() {
        assert!(matches!(
            classify_attention_property_reply::<()>(
                0x03a0_0006,
                Err(x11_error(ErrorKind::Window, 0x03a0_0006)),
            ),
            Ok(AttentionPropertyRead::WindowGone),
        ));
        assert!(matches!(
            classify_attention_property_reply(0x03a0_0006, Ok(42_u32)),
            Ok(AttentionPropertyRead::Value(42)),
        ));
    }

    #[test]
    fn render_targets_merge_without_losing_required_scope() {
        assert_eq!(
            RenderTarget::Popup.merge(RenderTarget::Popup),
            RenderTarget::Popup
        );
        assert_eq!(
            RenderTarget::Dock.merge(RenderTarget::Dock),
            RenderTarget::Dock
        );
        assert_eq!(RenderTarget::Dock.merge(RenderTarget::Popup).0, 511);
        assert_eq!(
            RenderTarget::DockRight.merge(RenderTarget::Popup),
            RenderTarget::DockRightPopup
        );
        assert_eq!(
            RenderTarget::Popup.merge(RenderTarget::DockRight),
            RenderTarget::DockRightPopup
        );
        assert_eq!(
            RenderTarget::All.merge(RenderTarget::Popup),
            RenderTarget::All
        );
    }

    #[test]
    fn focus_change_does_not_touch_pluginzone() {
        assert!(!RenderTarget::DockContext.contains(RenderTarget::PLUGIN_ZONE));
    }

    #[test]
    fn focus_change_does_not_touch_right_status() {
        for region in [
            RenderTarget::TRAY,
            RenderTarget::NETWORK,
            RenderTarget::BLUETOOTH,
            RenderTarget::AUDIO,
            RenderTarget::DATETIME,
        ] {
            assert!(!RenderTarget::DockContext.contains(region));
        }
    }

    #[test]
    fn global_menu_change_draws_only_global_menu() {
        assert_eq!(RenderTarget::DockContext.0, RenderTarget::CONTEXT);
        assert!(!RenderTarget::DockContext.contains(RenderTarget::PLUGIN_ZONE));
        assert!(!RenderTarget::DockContext.contains(RenderTarget::WORKSPACES));
    }

    #[test]
    fn workspace_change_draws_only_workspaces() {
        assert_eq!(RenderTarget::Workspaces.0, RenderTarget::WORKSPACES);
        assert!(!RenderTarget::Workspaces.contains(RenderTarget::CONTEXT));
        assert!(!RenderTarget::Workspaces.contains(RenderTarget::PLUGIN_ZONE));
    }

    #[test]
    fn plugin_visual_change_draws_only_pluginzone() {
        assert_eq!(RenderTarget::PluginZone.0, RenderTarget::PLUGIN_ZONE);
    }

    #[test]
    fn plugin_metadata_only_change_draws_nothing() {
        assert_eq!(RenderTarget(0).0, 0);
    }

    #[test]
    fn plugin_geometry_change_draws_pluginzone() {
        assert_eq!(RenderTarget::PluginZone.0, RenderTarget::PLUGIN_ZONE);
    }

    #[test]
    fn tray_change_draws_only_tray() {
        assert_eq!(RenderTarget::Tray.0, RenderTarget::TRAY);
    }

    #[test]
    fn network_change_draws_only_network() {
        assert_eq!(RenderTarget::Network.0, RenderTarget::NETWORK);
    }

    #[test]
    fn bluetooth_change_draws_only_bluetooth() {
        assert_eq!(RenderTarget::Bluetooth.0, RenderTarget::BLUETOOTH);
    }

    #[test]
    fn audio_change_draws_only_audio() {
        assert_eq!(RenderTarget::Audio.0, RenderTarget::AUDIO);
    }

    #[test]
    fn datetime_change_draws_only_datetime() {
        assert_eq!(RenderTarget::DateTime.0, RenderTarget::DATETIME);
    }

    #[test]
    fn unrelated_region_change_does_not_touch_pluginzone() {
        for target in [
            RenderTarget::Workspaces,
            RenderTarget::DockContext,
            RenderTarget::Tray,
            RenderTarget::Network,
            RenderTarget::Bluetooth,
            RenderTarget::Audio,
            RenderTarget::DateTime,
        ] {
            assert!(!target.contains(RenderTarget::PLUGIN_ZONE));
        }
    }

    #[test]
    fn structural_full_redraw_still_draws_all_required_regions() {
        assert!(RenderTarget::Dock.is_full_dock());
        for region in [
            RenderTarget::WORKSPACES,
            RenderTarget::CONTEXT,
            RenderTarget::PLUGIN_ZONE,
            RenderTarget::TRAY,
            RenderTarget::NETWORK,
            RenderTarget::BLUETOOTH,
            RenderTarget::AUDIO,
            RenderTarget::DATETIME,
        ] {
            assert!(RenderTarget::Dock.contains(region));
        }
    }

    #[test]
    fn right_cluster_geometry_change_invalidates_only_affected_old_new_rects() {
        let target = RenderTarget::DockRight;
        assert!(target.contains(RenderTarget::PLUGIN_ZONE));
        assert!(target.contains(RenderTarget::TRAY));
        assert!(target.contains(RenderTarget::NETWORK));
        assert!(target.contains(RenderTarget::BLUETOOTH));
        assert!(target.contains(RenderTarget::AUDIO));
        assert!(target.contains(RenderTarget::DATETIME));
        assert!(!target.contains(RenderTarget::WORKSPACES));
        assert!(!target.contains(RenderTarget::CONTEXT));
    }

    #[test]
    fn owned_windows_are_not_gmenu_candidates() {
        let docks = [20];
        let popups = [30];
        assert!(is_xbar_owned_window(
            1,
            1,
            Some(10),
            docks.into_iter(),
            popups.into_iter()
        ));
        assert!(is_xbar_owned_window(
            10,
            1,
            Some(10),
            [].into_iter(),
            [].into_iter()
        ));
        assert!(is_xbar_owned_window(
            20,
            1,
            None,
            docks.into_iter(),
            [].into_iter()
        ));
        assert!(is_xbar_owned_window(
            30,
            1,
            None,
            [].into_iter(),
            popups.into_iter()
        ));
        assert!(!is_xbar_owned_window(
            40,
            1,
            Some(10),
            docks.into_iter(),
            popups.into_iter()
        ));
    }

    #[test]
    fn tray_hit_returns_only_visible_item_endpoint() {
        let endpoint = StatusNotifierEndpoint {
            service: ":1.2".into(),
            object_path: "/StatusNotifierItem".into(),
        };
        let item = TrayVisualItem {
            endpoint: endpoint.clone(),
            icon: StatusNotifierIcon::Pixmap {
                width: 1,
                height: 1,
                argb: vec![0xffff_ffff],
            },
            render_mode: TrayIconRenderMode::Template,
            rect: MenuRect {
                x: 100,
                y: 0,
                width: 20,
                height: 26,
            },
        };
        assert_eq!(tray_hit(&[item], 110, 12), Some(endpoint));
        assert_eq!(tray_hit(&[], 110, 12), None);
        assert_eq!(tray_hit(&[], 110, 12), None);
    }

    #[test]
    fn template_icon_rendering_uses_bar_foreground_and_preserves_alpha_mask() {
        assert_eq!(template_icon_pixel(0x0000_00ff, 0xe6eaf0, 0x20242b), None);
        assert_eq!(
            template_icon_pixel(0xff00_00ff, 0xe6eaf0, 0x20242b),
            Some(0xe6eaf0)
        );
        assert_eq!(
            template_icon_pixel(0x8000_00ff, 0xffffff, 0x000000),
            Some(0x808080)
        );
    }

    #[test]
    fn template_icon_rendering_ignores_source_rgb_for_equal_alpha() {
        assert_eq!(
            template_icon_pixel(0x7f00_0000, 0x123456, 0x20242b),
            template_icon_pixel(0x7fffffff, 0x123456, 0x20242b)
        );
    }

    #[test]
    fn preserve_color_rendering_keeps_source_rgb() {
        assert_eq!(preserve_color_pixel(0x8012_3456), Some(0x123456));
        assert_eq!(preserve_color_pixel(0x0012_3456), None);
    }

    #[test]
    fn tray_icons_are_smaller_but_keep_aspect_ratio() {
        assert_eq!(tray_draw_size(16, 16), (14, 14));
        assert_eq!(tray_draw_size(32, 16), (14, 7));
        assert_eq!(tray_draw_size(8, 16), (7, 14));
    }

    #[test]
    fn popup_hover_uses_only_existing_interactive_hit_targets() {
        assert_eq!(
            popup_hover_for(Some(&HitTarget::AudioOutputDevice("sink.a".into()))),
            Some(PopupHover::AudioOutputDevice("sink.a".into()))
        );
        assert_eq!(
            popup_hover_for(Some(&HitTarget::NetworkWireless)),
            Some(PopupHover::NetworkWireless)
        );
        assert_eq!(
            popup_hover_for(Some(&HitTarget::BluetoothPower)),
            Some(PopupHover::BluetoothPower)
        );
        assert_eq!(
            popup_hover_for(Some(&HitTarget::BluetoothDevice("device.a".into()))),
            Some(PopupHover::BluetoothDevice("device.a".into()))
        );
        assert_eq!(
            popup_hover_for(Some(&HitTarget::Item(vec![crate::core::MenuItemId(7)]))),
            Some(PopupHover::MenuItem(crate::core::MenuItemId(7)))
        );
        assert_eq!(popup_hover_for(Some(&HitTarget::AudioInside)), None);
        assert_eq!(popup_hover_for(Some(&HitTarget::NetworkInside)), None);
    }

    fn menu_popup(window: u32, parent_id: i32, item_ids: &[i32]) -> PopupWindow {
        PopupWindow {
            window,
            layout: PopupLayout {
                parent_id: MenuItemId(parent_id),
                rect: MenuRect {
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                items: item_ids
                    .iter()
                    .map(|id| PopupItemRect {
                        id: MenuItemId(*id),
                        rect: MenuRect {
                            x: 0,
                            y: 0,
                            width: 100,
                            height: 20,
                        },
                        label: String::new(),
                        enabled: true,
                        separator: false,
                        has_submenu: false,
                        shortcut: None,
                    })
                    .collect(),
            },
            backing: None,
        }
    }

    #[test]
    fn menu_popup_hover_marks_only_the_structural_slot_at_each_depth() {
        let popups = vec![
            menu_popup(10, 1, &[11, 12]),
            menu_popup(20, 12, &[21, 22]),
            menu_popup(30, 22, &[31, 32]),
        ];
        assert_eq!(
            menu_popup_slots_for_item(&popups, MenuItemId(11)),
            vec![PopupSlot(0)]
        );
        assert_eq!(
            menu_popup_slots_for_item(&popups, MenuItemId(21)),
            vec![PopupSlot(1)]
        );
        assert_eq!(
            menu_popup_slots_for_item(&popups, MenuItemId(31)),
            vec![PopupSlot(2)]
        );

        for (old_item, new_item, slot) in [(11, 12, 0), (21, 22, 1), (31, 32, 2)] {
            assert_eq!(
                menu_popup_dirty_for_interaction_change(
                    &popups,
                    Some(MenuItemId(1)),
                    &[MenuItemId(1), MenuItemId(12), MenuItemId(22)],
                    &[MenuItemId(old_item)],
                    Some(MenuItemId(1)),
                    &[MenuItemId(1), MenuItemId(12), MenuItemId(22)],
                    &[MenuItemId(new_item)],
                ),
                MenuPopupDirty::Specific(std::collections::HashSet::from([PopupSlot(slot)])),
            );
        }
    }

    #[test]
    fn menu_popup_cross_popup_and_outside_hover_merge_affected_slots() {
        let popups = vec![menu_popup(10, 1, &[11]), menu_popup(20, 11, &[21])];
        assert_eq!(
            menu_popup_dirty_for_interaction_change(
                &popups,
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[MenuItemId(11)],
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[MenuItemId(21)],
            ),
            MenuPopupDirty::Specific(std::collections::HashSet::from([
                PopupSlot(0),
                PopupSlot(1),
            ])),
        );
        assert_eq!(
            menu_popup_dirty_for_interaction_change(
                &popups,
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[MenuItemId(21)],
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[],
            ),
            MenuPopupDirty::Specific(std::collections::HashSet::from([PopupSlot(1)])),
        );
    }

    #[test]
    fn popup_selection_uses_slots_not_provider_item_ids() {
        let popups = vec![
            menu_popup(10, 1, &[7]),
            // Provider IDs may collide across layouts; the structural slots
            // remain independent and select both affected rendered frames.
            menu_popup(20, 7, &[7]),
            menu_popup(30, 7, &[9]),
        ];
        assert_eq!(
            menu_popup_slots_for_item(&popups, MenuItemId(7)),
            vec![PopupSlot(0), PopupSlot(1)]
        );
        let selected = |dirty: &MenuPopupDirty| {
            (0..popups.len())
                .map(PopupSlot)
                .filter(|slot| popup_slot_is_selected(dirty, *slot))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            selected(&MenuPopupDirty::Specific(std::collections::HashSet::from(
                [PopupSlot(1),]
            ))),
            vec![PopupSlot(1)]
        );
        assert_eq!(
            selected(&MenuPopupDirty::Specific(std::collections::HashSet::from(
                [PopupSlot(0), PopupSlot(2),]
            ))),
            vec![PopupSlot(0), PopupSlot(2)]
        );
        assert_eq!(
            selected(&MenuPopupDirty::All),
            vec![PopupSlot(0), PopupSlot(1), PopupSlot(2)]
        );
    }

    #[test]
    fn popup_target_without_menu_plan_selects_no_global_menu_windows() {
        let selected = |dirty: &MenuPopupDirty| {
            (0..3)
                .map(PopupSlot)
                .filter(|slot| popup_slot_is_selected(dirty, *slot))
                .collect::<Vec<_>>()
        };
        assert!(RenderTarget::Popup.contains(RenderTarget::POPUP));
        assert_eq!(selected(&MenuPopupDirty::None), Vec::<PopupSlot>::new());
    }

    #[test]
    fn menu_popup_structure_change_overrides_specific_dirty_ownership() {
        let popups = vec![menu_popup(10, 1, &[11]), menu_popup(20, 11, &[21])];
        assert_eq!(
            menu_popup_dirty_for_interaction_change(
                &popups,
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[MenuItemId(21)],
                Some(MenuItemId(1)),
                &[MenuItemId(1)],
                &[MenuItemId(11)],
            ),
            MenuPopupDirty::All,
        );
    }

    #[test]
    fn menu_popup_root_switch_invalidates_all_structural_slots() {
        let popups = vec![menu_popup(10, 1, &[11]), menu_popup(20, 11, &[21])];
        assert_eq!(
            menu_popup_dirty_for_interaction_change(
                &popups,
                Some(MenuItemId(1)),
                &[MenuItemId(1), MenuItemId(11)],
                &[MenuItemId(21)],
                Some(MenuItemId(2)),
                &[MenuItemId(2)],
                &[],
            ),
            MenuPopupDirty::All,
        );
    }

    #[test]
    fn menu_popup_expose_and_merged_causes_preserve_specific_window_slots() {
        let popups = vec![menu_popup(10, 1, &[11]), menu_popup(20, 11, &[21])];
        assert_eq!(menu_popup_slot_for_window(&popups, 20), Some(PopupSlot(1)));
        assert_eq!(menu_popup_slot_for_window(&popups, 99), None);

        let mut dirty = MenuPopupDirty::None;
        dirty.mark(PopupSlot(0));
        dirty.merge(MenuPopupDirty::Specific(std::collections::HashSet::from([
            PopupSlot(1),
        ])));
        assert!(dirty.renders(PopupSlot(0)));
        assert!(dirty.renders(PopupSlot(1)));
        assert!(!dirty.renders(PopupSlot(2)));
        dirty.mark_all();
        assert!(dirty.renders(PopupSlot(2)));
    }

    #[test]
    fn switch_thumb_tracks_binary_state_inside_the_canonical_target() {
        let target = MenuRect {
            x: 100,
            y: 20,
            width: 46,
            height: 22,
        };
        let off = super::X11Platform::switch_thumb_rect(target, false);
        let on = super::X11Platform::switch_thumb_rect(target, true);

        assert_eq!(
            off,
            MenuRect {
                x: 103,
                y: 23,
                width: 16,
                height: 16
            }
        );
        assert_eq!(
            on,
            MenuRect {
                x: 127,
                y: 23,
                width: 16,
                height: 16
            }
        );
        for thumb in [off, on] {
            assert!(thumb.x >= target.x);
            assert!(thumb.y >= target.y);
            assert!(thumb.x + thumb.width as i16 <= target.x + target.width as i16);
            assert!(thumb.y + thumb.height as i16 <= target.y + target.height as i16);
        }
    }

    #[test]
    fn same_popup_hover_target_is_a_render_no_op() {
        let old = Some(PopupHover::AudioOutputDevice("sink.a".into()));
        let (next, changed) =
            popup_hover_transition(&old, Some(&HitTarget::AudioOutputDevice("sink.a".into())));
        assert_eq!(next, old);
        assert!(!changed);
    }

    #[test]
    fn network_primary_label_keeps_only_the_compact_name_and_active_marker() {
        assert_eq!(
            network_primary_row_label("Guest 5 GHz", false),
            "  Guest 5 GHz"
        );
        assert_eq!(
            network_primary_row_label("Guest 5 GHz", true),
            "● Guest 5 GHz"
        );
    }

    #[test]
    fn popup_backing_reuses_only_matching_geometry_and_depth() {
        let backing = Some(PopupBacking {
            pixmap: 1,
            gc: 2,
            width: 340,
            height: 400,
            depth: 32,
        });
        assert!(backing_matches(backing, 340, 400, 32));
        assert!(!backing_matches(backing, 341, 400, 32));
        assert!(!backing_matches(backing, 340, 401, 32));
        assert!(!backing_matches(backing, 340, 400, 24));
        assert!(!backing_matches(None, 340, 400, 32));
    }

    #[test]
    fn bar_backing_reuses_only_matching_geometry_and_depth() {
        let backing = Some(BarBacking {
            pixmap: 1,
            gc: 2,
            width: 1920,
            height: BAR_HEIGHT,
            depth: 32,
        });

        assert!(bar_backing_matches(backing, 1920, BAR_HEIGHT, 32));
        assert!(!bar_backing_matches(backing, 1919, BAR_HEIGHT, 32));
        assert!(!bar_backing_matches(backing, 1920, BAR_HEIGHT - 1, 32));
        assert!(!bar_backing_matches(backing, 1920, BAR_HEIGHT, 24));
        assert!(!bar_backing_matches(None, 1920, BAR_HEIGHT, 32));
    }

    #[test]
    fn bar_backings_are_independent_per_window() {
        let first = BarBacking {
            pixmap: 1,
            gc: 2,
            width: 1920,
            height: BAR_HEIGHT,
            depth: 32,
        };
        let second = BarBacking {
            pixmap: 3,
            gc: 4,
            width: 1280,
            height: BAR_HEIGHT,
            depth: 32,
        };

        assert_ne!(first.pixmap, second.pixmap);
        assert_ne!(first.gc, second.gc);
    }

    #[test]
    fn context_dirty_union_covers_old_and_new_geometry() {
        let old = MenuRect {
            x: 100,
            y: 0,
            width: 600,
            height: BAR_HEIGHT,
        };
        let smaller = MenuRect {
            x: 100,
            y: 0,
            width: 400,
            height: BAR_HEIGHT,
        };
        let shifted = MenuRect {
            x: 300,
            y: 0,
            width: 600,
            height: BAR_HEIGHT,
        };

        assert_eq!(union_menu_rects(&[old, old]), Some(old));
        assert_eq!(union_menu_rects(&[old, smaller]), Some(old));
        assert_eq!(
            union_menu_rects(&[old, shifted]),
            Some(MenuRect {
                x: 100,
                y: 0,
                width: 800,
                height: BAR_HEIGHT,
            })
        );
    }

    #[test]
    fn popup_backings_are_independent_per_window() {
        let root_backing = PopupBacking {
            pixmap: 1,
            gc: 2,
            width: 340,
            height: 400,
            depth: 32,
        };
        let submenu_backing = PopupBacking {
            pixmap: 3,
            gc: 4,
            width: 280,
            height: 320,
            depth: 32,
        };

        assert_ne!(root_backing.pixmap, submenu_backing.pixmap);
        assert_ne!(root_backing.gc, submenu_backing.gc);
    }

    #[test]
    fn notification_hover_transitions_dirty_only_on_identity_change() {
        let a = crate::core::HistoryEntryId(1);
        let b = crate::core::HistoryEntryId(2);
        assert!(notification_hover_transition(None, Some(a)).1);
        assert!(!notification_hover_transition(Some(a), Some(a)).1);
        assert!(notification_hover_transition(Some(a), Some(b)).1);
        assert!(notification_hover_transition(Some(a), None).1);
        assert!(!notification_hover_transition(None, None).1);
    }

    #[test]
    fn notification_card_rectangles_are_canonical_and_empty_has_none() {
        let rect = MenuRect {
            x: 4,
            y: 4,
            width: 352,
            height: 54,
        };
        let cards = [(crate::core::HistoryEntryId(1), rect)];
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].1, rect);
        let empty: Vec<(crate::core::HistoryEntryId, MenuRect)> = Vec::new();
        assert!(empty.is_empty());
    }

    #[test]
    fn notification_indicator_exists_for_empty_and_non_empty_history() {
        let output = OutputState {
            id: OutputId(7),
            name: "HDMI-1".into(),
            x: 100,
            y: 20,
            width: 1920,
            height: 1080,
        };
        let rect = notification_indicator_rect(&output);
        let indicators = vec![(42, output.id, rect)];
        assert_eq!(rect, notification_indicator_rect(&output));
        assert_eq!(
            notification_indicator_hit(&indicators, 42, rect.x + 1, rect.y + 1),
            Some(HitTarget::NotificationCenter(output.id))
        );
        // The affordance is independent of history population.
        let empty_history: Vec<crate::core::NotificationHistoryEntry> = Vec::new();
        assert!(empty_history.is_empty());
        assert!(notification_indicator_hit(&indicators, 42, rect.x + 1, rect.y + 1).is_some());
    }

    #[test]
    fn notification_indicator_hit_carries_each_originating_output_id() {
        let output_a = OutputState {
            id: OutputId(1),
            name: "A".into(),
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        };
        let output_b = OutputState {
            id: OutputId(2),
            name: "B".into(),
            x: 800,
            y: 0,
            width: 800,
            height: 600,
        };
        let rect_a = notification_indicator_rect(&output_a);
        let rect_b = notification_indicator_rect(&output_b);
        let indicators = vec![(11, output_a.id, rect_a), (22, output_b.id, rect_b)];
        assert_eq!(
            notification_indicator_hit(&indicators, 11, rect_a.x + 2, rect_a.y + 2),
            Some(HitTarget::NotificationCenter(output_a.id))
        );
        assert_eq!(
            notification_indicator_hit(&indicators, 22, rect_b.x + 2, rect_b.y + 2),
            Some(HitTarget::NotificationCenter(output_b.id))
        );
        assert_eq!(rect_a, notification_indicator_rect(&output_a));
        assert_eq!(rect_b, notification_indicator_rect(&output_b));
    }

    fn test_history(ids: &[u32]) -> Vec<crate::core::NotificationHistoryEntry> {
        ids.iter()
            .enumerate()
            .map(|(order, id)| crate::core::NotificationHistoryEntry {
                id: crate::core::HistoryEntryId(u64::from(*id)),
                live_notification_id: None,
                source: crate::core::NotificationSource::Freedesktop,
                app_name: String::new(),
                summary: String::new(),
                body: String::new(),
                order: order as u64,
            })
            .collect()
    }

    #[test]
    fn notification_scroll_is_card_aligned_and_clamped() {
        assert_eq!(notification_scroll_target(0, 4, 5, 3), 0);
        assert_eq!(notification_scroll_target(0, 5, 5, 3), 1);
        assert_eq!(notification_scroll_target(1, 5, 5, 3), 2);
        assert_eq!(notification_scroll_target(2, 5, 5, 3), 2);
        assert_eq!(notification_scroll_target(2, 4, 5, 3), 1);
        assert_eq!(notification_scroll_target(0, 4, 0, 3), 0);
        assert_eq!(notification_scroll_target(0, 5, 2, 3), 0);
    }

    #[test]
    fn notification_scroll_visible_slice_contains_only_complete_entries() {
        let history = test_history(&[1, 2, 3, 4, 5]);
        let visible = |scroll| {
            history
                .iter()
                .skip(scroll)
                .take(3)
                .map(|entry| entry.id.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(0), vec![1, 2, 3]);
        assert_eq!(visible(1), vec![2, 3, 4]);
        assert_eq!(visible(2), vec![3, 4, 5]);
    }

    #[test]
    fn notification_scroll_preserves_first_visible_anchor_on_history_changes() {
        let history = test_history(&[1, 2, 3, 4, 5]);
        let anchor = Some(crate::core::HistoryEntryId(3));
        assert_eq!(
            reconcile_notification_scroll(2, anchor, &test_history(&[9, 1, 2, 3, 4, 5]), 3),
            3
        );
        assert_eq!(
            reconcile_notification_scroll(2, anchor, &test_history(&[3, 1, 2, 4, 5]), 3),
            0
        );
        assert_eq!(
            reconcile_notification_scroll(2, anchor, &test_history(&[1, 3, 4, 5]), 3),
            1
        );
        assert_eq!(
            reconcile_notification_scroll(2, anchor, &test_history(&[1, 2, 4, 5]), 3),
            1
        );
        assert_eq!(history[2].id, crate::core::HistoryEntryId(3));
    }

    #[test]
    fn notification_scroll_clamps_when_capacity_changes_or_history_is_empty() {
        let history = test_history(&[1, 2, 3, 4, 5]);
        assert_eq!(reconcile_notification_scroll(4, None, &history, 3), 2);
        assert_eq!(reconcile_notification_scroll(2, None, &history, 5), 0);
        assert_eq!(reconcile_notification_scroll(2, None, &Vec::new(), 3), 0);
    }

    #[test]
    fn notification_scroll_resets_on_reopen_or_output_switch() {
        assert_eq!(notification_previous_scroll(true, 2), 2);
        assert_eq!(notification_previous_scroll(false, 2), 0);
        assert_eq!(notification_previous_scroll(false, 0), 0);
    }

    #[test]
    fn notification_center_wheel_dispatch_preserves_vertical_buttons() {
        assert_eq!(notification_wheel_direction(4), Some(-1));
        assert_eq!(notification_wheel_direction(5), Some(1));
        assert_eq!(notification_wheel_direction(1), None);
        assert_eq!(notification_wheel_direction(6), None);
    }
}
