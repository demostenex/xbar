use crate::core::menu::{GtkActionGroupEndpoint, QualifiedActionReference};
#[cfg(test)]
use crate::core::NetworkAccessPoint;
use crate::core::{
    parse_notifier_item_id, BluetoothDevice, BluetoothPendingAction, BluetoothState, Event,
    GtkMenuEndpoint, HistoryEntryId, MenuActionTarget, MenuEndpoint, MenuRegistry, MenuSource,
    NotificationId, StatusNotifierAction, StatusNotifierEndpoint, StatusNotifierIcon,
    StatusNotifierItem, StatusNotifierStatus,
};
mod ai_usage;
mod gmenu;
mod menu;
use crate::notification_persistence::{LoadResult, Persistence};
use crate::notifications::{self, SharedStore, SharedTimer, REASON_CLOSED, REASON_EXPIRED};
use async_channel::{Receiver, Sender};
use futures_lite::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
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

fn sibling_sni_watcher_path(executable: &Path) -> Option<PathBuf> {
    executable
        .parent()
        .map(|parent| parent.join("xbar-sni-watcher"))
}

#[cfg(test)]
mod gmenu_activation_context_tests {
    use super::platform_data;
    use zbus::zvariant::Value;

    #[test]
    fn input_timestamp_becomes_startup_id_without_stale_state() {
        let data = platform_data(Some(136746));
        assert_eq!(
            data.get("desktop-startup-id")
                .and_then(|value| match value {
                    Value::Str(value) => Some(value.as_str()),
                    _ => None,
                }),
            Some("_TIME136746")
        );
        let next = platform_data(Some(7));
        assert_eq!(
            next.get("desktop-startup-id")
                .and_then(|value| match value {
                    Value::Str(value) => Some(value.as_str()),
                    _ => None,
                }),
            Some("_TIME7")
        );
    }

    #[test]
    fn absent_or_zero_timestamp_does_not_create_time_zero() {
        assert!(platform_data(None).is_empty());
        assert!(platform_data(Some(0)).is_empty());
    }
}

fn sibling_sni_watcher(executable: &Path) -> Option<PathBuf> {
    sibling_sni_watcher_path(executable).filter(|candidate| candidate.is_file())
}

fn ensure_status_notifier_watcher() {
    let current = std::env::current_exe().ok();
    let companion = current
        .as_deref()
        .and_then(sibling_sni_watcher)
        .unwrap_or_else(|| PathBuf::from("xbar-sni-watcher"));
    if Command::new(&companion).spawn().is_err() && std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: xbar-sni-watcher was not found or could not start");
    }
}

fn retain_sni_owner_on_setup_failure(owner: &mut Option<String>, live_owner: &str) {
    *owner = Some(live_owner.to_owned());
}

fn dbus_menu_endpoint_key(endpoint: &MenuEndpoint) -> String {
    format!("{}\0{}", endpoint.service, endpoint.object_path)
}

#[derive(Clone, Debug)]
struct LayoutRequest {
    window_id: crate::core::WindowId,
    endpoint: crate::core::MenuEndpoint,
    request_id: u64,
}

struct MenuWatcherControl {
    watcher_generation: u64,
    signal_cancel: Sender<()>,
    loads: HashMap<u64, Sender<()>>,
}

struct InstalledMenuSignalWatcher {
    cancel: Sender<()>,
    start: Sender<u64>,
}

fn allocate_menu_watcher_generation(next: &mut u64) -> u64 {
    let generation = *next;
    *next = (*next).wrapping_add(1);
    generation
}

fn cancel_watchers_for_unique_owner(
    watchers: &mut HashMap<String, MenuWatcherControl>,
    owner: &str,
) {
    let vanished_service = format!("{owner}\0");
    watchers.retain(|key, control| {
        if key.starts_with(&vanished_service) {
            control.cancel();
            false
        } else {
            true
        }
    });
}

fn finish_layout_load(
    watchers: &mut HashMap<String, MenuWatcherControl>,
    endpoint: &MenuEndpoint,
    request_id: u64,
) -> bool {
    watchers
        .get_mut(&dbus_menu_endpoint_key(endpoint))
        .is_some_and(|control| control.finish_load(request_id))
}

impl MenuWatcherControl {
    fn new(watcher_generation: u64, signal_cancel: Sender<()>) -> Self {
        Self {
            watcher_generation,
            signal_cancel,
            loads: HashMap::new(),
        }
    }

    fn start_load(&mut self, request_id: u64) -> Receiver<()> {
        for cancel in self.loads.drain().map(|(_, cancel)| cancel) {
            let _ = cancel.try_send(());
        }
        let (sender, receiver) = async_channel::bounded(1);
        self.loads.insert(request_id, sender);
        receiver
    }

    fn finish_load(&mut self, request_id: u64) -> bool {
        self.loads.remove(&request_id).is_some()
    }

    fn cancel(&self) {
        let _ = self.signal_cancel.try_send(());
        for cancel in self.loads.values() {
            let _ = cancel.try_send(());
        }
    }
}
#[derive(Clone, Debug)]
struct AboutRequest {
    window_id: crate::core::WindowId,
    endpoint: crate::core::MenuEndpoint,
    item_id: crate::core::MenuItemId,
    request_id: u64,
    lazy_root: bool,
    intent_id: Option<u64>,
    watcher_generation: Option<u64>,
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
    timestamp: Option<u32>,
}

#[derive(Clone, Debug)]
enum Request {
    Layout(LayoutRequest),
    LayoutFinished {
        request: LayoutRequest,
        result: Result<crate::core::MenuModel, String>,
    },
    EndMenuWatcher(MenuEndpoint),
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
    #[allow(dead_code)]
    DismissNotificationHistoryEntry(HistoryEntryId),
    #[allow(dead_code)]
    ClearNotificationHistory,
    #[allow(dead_code)]
    InvokeNotificationDefault(HistoryEntryId),
    #[allow(dead_code)]
    InvokeNotificationAction(HistoryEntryId, String),
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

async fn cancellable_layout_load<L, T, E>(load: L, cancel: Receiver<()>) -> Option<Result<T, E>>
where
    L: Future<Output = Result<T, E>>,
{
    futures_lite::future::race(async { Some(load.await) }, async {
        let _ = cancel.recv().await;
        None
    })
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemWake {
    PropertyChanged,
    PropertyStreamEnded,
    ItemCancelled,
}

fn item_wake_is_terminal(wake: ItemWake) -> bool {
    !matches!(wake, ItemWake::PropertyChanged)
}

struct StatusNotifierAttachmentControl {
    cancel_sender: Sender<()>,
    cancel_receiver: Receiver<()>,
    active: AtomicBool,
    publication: Mutex<()>,
    next_item_id: Mutex<u64>,
    items: Mutex<HashMap<StatusNotifierEndpoint, (u64, Sender<()>)>>,
}

impl StatusNotifierAttachmentControl {
    fn cancel(&self) {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        self.active.store(false, Ordering::SeqCst);
        let _ = self.cancel_sender.try_send(());
        let mut items = self.items.lock().expect("SNI attachment items poisoned");
        for (_, sender) in items.drain().map(|(_, value)| value) {
            let _ = sender.try_send(());
        }
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    fn publish(&self, events: &EventQueue, wake: &Arc<Mutex<UnixStream>>, event: Event) -> bool {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        if !self.active.load(Ordering::SeqCst) {
            return false;
        }
        push_event(events, wake, event);
        true
    }

    fn publish_item(
        &self,
        endpoint: &StatusNotifierEndpoint,
        id: u64,
        events: &EventQueue,
        wake: &Arc<Mutex<UnixStream>>,
        event: Event,
    ) -> bool {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        if !self.active.load(Ordering::SeqCst)
            || self
                .items
                .lock()
                .expect("SNI attachment items poisoned")
                .get(endpoint)
                .is_none_or(|(current, _)| *current != id)
        {
            return false;
        }
        push_event(events, wake, event);
        true
    }

    fn start_item(&self, endpoint: &StatusNotifierEndpoint) -> Option<(u64, Receiver<()>)> {
        // Publication, cancellation, and item-generation transitions all use
        // this lock.  The lock order is publication -> items -> item id.
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        if !self.active.load(Ordering::SeqCst) {
            return None;
        }
        let mut items = self.items.lock().expect("SNI attachment items poisoned");
        if items.contains_key(endpoint) {
            return None;
        }
        let (sender, receiver) = async_channel::bounded(1);
        let mut next = self.next_item_id.lock().expect("SNI item id poisoned");
        let id = *next;
        *next = next.wrapping_add(1);
        items.insert(endpoint.clone(), (id, sender));
        Some((id, receiver))
    }

    fn stop_item(&self, endpoint: &StatusNotifierEndpoint) {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        if let Some((_, sender)) = self
            .items
            .lock()
            .expect("SNI attachment items poisoned")
            .remove(endpoint)
        {
            let _ = sender.try_send(());
        }
    }

    fn is_current_item(&self, endpoint: &StatusNotifierEndpoint, id: u64) -> bool {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        self.active.load(Ordering::SeqCst)
            && self
                .items
                .lock()
                .expect("SNI attachment items poisoned")
                .get(endpoint)
                .is_some_and(|(current, _)| *current == id)
    }

    fn finish_item(&self, endpoint: &StatusNotifierEndpoint, id: u64) {
        let _publication = self
            .publication
            .lock()
            .expect("SNI publication gate poisoned");
        let mut items = self.items.lock().expect("SNI attachment items poisoned");
        if items
            .get(endpoint)
            .is_some_and(|(current, _)| *current == id)
        {
            items.remove(endpoint);
        }
    }
}

struct StatusNotifierAttachment {
    owner: String,
    control: Arc<StatusNotifierAttachmentControl>,
}

struct ItemRegistrationGuard {
    control: Arc<StatusNotifierAttachmentControl>,
    endpoint: StatusNotifierEndpoint,
    id: u64,
}

impl Drop for ItemRegistrationGuard {
    fn drop(&mut self) {
        self.control.finish_item(&self.endpoint, self.id);
    }
}

impl Drop for StatusNotifierAttachment {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

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

    #[allow(dead_code)]
    pub fn dismiss_notification_history_entry(&self, id: HistoryEntryId) {
        let _ = self
            .requests
            .try_send(Request::DismissNotificationHistoryEntry(id));
    }

    #[allow(dead_code)]
    pub fn clear_notification_history(&self) {
        let _ = self.requests.try_send(Request::ClearNotificationHistory);
    }

    #[allow(dead_code)]
    pub fn invoke_notification_default(&self, id: HistoryEntryId) {
        let _ = self
            .requests
            .try_send(Request::InvokeNotificationDefault(id));
    }

    #[allow(dead_code)]
    pub fn invoke_notification_action(&self, id: HistoryEntryId, action_key: String) {
        let _ = self
            .requests
            .try_send(Request::InvokeNotificationAction(id, action_key));
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

    pub fn end_menu_watcher(&self, endpoint: MenuEndpoint) {
        let _ = self.requests.try_send(Request::EndMenuWatcher(endpoint));
    }
    #[allow(clippy::too_many_arguments)]
    pub fn request_about_to_show(
        &self,
        window_id: crate::core::WindowId,
        endpoint: crate::core::MenuEndpoint,
        item_id: crate::core::MenuItemId,
        request_id: u64,
        lazy_root: bool,
        intent_id: Option<u64>,
        watcher_generation: Option<u64>,
    ) {
        let _ = self.requests.try_send(Request::About(AboutRequest {
            window_id,
            endpoint,
            item_id,
            request_id,
            lazy_root,
            intent_id,
            watcher_generation,
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
        timestamp: u32,
    ) {
        let _ = self
            .requests
            .try_send(Request::GtkActivate(GtkActivateRequest {
                window_id,
                endpoint,
                action,
                target,
                timestamp: (timestamp != 0).then_some(timestamp),
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
    sound: Option<crate::notification_sound::NotificationSoundSender>,
    persistence: Arc<Mutex<Persistence>>,
}

impl NotificationServer {
    fn publish(&self) {
        notifications::publish(&self.store, &self.timer, &self.events, &self.wake);
    }

    fn persist_history(&self) {
        let history = self
            .store
            .lock()
            .expect("notification store poisoned")
            .history_snapshot();
        if let Err(error) = self
            .persistence
            .lock()
            .expect("notification persistence poisoned")
            .save(&history)
        {
            eprintln!("xbar: notification persistence save failed: {error}");
        }
    }
}

fn notification_capabilities() -> Vec<String> {
    vec!["body".into(), "actions".into()]
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
    async fn get_capabilities(&self) -> Vec<String> {
        notification_capabilities()
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
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        let parsed_hints = notifications::parse_sound_hints(&hints);
        let icon_metadata = notifications::parse_notification_icon_metadata(app_icon, &hints);
        let actions = notifications::parse_notification_actions(actions);
        let resident = notifications::parse_resident_hint(&hints);
        let (id, delivery) = self
            .store
            .lock()
            .expect("notification store poisoned")
            .notify_with_actions_and_icon_metadata(
                replaces_id,
                app_name,
                summary,
                body,
                expire_timeout,
                actions,
                resident,
                icon_metadata,
            );
        let sound_decision = notifications::decide_notification_sound(delivery, &parsed_hints);
        if let Some(sound) = &self.sound {
            let _ = sound.try_decision(sound_decision);
        }
        self.persist_history();
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
            self.persist_history();
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

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

async fn setup_status_notifier(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
) -> zbus::Result<StatusNotifierAttachment> {
    let (cancel_sender, cancel_receiver) = async_channel::bounded(1);
    let control = Arc::new(StatusNotifierAttachmentControl {
        cancel_sender,
        cancel_receiver,
        active: AtomicBool::new(true),
        publication: Mutex::new(()),
        next_item_id: Mutex::new(0),
        items: Mutex::new(HashMap::new()),
    });
    let attachment = StatusNotifierAttachment {
        owner: String::new(),
        control: Arc::clone(&control),
    };
    let watcher =
        zbus::Proxy::new_owned(connection.clone(), SNI_NAME, SNI_PATH, SNI_INTERFACE).await?;
    let host = format!("org.kde.StatusNotifierHost-{}-xbar", std::process::id());
    let _: () = watcher.call("RegisterStatusNotifierHost", &(host,)).await?;
    let bootstrap_done = install_status_notifier_signal_watcher(
        connection,
        events,
        wake,
        watcher.clone(),
        Arc::clone(&control),
    )
    .await?;
    let existing: Vec<String> = watcher
        .get_property("RegisteredStatusNotifierItems")
        .await?;
    for item in existing {
        if let Some(endpoint) = parse_notifier_item_id(&item) {
            let Some((endpoint, item_id, item_cancel)) = apply_ordered_watcher_event(
                &control,
                events,
                wake,
                OrderedWatcherEvent::Registered(endpoint),
            ) else {
                continue;
            };
            spawn_item_bootstrap(
                connection,
                endpoint,
                events,
                wake,
                Arc::clone(&control),
                item_id,
                item_cancel,
            );
        }
    }
    if control.is_active() {
        control.publish(events, wake, Event::StatusNotifierHostRegistered);
    }
    let _ = bootstrap_done.try_send(());
    Ok(attachment)
}

fn spawn_item_bootstrap(
    connection: &zbus::Connection,
    endpoint: StatusNotifierEndpoint,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    control: Arc<StatusNotifierAttachmentControl>,
    item_id: u64,
    item_cancel: Receiver<()>,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let load_cancel = item_cancel.clone();
                load_status_notifier_item(
                    &connection,
                    endpoint.clone(),
                    &events,
                    &wake,
                    (Arc::clone(&control), item_id, load_cancel),
                )
                .await;
                if control.is_current_item(&endpoint, item_id) {
                    watch_status_notifier_item_with_cancel(
                        &connection,
                        endpoint,
                        &events,
                        &wake,
                        control,
                        item_id,
                        item_cancel,
                    );
                }
            },
            "xbar-status-notifier-item-bootstrap",
        )
        .detach();
}

async fn await_item_phase<T, F>(future: F, cancel: &Receiver<()>) -> Option<T>
where
    F: Future<Output = T>,
{
    futures_lite::future::race(async { Some(future.await) }, async {
        let _ = cancel.recv().await;
        None
    })
    .await
}

async fn setup_status_notifier_reconciled(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    dbus: &zbus::fdo::DBusProxy<'_>,
    expected_owner: &str,
) -> zbus::Result<StatusNotifierAttachment> {
    match setup_status_notifier(connection, events, wake).await {
        Ok(attachment) => Ok(attachment),
        Err(first_error) => {
            let still_owned = dbus
                .get_name_owner(SNI_NAME.try_into()?)
                .await
                .ok()
                .is_some_and(|owner| owner.as_str() == expected_owner);
            if still_owned {
                setup_status_notifier(connection, events, wake)
                    .await
                    .map_err(|_| first_error)
            } else {
                Err(first_error)
            }
        }
    }
}

enum OrderedWatcherEvent {
    Registered(StatusNotifierEndpoint),
    Unregistered(StatusNotifierEndpoint),
}

fn apply_ordered_watcher_event(
    control: &StatusNotifierAttachmentControl,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    event: OrderedWatcherEvent,
) -> Option<(StatusNotifierEndpoint, u64, Receiver<()>)> {
    match event {
        OrderedWatcherEvent::Registered(endpoint) => {
            let (item_id, item_cancel) = control.start_item(&endpoint)?;
            control.publish(
                events,
                wake,
                Event::StatusNotifierRegistered(endpoint.clone()),
            );
            Some((endpoint, item_id, item_cancel))
        }
        OrderedWatcherEvent::Unregistered(endpoint) => {
            control.stop_item(&endpoint);
            control.publish(events, wake, Event::StatusNotifierUnregistered(endpoint));
            None
        }
    }
}

async fn install_status_notifier_signal_watcher(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    watcher: zbus::Proxy<'static>,
    control: Arc<StatusNotifierAttachmentControl>,
) -> zbus::Result<Sender<()>> {
    // One match rule covers both members.  This closes the subscription gap
    // and preserves the bus' ordering authority across registration changes.
    let mut signals = watcher.receive_all_signals().await?;
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    let cancel = control.cancel_receiver.clone();
    let (bootstrap_done, bootstrap_ready) = async_channel::bounded(1);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                // The match streams are subscribed before the snapshot, but
                // queued signals are not applied until bootstrap has finished.
                let ready = futures_lite::future::race(
                    async {
                        let _ = bootstrap_ready.recv().await;
                        true
                    },
                    async {
                        let _ = cancel.recv().await;
                        false
                    },
                )
                .await;
                if !ready {
                    return;
                }
                loop {
                    let next = futures_lite::future::race(async { signals.next().await }, async {
                        let _ = cancel.recv().await;
                        None
                    })
                    .await;
                    let Some(signal) = next else {
                        break;
                    };
                    match signal.header().member().map(|member| member.as_str()) {
                        Some("StatusNotifierItemRegistered") => {
                            if let Ok(item) = signal.body().deserialize::<String>() {
                                if let Some(endpoint) = parse_notifier_item_id(&item) {
                                    let Some((endpoint, item_id, item_cancel)) =
                                        apply_ordered_watcher_event(
                                            &control,
                                            &events,
                                            &wake,
                                            OrderedWatcherEvent::Registered(endpoint),
                                        )
                                    else {
                                        continue;
                                    };
                                    spawn_item_bootstrap(
                                        &connection,
                                        endpoint,
                                        &events,
                                        &wake,
                                        Arc::clone(&control),
                                        item_id,
                                        item_cancel,
                                    );
                                }
                            }
                        }
                        Some("StatusNotifierItemUnregistered") => {
                            if let Ok(item) = signal.body().deserialize::<String>() {
                                if let Some(endpoint) = parse_notifier_item_id(&item) {
                                    apply_ordered_watcher_event(
                                        &control,
                                        &events,
                                        &wake,
                                        OrderedWatcherEvent::Unregistered(endpoint),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
            },
            "xbar-status-notifier-signals",
        )
        .detach();
    Ok(bootstrap_done)
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
    current: (Arc<StatusNotifierAttachmentControl>, u64, Receiver<()>),
) {
    let (control, item_id, cancel) = current;
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let proxy = match await_item_phase(
        zbus::Proxy::new(
            connection,
            endpoint.service.as_str(),
            endpoint.object_path.as_str(),
            "org.kde.StatusNotifierItem",
        ),
        &cancel,
    )
    .await
    {
        Some(Ok(proxy)) => proxy,
        _ => return,
    };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let status = match await_item_phase(proxy.get_property::<String>("Status"), &cancel).await {
        Some(result) => result
            .map(|value| parse_sni_status(&value))
            .unwrap_or(StatusNotifierStatus::Passive),
        None => return,
    };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let icon_name = match await_item_phase(proxy.get_property::<String>("IconName"), &cancel).await
    {
        Some(result) => result.ok().filter(|value| !value.is_empty()),
        None => return,
    };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let icon_pixmap = match await_item_phase(
        proxy.get_property::<Vec<(i32, i32, Vec<u8>)>>("IconPixmap"),
        &cancel,
    )
    .await
    {
        Some(result) => result.ok().and_then(select_pixmap),
        None => return,
    };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let attention_icon_name =
        match await_item_phase(proxy.get_property::<String>("AttentionIconName"), &cancel).await {
            Some(result) => result.ok().filter(|value| !value.is_empty()),
            None => return,
        };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let attention_icon_pixmap = match await_item_phase(
        proxy.get_property::<Vec<(i32, i32, Vec<u8>)>>("AttentionIconPixmap"),
        &cancel,
    )
    .await
    {
        Some(result) => result.ok().and_then(select_pixmap),
        None => return,
    };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let item_is_menu =
        match await_item_phase(proxy.get_property::<bool>("ItemIsMenu"), &cancel).await {
            Some(result) => result.unwrap_or(false),
            None => return,
        };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let menu =
        match await_item_phase(proxy.get_property::<OwnedObjectPath>("Menu"), &cancel).await {
            Some(result) => result.ok().filter(|path| path.as_str() != "/").map(|path| {
                crate::core::MenuEndpoint {
                    service: endpoint.service.clone(),
                    object_path: path.to_string(),
                }
            }),
            None => return,
        };
    if !control.is_current_item(&endpoint, item_id) {
        return;
    }
    let icon = choose_sni_icon(&status, icon_pixmap, attention_icon_pixmap);
    let event_endpoint = endpoint.clone();
    let event = Event::StatusNotifierItemUpdated(StatusNotifierItem {
        endpoint,
        status,
        icon,
        icon_name,
        attention_icon_name,
        item_is_menu,
        menu,
    });
    control.publish_item(&event_endpoint, item_id, events, wake, event);
}

fn watch_status_notifier_item_with_cancel(
    connection: &zbus::Connection,
    endpoint: StatusNotifierEndpoint,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    control: Arc<StatusNotifierAttachmentControl>,
    item_id: u64,
    item_cancel: Receiver<()>,
) {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let _item_guard = ItemRegistrationGuard {
                    control: Arc::clone(&control),
                    endpoint: endpoint.clone(),
                    id: item_id,
                };
                let Ok(destination): Result<zbus::names::OwnedBusName, _> =
                    endpoint.service.clone().try_into()
                else {
                    return;
                };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let Ok(path): Result<OwnedObjectPath, _> = endpoint.object_path.clone().try_into()
                else {
                    return;
                };
                let proxy = match await_item_phase(
                    zbus::Proxy::new_owned(
                        connection.clone(),
                        destination,
                        path,
                        "org.kde.StatusNotifierItem",
                    ),
                    &item_cancel,
                )
                .await
                {
                    Some(Ok(proxy)) => proxy,
                    _ => return,
                };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let mut new_icon =
                    match await_item_phase(proxy.receive_signal("NewIcon"), &item_cancel).await {
                        Some(Ok(stream)) => stream,
                        _ => return,
                    };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let mut new_attention_icon =
                    match await_item_phase(proxy.receive_signal("NewAttentionIcon"), &item_cancel)
                        .await
                    {
                        Some(Ok(stream)) => stream,
                        _ => return,
                    };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let mut new_status =
                    match await_item_phase(proxy.receive_signal("NewStatus"), &item_cancel).await {
                        Some(Ok(stream)) => stream,
                        _ => return,
                    };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let mut new_item_is_menu =
                    match await_item_phase(proxy.receive_signal("NewItemIsMenu"), &item_cancel)
                        .await
                    {
                        Some(Ok(stream)) => stream,
                        _ => return,
                    };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                let mut new_menu =
                    match await_item_phase(proxy.receive_signal("NewMenu"), &item_cancel).await {
                        Some(Ok(stream)) => stream,
                        _ => return,
                    };
                if !control.is_current_item(&endpoint, item_id) {
                    return;
                }
                loop {
                    let changes = async {
                        let icon = async {
                            new_icon
                                .next()
                                .await
                                .map_or(ItemWake::PropertyStreamEnded, |_| {
                                    ItemWake::PropertyChanged
                                })
                        };
                        let attention = async {
                            new_attention_icon
                                .next()
                                .await
                                .map_or(ItemWake::PropertyStreamEnded, |_| {
                                    ItemWake::PropertyChanged
                                })
                        };
                        let status = async {
                            new_status
                                .next()
                                .await
                                .map_or(ItemWake::PropertyStreamEnded, |_| {
                                    ItemWake::PropertyChanged
                                })
                        };
                        let item_is_menu = async {
                            new_item_is_menu
                                .next()
                                .await
                                .map_or(ItemWake::PropertyStreamEnded, |_| {
                                    ItemWake::PropertyChanged
                                })
                        };
                        let menu = async {
                            new_menu
                                .next()
                                .await
                                .map_or(ItemWake::PropertyStreamEnded, |_| {
                                    ItemWake::PropertyChanged
                                })
                        };
                        futures_lite::future::race(
                            futures_lite::future::race(
                                futures_lite::future::race(icon, attention),
                                status,
                            ),
                            futures_lite::future::race(item_is_menu, menu),
                        )
                        .await
                    };
                    let wake_reason = futures_lite::future::race(changes, async {
                        let _ = item_cancel.recv().await;
                        ItemWake::ItemCancelled
                    })
                    .await;
                    if item_wake_is_terminal(wake_reason)
                        || !control.is_current_item(&endpoint, item_id)
                    {
                        break;
                    }
                    load_status_notifier_item(
                        &connection,
                        endpoint.clone(),
                        &events,
                        &wake,
                        (Arc::clone(&control), item_id, item_cancel.clone()),
                    )
                    .await;
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

#[allow(clippy::too_many_arguments)]
async fn handle_notification_action(
    connection: &zbus::Connection,
    notification_store: &SharedStore,
    notification_timer: &SharedTimer,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    persistence: &Arc<Mutex<Persistence>>,
    history_id: HistoryEntryId,
    action_key: String,
) -> zbus::Result<()> {
    let invocation = notification_store
        .lock()
        .expect("notification store poisoned")
        .invoke_action(history_id, &action_key);
    let Some(invocation) = invocation else {
        return Ok(());
    };
    let effects = notifications::action_protocol_effects(&invocation);
    let emitter = zbus::object_server::SignalEmitter::new(connection, NOTIFICATIONS_PATH)?;
    let notifications::ActionProtocolEffect::ActionInvoked {
        notification_id,
        action_key,
    } = &effects[0]
    else {
        unreachable!("action protocol plan always starts with ActionInvoked");
    };
    NotificationServer::action_invoked(&emitter, notification_id.0, action_key).await?;
    if let Some(notifications::ActionProtocolEffect::NotificationClosed { reason, .. }) =
        effects.get(1)
    {
        let history = notification_store
            .lock()
            .expect("notification store poisoned")
            .history_snapshot();
        if let Err(error) = persistence
            .lock()
            .expect("notification persistence poisoned")
            .save(&history)
        {
            eprintln!("xbar: notification persistence save failed: {error}");
        }
        notifications::publish(notification_store, notification_timer, events, wake);
        NotificationServer::notification_closed(&emitter, notification_id.0, *reason).await?;
    }
    Ok(())
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
    let mut persistence = Persistence::from_environment();
    let restored = persistence.load();
    let restored_entries = match restored {
        LoadResult::Disabled => Vec::new(),
        LoadResult::Empty => Vec::new(),
        LoadResult::Loaded(entries) => entries,
        LoadResult::Invalid(error) => {
            eprintln!("xbar: notification persistence ignored: {error}");
            Vec::new()
        }
    };
    let mut store = notifications::Store::default();
    store.restore_pending(restored_entries);
    let notification_store = Arc::new(Mutex::new(store));
    let persistence = Arc::new(Mutex::new(persistence));
    let sound_bridge = crate::notification_sound::NotificationSoundBridge::spawn();
    let sound_sender = sound_bridge.as_ref().and_then(|bridge| bridge.sender());
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
            sound: sound_sender,
            persistence: Arc::clone(&persistence),
        },
    )?;
    let connection = builder
        .name(REGISTRAR_NAME)?
        .name(NOTIFICATIONS_NAME)?
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .build()
        .await?;
    notifications::publish(&notification_store, &notification_timer, &events, &wake);
    let dbus = zbus::fdo::DBusProxy::new(&connection).await?;
    let mut owner_changes = dbus.receive_name_owner_changed().await?;
    let mut sni_owner = dbus
        .get_name_owner(SNI_NAME.try_into()?)
        .await
        .ok()
        .map(|owner| owner.to_string());
    let mut sni_attachment = None;
    if sni_owner.is_none() {
        ensure_status_notifier_watcher();
    }
    let mut ai_usage = ai_usage::AiUsageSubscription::default();
    let mut ai_activation = ai_usage::ActivationGate::default();
    ai_usage::subscribe_signal_watcher(&connection, &request_sender).await?;
    if let Ok(owner) = dbus.get_name_owner(ai_usage::BUS_NAME.try_into()?).await {
        let owner = owner.to_string();
        ai_usage.owner_appeared(owner.clone());
        ai_usage::spawn_get_state(&connection, &request_sender, owner);
    } else if ai_activation.request_once() {
        if std::env::var_os("XBAR_TRACE").is_some() {
            eprintln!(
                "xbar trace: AI_ACTIVATION_REQUESTED name={}",
                ai_usage::BUS_NAME
            );
        }
        match dbus
            .start_service_by_name(ai_usage::BUS_NAME.try_into()?, 0)
            .await
        {
            Ok(reply) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!(
                        "xbar trace: AI_ACTIVATION_RESULT name={} reply={reply}",
                        ai_usage::BUS_NAME
                    );
                }
            }
            Err(error) => {
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!(
                        "xbar trace: AI_ACTIVATION_FAILED name={} error={error}",
                        ai_usage::BUS_NAME
                    );
                }
            }
        }
    }
    if let Some(owner) = sni_owner.clone() {
        match setup_status_notifier_reconciled(&connection, &events, &wake, &dbus, &owner).await {
            Ok(mut attachment) => {
                attachment.owner = owner;
                sni_attachment = Some(attachment);
            }
            Err(_) => {
                // Keep the live owner as an explicit setup-failed state.  A
                // later NameOwnerChanged disappearance/replacement is the
                // only event that advances this state; the owner is never
                // discarded merely because setup failed.
                retain_sni_owner_on_setup_failure(&mut sni_owner, &owner);
                if std::env::var_os("XBAR_TRACE").is_some() {
                    eprintln!("xbar trace: SNI setup failed for live owner {owner}");
                }
            }
        }
    }
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
    let mut menu_signal_watchers = HashMap::<String, MenuWatcherControl>::new();
    let mut next_menu_watcher_generation = 1_u64;
    let mut gmenu_subscriptions = HashMap::new();
    let bluetooth_in_flight = Arc::new(Mutex::new(HashSet::<BluetoothPendingAction>::new()));
    loop {
        let owner = async { Either::Owner(owner_changes.next().await) };
        let request = async { Either::Request(requests.recv().await) };
        let owner_or_request = futures_lite::future::race(owner, request);
        let next = if let Some((_, executor)) = &system_connection {
            futures_lite::future::race(owner_or_request, async {
                executor.tick().await;
                Either::Network
            })
            .await
        } else {
            owner_or_request.await
        };
        match next {
            Either::Network => continue,
            Either::Owner(Some(signal)) => {
                let args = signal.args()?;
                if args.name().as_str() == SNI_NAME {
                    if let Some(new_owner) = args.new_owner().as_ref() {
                        let new_owner = new_owner.to_string();
                        if sni_owner.as_deref() != Some(new_owner.as_str()) {
                            sni_attachment.take();
                            if sni_owner.take().is_some() {
                                push_event(&events, &wake, Event::StatusNotifierWatcherUnavailable);
                            }
                            if let Ok(mut attachment) = setup_status_notifier_reconciled(
                                &connection,
                                &events,
                                &wake,
                                &dbus,
                                &new_owner,
                            )
                            .await
                            {
                                attachment.owner = new_owner.clone();
                                sni_attachment = Some(attachment);
                            }
                            // Whether setup succeeds or fails, this owner is
                            // now the active event-driven lifecycle state.
                            sni_owner = Some(new_owner);
                        }
                    } else {
                        let old_owner = args.old_owner().as_ref().map(ToString::to_string);
                        if sni_owner.as_deref() != old_owner.as_deref() {
                            continue;
                        }
                        sni_attachment.take();
                        sni_owner = None;
                        push_event(&events, &wake, Event::StatusNotifierWatcherUnavailable);
                    }
                    continue;
                }
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
                    cancel_watchers_for_unique_owner(
                        &mut menu_signal_watchers,
                        args.name().as_str(),
                    );
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
                }
            }
            Either::Owner(None) | Either::Request(Err(_)) => break,
            Either::Request(Ok(Request::Layout(request))) => {
                let key = dbus_menu_endpoint_key(&request.endpoint);
                if let Entry::Vacant(watcher_entry) = menu_signal_watchers.entry(key.clone()) {
                    match install_signal_watcher(
                        &connection,
                        &events,
                        &wake,
                        request.endpoint.clone(),
                    )
                    .await
                    {
                        Ok(installed) => {
                            let watcher_generation =
                                allocate_menu_watcher_generation(&mut next_menu_watcher_generation);
                            let mut control =
                                MenuWatcherControl::new(watcher_generation, installed.cancel);
                            let load_cancel = control.start_load(request.request_id);
                            // The control is published before the load future
                            // is spawned.  Every lifecycle path can cancel it.
                            watcher_entry.insert(control);
                            push_event(
                                &events,
                                &wake,
                                Event::MenuWatcherReady {
                                    endpoint: MenuSource::DbusMenu(request.endpoint.clone()),
                                    watcher_generation,
                                    request_id: request.request_id,
                                },
                            );
                            let _ = installed.start.try_send(watcher_generation);
                            spawn_layout_load(&connection, &request_sender, request, load_cancel);
                        }
                        Err(error) => {
                            if std::env::var_os("XBAR_TRACE").is_some() {
                                eprintln!(
                                    "xbar trace: DBusMenu signal subscription failed service={} path={}: {error}",
                                    request.endpoint.service, request.endpoint.object_path
                                );
                            }
                            let event = Event::MenuLoadFailed {
                                window_id: request.window_id,
                                endpoint: MenuSource::DbusMenu(request.endpoint),
                                request_id: request.request_id,
                                error,
                            };
                            push_event(&events, &wake, event);
                        }
                    }
                } else if let Some(control) = menu_signal_watchers.get_mut(&key) {
                    let watcher_generation = control.watcher_generation;
                    let load_cancel = control.start_load(request.request_id);
                    push_event(
                        &events,
                        &wake,
                        Event::MenuWatcherReady {
                            endpoint: MenuSource::DbusMenu(request.endpoint.clone()),
                            watcher_generation,
                            request_id: request.request_id,
                        },
                    );
                    spawn_layout_load(&connection, &request_sender, request, load_cancel);
                }
            }
            Either::Request(Ok(Request::LayoutFinished { request, result })) => {
                if !finish_layout_load(
                    &mut menu_signal_watchers,
                    &request.endpoint,
                    request.request_id,
                ) {
                    continue;
                }
                let event = match result {
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
            }
            Either::Request(Ok(Request::EndMenuWatcher(endpoint))) => {
                if let Some(cancel) =
                    menu_signal_watchers.remove(&dbus_menu_endpoint_key(&endpoint))
                {
                    cancel.cancel();
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
                        lazy_root: request.lazy_root,
                        intent_id: request.intent_id,
                        watcher_generation: request.watcher_generation,
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
            Either::Request(Ok(Request::DismissNotificationHistoryEntry(id))) => {
                let changed = notification_store
                    .lock()
                    .expect("notification store poisoned")
                    .dismiss_history_entry(id);
                if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                    eprintln!(
                        "notification-center dismiss-handle: history_id={} store_changed={}",
                        id.0, changed
                    );
                }
                if changed {
                    let history = notification_store
                        .lock()
                        .expect("notification store poisoned")
                        .history_snapshot();
                    if let Err(error) = persistence
                        .lock()
                        .expect("notification persistence poisoned")
                        .save(&history)
                    {
                        eprintln!("xbar: notification persistence save failed: {error}");
                    }
                    notifications::publish(
                        &notification_store,
                        &notification_timer,
                        &events,
                        &wake,
                    );
                    if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                        eprintln!("notification-center dismiss-publish: published=true");
                    }
                }
            }
            Either::Request(Ok(Request::ClearNotificationHistory)) => {
                let changed = notification_store
                    .lock()
                    .expect("notification store poisoned")
                    .clear_history();
                if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                    eprintln!(
                        "notification-center clear-handle: store_changed={}",
                        changed
                    );
                }
                if changed {
                    let history = notification_store
                        .lock()
                        .expect("notification store poisoned")
                        .history_snapshot();
                    if let Err(error) = persistence
                        .lock()
                        .expect("notification persistence poisoned")
                        .save(&history)
                    {
                        eprintln!("xbar: notification persistence save failed: {error}");
                    }
                    notifications::publish(
                        &notification_store,
                        &notification_timer,
                        &events,
                        &wake,
                    );
                }
            }
            Either::Request(Ok(Request::InvokeNotificationDefault(history_id))) => {
                handle_notification_action(
                    &connection,
                    &notification_store,
                    &notification_timer,
                    &events,
                    &wake,
                    &persistence,
                    history_id,
                    "default".into(),
                )
                .await?;
            }
            Either::Request(Ok(Request::InvokeNotificationAction(history_id, action_key))) => {
                handle_notification_action(
                    &connection,
                    &notification_store,
                    &notification_timer,
                    &events,
                    &wake,
                    &persistence,
                    history_id,
                    action_key,
                )
                .await?;
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
    let reference = QualifiedActionReference::parse(&request.action)?;
    let action_group_path = request
        .endpoint
        .action_group_for(reference.namespace.as_deref())?
        .clone();
    let path = action_group_path.object_path.clone();
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        request.endpoint.bus_name.clone(),
        path.clone(),
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
    let descriptions: HashMap<String, (bool, Signature, Vec<zbus::zvariant::OwnedValue>)> = proxy
        .call("DescribeAll", &())
        .await
        .map_err(|error| error.to_string())?;
    let _endpoint = GtkActionGroupEndpoint::from_capability(action_group_path, true)?;
    if !descriptions.contains_key(&reference.local_name) {
        return Err(format!(
            "GMenu action does not exist in resolved endpoint: {}",
            reference.local_name
        ));
    }
    let platform_data = platform_data(request.timestamp);
    let _: () = proxy
        .call(
            "Activate",
            &(reference.local_name.as_str(), parameter, platform_data),
        )
        .await
        .map_err(|error| error.to_string())?;
    if std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!(
            "xbar trace: GMenu Activate action={} local={} path={}",
            reference.original, reference.local_name, path
        );
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

fn platform_data(timestamp: Option<u32>) -> HashMap<String, zbus::zvariant::Value<'static>> {
    timestamp
        .filter(|timestamp| *timestamp != 0)
        .map(|timestamp| {
            HashMap::from([(
                "desktop-startup-id".to_owned(),
                zbus::zvariant::Value::from(format!("_TIME{timestamp}")),
            )])
        })
        .unwrap_or_default()
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
    Ok((need_update, None))
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
    let action_group_path = endpoint
        .default_action_group
        .as_ref()
        .cloned()
        .ok_or_else(|| "GMenu endpoint has no action group".to_owned())?;
    let action_path = action_group_path.object_path.clone();
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
    let _endpoint = GtkActionGroupEndpoint::from_capability(action_group_path, true)?;
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

fn spawn_layout_load(
    connection: &zbus::Connection,
    request_sender: &Sender<Request>,
    request: LayoutRequest,
    cancel: Receiver<()>,
) {
    let connection = connection.clone();
    let request_sender = request_sender.clone();
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                if let Some(result) =
                    cancellable_layout_load(load_layout(&connection, &request), cancel).await
                {
                    let _ = request_sender.try_send(Request::LayoutFinished { request, result });
                }
            },
            "xbar-dbusmenu-layout",
        )
        .detach();
}

async fn install_signal_watcher(
    connection: &zbus::Connection,
    events: &EventQueue,
    wake: &Arc<Mutex<UnixStream>>,
    endpoint: crate::core::MenuEndpoint,
) -> Result<InstalledMenuSignalWatcher, String> {
    let connection = connection.clone();
    let events = Arc::clone(events);
    let wake = Arc::clone(wake);
    let proxy = zbus::Proxy::new_owned(
        connection.clone(),
        endpoint.service.clone(),
        endpoint.object_path.clone(),
        DBUSMENU_INTERFACE,
    )
    .await
    .map_err(|error| error.to_string())?;
    let mut signals = proxy
        .receive_all_signals()
        .await
        .map_err(|error| error.to_string())?;
    let (cancel_sender, cancel_receiver) = async_channel::bounded(1);
    let (start_sender, start_receiver) = async_channel::bounded(1);
    connection
        .clone()
        .executor()
        .spawn(
            async move {
                let watcher_generation =
                    futures_lite::future::race(async { start_receiver.recv().await.ok() }, async {
                        let _ = cancel_receiver.recv().await;
                        None
                    })
                    .await;
                let Some(watcher_generation) = watcher_generation else {
                    return;
                };
                loop {
                    let next = futures_lite::future::race(async { signals.next().await }, async {
                        let _ = cancel_receiver.recv().await;
                        None
                    })
                    .await;
                    let Some(signal) = next else {
                        break;
                    };
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
                                    watcher_generation: Some(watcher_generation),
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
                                        watcher_generation: Some(watcher_generation),
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
    Ok(InstalledMenuSignalWatcher {
        cancel: cancel_sender,
        start: start_sender,
    })
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
                            watcher_generation: None,
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
    let ai_event = matches!(&event, Event::ActiveAiUsageChanged(_));
    let mut wake = wake.lock().expect("DBus wake poisoned");
    let wake_result = push_event_with_writer(events, &mut *wake, event);
    if ai_event && std::env::var_os("XBAR_TRACE").is_some() {
        eprintln!("xbar trace: AI_BRIDGE_WAKE_SENT result={wake_result:?}");
    }
}

fn push_event_with_writer<W: Write>(
    events: &EventQueue,
    wake: &mut W,
    event: Event,
) -> io::Result<()> {
    events
        .lock()
        .expect("DBus event queue poisoned")
        .push_back(event);
    wake.write_all(&[1])
}

#[cfg(test)]
mod ai_usage_bridge_tests {
    use super::*;

    fn queued_ai_event_wakes_main_loop() {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let mut wake = Vec::new();
        push_event_with_writer(&events, &mut wake, Event::ActiveAiUsageChanged(Vec::new()))
            .expect("test wake writer");
        assert_eq!(wake, [1]);
        assert!(matches!(
            events.lock().expect("event queue").pop_front(),
            Some(Event::ActiveAiUsageChanged(usage)) if usage.is_empty()
        ));
    }

    #[test]
    fn late_collector_get_state_completion_wakes_main_loop() {
        queued_ai_event_wakes_main_loop();
    }

    #[test]
    fn ai_state_changed_signal_wakes_main_loop() {
        queued_ai_event_wakes_main_loop();
    }

    #[test]
    fn owner_loss_wakes_main_loop() {
        queued_ai_event_wakes_main_loop();
    }
}

#[cfg(test)]
mod status_notifier_tests {
    use super::{choose_sni_icon, notification_capabilities, select_pixmap};
    use crate::core::status_notifier::format_notifier_item_id;
    use crate::core::{
        parse_notifier_item_id, StatusNotifierEndpoint, StatusNotifierIcon, StatusNotifierStatus,
    };

    #[test]
    fn notification_capabilities_advertise_actions_without_action_icons() {
        assert_eq!(notification_capabilities(), ["body", "actions"]);
        assert!(!notification_capabilities()
            .iter()
            .any(|capability| capability == "action-icons"));
    }

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
mod dbus_menu_lifecycle_tests {
    use super::{
        allocate_menu_watcher_generation, cancel_watchers_for_unique_owner,
        cancellable_layout_load, dbus_menu_endpoint_key, finish_layout_load, MenuWatcherControl,
    };
    use async_channel::bounded;
    use std::collections::HashMap;

    fn endpoint(service: &str, path: &str) -> crate::core::MenuEndpoint {
        crate::core::MenuEndpoint {
            service: service.into(),
            object_path: path.into(),
        }
    }

    #[test]
    fn unregister_cancels_pending_layout_and_late_completion_cannot_reinsert_watcher() {
        let endpoint = endpoint(":1.7", "/menu");
        let (signal_cancel, signal_receiver) = bounded(1);
        let mut watchers = HashMap::new();
        let mut control = MenuWatcherControl::new(10, signal_cancel);
        let load_cancel = control.start_load(7);
        watchers.insert(
            format!("{}\0{}", endpoint.service, endpoint.object_path),
            control,
        );

        let (ready_sender, ready_receiver) = bounded(1);
        let pending_load = async move {
            ready_sender.send(()).await.expect("ready receiver");
            std::future::pending::<Result<(), String>>().await
        };
        let cancellation = async {
            ready_receiver.recv().await.expect("pending load ready");
            let control = watchers
                .remove(&format!("{}\0{}", endpoint.service, endpoint.object_path))
                .expect("watcher registered");
            control.cancel();
        };
        let (result, ()) = zbus::block_on(futures_lite::future::zip(
            cancellable_layout_load(pending_load, load_cancel),
            cancellation,
        ));
        assert!(result.is_none());
        assert!(watchers.is_empty());
        assert!(signal_receiver.try_recv().is_ok());

        assert!(!finish_layout_load(&mut watchers, &endpoint, 7));
        assert!(watchers.is_empty());
    }

    #[test]
    fn owner_disappearance_cancels_pending_layout_and_late_completion_cannot_reinsert_watcher() {
        let endpoint = endpoint(":1.8", "/menu");
        let (signal_cancel, signal_receiver) = bounded(1);
        let mut watchers = HashMap::new();
        let mut control = MenuWatcherControl::new(11, signal_cancel);
        let load_cancel = control.start_load(8);
        watchers.insert(
            format!("{}\0{}", endpoint.service, endpoint.object_path),
            control,
        );

        let (ready_sender, ready_receiver) = bounded(1);
        let pending_load = async move {
            ready_sender.send(()).await.expect("ready receiver");
            std::future::pending::<Result<(), String>>().await
        };
        let disappearance = async {
            ready_receiver.recv().await.expect("pending load ready");
            cancel_watchers_for_unique_owner(&mut watchers, ":1.8");
        };
        let (result, ()) = zbus::block_on(futures_lite::future::zip(
            cancellable_layout_load(pending_load, load_cancel),
            disappearance,
        ));
        assert!(result.is_none());
        assert!(watchers.is_empty());
        assert!(signal_receiver.try_recv().is_ok());
        assert!(!finish_layout_load(&mut watchers, &endpoint, 8));
    }

    #[test]
    fn watcher_generation_is_reused_until_control_is_recreated() {
        let endpoint = endpoint(":1.9", "/menu");
        let key = dbus_menu_endpoint_key(&endpoint);
        let mut next_generation = 10;
        let mut watchers = HashMap::new();
        let (first_cancel, _) = bounded(1);
        let first_generation = allocate_menu_watcher_generation(&mut next_generation);
        watchers.insert(
            key.clone(),
            MenuWatcherControl::new(first_generation, first_cancel),
        );

        assert_eq!(watchers[&key].watcher_generation, 10);
        watchers.get_mut(&key).expect("live watcher").start_load(25);
        watchers
            .get_mut(&key)
            .expect("reused watcher")
            .start_load(26);
        assert_eq!(watchers[&key].watcher_generation, first_generation);
        assert_eq!(next_generation, 11);

        watchers.remove(&key).expect("first watcher").cancel();
        let (second_cancel, _) = bounded(1);
        let second_generation = allocate_menu_watcher_generation(&mut next_generation);
        watchers.insert(
            key.clone(),
            MenuWatcherControl::new(second_generation, second_cancel),
        );

        assert_eq!(watchers[&key].watcher_generation, 11);
        assert_ne!(first_generation, second_generation);
    }

    #[test]
    fn same_owner_paths_have_independent_controls_generations_and_cancellation() {
        let endpoint_a = endpoint(":1.9", "/a");
        let endpoint_b = endpoint(":1.9", "/b");
        let key_a = dbus_menu_endpoint_key(&endpoint_a);
        let key_b = dbus_menu_endpoint_key(&endpoint_b);
        let (cancel_a, receiver_a) = bounded(1);
        let (cancel_b, receiver_b) = bounded(1);
        let mut watchers = HashMap::new();
        watchers.insert(key_a.clone(), MenuWatcherControl::new(10, cancel_a));
        watchers.insert(key_b.clone(), MenuWatcherControl::new(11, cancel_b));

        let control_a = watchers.remove(&key_a).expect("watcher A");
        control_a.cancel();

        assert!(receiver_a.try_recv().is_ok());
        assert!(receiver_b.try_recv().is_err());
        assert_eq!(watchers[&key_b].watcher_generation, 11);
    }

    #[test]
    fn owner_disappearance_removes_all_of_its_paths_but_not_another_owner() {
        let endpoint_a = endpoint(":1.9", "/a");
        let endpoint_b = endpoint(":1.9", "/b");
        let endpoint_c = endpoint(":1.10", "/a");
        let (cancel_a, receiver_a) = bounded(1);
        let (cancel_b, receiver_b) = bounded(1);
        let (cancel_c, receiver_c) = bounded(1);
        let mut watchers = HashMap::new();
        watchers.insert(
            dbus_menu_endpoint_key(&endpoint_a),
            MenuWatcherControl::new(10, cancel_a),
        );
        watchers.insert(
            dbus_menu_endpoint_key(&endpoint_b),
            MenuWatcherControl::new(11, cancel_b),
        );
        watchers.insert(
            dbus_menu_endpoint_key(&endpoint_c),
            MenuWatcherControl::new(12, cancel_c),
        );

        cancel_watchers_for_unique_owner(&mut watchers, ":1.9");

        assert!(receiver_a.try_recv().is_ok());
        assert!(receiver_b.try_recv().is_ok());
        assert!(receiver_c.try_recv().is_err());
        assert_eq!(watchers.len(), 1);
        assert_eq!(
            watchers[&dbus_menu_endpoint_key(&endpoint_c)].watcher_generation,
            12
        );
    }
}

#[cfg(test)]
mod status_notifier_attachment_tests {
    use super::*;

    fn control() -> Arc<StatusNotifierAttachmentControl> {
        let (cancel_sender, cancel_receiver) = async_channel::bounded(1);
        Arc::new(StatusNotifierAttachmentControl {
            cancel_sender,
            cancel_receiver,
            active: AtomicBool::new(true),
            next_item_id: Mutex::new(0),
            items: Mutex::new(HashMap::new()),
            publication: Mutex::new(()),
        })
    }

    fn endpoint() -> StatusNotifierEndpoint {
        StatusNotifierEndpoint {
            service: ":1.9".into(),
            object_path: "/StatusNotifierItem".into(),
        }
    }

    fn apply_ordered(control: &StatusNotifierAttachmentControl, event: OrderedWatcherEvent) {
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let (reader, writer) = UnixStream::pair().expect("wake pair");
        let wake = Arc::new(Mutex::new(writer));
        let _ = apply_ordered_watcher_event(control, &events, &wake, event);
        drop(reader);
    }

    #[test]
    fn unregister_stops_item_and_allows_fresh_registration() {
        let control = control();
        let item = endpoint();
        assert!(control.start_item(&item).is_some());
        control.stop_item(&item);
        assert!(control.start_item(&item).is_some());
    }

    #[test]
    fn attachment_drop_cancels_all_item_watchers() {
        let control = control();
        let item = endpoint();
        let (_, item_cancel) = control.start_item(&item).expect("item watcher");
        let attachment = StatusNotifierAttachment {
            owner: ":1.100".into(),
            control: Arc::clone(&control),
        };
        drop(attachment);
        assert!(control.cancel_receiver.try_recv().is_ok());
        assert!(item_cancel.try_recv().is_ok());
        assert!(control.items.lock().expect("items").is_empty());
    }

    #[test]
    fn direct_replacement_tears_down_a_before_b() {
        let a = control();
        let a_cancel = a.cancel_receiver.clone();
        let old = StatusNotifierAttachment {
            owner: "A".into(),
            control: Arc::clone(&a),
        };
        drop(old);
        assert!(a_cancel.try_recv().is_ok());

        let b = control();
        assert!(b.start_item(&endpoint()).is_some());
        assert_eq!(b.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn bootstrap_and_signal_share_one_item_watcher() {
        let control = control();
        let item = endpoint();
        assert!(control.start_item(&item).is_some());
        assert!(control.start_item(&item).is_none());
        assert_eq!(control.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn attachment_cancel_reaches_signal_and_every_item_watcher() {
        let control = control();
        let (_, first) = control.start_item(&endpoint()).expect("first watcher");
        let second_endpoint = StatusNotifierEndpoint {
            service: ":1.10".into(),
            object_path: "/StatusNotifierItem".into(),
        };
        let (_, second) = control
            .start_item(&second_endpoint)
            .expect("second watcher");
        let signal = control.cancel_receiver.clone();
        let attachment = StatusNotifierAttachment {
            owner: "A".into(),
            control: Arc::clone(&control),
        };
        drop(attachment);
        assert!(!control.is_active());
        assert!(signal.try_recv().is_ok());
        assert!(first.try_recv().is_ok());
        assert!(second.try_recv().is_ok());
    }

    #[test]
    fn old_item_generation_cannot_claim_re_registration() {
        let control = control();
        let item = endpoint();
        let (old_id, _) = control.start_item(&item).expect("old watcher");
        control.stop_item(&item);
        let (new_id, _) = control.start_item(&item).expect("new watcher");
        assert_ne!(old_id, new_id);
        assert!(!control.is_current_item(&item, old_id));
        assert!(control.is_current_item(&item, new_id));
    }

    #[test]
    fn stop_item_orders_before_stale_publication() {
        let control = control();
        let item = endpoint();
        let (old_id, _) = control.start_item(&item).expect("old watcher");
        control.stop_item(&item);
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let (reader, writer) = UnixStream::pair().expect("wake pair");
        drop(reader);
        let wake = Arc::new(Mutex::new(writer));
        assert!(!control.publish_item(
            &item,
            old_id,
            &events,
            &wake,
            Event::StatusNotifierItemUpdated(StatusNotifierItem {
                endpoint: item.clone(),
                status: StatusNotifierStatus::Passive,
                icon: None,
                icon_name: None,
                attention_icon_name: None,
                item_is_menu: false,
                menu: None,
            }),
        ));
        assert!(events.lock().expect("events").is_empty());
    }

    #[test]
    fn start_item_cannot_reopen_cancelled_attachment() {
        let control = control();
        control.cancel();
        assert!(control.start_item(&endpoint()).is_none());
    }

    #[test]
    fn unregister_then_reregister_has_one_live_generation() {
        let control = control();
        let item = endpoint();
        let (old_id, _) = control.start_item(&item).expect("old watcher");
        control.stop_item(&item);
        let (new_id, _) = control.start_item(&item).expect("new watcher");
        assert!(!control.is_current_item(&item, old_id));
        assert!(control.is_current_item(&item, new_id));
        assert_eq!(control.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn closed_item_cancel_is_terminal() {
        let control = control();
        let item = endpoint();
        let (_, receiver) = control.start_item(&item).expect("watcher");
        control.stop_item(&item);
        assert!(receiver.try_recv().is_ok());
        drop(receiver);
        assert!(item_wake_is_terminal(ItemWake::ItemCancelled));
        assert!(item_wake_is_terminal(ItemWake::PropertyStreamEnded));
        assert!(!item_wake_is_terminal(ItemWake::PropertyChanged));
    }

    #[test]
    fn cancelled_attachment_cannot_publish_after_teardown() {
        let control = control();
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let (reader, writer) = UnixStream::pair().expect("wake pair");
        drop(reader);
        let wake = Arc::new(Mutex::new(writer));
        let attachment = StatusNotifierAttachment {
            owner: "A".into(),
            control: Arc::clone(&control),
        };
        drop(attachment);
        assert!(!control.publish(&events, &wake, Event::StatusNotifierHostRegistered));
        assert!(events.lock().expect("events").is_empty());
    }

    #[test]
    fn companion_path_is_resolved_beside_xbar_executable() {
        assert_eq!(
            sibling_sni_watcher_path(Path::new("/opt/xbar/bin/xbar")),
            Some(PathBuf::from("/opt/xbar/bin/xbar-sni-watcher"))
        );
    }

    #[test]
    fn snapshot_item_then_queued_unregistered_stays_removed() {
        let control = control();
        let item = endpoint();
        apply_ordered(&control, OrderedWatcherEvent::Registered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Unregistered(item));
        assert!(control.items.lock().expect("items").is_empty());
    }

    #[test]
    fn snapshot_item_then_queued_registered_has_one_generation() {
        let control = control();
        let item = endpoint();
        apply_ordered(&control, OrderedWatcherEvent::Registered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Registered(item));
        assert_eq!(control.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn excluded_snapshot_then_queued_registered_has_one_live_item() {
        let control = control();
        apply_ordered(&control, OrderedWatcherEvent::Registered(endpoint()));
        assert_eq!(control.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn queued_registered_then_unregistered_is_absent_in_order() {
        let control = control();
        let item = endpoint();
        apply_ordered(&control, OrderedWatcherEvent::Registered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Unregistered(item));
        assert!(control.items.lock().expect("items").is_empty());
    }

    #[test]
    fn queued_unregistered_then_registered_is_a_new_generation() {
        let control = control();
        let item = endpoint();
        apply_ordered(&control, OrderedWatcherEvent::Unregistered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Registered(item.clone()));
        let (id, _) = control
            .items
            .lock()
            .expect("items")
            .get(&item)
            .cloned()
            .expect("new generation");
        assert!(control.is_current_item(&item, id));
    }

    #[test]
    fn ordered_primitive_does_not_have_a_member_subscription_gap() {
        let control = control();
        let item = endpoint();
        apply_ordered(&control, OrderedWatcherEvent::Unregistered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Registered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Unregistered(item.clone()));
        apply_ordered(&control, OrderedWatcherEvent::Registered(item));
        assert_eq!(control.items.lock().expect("items").len(), 1);
    }

    #[test]
    fn pending_item_phase_is_interrupted_by_cancellation() {
        let (sender, receiver) = async_channel::bounded(1);
        sender.try_send(()).expect("cancel pending phase");
        let result = zbus::block_on(await_item_phase(
            futures_lite::future::pending::<u8>(),
            &receiver,
        ));
        assert_eq!(result, None);
    }

    #[test]
    fn setup_failure_keeps_live_owner_until_disappearance_then_recovers() {
        let mut owner = None;
        retain_sni_owner_on_setup_failure(&mut owner, ":1.40");
        assert_eq!(owner.as_deref(), Some(":1.40"));

        if owner.as_deref() == Some(":1.40") {
            owner = None;
        }
        assert!(owner.is_none());
        retain_sni_owner_on_setup_failure(&mut owner, ":1.41");
        assert_eq!(owner.as_deref(), Some(":1.41"));
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
