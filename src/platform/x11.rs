use crate::core::{
    GtkMenuEndpoint, NetworkWifiTarget, OutputId, OutputState, State, StatusNotifierEndpoint,
    WindowId,
};
use crate::ui::style::{self, TextMeasurer, BAR_STYLE};
use crate::ui::{layout, view};
use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as XprotoExt, EventMask, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as WrapperExt;
use x11rb::xcb_ffi::XCBConnection;

use super::x11_text::X11Text;

fn trace_x11_resource(event: &str, role: &str, xid: u32) {
    if std::env::var_os("XBAR_TRACE_XFT").is_some() {
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let _ = writeln!(stderr, "xbar xft: {event} role={role} xid=0x{xid:x}");
        let _ = stderr.flush();
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
    conn: XCBConnection,
    root: u32,
    atoms: Atoms,
    text: X11Text,
    instance_window: Option<u32>,
    windows: Vec<BarWindow>,
    popups: Vec<PopupWindow>,
    audio_popup: Option<AudioPopupWindow>,
    bluetooth_popup: Option<BluetoothPopupWindow>,
    network_popup: Option<NetworkPopupWindow>,
    notification: Option<NotificationWindow>,
    pointer_grabbed: bool,
    bar_hits: Vec<BarHitMap>,
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
}
struct BarWindow {
    output: OutputId,
    window: u32,
}
struct PopupWindow {
    window: u32,
    layout: layout::PopupLayout,
}
struct AudioPopupWindow {
    window: u32,
    rect: layout::MenuRect,
    track: layout::MenuRect,
    mute: layout::MenuRect,
    input_track: layout::MenuRect,
    input_mute: layout::MenuRect,
    output_devices: Vec<(String, layout::MenuRect)>,
    input_devices: Vec<(String, layout::MenuRect)>,
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
struct NotificationWindow {
    window: u32,
    width: u16,
    height: u16,
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

fn workspace_as_menu(rect: layout::WorkspaceRect) -> layout::MenuRect {
    layout::MenuRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

impl X11Platform {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (conn, screen) = XCBConnection::connect(None)?;
        let root = conn.setup().roots[screen].root;
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
        };
        let text = X11Text::open()?;
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
        Ok(Self {
            conn,
            root,
            atoms,
            text,
            instance_window: None,
            windows: Vec::new(),
            popups: Vec::new(),
            audio_popup: None,
            bluetooth_popup: None,
            network_popup: None,
            notification: None,
            pointer_grabbed: false,
            bar_hits: Vec::new(),
            previous_contexts: HashMap::new(),
        })
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
                self.attention_event(event.window)?
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

    fn attention_event(&self, window: u32) -> Result<Option<X11Event>, Box<dyn Error>> {
        if self.is_xbar_owned_window(window) {
            return Ok(None);
        }
        let states = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.net_wm_state,
                AtomEnum::ATOM,
                0,
                u32::MAX,
            )?
            .reply()?
            .value32()
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default();
        let urgency = self
            .conn
            .get_property(false, window, self.atoms.wm_hints, AtomEnum::ANY, 0, 9)?
            .reply()?
            .value32()
            .and_then(|mut values| values.next())
            .is_some_and(|flags| flags & (1 << 8) != 0);
        Ok(Some(X11Event::WindowAttentionChanged {
            window: WindowId(window),
            app_name: self.window_name(window),
            attention: states.contains(&self.atoms.demands_attention) || urgency,
        }))
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
                if let Some(event) = self.attention_event(window)? {
                    events.push(event);
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
            .reply()?;
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
    pub fn sync_windows(&mut self, outputs: &[OutputState]) -> Result<(), Box<dyn Error>> {
        self.close_popups(None)?;
        self.previous_contexts.clear();
        for old in self.windows.drain(..) {
            self.text.release_drawable(old.window);
            trace_x11_resource("WINDOW_DESTROY", "bar", old.window);
            self.conn.destroy_window(old.window)?.check()?;
        }
        for output in outputs {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "bar", window);
            self.conn
                .create_window(
                    x11rb::COPY_FROM_PARENT as u8,
                    window,
                    self.root,
                    output.x,
                    output.y,
                    output.width,
                    BAR_HEIGHT,
                    0_u16,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &xproto::CreateWindowAux::new()
                        .background_pixel(BAR_STYLE.background)
                        .event_mask(
                            EventMask::EXPOSURE
                                | EventMask::BUTTON_PRESS
                                | EventMask::POINTER_MOTION
                                | EventMask::ENTER_WINDOW
                                | EventMask::LEAVE_WINDOW,
                        ),
                )?
                .check()?;
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
            self.conn
                .change_property32(
                    xproto::PropMode::REPLACE,
                    window,
                    self.atoms.net_wm_window_opacity,
                    AtomEnum::CARDINAL,
                    &[style::opacity_cardinal(BAR_STYLE.opacity)],
                )?
                .check()?;
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
            self.reconcile_interactive_popup_surfaces(state)?;
            if !state.audio_popup_open && !state.bluetooth_popup_open && !state.network_popup_open {
                self.render_popups(state)?;
            } else {
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
        }
        if target.contains(RenderTarget::NOTIFICATION) {
            self.render_notification(state)?;
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
            self.conn
                .create_window(
                    x11rb::COPY_FROM_PARENT as u8,
                    window,
                    self.root,
                    x,
                    y,
                    width,
                    height,
                    1,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &xproto::CreateWindowAux::new()
                        .background_pixel(BAR_STYLE.popup_background)
                        .override_redirect(1)
                        .event_mask(EventMask::EXPOSURE),
                )?
                .check()?;
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
        self.notification = Some(NotificationWindow {
            window,
            width,
            height,
        });
        let geometry = self.conn.get_geometry(window)?.reply()?;
        let attrs = self.conn.get_window_attributes(window)?.reply()?;
        self.text
            .prepare_drawable("notification", window, attrs.visual, geometry.depth)?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(
                gc,
                window,
                &xproto::CreateGCAux::new().foreground(BAR_STYLE.popup_background),
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
        self.conn.poly_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width,
                height,
            }],
        )?;
        self.text
            .draw_popup_utf8(&summary, 12, 25, BAR_STYLE.popup_foreground)?;
        if !body.is_empty() {
            self.text
                .draw_popup_utf8(&body, 12, 52, BAR_STYLE.popup_foreground)?;
        }
        self.conn.free_gc(gc)?.check()?;
        Ok(())
    }

    fn render_dock(&mut self, state: &State, target: RenderTarget) -> Result<(), Box<dyn Error>> {
        self.bar_hits.clear();
        let full = target.is_full_dock();
        let draw_workspaces = full || target.contains(RenderTarget::WORKSPACES);
        let draw_context = full || target.contains(RenderTarget::CONTEXT);
        let draw_plugins = full || target.contains(RenderTarget::PLUGIN_ZONE);
        let draw_tray = full || target.contains(RenderTarget::TRAY);
        let draw_network = full || target.contains(RenderTarget::NETWORK);
        let draw_bluetooth = full || target.contains(RenderTarget::BLUETOOTH);
        let draw_audio = full || target.contains(RenderTarget::AUDIO);
        let draw_datetime = full || target.contains(RenderTarget::DATETIME);
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
        for bar in &self.windows {
            let Some(output) = state.outputs.iter().find(|output| output.id == bar.output) else {
                continue;
            };
            let geometry = self.conn.get_geometry(bar.window)?.reply()?;
            let attributes = self.conn.get_window_attributes(bar.window)?.reply()?;
            self.text
                .prepare_drawable("bar", bar.window, attributes.visual, geometry.depth)?;
            let gc = self.conn.generate_id()?;
            self.conn
                .create_gc(
                    gc,
                    bar.window,
                    &xproto::CreateGCAux::new().foreground(BAR_STYLE.background),
                )?
                .check()?;
            let previous_context = self.previous_contexts.get(&bar.window).cloned();
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
                                    .any(|o| o.id == bar.output && o.name == n)
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
            self.bar_hits.push((
                bar.window,
                bar.output,
                output.x,
                output.y,
                context.menu.clone(),
                context.tray.clone(),
                context.network.clone(),
                context.audio.clone(),
                context.bluetooth.clone(),
            ));
            if full {
                self.conn.poly_fill_rectangle(
                    bar.window,
                    gc,
                    &[xproto::Rectangle {
                        x: 0,
                        y: 0,
                        width: output.width,
                        height: BAR_HEIGHT,
                    }],
                )?;
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
                for rect in clear {
                    self.conn
                        .poly_fill_rectangle(bar.window, gc, &[x11_rect(rect, output)])?;
                }
            }
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
                    BAR_STYLE.workspace_background
                } else {
                    BAR_STYLE.background
                };
                self.conn
                    .change_gc(gc, &xproto::ChangeGCAux::new().foreground(color))?
                    .check()?;
                self.conn.poly_fill_rectangle(
                    bar.window,
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
                        &xproto::ChangeGCAux::new().foreground(BAR_STYLE.workspace_foreground),
                    )?
                    .check()?;
                self.text.draw_utf8(
                    &workspace.name,
                    x as i32 + BAR_STYLE.horizontal_padding as i32,
                    self.text.baseline(BAR_HEIGHT) as i32,
                    BAR_STYLE.workspace_foreground,
                )?;
            }
            for item in &context.menu {
                if !draw_context {
                    break;
                }
                let x = item.rect.x.saturating_sub(output.x).saturating_add(8);
                self.conn
                    .change_gc(
                        gc,
                        &xproto::ChangeGCAux::new().foreground(
                            if state.menu_interaction.hovered_path.last() == Some(&item.id) {
                                BAR_STYLE.menu_hover_foreground
                            } else if item.enabled {
                                BAR_STYLE.foreground
                            } else {
                                BAR_STYLE.menu_disabled_foreground
                            },
                        ),
                    )?
                    .check()?;
                let color = if state.menu_interaction.hovered_path.last() == Some(&item.id) {
                    BAR_STYLE.menu_hover_foreground
                } else if item.enabled {
                    BAR_STYLE.foreground
                } else {
                    BAR_STYLE.menu_disabled_foreground
                };
                self.text.draw_utf8(
                    &item.label,
                    x as i32,
                    self.text.baseline(BAR_HEIGHT) as i32,
                    color,
                )?;
            }
            if draw_context {
                if let Some(title) = &context.app_name {
                    let x = title.rect.x.saturating_sub(output.x) as i32;
                    self.text.draw_utf8(
                        &title.text,
                        x,
                        self.text.baseline(BAR_HEIGHT) as i32,
                        BAR_STYLE.foreground,
                    )?;
                }
            }
            if draw_network || draw_audio || draw_bluetooth {
                if draw_network {
                    if let Some(network) = &context.network {
                        let width = self.text.measure_status_icon_width(&network.text);
                        let x = network.rect.x.saturating_sub(output.x) as i32
                            + (network.rect.width.saturating_sub(width) / 2) as i32;
                        let metrics = self.text.status_icon_metrics();
                        let baseline = ((BAR_HEIGHT as i16 - metrics.descent + metrics.ascent) / 2)
                            .max(1) as i32;
                        self.text.draw_status_icon_utf8(
                            &network.text,
                            x,
                            baseline,
                            BAR_STYLE.foreground,
                        )?;
                    }
                }
                if draw_audio {
                    if let Some(audio) = &context.audio {
                        let width = self.text.measure_status_icon_width(&audio.text);
                        let x = audio.rect.x.saturating_sub(output.x) as i32
                            + (audio.rect.width.saturating_sub(width) / 2) as i32;
                        let metrics = self.text.status_icon_metrics();
                        let baseline = ((BAR_HEIGHT as i16 - metrics.descent + metrics.ascent) / 2)
                            .max(1) as i32;
                        self.text.draw_status_icon_utf8(
                            &audio.text,
                            x,
                            baseline,
                            BAR_STYLE.foreground,
                        )?;
                    }
                }
                if draw_bluetooth {
                    if let Some(bluetooth) = &context.bluetooth {
                        let width = self.text.measure_status_icon_width(&bluetooth.text);
                        let x = bluetooth.rect.x.saturating_sub(output.x) as i32
                            + (bluetooth.rect.width.saturating_sub(width) / 2) as i32;
                        let metrics = self.text.status_icon_metrics();
                        let baseline = ((BAR_HEIGHT as i16 - metrics.descent + metrics.ascent) / 2)
                            .max(1) as i32;
                        self.text.draw_status_icon_utf8(
                            &bluetooth.text,
                            x,
                            baseline,
                            BAR_STYLE.foreground,
                        )?;
                    }
                }
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
                let draw_width = (*width).min(tray.rect.width);
                let draw_height = (*height).min(tray.rect.height);
                let x0 = tray.rect.x.saturating_sub(output.x)
                    + ((tray.rect.width - draw_width) / 2) as i16;
                let y0 = tray.rect.y.saturating_sub(output.y)
                    + ((tray.rect.height - draw_height) / 2) as i16;
                for py in 0..draw_height {
                    for px in 0..draw_width {
                        let source_x = px * *width / draw_width.max(1);
                        let source_y = py * *height / draw_height.max(1);
                        let index = (source_y * *width + source_x) as usize;
                        let pixel = &argb[index];
                        let alpha = (pixel >> 24) as u8;
                        if alpha == 0 {
                            continue;
                        }
                        self.conn
                            .change_gc(
                                gc,
                                &xproto::ChangeGCAux::new().foreground(pixel & 0x00ff_ffff),
                            )?
                            .check()?;
                        self.conn.poly_fill_rectangle(
                            bar.window,
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
                self.text.draw_utf8(
                    &plugin.text,
                    x,
                    self.text.baseline(BAR_HEIGHT) as i32,
                    BAR_STYLE.foreground,
                )?;
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
                    self.text.draw_utf8(
                        &datetime.text,
                        x as i32,
                        self.text.baseline(BAR_HEIGHT) as i32,
                        BAR_STYLE.foreground,
                    )?;
                }
            }
            self.conn.free_gc(gc)?.check()?;
            self.previous_contexts.insert(bar.window, context);
        }
        Ok(())
    }

    fn close_popups(&mut self, state: Option<&State>) -> Result<(), Box<dyn Error>> {
        self.destroy_popup_suffix(0)?;
        if let Some(popup) = self.audio_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "audio-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: audio popup destroyed xid={}", popup.window);
            }
        }
        if let Some(popup) = self.bluetooth_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "bluetooth-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: UNMAP popup=Bluetooth xid={}", popup.window);
            }
        }
        if let Some(popup) = self.network_popup.take() {
            self.text.release_drawable(popup.window);
            trace_x11_resource("WINDOW_DESTROY", "network-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
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
        let content_height = 232 + 22 + output_count as u16 * 24 + 22 + input_count as u16 * 24 + 8;
        let popup_height = content_height
            .max(280)
            .min(output.height.saturating_sub(26).max(280));
        let rect = layout::MenuRect {
            x: output.x + output.width as i16 - 340,
            y: output.y + 26,
            width: 340,
            height: popup_height,
        };
        let track = layout::MenuRect {
            x: rect.x + 60,
            y: rect.y + 56,
            width: 240,
            height: 22,
        };
        let mute = layout::MenuRect {
            x: rect.x + 14,
            y: rect.y + 28,
            width: 46,
            height: 48,
        };
        let input_track = layout::MenuRect {
            x: rect.x + 60,
            y: rect.y + 152,
            width: 240,
            height: 22,
        };
        let input_mute = layout::MenuRect {
            x: rect.x + 14,
            y: rect.y + 112,
            width: 46,
            height: 48,
        };
        let output_devices = state
            .audio
            .outputs
            .iter()
            .take(8)
            .enumerate()
            .map(|(index, device)| {
                (
                    device.name.clone(),
                    layout::MenuRect {
                        x: rect.x + 14,
                        y: rect.y + 254 + index as i16 * 24,
                        width: rect.width - 28,
                        height: 24,
                    },
                )
            })
            .collect::<Vec<_>>();
        let input_start = 278 + output_count as i16 * 24;
        let input_devices = state
            .audio
            .inputs
            .iter()
            .take(8)
            .enumerate()
            .map(|(index, device)| {
                (
                    device.name.clone(),
                    layout::MenuRect {
                        x: rect.x + 14,
                        y: rect.y + input_start + index as i16 * 24,
                        width: rect.width - 28,
                        height: 24,
                    },
                )
            })
            .collect::<Vec<_>>();
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: audio device layout outputs={output_devices:?} inputs={input_devices:?}");
        }
        let window = if let Some(popup) = &self.audio_popup {
            popup.window
        } else {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "audio-popup", window);
            self.conn
                .create_window(
                    x11rb::COPY_FROM_PARENT as u8,
                    window,
                    self.root,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    1,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &xproto::CreateWindowAux::new()
                        .background_pixel(BAR_STYLE.popup_background)
                        .override_redirect(1)
                        .event_mask(
                            EventMask::EXPOSURE
                                | EventMask::BUTTON_PRESS
                                | EventMask::BUTTON_RELEASE
                                | EventMask::POINTER_MOTION,
                        ),
                )?
                .check()?;
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
        if self.audio_popup.is_some() && needs_resize {
            self.conn
                .configure_window(
                    window,
                    &xproto::ConfigureWindowAux::new()
                        .width(rect.width as u32)
                        .height(rect.height as u32),
                )?
                .check()?;
        }
        let geometry = self.conn.get_geometry(window)?.reply()?;
        let attributes = self.conn.get_window_attributes(window)?.reply()?;
        self.text
            .prepare_drawable("audio-popup", window, attributes.visual, geometry.depth)?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(
                gc,
                window,
                &xproto::CreateGCAux::new().foreground(BAR_STYLE.popup_background),
            )?
            .check()?;
        self.conn.poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width: rect.width,
                height: rect.height,
            }],
        )?;
        self.conn.poly_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: 0,
                y: 0,
                width: rect.width,
                height: rect.height,
            }],
        )?;
        self.text
            .draw_popup_utf8("Som", 14, 26, BAR_STYLE.popup_foreground)?;
        self.text.draw_popup_utf8(
            &format!(
                "{}   {}%",
                view::audio_glyph(&state.audio),
                state.audio.volume_percent
            ),
            14,
            52,
            BAR_STYLE.popup_foreground,
        )?;
        let output_label_y = 232_i32;
        self.text
            .draw_popup_utf8("Saída", 14, output_label_y, BAR_STYLE.popup_foreground)?;
        for (index, device) in state.audio.outputs.iter().take(8).enumerate() {
            let marker = if state.audio.default_output.as_deref() == Some(device.name.as_str()) {
                "✓"
            } else {
                " "
            };
            self.text.draw_popup_utf8(
                &format!("{marker} {}", device.display_name),
                22,
                output_label_y + 22 + index as i32 * 24,
                BAR_STYLE.popup_foreground,
            )?;
        }
        let input_label_y = output_label_y + 22 + output_count as i32 * 24;
        self.text
            .draw_popup_utf8("Entrada", 14, input_label_y, BAR_STYLE.popup_foreground)?;
        for (index, device) in state.audio.inputs.iter().take(8).enumerate() {
            let marker = if state.audio.default_input.as_deref() == Some(device.name.as_str()) {
                "✓"
            } else {
                " "
            };
            self.text.draw_popup_utf8(
                &format!("{marker} {}", device.display_name),
                22,
                input_label_y + 22 + index as i32 * 24,
                BAR_STYLE.popup_foreground,
            )?;
        }
        self.draw_audio_slider(window, gc, track, state.audio.volume_percent)?;
        self.text
            .draw_popup_utf8("Mudo", 14, 94, BAR_STYLE.popup_foreground)?;
        self.text
            .draw_popup_utf8("Microfone", 14, 122, BAR_STYLE.popup_foreground)?;
        self.text.draw_popup_utf8(
            &format!(
                "{}   {}%",
                view::microphone_glyph(&state.audio),
                state.audio.input_volume_percent
            ),
            14,
            148,
            BAR_STYLE.popup_foreground,
        )?;
        self.draw_audio_slider(window, gc, input_track, state.audio.input_volume_percent)?;
        self.text
            .draw_popup_utf8("Mudo", 14, 204, BAR_STYLE.popup_foreground)?;
        self.conn.free_gc(gc)?.check()?;
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
        let devices: Vec<_> = state
            .bluetooth
            .devices
            .iter()
            .filter(|d| d.connected || d.paired)
            .collect();
        let rect = layout::MenuRect {
            x: (output.x + output.width as i16 - 330).max(output.x),
            y: output.y + 26,
            width: 330,
            height: (72 + devices.len() as u16 * 30).min(output.height.saturating_sub(26).max(120)),
        };
        let power = layout::MenuRect {
            x: rect.x + rect.width as i16 - 82,
            y: rect.y + 8,
            width: 68,
            height: 28,
        };
        let rows: Vec<_> = devices
            .iter()
            .enumerate()
            .map(|(i, d)| {
                (
                    d.path.clone(),
                    layout::MenuRect {
                        x: rect.x + 10,
                        y: rect.y + 58 + i as i16 * 30,
                        width: rect.width - 20,
                        height: 28,
                    },
                )
            })
            .collect();
        let window = if let Some(p) = &self.bluetooth_popup {
            p.window
        } else {
            let w = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "bluetooth-popup", w);
            self.conn
                .create_window(
                    x11rb::COPY_FROM_PARENT as u8,
                    w,
                    self.root,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    1,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &xproto::CreateWindowAux::new()
                        .background_pixel(BAR_STYLE.popup_background)
                        .override_redirect(1)
                        .event_mask(
                            EventMask::EXPOSURE
                                | EventMask::BUTTON_PRESS
                                | EventMask::POINTER_MOTION,
                        ),
                )?
                .check()?;
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
        if resize {
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
        }
        let geometry = self.conn.get_geometry(window)?.reply()?;
        let attrs = self.conn.get_window_attributes(window)?.reply()?;
        self.text
            .prepare_drawable("bluetooth-popup", window, attrs.visual, geometry.depth)?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(
                gc,
                window,
                &xproto::CreateGCAux::new().foreground(BAR_STYLE.popup_foreground),
            )?
            .check()?;
        self.conn
            .poly_rectangle(
                window,
                gc,
                &[xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: rect.width,
                    height: rect.height,
                }],
            )?
            .check()?;
        let power_label = state
            .bluetooth_pending
            .iter()
            .find_map(|pending| match pending {
                crate::core::BluetoothPendingAction::SetPowered(powered) => Some(if *powered {
                    "Ligando..."
                } else {
                    "Desligando..."
                }),
                _ => None,
            })
            .unwrap_or(if state.bluetooth.powered { "ON" } else { "OFF" });
        self.text.draw_popup_utf8(
            &format!("Bluetooth                 {power_label}"),
            12,
            25,
            BAR_STYLE.popup_foreground,
        )?;
        self.text
            .draw_popup_utf8("Dispositivos", 12, 50, BAR_STYLE.popup_foreground)?;
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
            self.text.draw_popup_utf8(
                &format!("{marker} {name:<20} {status}"),
                14,
                78 + i as i32 * 30,
                BAR_STYLE.popup_foreground,
            )?;
        }
        self.conn.free_gc(gc)?.check()?;
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
        let mut rows = Vec::new();
        let mut headers = Vec::new();
        let mut row_index = 0;
        for device in &state.network.wifi_devices {
            headers.push((
                device.interface.clone(),
                device.driver.clone(),
                crate::core::wifi_device_state_label(device.state).to_owned(),
                device
                    .access_points
                    .iter()
                    .find(|access_point| access_point.is_active)
                    .map(|access_point| {
                        format!(
                            "{} · {}",
                            access_point.ssid,
                            crate::core::wifi_band(access_point.frequency)
                        )
                    }),
                row_index,
            ));
            row_index += 2;
            for access_point in &device.access_points {
                rows.push((
                    NetworkWifiTarget {
                        interface: access_point.interface.clone(),
                        ssid: access_point.ssid.clone(),
                        band: crate::core::wifi_band(access_point.frequency).into(),
                        saved: access_point.saved_profile.is_some(),
                        active: access_point.is_active,
                    },
                    layout::MenuRect {
                        x: output.x + output.width as i16 - 350 + 10,
                        y: output.y + 26 + 82 + row_index * 24,
                        width: 330,
                        height: 22,
                    },
                ));
                row_index += 1;
            }
        }
        let rect = layout::MenuRect {
            x: (output.x + output.width as i16 - 350).max(output.x),
            y: output.y + 26,
            width: 350,
            height: (78 + row_index as u16 * 24 + 10)
                .min(output.height.saturating_sub(26).max(120)),
        };
        let wireless = layout::MenuRect {
            x: rect.x + rect.width as i16 - 100,
            y: rect.y + 8,
            width: 84,
            height: 28,
        };
        let window = if let Some(popup) = &self.network_popup {
            popup.window
        } else {
            let window = self.conn.generate_id()?;
            trace_x11_resource("WINDOW_CREATE", "network-popup", window);
            self.conn
                .create_window(
                    x11rb::COPY_FROM_PARENT as u8,
                    window,
                    self.root,
                    rect.x,
                    rect.y,
                    rect.width,
                    rect.height,
                    1,
                    WindowClass::INPUT_OUTPUT,
                    x11rb::COPY_FROM_PARENT,
                    &xproto::CreateWindowAux::new()
                        .background_pixel(BAR_STYLE.popup_background)
                        .override_redirect(1)
                        .event_mask(
                            EventMask::EXPOSURE
                                | EventMask::BUTTON_PRESS
                                | EventMask::POINTER_MOTION,
                        ),
                )?
                .check()?;
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
        if resize {
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
        }
        let geometry = self.conn.get_geometry(window)?.reply()?;
        let attrs = self.conn.get_window_attributes(window)?.reply()?;
        self.text
            .prepare_drawable("network-popup", window, attrs.visual, geometry.depth)?;
        let gc = self.conn.generate_id()?;
        self.conn
            .create_gc(
                gc,
                window,
                &xproto::CreateGCAux::new().foreground(BAR_STYLE.popup_foreground),
            )?
            .check()?;
        self.conn
            .poly_rectangle(
                window,
                gc,
                &[xproto::Rectangle {
                    x: 0,
                    y: 0,
                    width: rect.width,
                    height: rect.height,
                }],
            )?
            .check()?;
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
        self.text.draw_popup_utf8(
            &format!("Wi-Fi                         {wireless_label}"),
            12,
            25,
            BAR_STYLE.popup_foreground,
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
            12,
            50,
            BAR_STYLE.popup_foreground,
        )?;
        self.text
            .draw_popup_utf8("Redes disponíveis", 12, 74, BAR_STYLE.popup_foreground)?;
        for (interface, driver, device_state, active, index) in headers {
            self.text.draw_popup_utf8(
                &format!(
                    "{}{}",
                    interface,
                    driver
                        .map(|driver| format!(" · {driver}"))
                        .unwrap_or_default()
                ),
                14,
                98 + index as i32 * 24,
                BAR_STYLE.popup_foreground,
            )?;
            self.text.draw_popup_utf8(
                &format!(
                    "{}{}",
                    device_state,
                    active
                        .map(|active| format!(" · {active}"))
                        .unwrap_or_default()
                ),
                14,
                98 + (index as i32 + 1) * 24,
                BAR_STYLE.popup_foreground,
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
            let band = crate::core::wifi_band(access_point.frequency);
            let marker = if access_point.is_active { "●" } else { " " };
            let saved = if access_point.saved_profile.is_some() {
                "saved"
            } else {
                "unsaved"
            };
            self.text.draw_popup_utf8(
                &format!(
                    "{marker} {:<7} {:<24} {band:<9} {:>3}% {saved}",
                    access_point.interface, access_point.ssid, access_point.strength
                ),
                14,
                (row.y - rect.y + 16) as i32,
                BAR_STYLE.popup_foreground,
            )?;
        }
        self.conn.free_gc(gc)?.check()?;
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
            .change_gc(gc, &xproto::ChangeGCAux::new().foreground(0x596273))?
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
                &xproto::ChangeGCAux::new().foreground(BAR_STYLE.menu_hover_foreground),
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
            if trace {
                eprintln!("xbar trace: popup destroyed xid={}", popup.window);
            }
            trace_x11_resource("WINDOW_DESTROY", "menu-popup", popup.window);
            self.conn.destroy_window(popup.window)?.check()?;
        }
        Ok(())
    }

    fn render_popups(&mut self, state: &State) -> Result<(), Box<dyn Error>> {
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
            if reuse {
                if self.popups[level].layout.rect != popup_layout.rect {
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
                }
            } else {
                self.conn
                    .create_window(
                        x11rb::COPY_FROM_PARENT as u8,
                        window,
                        self.root,
                        popup_layout.rect.x,
                        popup_layout.rect.y,
                        popup_layout.rect.width,
                        popup_layout.rect.height,
                        1,
                        WindowClass::INPUT_OUTPUT,
                        x11rb::COPY_FROM_PARENT,
                        &xproto::CreateWindowAux::new()
                            .background_pixel(BAR_STYLE.popup_background)
                            .override_redirect(1)
                            .event_mask(
                                EventMask::EXPOSURE
                                    | EventMask::BUTTON_PRESS
                                    | EventMask::POINTER_MOTION
                                    | EventMask::ENTER_WINDOW
                                    | EventMask::LEAVE_WINDOW,
                            ),
                    )?
                    .check()?;
                self.conn.map_window(window)?.check()?;
            }
            let geometry = self.conn.get_geometry(window)?.reply()?;
            let attributes = self.conn.get_window_attributes(window)?.reply()?;
            self.text
                .prepare_drawable("menu-popup", window, attributes.visual, geometry.depth)?;
            let gc = self.conn.generate_id()?;
            self.conn
                .create_gc(
                    gc,
                    window,
                    &xproto::CreateGCAux::new().foreground(BAR_STYLE.popup_foreground),
                )?
                .check()?;
            self.conn
                .poly_rectangle(
                    window,
                    gc,
                    &[xproto::Rectangle {
                        x: 0,
                        y: 0,
                        width: popup_layout.rect.width,
                        height: popup_layout.rect.height,
                    }],
                )?
                .check()?;
            for item in &popup_layout.items {
                if item.separator {
                    self.conn.poly_fill_rectangle(
                        window,
                        gc,
                        &[xproto::Rectangle {
                            x: 4,
                            y: item.rect.y - popup_layout.rect.y + 4,
                            width: popup_layout.rect.width.saturating_sub(8),
                            height: 1,
                        }],
                    )?;
                    continue;
                }
                let hovered = state.menu_interaction.hovered_path.last() == Some(&item.id);
                if hovered {
                    self.conn
                        .change_gc(
                            gc,
                            &xproto::ChangeGCAux::new().foreground(BAR_STYLE.menu_hover_background),
                        )?
                        .check()?;
                    self.conn.poly_fill_rectangle(
                        window,
                        gc,
                        &[xproto::Rectangle {
                            x: 2,
                            y: item.rect.y - popup_layout.rect.y,
                            width: popup_layout.rect.width.saturating_sub(4),
                            height: item.rect.height,
                        }],
                    )?;
                    self.conn
                        .change_gc(
                            gc,
                            &xproto::ChangeGCAux::new().foreground(BAR_STYLE.menu_hover_foreground),
                        )?
                        .check()?;
                }
                let color = if item.enabled {
                    BAR_STYLE.popup_foreground
                } else {
                    BAR_STYLE.menu_disabled_foreground
                };
                self.conn
                    .change_gc(gc, &xproto::ChangeGCAux::new().foreground(color))?
                    .check()?;
                self.text.draw_popup_utf8(
                    &item.label,
                    8,
                    (item.rect.y - popup_layout.rect.y) as i32
                        + self.text.popup_baseline(26) as i32,
                    color,
                )?;
                if let Some(shortcut) = &item.shortcut {
                    let width = self.text.measure_popup_width(shortcut);
                    self.text.draw_popup_utf8(
                        shortcut,
                        popup_layout
                            .rect
                            .width
                            .saturating_sub(BAR_STYLE.horizontal_padding + width)
                            as i32,
                        (item.rect.y - popup_layout.rect.y) as i32
                            + self.text.popup_baseline(26) as i32,
                        color,
                    )?;
                }
                if item.has_submenu {
                    self.text.draw_popup_utf8(
                        ">",
                        popup_layout.rect.width.saturating_sub(14) as i32,
                        (item.rect.y - popup_layout.rect.y) as i32
                            + self.text.popup_baseline(26) as i32,
                        color,
                    )?;
                }
            }
            self.conn.free_gc(gc)?.check()?;
            if reuse {
                self.popups[level].layout = popup_layout;
            } else {
                self.popups.push(PopupWindow {
                    window,
                    layout: popup_layout,
                });
            }
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
        if let Some((_bar, _, ox, oy, items, tray, network, audio, bluetooth)) =
            self.bar_hits.iter().find(|(bar, _, _, _, _, _, _, _, _)| {
                *bar == window || (self.root == window && root_y < BAR_HEIGHT as i16)
            })
        {
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
                if let Some((name, _)) = popup.output_devices.iter().find(|(_, rect)| {
                    root_x >= rect.x
                        && root_x < rect.x + rect.width as i16
                        && root_y >= rect.y
                        && root_y < rect.y + rect.height as i16
                }) {
                    return HitTarget::AudioOutputDevice(name.clone());
                }
                if let Some((name, _)) = popup.input_devices.iter().find(|(_, rect)| {
                    root_x >= rect.x
                        && root_x < rect.x + rect.width as i16
                        && root_y >= rect.y
                        && root_y < rect.y + rect.height as i16
                }) {
                    return HitTarget::AudioInputDevice(name.clone());
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
                if let Some((path, _)) = popup.devices.iter().find(|(_, r)| {
                    rx >= r.x
                        && rx < r.x + r.width as i16
                        && ry >= r.y
                        && ry < r.y + r.height as i16
                }) {
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
                if let Some((target, _)) = popup.access_points.iter().find(|(_, rect)| {
                    rx >= rect.x
                        && rx < rect.x + rect.width as i16
                        && ry >= rect.y
                        && ry < rect.y + rect.height as i16
                }) {
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
            if let Some(item) = popup.layout.items.iter().find(|i| {
                popup_x >= (i.rect.x - popup.layout.rect.x)
                    && popup_x < (i.rect.x - popup.layout.rect.x + i.rect.width as i16)
                    && popup_y >= (i.rect.y - popup.layout.rect.y)
                    && popup_y < (i.rect.y - popup.layout.rect.y + i.rect.height as i16)
            }) {
                let mut path = Vec::new();
                path.extend(self.popups.iter().take(level).map(|p| p.layout.parent_id));
                path.push(item.id);
                return HitTarget::Item(path);
            }
        }
        HitTarget::Outside
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
    use super::{is_xbar_owned_window, tray_hit, RenderTarget};
    use crate::core::{StatusNotifierEndpoint, StatusNotifierIcon};
    use crate::ui::{layout::MenuRect, view::TrayVisualItem};

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
}
