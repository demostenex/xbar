#[cfg(test)]
use crate::core::NetworkAccessPoint;
use crate::core::{
    format_notifier_item_id, parse_notifier_item_id, BluetoothDevice, BluetoothPendingAction,
    BluetoothState, Event, GtkMenuEndpoint, MenuActionTarget, MenuRegistry, MenuSource,
    NotificationId, StatusNotifierAction, StatusNotifierEndpoint, StatusNotifierIcon,
    StatusNotifierItem, StatusNotifierStatus,
};
mod ai_usage;
mod gmenu;
mod menu;
use crate::notifications::{self, SharedStore, SharedTimer, REASON_CLOSED, REASON_EXPIRED};
use async_channel::{Receiver, Sender};
use futures_lite::StreamExt;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Signature};
use zbus::{MatchRule, MessageStream};

pub const REGISTRAR_NAME: &str = "com.canonical.AppMenu.Registrar";
pub const REGISTRAR_PATH: &str = "/com/canonical/AppMenu/Registrar";
const SNI_NAME: &str = "org.kde.StatusNotifierWatcher";
const SNI_PATH: &str = "/StatusNotifierWatcher";
const SNI_INTERFACE: &str = "org.kde.StatusNotifierWatcher";
const DBUSMENU_INTERFACE: &str = "com.canonical.dbusmenu";
const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

#[derive(Clone, Debug)]
struct LayoutRequest {
    window_id: crate::core::WindowId,
    endpoint: crate::core::MenuEndpoint,
    request_id: u64,
}
#[derive(Clone, Debug)]
struct AboutRequest {
    window_id: crate::core::WindowId,
    endpoint: crate::core::MenuEndpoint,
    item_id: crate::core::MenuItemId,
    request_id: u64,
}
#[derive(Clone, Debug)]
struct ActivateRequest {
    window_id: crate::core::WindowId,
    endpoint: crate::core::MenuEndpoint,
    item_id: crate::core::MenuItemId,
    timestamp: u32,
}
#[derive(Clone, Debug)]
struct GtkActivateRequest {
    window_id: crate::core::WindowId,
    endpoint: GtkMenuEndpoint,
    action: String,
    target: Option<MenuActionTarget>,
}

#[derive(Default)]
struct StatusNotifierWatcherState {
    items: Mutex<Vec<StatusNotifierEndpoint>>,
    hosts: Mutex<HashSet<String>>,
}

struct StatusNotifierWatcher {
    events: EventQueue,
    wake: Arc<Mutex<UnixStream>>,
    state: Arc<StatusNotifierWatcherState>,
    connection: Arc<Mutex<Option<zbus::Connection>>>,
}

impl StatusNotifierWatcher {
    fn push(&self, event: Event) {
        push_event(&self.events, &self.wake, event);
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    async fn register_status_notifier_item(
        &self,
        item: String,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("registration has no sender".into()))?
            .to_string();
        if !item.starts_with('/') {
            let connection = self
                .connection
                .lock()
                .expect("SNI connection poisoned")
                .clone()
                .ok_or_else(|| zbus::fdo::Error::Failed("watcher is not ready".into()))?;
            let name: zbus::names::BusName<'_> = item.as_str().try_into().map_err(|error| {
                zbus::fdo::Error::InvalidArgs(format!("invalid service: {error}"))
            })?;
            let dbus = zbus::fdo::DBusProxy::new(&connection)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
            if !dbus
                .name_has_owner(name)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?
            {
                return Err(zbus::fdo::Error::NameHasNoOwner(item));
            }
        }
        let endpoint = if item.starts_with('/') {
            StatusNotifierEndpoint {
                service: sender,
                object_path: item.clone(),
            }
        } else {
            StatusNotifierEndpoint {
                service: item.clone(),
                object_path: "/StatusNotifierItem".into(),
            }
        };
        let added = {
            let mut items = self.state.items.lock().expect("SNI items poisoned");
            if items.contains(&endpoint) {
                false
            } else {
                items.push(endpoint.clone());
                true
            }
        };
        if added {
            self.push(Event::StatusNotifierRegistered(endpoint.clone()));
            let connection = self
                .connection
                .lock()
                .expect("SNI connection poisoned")
                .clone();
            if let Some(connection) = connection {
                let emitter = zbus::object_server::SignalEmitter::new(&connection, SNI_PATH)?;
                Self::status_notifier_item_registered(&emitter, format_notifier_item_id(&endpoint))
                    .await?;
                load_status_notifier_item(&connection, endpoint.clone(), &self.events, &self.wake)
                    .await;
                watch_status_notifier_item(&connection, endpoint, &self.events, &self.wake);
            }
        }
        Ok(())
    }

    async fn register_status_notifier_host(&self, host: String) -> zbus::fdo::Result<()> {
        let added = self
            .state
            .hosts
            .lock()
            .expect("SNI hosts poisoned")
            .insert(host.clone());
        if added {
            self.push(Event::StatusNotifierHostRegistered);
            let connection = self
                .connection
                .lock()
                .expect("SNI connection poisoned")
                .clone();
            if let Some(connection) = connection {
                let emitter = zbus::object_server::SignalEmitter::new(&connection, SNI_PATH)?;
                Self::status_notifier_host_registered(&emitter, host).await?;
            }
        }
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.state
            .items
            .lock()
            .expect("SNI items poisoned")
            .iter()
            .map(format_notifier_item_id)
            .collect()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        !self
            .state
            .hosts
            .lock()
            .expect("SNI hosts poisoned")
            .is_empty()
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        item: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        item: String,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        host: String,
    ) -> zbus::Result<()>;
}
#[derive(Clone, Debug)]
enum Request {
    Layout(LayoutRequest),
    GtkLayout {
        window_id: crate::core::WindowId,
        endpoint: GtkMenuEndpoint,
        request_id: u64,
    },
    GtkEnd(GtkMenuEndpoint),
    About(AboutRequest),
    Activate(ActivateRequest),
    GtkActivate(GtkActivateRequest),
    StatusNotifierAction {
        endpoint: StatusNotifierEndpoint,
        action: StatusNotifierAction,
        root_x: i32,
        root_y: i32,
    },
    BluetoothSetPowered(bool),
    BluetoothConnectDevice(String),
    BluetoothDisconnectDevice(String),
    NotificationTimerFired,
    WindowAttention {
        window: crate::core::WindowId,
        app_name: String,
        attention: bool,
    },
    AiUsageSnapshot {
        owner: String,
        payload: Vec<u8>,
    },
}

type EventQueue = Arc<Mutex<VecDeque<Event>>>;
type PropertiesSignal = (
    Vec<(i32, HashMap<String, zbus::zvariant::OwnedValue>)>,
    Vec<(i32, Vec<String>)>,
);

pub struct DbusBridge {
    reader: UnixStream,
    events: EventQueue,
    _thread: JoinHandle<()>,
    requests: Sender<Request>,
    notification_timer: SharedTimer,
}

impl DbusBridge {
    pub fn start(registry: Arc<Mutex<MenuRegistry>>) -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        writer.set_nonblocking(true)?;
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let (requests, request_receiver) = async_channel::unbounded();
        let notification_timer = Arc::new(Mutex::new(notifications::DeadlineTimer::new()?));
        let timer_for_thread = Arc::clone(&notification_timer);
        let thread_events = Arc::clone(&events);
        let requests_for_thread = requests.clone();
        let thread = thread::Builder::new()
            .name("xbar-dbus".into())
            .spawn(move || {
                if let Err(error) = zbus::block_on(run(
                    thread_events,
                    writer,
                    registry,
                    requests_for_thread,
                    request_receiver,
                    timer_for_thread,
                )) {
                    eprintln!("xbar: DBus adapter stopped: {error}");
                }
            })?;
        Ok(Self {
            reader,
            events,
            _thread: thread,
            requests,
            notification_timer,
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }

    pub fn notification_timer_raw_fd(&self) -> RawFd {
        self.notification_timer
            .lock()
            .expect("notification timer poisoned")
            .as_raw_fd()
    }

    pub fn notification_timer_fired(&self) {
        let _ = self.requests.try_send(Request::NotificationTimerFired);
    }

    pub fn window_attention(
        &self,
        window: crate::core::WindowId,
        app_name: String,
        attention: bool,
    ) {
        let _ = self.requests.try_send(Request::WindowAttention {
            window,
            app_name,
            attention,
        });
    }

    pub fn drain_events(&mut self) -> io::Result<Vec<Event>> {
        let mut buffer = [0_u8; 128];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let mut events = self.events.lock().expect("DBus event queue poisoned");
        Ok(events.drain(..).collect())
    }

    pub fn request_layout(
        &self,
        window_id: crate::core::WindowId,
        endpoint: crate::core::MenuEndpoint,
        request_id: u64,
    ) {
        let _ = self.requests.try_send(Request::Layout(LayoutRequest {
            window_id,
            endpoint,
            request_id,
        }));
    }
    pub fn request_about_to_show(
        &self,
        window_id: crate::core::WindowId,
        endpoint: crate::core::MenuEndpoint,
        item_id: crate::core::MenuItemId,
        request_id: u64,
    ) {
        let _ = self.requests.try_send(Request::About(AboutRequest {
            window_id,
            endpoint,
            item_id,
            request_id,
        }));
    }

    pub fn request_gtk_layout(
        &self,
        window_id: crate::core::WindowId,
        endpoint: GtkMenuEndpoint,
        request_id: u64,
    ) {
        let _ = self.requests.try_send(Request::GtkLayout {
            window_id,
            endpoint,
            request_id,
        });
    }

    pub fn request_activation(
        &self,
        window_id: crate::core::WindowId,
        endpoint: crate::core::MenuEndpoint,
        item_id: crate::core::MenuItemId,
        timestamp: u32,
    ) {
        let _ = self.requests.try_send(Request::Activate(ActivateRequest {
            window_id,
            endpoint,
            item_id,
            timestamp,
        }));
    }

    pub fn request_gtk_activation(
        &self,
        window_id: crate::core::WindowId,
        endpoint: GtkMenuEndpoint,
        action: String,
        target: Option<MenuActionTarget>,
    ) {
        let _ = self
            .requests
            .try_send(Request::GtkActivate(GtkActivateRequest {
                window_id,
                endpoint,
                action,
                target,
            }));
    }

    pub fn end_gtk_menu(&self, endpoint: GtkMenuEndpoint) {
        let _ = self.requests.try_send(Request::GtkEnd(endpoint));
    }

    pub fn request_status_notifier_action(
        &self,
        endpoint: StatusNotifierEndpoint,
        action: StatusNotifierAction,
        root_x: i32,
        root_y: i32,
    ) {
        let _ = self.requests.try_send(Request::StatusNotifierAction {
            endpoint,
            action,
            root_x,
            root_y,
        });
    }

    pub fn bluetooth_set_powered(&self, powered: bool) {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: DBusWorker enqueue SetPowered powered={powered}");
        }
        let _ = self
            .requests
            .try_send(Request::BluetoothSetPowered(powered));
    }
    pub fn bluetooth_connect_device(&self, path: String) {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: DBusWorker enqueue ConnectDevice path={path}");
        }
        let _ = self
            .requests
            .try_send(Request::BluetoothConnectDevice(path));
    }
    pub fn bluetooth_disconnect_device(&self, path: String) {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: DBusWorker enqueue DisconnectDevice path={path}");
        }
        let _ = self
            .requests
            .try_send(Request::BluetoothDisconnectDevice(path));
    }
}

struct Registrar {
    events: EventQueue,
    wake: Arc<Mutex<UnixStream>>,
    registry: Arc<Mutex<MenuRegistry>>,
}

struct NotificationServer {
    store: SharedStore,
    timer: SharedTimer,
    events: EventQueue,
    wake: Arc<Mutex<UnixStream>>,
}

impl NotificationServer {
    fn publish(&self) {
        notifications::publish(&self.store, &self.timer, &self.events, &self.wake);
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
    async fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into()]
    }

    async fn get_server_information(&self) -> (String, String, String, String) {
        (
            "xbar".into(),
            "Demostenes Albert".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        let id = self
            .store
            .lock()
            .expect("notification store poisoned")
            .notify(replaces_id, app_name, summary, body, expire_timeout);
        self.publish();
        Ok(id.0)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let id = NotificationId(id);
        if self
            .store
            .lock()
            .expect("notification store poisoned")
            .close(id)
        {
            self.publish();
            NotificationServer::notification_closed(&emitter, id.0, REASON_CLOSED)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))?;
        }
        Ok(())
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;
}

async fn setup_status_notifier(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    watcher_exists: bool,
) -> zbus::Result<()> {
    let watcher =
        zbus::Proxy::new_owned(connection.clone(), SNI_NAME, SNI_PATH, SNI_INTERFACE).await?;
    let host = format!("org.kde.StatusNotifierHost-{}-xbar", std::process::id());
    let _: () = watcher.call("RegisterStatusNotifierHost", &(host,)).await?;
    push_event(events, wake, Event::StatusNotifierHostRegistered);
    if !watcher_exists {
        // The local Watcher has already emitted the internal host event.
        // This call also verifies the same public API used by external hosts.
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: StatusNotifierWatcher owned by xbar, ProtocolVersion=0");
        }
    }
    let existing: Vec<String> = watcher
        .get_property("RegisteredStatusNotifierItems")
        .await?;
    for item in existing {
        if let Some(endpoint) = parse_notifier_item_id(&item) {
            push_event(
                events,
                wake,
                Event::StatusNotifierRegistered(endpoint.clone()),
            );
            load_status_notifier_item(connection, endpoint.clone(), events, wake).await;
            watch_status_notifier_item(connection, endpoint, events, wake);
        }
    }
    if watcher_exists {
        install_status_notifier_signal_watcher(connection, events, wake, watcher);
    }
    Ok(())
}

async fn watcher_state_remove_service(
    state: &StatusNotifierWatcherState,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    connection: &zbus::Connection,
    service: &str,
) {
    let removed = {
        let mut items = state.items.lock().expect("SNI items poisoned");
        let mut removed = Vec::new();
        items.retain(|endpoint| {
            if endpoint.service == service {
                removed.push(endpoint.clone());
                false
            } else {
                true
            }
        });
        removed
    };
    for endpoint in removed {
        push_event(
            events,
            wake,
            Event::StatusNotifierUnregistered(endpoint.clone()),
        );
        if let Ok(emitter) = zbus::object_server::SignalEmitter::new(connection, SNI_PATH) {
            let _ = StatusNotifierWatcher::status_notifier_item_unregistered(
                &emitter,
                format_notifier_item_id(&endpoint),
            )
            .await;
        }
    }
}

fn install_status_notifier_signal_watcher(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    watcher: zbus::Proxy<'static>,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let mut registered =
                    match watcher.receive_signal("StatusNotifierItemRegistered").await {
                        Ok(stream) => stream,
                        Err(_) => return,
                    };
                let mut unregistered = match watcher
                    .receive_signal("StatusNotifierItemUnregistered")
                    .await
                {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                loop {
                    let registered_signal = async { Either::Owner(registered.next().await) };
                    let unregistered_signal = async { Either::Request(unregistered.next().await) };
                    match futures_lite::future::race(registered_signal, unregistered_signal).await {
                        Either::Owner(Some(signal)) => {
                            if let Ok(item) = signal.body().deserialize::<String>() {
                                if let Some(endpoint) = parse_notifier_item_id(&item) {
                                    push_event(
                                        &events,
                                        &wake,
                                        Event::StatusNotifierRegistered(endpoint.clone()),
                                    );
                                    if let Some(endpoint) = parse_notifier_item_id(&item) {
                                        load_status_notifier_item(
                                            &connection,
                                            endpoint.clone(),
                                            &events,
                                            &wake,
                                        )
                                        .await;
                                        watch_status_notifier_item(
                                            &connection,
                                            endpoint,
                                            &events,
                                            &wake,
                                        );
                                    }
                                }
                            }
                        }
                        Either::Request(Some(signal)) => {
                            if let Ok(item) = signal.body().deserialize::<String>() {
                                if let Some(endpoint) = parse_notifier_item_id(&item) {
                                    push_event(
                                        &events,
                                        &wake,
                                        Event::StatusNotifierUnregistered(endpoint),
                                    );
                                }
                            }
                        }
                        Either::Owner(None) | Either::Request(None) => break,
                        Either::Network => unreachable!("network tick is not used by this watcher"),
                    }
                }
            },
            "xbar-status-notifier-signals",
        )
        .detach();
}

fn parse_sni_status(status: &str) -> StatusNotifierStatus {
    match status {
        "Active" => StatusNotifierStatus::Active,
        "NeedsAttention" => StatusNotifierStatus::NeedsAttention,
        _ => StatusNotifierStatus::Passive,
    }
}

fn select_pixmap(pixmaps: Vec<(i32, i32, Vec<u8>)>) -> Option<StatusNotifierIcon> {
    let (width, height, bytes) = pixmaps
        .into_iter()
        .filter(|(width, height, bytes)| {
            *width > 0
                && *height > 0
                && *width <= 64
                && *height <= 64
                && (*width as usize)
                    .checked_mul(*height as usize)
                    .and_then(|pixels| pixels.checked_mul(4))
                    .is_some_and(|size| bytes.len() >= size)
        })
        .min_by_key(|(width, height, _)| {
            (
                (*width - 16).unsigned_abs() + (*height - 16).unsigned_abs(),
                *width,
                *height,
            )
        })?;
    let pixel_count = (width as usize).checked_mul(height as usize)?;
    let argb = bytes
        .chunks_exact(4)
        .take(pixel_count)
        .map(|pixel| u32::from_be_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]))
        .collect();
    Some(StatusNotifierIcon::Pixmap {
        width: width as u16,
        height: height as u16,
        argb,
    })
}

fn choose_sni_icon(
    status: &StatusNotifierStatus,
    normal: Option<StatusNotifierIcon>,
    attention: Option<StatusNotifierIcon>,
) -> Option<StatusNotifierIcon> {
    match status {
        StatusNotifierStatus::NeedsAttention => attention.or(normal),
        StatusNotifierStatus::Active | StatusNotifierStatus::Passive => normal,
    }
}

async fn load_status_notifier_item(
    connection: &zbus::Connection,
    endpoint: StatusNotifierEndpoint,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
) {
    let Ok(proxy) = zbus::Proxy::new(
        connection,
        endpoint.service.as_str(),
        endpoint.object_path.as_str(),
        "org.kde.StatusNotifierItem",
    )
    .await
    else {
        return;
    };
    let status = proxy
        .get_property::<String>("Status")
        .await
        .map(|value| parse_sni_status(&value))
        .unwrap_or(StatusNotifierStatus::Passive);
    let _icon_name = proxy
        .get_property::<String>("IconName")
        .await
        .ok()
        .filter(|value| !value.is_empty());
    let icon_pixmap = proxy
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap")
        .await
        .ok()
        .and_then(select_pixmap);
    let _attention_icon_name = proxy
        .get_property::<String>("AttentionIconName")
        .await
        .ok()
        .filter(|value| !value.is_empty());
    let attention_icon_pixmap = proxy
        .get_property::<Vec<(i32, i32, Vec<u8>)>>("AttentionIconPixmap")
        .await
        .ok()
        .and_then(select_pixmap);
    let item_is_menu = proxy
        .get_property::<bool>("ItemIsMenu")
        .await
        .unwrap_or(false);
    let menu = proxy
        .get_property::<OwnedObjectPath>("Menu")
        .await
        .ok()
        .filter(|path| path.as_str() != "/")
        .map(|path| crate::core::MenuEndpoint {
            service: endpoint.service.clone(),
            object_path: path.to_string(),
        });
    let icon = choose_sni_icon(&status, icon_pixmap, attention_icon_pixmap);
    push_event(
        events,
        wake,
        Event::StatusNotifierItemUpdated(StatusNotifierItem {
            endpoint,
            status,
            icon,
            item_is_menu,
            menu,
        }),
    );
}

fn watch_status_notifier_item(
    connection: &zbus::Connection,
    endpoint: StatusNotifierEndpoint,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let Ok(destination): Result<zbus::names::OwnedBusName, _> =
                    endpoint.service.clone().try_into()
                else {
                    return;
                };
                let Ok(path): Result<OwnedObjectPath, _> = endpoint.object_path.clone().try_into()
                else {
                    return;
                };
                let Ok(proxy) = zbus::Proxy::new_owned(
                    connection.clone(),
                    destination,
                    path,
                    "org.kde.StatusNotifierItem",
                )
                .await
                else {
                    return;
                };
                let Ok(mut new_icon) = proxy.receive_signal("NewIcon").await else {
                    return;
                };
                let Ok(mut new_attention_icon) = proxy.receive_signal("NewAttentionIcon").await
                else {
                    return;
                };
                let Ok(mut new_status) = proxy.receive_signal("NewStatus").await else {
                    return;
                };
                let Ok(mut new_item_is_menu) = proxy.receive_signal("NewItemIsMenu").await else {
                    return;
                };
                let Ok(mut new_menu) = proxy.receive_signal("NewMenu").await else {
                    return;
                };
                loop {
                    let icon = async { new_icon.next().await.map(|_| ()) };
                    let attention = async { new_attention_icon.next().await.map(|_| ()) };
                    let status = async { new_status.next().await.map(|_| ()) };
                    let item_is_menu = async { new_item_is_menu.next().await.map(|_| ()) };
                    let menu = async { new_menu.next().await.map(|_| ()) };
                    let changed = futures_lite::future::race(
                        futures_lite::future::race(
                            futures_lite::future::race(icon, attention),
                            status,
                        ),
                        futures_lite::future::race(item_is_menu, menu),
                    )
                    .await;
                    if changed.is_none() {
                        break;
                    }
                    load_status_notifier_item(&connection, endpoint.clone(), &events, &wake).await;
                }
            },
            "xbar-status-notifier-item",
        )
        .detach();
}

impl Registrar {
    fn push(&self, event: Event) {
        self.events
            .lock()
            .expect("DBus event queue poisoned")
            .push_back(event);
        let _ = self
            .wake
            .lock()
            .expect("DBus wake poisoned")
            .write_all(&[1]);
    }
}

#[zbus::interface(name = "com.canonical.AppMenu.Registrar")]
impl Registrar {
    async fn register_window(
        &self,
        window_id: u32,
        menu_object_path: ObjectPath<'_>,
        #[zbus(header)] header: Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("RegisterWindow has no sender".into()))?;
        self.push(Event::MenuRegistered {
            window_id: crate::core::WindowId(window_id),
            endpoint: MenuSource::DbusMenu(crate::core::MenuEndpoint {
                service: sender.to_string(),
                object_path: menu_object_path.to_string(),
            }),
        });
        Ok(())
    }

    async fn unregister_window(&self, window_id: u32) -> zbus::fdo::Result<()> {
        self.push(Event::MenuUnregistered {
            window_id: crate::core::WindowId(window_id),
        });
        Ok(())
    }

    async fn get_menu_for_window(
        &self,
        window_id: u32,
    ) -> zbus::fdo::Result<(String, OwnedObjectPath)> {
        let endpoint = self
            .registry
            .lock()
            .expect("DBus registry poisoned")
            .get(crate::core::WindowId(window_id))
            .cloned();
        match endpoint {
            Some(endpoint) => Ok((
                endpoint.service,
                endpoint.object_path.try_into().map_err(|error| {
                    zbus::fdo::Error::InvalidArgs(format!("invalid object path: {error}"))
                })?,
            )),
            None => Ok((
                String::new(),
                OwnedObjectPath::try_from("/").expect("root path"),
            )),
        }
    }
}

#[cfg(test)]
fn deduplicate_wifi_access_points(
    raw_access_points: Vec<NetworkAccessPoint>,
) -> Vec<NetworkAccessPoint> {
    let mut candidates = HashMap::<(String, String), NetworkAccessPoint>::new();
    for candidate in raw_access_points {
        if candidate.ssid.trim().is_empty() {
            continue;
        }
        let key = (
            candidate.ssid.clone(),
            crate::core::wifi_band(candidate.frequency).to_owned(),
        );
        if candidates
            .get(&key)
            .is_none_or(|current| candidate.strength > current.strength)
        {
            let is_active = candidate.is_active
                || candidates
                    .get(&key)
                    .is_some_and(|current| current.is_active);
            candidates.insert(
                key,
                NetworkAccessPoint {
                    is_active,
                    ..candidate
                },
            );
        }
    }
    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.strength
            .cmp(&a.strength)
            .then_with(|| a.ssid.cmp(&b.ssid))
    });
    candidates
}

fn bluetooth_string(properties: &HashMap<String, OwnedValue>, name: &str) -> String {
    properties
        .get(name)
        .and_then(|value| String::try_from(value.clone()).ok())
        .unwrap_or_default()
}

fn bluetooth_bool(properties: &HashMap<String, OwnedValue>, name: &str) -> bool {
    properties
        .get(name)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

async fn bluetooth_set_powered(connection: &zbus::Connection, powered: bool) -> Result<(), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        "org.bluez",
        "/org/bluez/hci0",
        "org.freedesktop.DBus.Properties",
    )
    .await
    .map_err(|e| e.to_string())?;
    proxy
        .call(
            "Set",
            &(
                "org.bluez.Adapter1",
                "Powered",
                zbus::zvariant::Value::from(powered),
            ),
        )
        .await
        .map(|_: ()| ())
        .map_err(|e| e.to_string())
}

async fn bluetooth_device_call(
    connection: &zbus::Connection,
    path: &str,
    method: &str,
) -> Result<(), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        "org.bluez",
        OwnedObjectPath::try_from(path.to_owned()).map_err(|e| e.to_string())?,
        "org.bluez.Device1",
    )
    .await
    .map_err(|e| e.to_string())?;
    proxy
        .call(method, &())
        .await
        .map(|_: ()| ())
        .map_err(|e| e.to_string())
}

async fn bluetooth_snapshot(connection: &zbus::Connection) -> zbus::Result<BluetoothState> {
    let proxy = zbus::fdo::ObjectManagerProxy::builder(connection)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let objects = proxy.get_managed_objects().await?;
    let mut state = BluetoothState::default();
    for (path, interfaces) in objects {
        if let Some(properties) = interfaces.get("org.bluez.Adapter1") {
            state.available = true;
            state.powered |= bluetooth_bool(properties, "Powered");
        }
        if let Some(properties) = interfaces.get("org.bluez.Device1") {
            state.devices.push(BluetoothDevice {
                path: path.to_string(),
                address: bluetooth_string(properties, "Address"),
                alias: {
                    let alias = bluetooth_string(properties, "Alias");
                    if alias.is_empty() {
                        bluetooth_string(properties, "Name")
                    } else {
                        alias
                    }
                },
                name: bluetooth_string(properties, "Name"),
                paired: bluetooth_bool(properties, "Paired"),
                trusted: bluetooth_bool(properties, "Trusted"),
                connected: bluetooth_bool(properties, "Connected"),
            });
        }
    }
    state
        .devices
        .retain(|device| device.connected || device.paired);
    state.devices.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(state)
}

async fn watch_bluetooth(
    connection: zbus::Connection,
    events: EventQueue,
    wake: Arc<Mutex<UnixStream>>,
) {
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: BlueZ watcher starting");
    }
    let proxy = match zbus::fdo::ObjectManagerProxy::builder(&connection)
        .destination("org.bluez")
        .and_then(|builder| builder.path("/").map(|builder| builder.build()))
    {
        Ok(builder) => match builder.await {
            Ok(proxy) => proxy,
            Err(error) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: BlueZ unavailable: {error}");
                }
                push_event(&events, &wake, Event::BluetoothUnavailable);
                return;
            }
        },
        Err(error) => {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: BlueZ proxy unavailable: {error}");
            }
            push_event(&events, &wake, Event::BluetoothUnavailable);
            return;
        }
    };
    match bluetooth_snapshot(&connection).await {
        Ok(snapshot) => {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!(
                    "xbar trace: BlueZ snapshot adapters={} devices={}",
                    snapshot.available as usize,
                    snapshot.devices.len()
                );
            }
            push_event(&events, &wake, Event::BluetoothSnapshotReceived(snapshot))
        }
        Err(error) => {
            if std::env::var_os("XBAR_TRACE").is_some() {
                eprintln!("xbar trace: BlueZ snapshot failed: {error}");
            }
            push_event(&events, &wake, Event::BluetoothUnavailable);
            return;
        }
    }
    let Ok(mut added) = proxy.receive_interfaces_added().await else {
        return;
    };
    let Ok(mut removed) = proxy.receive_interfaces_removed().await else {
        return;
    };
    let properties_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .expect("valid BlueZ sender")
        .interface("org.freedesktop.DBus.Properties")
        .expect("valid Properties interface")
        .member("PropertiesChanged")
        .expect("valid PropertiesChanged member")
        .build();
    let Ok(mut properties) =
        MessageStream::for_match_rule(properties_rule, &connection, Some(8)).await
    else {
        return;
    };
    loop {
        let change = futures_lite::future::race(
            futures_lite::future::race(async { added.next().await.map(|_| ()) }, async {
                removed.next().await.map(|_| ())
            }),
            async { properties.next().await.map(|_| ()) },
        );
        let _ = change.await;
        if let Ok(snapshot) = bluetooth_snapshot(&connection).await {
            push_event(&events, &wake, Event::BluetoothSnapshotReceived(snapshot));
        }
    }
}

async fn run(
    events: EventQueue,
    writer: UnixStream,
    registry: Arc<Mutex<MenuRegistry>>,
    request_sender: Sender<Request>,
    requests: Receiver<Request>,
    notification_timer: SharedTimer,
) -> zbus::Result<()> {
    let wake = Arc::new(Mutex::new(writer));
    let notification_store = Arc::new(Mutex::new(notifications::Store::default()));
    let probe = zbus::Connection::session().await?;
    let probe_dbus = zbus::fdo::DBusProxy::new(&probe).await?;
    let watcher_name: zbus::names::BusName<'_> = SNI_NAME.try_into()?;
    let watcher_exists = probe_dbus.name_has_owner(watcher_name).await?;
    drop(probe);
    let watcher_state = Arc::new(StatusNotifierWatcherState::default());
    let watcher_connection = Arc::new(Mutex::new(None));
    let mut builder = zbus::connection::Builder::session()?.serve_at(
        REGISTRAR_PATH,
        Registrar {
            events: Arc::clone(&events),
            wake: Arc::clone(&wake),
            registry: Arc::clone(&registry),
        },
    )?;
    builder = builder.serve_at(
        NOTIFICATIONS_PATH,
        NotificationServer {
            store: Arc::clone(&notification_store),
            timer: Arc::clone(&notification_timer),
            events: Arc::clone(&events),
            wake: Arc::clone(&wake),
        },
    )?;
    if !watcher_exists {
        builder = builder.serve_at(
            SNI_PATH,
            StatusNotifierWatcher {
                events: Arc::clone(&events),
                wake: Arc::clone(&wake),
                state: Arc::clone(&watcher_state),
                connection: Arc::clone(&watcher_connection),
            },
        )?;
    }
    if !watcher_exists {
        builder = builder.name(SNI_NAME)?;
    }
    let connection = builder
        .name(REGISTRAR_NAME)?
        .name(NOTIFICATIONS_NAME)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .build()
        .await?;
    *watcher_connection.lock().expect("SNI connection poisoned") = Some(connection.clone());
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
    let mut ai_usage = ai_usage::AiUsageSubscription::default();
    ai_usage::subscribe_signal_watcher(&connection, &request_sender).await?;
    if let Ok(owner) = dbus.get_name_owner(ai_usage::BUS_NAME.try_into()?).await {
        let owner = owner.to_string();
        ai_usage.owner_appeared(owner.clone());
        ai_usage::spawn_get_state(&connection, &request_sender, owner);
    }
    setup_status_notifier(&connection, &events, &wake, watcher_exists).await?;
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: NetworkManager system connection starting");
    }
    let system_connection = if let Ok(system) = zbus::Connection::system().await {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!("xbar trace: NetworkManager system connection ready");
        }
        let executor = system.executor().clone();
        let bluetooth_connection = system.clone();
        match bluetooth_snapshot(&system).await {
            Ok(snapshot) => push_event(&events, &wake, Event::BluetoothSnapshotReceived(snapshot)),
            Err(error) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: BlueZ initial snapshot failed: {error}");
                }
                push_event(&events, &wake, Event::BluetoothUnavailable);
            }
        }
        let bluetooth_events = Arc::clone(&events);
        let bluetooth_wake = Arc::clone(&wake);
        executor
            .spawn(
                async move {
                    watch_bluetooth(bluetooth_connection, bluetooth_events, bluetooth_wake).await
                },
                "xbar-bluetooth",
            )
            .detach();
        Some((system, executor))
    } else if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: NetworkManager system bus unavailable");
        None
    } else {
        None
    };
    let mut watched_endpoints = HashSet::new();
    let mut gmenu_subscriptions = HashMap::new();
    let bluetooth_in_flight = Arc::new(Mutex::new(HashSet::<BluetoothPendingAction>::new()));
    loop {
        let owner = async { Either::Owner(owner_changes.next().await) };
        let request = async { Either::Request(requests.recv().await) };
        let dbus = futures_lite::future::race(owner, request);
        let next = if let Some((_, executor)) = &system_connection {
            futures_lite::future::race(dbus, async {
                executor.tick().await;
                Either::Network
            })
            .await
        } else {
            dbus.await
        };
        match next {
            Either::Network => continue,
            Either::Owner(Some(signal)) => {
                let args = signal.args()?;
                if args.name().as_str() == ai_usage::BUS_NAME {
                    let new_owner = args.new_owner().as_ref().map(ToString::to_string);
                    let old_owner = args.old_owner().as_ref().map(ToString::to_string);
                    if let Some(owner) = new_owner {
                        if ai_usage.owner_appeared(owner.clone()) {
                            push_event(&events, &wake, Event::ActiveAiUsageChanged(Vec::new()));
                        }
                        ai_usage::spawn_get_state(&connection, &request_sender, owner);
                    } else if let Some(owner) = old_owner {
                        if ai_usage.owner_disappeared(&owner) {
                            push_event(&events, &wake, Event::ActiveAiUsageChanged(Vec::new()));
                        }
                    }
                    continue;
                }
                if args.name().as_str().starts_with(':') && args.new_owner().is_none() {
                    push_event(
                        &events,
                        &wake,
                        Event::MenuOwnerVanished {
                            sender: args.name().to_string(),
                        },
                    );
                    push_event(
                        &events,
                        &wake,
                        Event::StatusNotifierOwnerVanished(args.name().to_string()),
                    );
                    watcher_state_remove_service(
                        &watcher_state,
                        &events,
                        &wake,
                        &connection,
                        args.name().as_str(),
                    )
                    .await;
                }
            }
            Either::Owner(None) | Either::Request(Err(_)) => break,
            Either::Request(Ok(Request::Layout(request))) => {
                let event = match load_layout(&connection, &request).await {
                    Ok(model) if request.window_id.0 == u32::MAX => Event::TrayMenuLoaded {
                        endpoint: request.endpoint.clone(),
                        request_id: request.request_id,
                        model,
                    },
                    Ok(model) => Event::MenuLoaded {
                        window_id: request.window_id,
                        endpoint: MenuSource::DbusMenu(request.endpoint.clone()),
                        request_id: request.request_id,
                        model,
                    },
                    Err(error) if request.window_id.0 == u32::MAX => Event::TrayMenuLoadFailed {
                        endpoint: request.endpoint.clone(),
                        request_id: request.request_id,
                        error,
                    },
                    Err(error) => Event::MenuLoadFailed {
                        window_id: request.window_id,
                        endpoint: MenuSource::DbusMenu(request.endpoint.clone()),
                        request_id: request.request_id,
                        error,
                    },
                };
                push_event(&events, &wake, event);
                let key = format!(
                    "{}{}",
                    request.endpoint.service, request.endpoint.object_path
                );
                if watched_endpoints.insert(key) {
                    install_signal_watcher(&connection, &events, &wake, request.endpoint);
                }
            }
            Either::Request(Ok(Request::GtkLayout {
                window_id,
                endpoint,
                request_id,
            })) => {
                let key = gmenu::endpoint_key(&endpoint);
                let event = match load_gmenu(&connection, &endpoint, request_id).await {
                    Ok((model, groups)) => {
                        gmenu_subscriptions.insert(key.clone(), groups);
                        Event::MenuLoaded {
                            window_id,
                            endpoint: MenuSource::GtkGMenu(endpoint.clone()),
                            request_id,
                            model,
                        }
                    }
                    Err(error) => Event::MenuLoadFailed {
                        window_id,
                        endpoint: MenuSource::GtkGMenu(endpoint.clone()),
                        request_id,
                        error,
                    },
                };
                push_event(&events, &wake, event);
                if watched_endpoints.insert(key) {
                    install_gmenu_signal_watcher(&connection, &events, &wake, endpoint);
                }
            }
            Either::Request(Ok(Request::GtkEnd(endpoint))) => {
                let key = gmenu::endpoint_key(&endpoint);
                watched_endpoints.remove(&key);
                let groups = gmenu_subscriptions.remove(&key).unwrap_or_else(|| vec![0]);
                if let Err(error) = end_gmenu(&connection, &endpoint, groups).await {
                    if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!(
                            "xbar trace: GMenu End failed bus={} path={}: {error}",
                            endpoint.bus_name, endpoint.menu_object_path
                        );
                    }
                }
            }
            Either::Request(Ok(Request::About(request))) => {
                let (need_update, model, error) = match about_to_show(&connection, &request).await {
                    Ok((need_update, model)) => (need_update, model, None),
                    Err(error) => (false, None, Some(error)),
                };
                push_event(
                    &events,
                    &wake,
                    Event::MenuAboutToShowCompleted {
                        window_id: request.window_id,
                        endpoint: MenuSource::DbusMenu(request.endpoint),
                        item_id: request.item_id,
                        request_id: request.request_id,
                        need_update,
                        model,
                        error,
                    },
                );
            }
            Either::Request(Ok(Request::Activate(request))) => {
                if let Err(error) = activate(&connection, &request).await {
                    eprintln!(
                        "xbar: DBusMenu Event(clicked) failed for window {} item {}: {error}",
                        request.window_id.0, request.item_id.0
                    );
                }
            }
            Either::Request(Ok(Request::GtkActivate(request))) => {
                if let Err(error) = activate_gmenu(&connection, &request).await {
                    eprintln!(
                        "xbar: GMenu action failed for window {} action {}: {error}",
                        request.window_id.0, request.action
                    );
                }
            }
            Either::Request(Ok(Request::StatusNotifierAction {
                endpoint,
                action,
                root_x,
                root_y,
            })) => {
                if let Err(error) =
                    status_notifier_action(&connection, &endpoint, action, root_x, root_y).await
                {
                    eprintln!(
                        "xbar: SNI action {:?} failed for {}{}: {error}",
                        action, endpoint.service, endpoint.object_path
                    );
                }
            }
            Either::Request(Ok(Request::BluetoothSetPowered(powered))) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: DBusWorker receive SetPowered powered={powered}");
                }
                if let Some((system, executor)) = &system_connection {
                    let action = BluetoothPendingAction::SetPowered(powered);
                    let should_start = bluetooth_in_flight
                        .lock()
                        .expect("Bluetooth in-flight lock poisoned")
                        .insert(action.clone());
                    if should_start {
                        let system = system.clone();
                        let events = Arc::clone(&events);
                        let wake = Arc::clone(&wake);
                        let in_flight = Arc::clone(&bluetooth_in_flight);
                        executor
                            .spawn(
                                async move {
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call begin SetPowered powered={powered}");
                                    }
                                    if let Err(error) =
                                        bluetooth_set_powered(&system, powered).await
                                    {
                                        eprintln!("xbar: BlueZ Powered update failed: {error}");
                                    }
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call end SetPowered powered={powered}");
                                    }
                                    in_flight
                                        .lock()
                                        .expect("Bluetooth in-flight lock poisoned")
                                        .remove(&action);
                                    push_event(
                                        &events,
                                        &wake,
                                        Event::BluetoothActionFinished(action),
                                    );
                                },
                                "xbar-bluetooth-command",
                            )
                            .detach();
                    } else if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: Bluetooth action suppressed in-flight SetPowered powered={powered}");
                    }
                } else {
                    eprintln!("xbar: BlueZ Powered update skipped: system bus unavailable");
                }
            }
            Either::Request(Ok(Request::BluetoothConnectDevice(path))) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: DBusWorker receive ConnectDevice path={path}");
                    eprintln!("xbar trace: DBus call org.bluez.Device1.Connect path={path}");
                }
                if let Some((system, executor)) = &system_connection {
                    let action = BluetoothPendingAction::ConnectDevice(path.clone());
                    let should_start = bluetooth_in_flight
                        .lock()
                        .expect("Bluetooth in-flight lock poisoned")
                        .insert(action.clone());
                    if should_start {
                        let system = system.clone();
                        let events = Arc::clone(&events);
                        let wake = Arc::clone(&wake);
                        let in_flight = Arc::clone(&bluetooth_in_flight);
                        executor
                            .spawn(
                                async move {
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call begin Device1.Connect path={path}");
                                    }
                                    if let Err(error) =
                                        bluetooth_device_call(&system, &path, "Connect").await
                                    {
                                        eprintln!("xbar: BlueZ Connect failed: {error}");
                                    }
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call end Device1.Connect path={path}");
                                    }
                                    in_flight
                                        .lock()
                                        .expect("Bluetooth in-flight lock poisoned")
                                        .remove(&action);
                                    push_event(
                                        &events,
                                        &wake,
                                        Event::BluetoothActionFinished(action),
                                    );
                                },
                                "xbar-bluetooth-command",
                            )
                            .detach();
                    } else if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: Bluetooth action suppressed in-flight ConnectDevice path={path}");
                    }
                } else {
                    eprintln!("xbar: BlueZ Connect skipped: system bus unavailable");
                }
            }
            Either::Request(Ok(Request::BluetoothDisconnectDevice(path))) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: DBusWorker receive DisconnectDevice path={path}");
                    eprintln!("xbar trace: DBus call org.bluez.Device1.Disconnect path={path}");
                }
                if let Some((system, executor)) = &system_connection {
                    let action = BluetoothPendingAction::DisconnectDevice(path.clone());
                    let should_start = bluetooth_in_flight
                        .lock()
                        .expect("Bluetooth in-flight lock poisoned")
                        .insert(action.clone());
                    if should_start {
                        let system = system.clone();
                        let events = Arc::clone(&events);
                        let wake = Arc::clone(&wake);
                        let in_flight = Arc::clone(&bluetooth_in_flight);
                        executor
                            .spawn(
                                async move {
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call begin Device1.Disconnect path={path}");
                                    }
                                    if let Err(error) =
                                        bluetooth_device_call(&system, &path, "Disconnect").await
                                    {
                                        eprintln!("xbar: BlueZ Disconnect failed: {error}");
                                    }
                                    if std::env::var_os("XBAR_TRACE").is_some() {
                                        eprintln!("xbar trace: DBus call end Device1.Disconnect path={path}");
                                    }
                                    in_flight
                                        .lock()
                                        .expect("Bluetooth in-flight lock poisoned")
                                        .remove(&action);
                                    push_event(
                                        &events,
                                        &wake,
                                        Event::BluetoothActionFinished(action),
                                    );
                                },
                                "xbar-bluetooth-command",
                            )
                            .detach();
                    } else if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: Bluetooth action suppressed in-flight DisconnectDevice path={path}");
                    }
                } else {
                    eprintln!("xbar: BlueZ Disconnect skipped: system bus unavailable");
                }
            }
            Either::Request(Ok(Request::NotificationTimerFired)) => {
                let ids =
                    notifications::expire(&notification_store, &notification_timer, &events, &wake);
                if !ids.is_empty() {
                    let emitter = zbus::object_server::SignalEmitter::new(
                        &connection,
                        "/org/freedesktop/Notifications",
                    )?;
                    for id in ids {
                        NotificationServer::notification_closed(&emitter, id.0, REASON_EXPIRED)
                            .await?;
                    }
                }
            }
            Either::Request(Ok(Request::WindowAttention {
                window,
                app_name,
                attention,
            })) => {
                notification_store
                    .lock()
                    .expect("notification store poisoned")
                    .attention(window, app_name, attention);
                notifications::publish(&notification_store, &notification_timer, &events, &wake);
            }
            Either::Request(Ok(Request::AiUsageSnapshot { owner, payload })) => {
                match ai_usage.accept_snapshot(&owner, &payload) {
                    Ok(ai_usage::SnapshotDisposition::Accepted(usage)) => {
                        if std::env::var_os("XBAR_TRACE").is_some() {
                            eprintln!(
                                "xbar trace: AI_BRIDGE_QUEUED owner={owner} agents={}",
                                usage.len()
                            );
                        }
                        push_event(&events, &wake, Event::ActiveAiUsageChanged(usage));
                    }
                    Ok(ai_usage::SnapshotDisposition::Rejected) => {}
                    Err(error) => {
                        if std::env::var_os("XBAR_TRACE").is_some() {
                            eprintln!("xbar trace: AI_USAGE_SNAPSHOT_REJECTED reason={error}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn status_notifier_action(
    connection: &zbus::Connection,
    endpoint: &StatusNotifierEndpoint,
    action: StatusNotifierAction,
    root_x: i32,
    root_y: i32,
) -> Result<(), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        zbus::names::OwnedBusName::try_from(endpoint.service.clone()).map_err(|e| e.to_string())?,
        OwnedObjectPath::try_from(endpoint.object_path.clone()).map_err(|e| e.to_string())?,
        "org.kde.StatusNotifierItem",
    )
    .await
    .map_err(|e| e.to_string())?;
    let method = match action {
        StatusNotifierAction::Activate => "Activate",
        StatusNotifierAction::SecondaryActivate => "SecondaryActivate",
        StatusNotifierAction::ContextMenu => "ContextMenu",
        StatusNotifierAction::Scroll { .. } => "Scroll",
    };
    match action {
        StatusNotifierAction::Scroll { delta, orientation } => proxy
            .call_method(method, &(delta, orientation))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
        _ => proxy
            .call_method(method, &(root_x, root_y))
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
    }
}

async fn end_gmenu(
    connection: &zbus::Connection,
    endpoint: &GtkMenuEndpoint,
    groups: Vec<u32>,
) -> Result<(), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        endpoint.bus_name.clone(),
        endpoint.menu_object_path.clone(),
        "org.gtk.Menus",
    )
    .await
    .map_err(|error| error.to_string())?;
    let _: () = proxy
        .call("End", &(groups,))
        .await
        .map_err(|error| error.to_string())?;
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!(
            "xbar trace: GMenu End bus={} path={}",
            endpoint.bus_name, endpoint.menu_object_path
        );
    }
    Ok(())
}

async fn activate(connection: &zbus::Connection, request: &ActivateRequest) -> Result<(), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        request.endpoint.service.clone(),
        request.endpoint.object_path.clone(),
        DBUSMENU_INTERFACE,
    )
    .await
    .map_err(|error| error.to_string())?;
    let data = zbus::zvariant::Value::from(0_i32);
    let _: () = proxy
        .call(
            "Event",
            &(request.item_id.0, "clicked", data, request.timestamp),
        )
        .await
        .map_err(|error| error.to_string())?;
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!(
            "xbar trace: DBusMenu Event clicked item={} timestamp={}",
            request.item_id.0, request.timestamp
        );
    }
    Ok(())
}

async fn activate_gmenu(
    connection: &zbus::Connection,
    request: &GtkActivateRequest,
) -> Result<(), String> {
    let path = request
        .endpoint
        .actions_object_paths
        .first()
        .cloned()
        .unwrap_or_else(|| request.endpoint.menu_object_path.clone());
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        request.endpoint.bus_name.clone(),
        path,
        "org.gtk.Actions",
    )
    .await
    .map_err(|error| error.to_string())?;
    let parameter = request
        .target
        .as_ref()
        .map(menu_action_target)
        .transpose()?
        .into_iter()
        .collect::<Vec<_>>();
    let platform_data: HashMap<String, zbus::zvariant::Value<'static>> = HashMap::new();
    let _: () = proxy
        .call(
            "Activate",
            &(request.action.as_str(), parameter, platform_data),
        )
        .await
        .map_err(|error| error.to_string())?;
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: GMenu Activate action={}", request.action);
    }
    Ok(())
}

fn menu_action_target(target: &MenuActionTarget) -> Result<zbus::zvariant::Value<'static>, String> {
    Ok(match target {
        MenuActionTarget::String(value) => zbus::zvariant::Value::from(value.clone()),
        MenuActionTarget::Boolean(value) => zbus::zvariant::Value::from(*value),
        MenuActionTarget::Int32(value) => zbus::zvariant::Value::from(*value),
        MenuActionTarget::Uint32(value) => zbus::zvariant::Value::from(*value),
    })
}

enum Either<O, R> {
    Owner(O),
    Request(R),
    Network,
}

async fn about_to_show(
    connection: &zbus::Connection,
    request: &AboutRequest,
) -> Result<(bool, Option<crate::core::MenuModel>), String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        request.endpoint.service.clone(),
        request.endpoint.object_path.clone(),
        DBUSMENU_INTERFACE,
    )
    .await
    .map_err(|e| e.to_string())?;
    let need_update: bool = proxy
        .call("AboutToShow", &(request.item_id.0,))
        .await
        .map_err(|e| e.to_string())?;
    if need_update {
        let layout_request = LayoutRequest {
            window_id: request.window_id,
            endpoint: request.endpoint.clone(),
            request_id: request.request_id,
        };
        Ok((true, Some(load_layout(connection, &layout_request).await?)))
    } else {
        Ok((false, None))
    }
}

async fn load_layout(
    connection: &zbus::Connection,
    request: &LayoutRequest,
) -> Result<crate::core::MenuModel, String> {
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        request.endpoint.service.clone(),
        request.endpoint.object_path.clone(),
        DBUSMENU_INTERFACE,
    )
    .await
    .map_err(|error| error.to_string())?;
    let properties = vec![
        "label",
        "enabled",
        "visible",
        "type",
        "children-display",
        "shortcut",
        "icon-name",
    ];
    let (revision, wire_layout): (u32, menu::WireLayoutNode) = proxy
        .call("GetLayout", &(0_i32, -1_i32, properties))
        .await
        .map_err(|error| error.to_string())?;
    menu::convert_layout(revision, menu::parse_wire_layout(wire_layout)?)
}

async fn load_gmenu(
    connection: &zbus::Connection,
    endpoint: &GtkMenuEndpoint,
    revision: u64,
) -> Result<(crate::core::MenuModel, Vec<u32>), String> {
    let menu_proxy = zbus::Proxy::new_owned(
        connection.clone(),
        endpoint.bus_name.clone(),
        endpoint.menu_object_path.clone(),
        "org.gtk.Menus",
    )
    .await
    .map_err(|error| error.to_string())?;
    let mut content: Vec<gmenu::RawMenu> = menu_proxy
        .call("Start", &(vec![0_u32],))
        .await
        .map_err(|error| error.to_string())?;
    let mut requested_groups = HashSet::from([0_u32]);
    let mut groups = gmenu::referenced_groups(&content);
    while !groups.is_empty() {
        groups.retain(|group| requested_groups.insert(*group));
        if groups.is_empty() {
            break;
        }
        let loaded: Vec<gmenu::RawMenu> = menu_proxy
            .call("Start", &(groups.clone(),))
            .await
            .map_err(|error| error.to_string())?;
        if loaded.is_empty() {
            break;
        }
        content.extend(loaded);
        groups = gmenu::referenced_groups(&content);
    }
    let action_path = endpoint
        .actions_object_paths
        .first()
        .cloned()
        .unwrap_or_else(|| endpoint.menu_object_path.clone());
    let action_proxy = zbus::Proxy::new_owned(
        connection.clone(),
        endpoint.bus_name.clone(),
        action_path,
        "org.gtk.Actions",
    )
    .await
    .map_err(|error| error.to_string())?;
    let descriptions: HashMap<String, (bool, Signature, Vec<zbus::zvariant::OwnedValue>)> =
        action_proxy
            .call("DescribeAll", &())
            .await
            .map_err(|error| error.to_string())?;
    let actions = descriptions
        .into_iter()
        .map(|(name, (enabled, _, _))| (name, enabled))
        .collect();
    let mut groups: Vec<_> = requested_groups.into_iter().collect();
    groups.sort_unstable();
    Ok((
        gmenu::convert_start(revision.min(u32::MAX as u64) as u32, content, &actions)?,
        groups,
    ))
}

fn install_signal_watcher(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    endpoint: crate::core::MenuEndpoint,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let proxy = match zbus::Proxy::new_owned(
                    connection,
                    endpoint.service.clone(),
                    endpoint.object_path.clone(),
                    DBUSMENU_INTERFACE,
                )
                .await
                {
                    Ok(proxy) => proxy,
                    Err(_) => return,
                };
                let mut signals = match proxy.receive_all_signals().await {
                    Ok(signals) => signals,
                    Err(_) => return,
                };
                while let Some(signal) = signals.next().await {
                    match signal.header().member().map(|member| member.as_str()) {
                        Some("LayoutUpdated") => {
                            let (revision, _parent): (u32, i32) = match signal.body().deserialize()
                            {
                                Ok(args) => args,
                                Err(_) => continue,
                            };
                            push_event(
                                &events,
                                &wake,
                                Event::MenuLayoutInvalidated {
                                    endpoint: MenuSource::DbusMenu(endpoint.clone()),
                                    revision: Some(revision),
                                },
                            );
                        }
                        Some("ItemsPropertiesUpdated") => {
                            let (updated, removed): PropertiesSignal =
                                match signal.body().deserialize() {
                                    Ok(args) => args,
                                    Err(_) => continue,
                                };
                            if let Ok(updates) = menu::convert_property_updates(updated, removed) {
                                push_event(
                                    &events,
                                    &wake,
                                    Event::MenuPropertiesUpdated {
                                        endpoint: MenuSource::DbusMenu(endpoint.clone()),
                                        updates,
                                    },
                                );
                            }
                        }
                        _ => {}
                    }
                }
            },
            "xbar-dbusmenu-signals",
        )
        .detach();
}

fn install_gmenu_signal_watcher(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    endpoint: GtkMenuEndpoint,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let proxy = match zbus::Proxy::new_owned(
                    connection,
                    endpoint.bus_name.clone(),
                    endpoint.menu_object_path.clone(),
                    "org.gtk.Menus",
                )
                .await
                {
                    Ok(proxy) => proxy,
                    Err(_) => return,
                };
                let mut signals = match proxy.receive_signal("Changed").await {
                    Ok(signals) => signals,
                    Err(_) => return,
                };
                while signals.next().await.is_some() {
                    push_event(
                        &events,
                        &wake,
                        Event::MenuLayoutInvalidated {
                            endpoint: MenuSource::GtkGMenu(endpoint.clone()),
                            revision: None,
                        },
                    );
                }
            },
            "xbar-gmenu-signals",
        )
        .detach();
}

pub(crate) fn push_event(events: &EventQueue, wake: &Arc<Mutex<UnixStream>>, event: Event) {
    events
        .lock()
        .expect("DBus event queue poisoned")
        .push_back(event);
    let _ = wake.lock().expect("DBus wake poisoned").write_all(&[1]);
}

#[cfg(test)]
mod status_notifier_tests {
    use super::{choose_sni_icon, select_pixmap};
    use crate::core::{
        format_notifier_item_id, parse_notifier_item_id, StatusNotifierEndpoint,
        StatusNotifierIcon, StatusNotifierStatus,
    };

    #[test]
    fn registration_forms_resolve_to_service_and_path() {
        let service_form = StatusNotifierEndpoint {
            service: "org.example.Item".into(),
            object_path: "/StatusNotifierItem".into(),
        };
        assert_eq!(
            parse_notifier_item_id(&format_notifier_item_id(&service_form)),
            Some(service_form)
        );
        let endpoint =
            parse_notifier_item_id(":1.50/StatusNotifierItem/2").expect("path form canonical id");
        assert_eq!(endpoint.service, ":1.50");
        assert_eq!(endpoint.object_path, "/StatusNotifierItem/2");
        assert!(parse_notifier_item_id("/StatusNotifierItem").is_none());
    }

    #[test]
    fn pixmap_selection_targets_sixteen_pixels_and_rejects_invalid_data() {
        let selected = select_pixmap(vec![
            (32, 32, vec![0; 32 * 32 * 4]),
            (16, 16, [0xff, 0, 0, 0].repeat(16 * 16)),
        ])
        .expect("valid pixmap");
        assert_eq!(
            selected,
            StatusNotifierIcon::Pixmap {
                width: 16,
                height: 16,
                argb: vec![0xff00_0000; 16 * 16],
            }
        );
        assert!(select_pixmap(vec![(16, 16, vec![0; 3])]).is_none());
    }

    #[test]
    fn attention_status_prefers_attention_and_falls_back_to_normal() {
        let normal = StatusNotifierIcon::Pixmap {
            width: 16,
            height: 16,
            argb: vec![1],
        };
        let attention = StatusNotifierIcon::Pixmap {
            width: 16,
            height: 16,
            argb: vec![2],
        };
        assert_eq!(
            choose_sni_icon(
                &StatusNotifierStatus::NeedsAttention,
                Some(normal.clone()),
                Some(attention.clone()),
            ),
            Some(attention)
        );
        assert_eq!(
            choose_sni_icon(
                &StatusNotifierStatus::NeedsAttention,
                Some(normal.clone()),
                None
            ),
            Some(normal)
        );
    }
}

#[cfg(test)]
mod network_inventory_tests {
    use super::deduplicate_wifi_access_points;
    use crate::core::NetworkAccessPoint;

    fn ap(ssid: &str, frequency: u32, strength: u8, active: bool) -> NetworkAccessPoint {
        NetworkAccessPoint {
            path: format!("/ap/{ssid}/{frequency}/{strength}"),
            device_path: "/device/0".into(),
            interface: "wlan0".into(),
            ssid: ssid.into(),
            strength,
            frequency,
            is_active: active,
            saved_profile: None,
        }
    }

    #[test]
    fn empty_ssids_are_filtered_without_synthesizing_hidden_rows() {
        let candidates = deduplicate_wifi_access_points(vec![
            ap("", 2412, 90, false),
            ap("   ", 5180, 80, false),
            ap("Visible", 2412, 70, false),
        ]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].ssid, "Visible");
        assert!(candidates.iter().all(|candidate| {
            !candidate.ssid.is_empty() && !candidate.ssid.eq_ignore_ascii_case("hidden")
        }));
    }

    #[test]
    fn saved_and_unsaved_networks_are_both_candidates() {
        let mut saved = ap("Saved", 2412, 50, false);
        saved.saved_profile = Some("/profile/saved".into());
        let unsaved = ap("Unsaved", 2412, 60, false);
        let other_unsaved = ap("Other", 5180, 55, false);
        let candidates = deduplicate_wifi_access_points(vec![saved, unsaved, other_unsaved]);
        assert_eq!(candidates.len(), 3);
        assert!(candidates
            .iter()
            .any(|candidate| candidate.saved_profile.is_none()));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.saved_profile.is_some()));
    }

    #[test]
    fn strongest_ap_wins_per_ssid_and_band_while_bands_are_preserved() {
        let candidates = deduplicate_wifi_access_points(vec![
            ap("Foo", 5180, 40, false),
            ap("Foo", 5200, 75, true),
            ap("Foo", 2412, 55, false),
        ]);
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            candidates
                .iter()
                .find(|candidate| candidate.frequency == 5200)
                .map(|candidate| candidate.strength),
            Some(75)
        );
        assert!(candidates
            .iter()
            .any(|candidate| candidate.frequency == 2412));
        assert!(candidates
            .iter()
            .find(|candidate| candidate.frequency == 5200)
            .is_some_and(|candidate| candidate.is_active));
    }

    #[test]
    fn inventory_does_not_truncate_large_candidate_sets() {
        let raw = (0..64)
            .map(|index| ap(&format!("Network-{index}"), 2412, index as u8, false))
            .collect();
        assert_eq!(deduplicate_wifi_access_points(raw).len(), 64);
    }

    #[test]
    fn same_ssid_on_different_devices_is_not_globally_deduplicated() {
        let wlan0 = deduplicate_wifi_access_points(vec![ap("Foo", 2412, 80, true)]);
        let mut wlan1_ap = ap("Foo", 2412, 70, true);
        wlan1_ap.device_path = "/device/1".into();
        wlan1_ap.interface = "wlan1".into();
        let wlan1 = deduplicate_wifi_access_points(vec![wlan1_ap]);
        assert_eq!(wlan0.len(), 1);
        assert_eq!(wlan1.len(), 1);
        assert_ne!(wlan0[0].device_path, wlan1[0].device_path);
    }
}
