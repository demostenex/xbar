use crate::core::{
    NetworkAccessPoint, NetworkConnectivity, NetworkLinkKind, NetworkState, WifiDevice,
};
use async_channel::{Receiver, Sender};
use futures_lite::future::or;
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use xnm::{
    AccessPoint, ActiveConnection, Client, CurrentWifiState, DeviceId, NetworkEvent,
    SavedConnection,
};

#[derive(Clone, Debug, PartialEq)]
pub struct XnmShadowDevice {
    pub device: DeviceId,
    pub state: CurrentWifiState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct XnmInitialState {
    pub wireless_enabled: bool,
    pub devices: Vec<XnmShadowDevice>,
    pub access_points: Vec<AccessPoint>,
    pub saved_connections: Vec<SavedConnection>,
    pub active_connections: Vec<ActiveConnection>,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum XnmBridgeEvent {
    InitialState(XnmInitialState),
    Network {
        event: NetworkEvent,
        device: Option<XnmShadowDevice>,
    },
    BackendFailed(String),
    ScanFailed {
        interface: String,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XnmBackendCommand {
    RequestScan { interface: String },
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct XnmShadowState {
    pub wireless_enabled: bool,
    pub devices: Vec<XnmShadowDevice>,
    pub access_points: Vec<AccessPoint>,
    pub saved_connections: Vec<SavedConnection>,
    pub active_connections: Vec<ActiveConnection>,
    pub available: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct XnmStatus {
    pub available: bool,
    pub connected: bool,
    pub interface: Option<String>,
    pub ssid: Option<String>,
    pub frequency: Option<u32>,
    pub strength: Option<u8>,
}

impl XnmShadowState {
    pub fn interfaces(&self) -> Vec<String> {
        let mut interfaces = self
            .devices
            .iter()
            .map(|device| device.state.interface.clone())
            .collect::<Vec<_>>();
        interfaces.sort();
        interfaces.dedup();
        interfaces
    }

    pub fn status(&self) -> XnmStatus {
        let selected = self
            .devices
            .iter()
            .filter(|device| {
                device.state.device_state == xnm::DeviceState::Activated
                    && device.state.ssid.is_some()
            })
            .min_by(|a, b| a.state.interface.cmp(&b.state.interface));
        XnmStatus {
            available: self.available,
            connected: selected.is_some(),
            interface: selected.map(|device| device.state.interface.clone()),
            ssid: selected.and_then(|device| device.state.ssid.clone()),
            frequency: selected.and_then(|device| device.state.frequency),
            strength: selected.and_then(|device| device.state.strength),
        }
    }

    pub fn popup_projection(&self) -> NetworkState {
        let mut devices = self
            .devices
            .iter()
            .map(|device| {
                let mut candidates = self
                    .access_points
                    .iter()
                    .filter(|ap| ap.device == device.device)
                    .filter(|ap| !ap.ssid.is_empty())
                    .map(|ap| {
                        let saved = self
                            .saved_connections
                            .iter()
                            .filter(|profile| profile.ssid.as_deref() == Some(ap.ssid.as_str()))
                            .filter(|profile| {
                                profile.interface_name.is_none()
                                    || profile.interface_name.as_deref()
                                        == Some(device.state.interface.as_str())
                            })
                            .min_by(|a, b| {
                                (a.interface_name.is_none(), a.id.as_str())
                                    .cmp(&(b.interface_name.is_none(), b.id.as_str()))
                            })
                            .map(|profile| profile.id.to_string());
                        NetworkAccessPoint {
                            path: ap.id.to_string(),
                            device_path: ap.device.to_string(),
                            interface: device.state.interface.clone(),
                            ssid: ap.ssid.clone(),
                            strength: ap.strength,
                            frequency: ap.frequency,
                            is_active: device.state.active_ap.as_ref() == Some(&ap.id),
                            saved_profile: saved,
                        }
                    })
                    .collect::<Vec<_>>();
                candidates.sort_by(|a, b| {
                    b.strength
                        .cmp(&a.strength)
                        .then_with(|| a.path.cmp(&b.path))
                });
                let mut winners = Vec::new();
                for candidate in candidates {
                    let key = (
                        candidate.ssid.clone(),
                        crate::core::wifi_band(candidate.frequency),
                    );
                    if !winners.iter().any(|winner: &NetworkAccessPoint| {
                        (
                            winner.ssid.clone(),
                            crate::core::wifi_band(winner.frequency),
                        ) == key
                    }) {
                        winners.push(candidate);
                    }
                }
                winners.sort_by(|a, b| {
                    b.is_active
                        .cmp(&a.is_active)
                        .then_with(|| b.saved_profile.is_some().cmp(&a.saved_profile.is_some()))
                        .then_with(|| b.strength.cmp(&a.strength))
                        .then_with(|| a.ssid.cmp(&b.ssid))
                });
                WifiDevice {
                    path: device.device.to_string(),
                    interface: device.state.interface.clone(),
                    driver: None,
                    state: device_state_number(device.state.device_state),
                    raw_access_points: self
                        .access_points
                        .iter()
                        .filter(|ap| ap.device == device.device)
                        .count(),
                    named_access_points: self
                        .access_points
                        .iter()
                        .filter(|ap| ap.device == device.device && !ap.ssid.is_empty())
                        .count(),
                    active_connection: device
                        .state
                        .active_connection
                        .as_ref()
                        .map(ToString::to_string),
                    active_ap: device.state.active_ap.as_ref().map(ToString::to_string),
                    access_points: winners,
                }
            })
            .collect::<Vec<_>>();
        devices.sort_by(|a, b| {
            a.interface
                .cmp(&b.interface)
                .then_with(|| a.path.cmp(&b.path))
        });
        let access_points = devices
            .iter()
            .flat_map(|device| device.access_points.iter().cloned())
            .collect();
        let status = self.status();
        NetworkState {
            available: status.available,
            wireless_enabled: self.wireless_enabled,
            connectivity: if status.connected {
                NetworkConnectivity::Connected
            } else {
                NetworkConnectivity::Disconnected
            },
            link_kind: if status.connected {
                NetworkLinkKind::Wifi
            } else {
                NetworkLinkKind::Other
            },
            interface: status.interface,
            display_name: status.ssid,
            signal_percent: status.strength,
            access_points,
            wifi_devices: devices,
        }
    }
}

fn device_state_number(state: xnm::DeviceState) -> u32 {
    match state {
        xnm::DeviceState::Activated => 100,
        xnm::DeviceState::Deactivating => 110,
        xnm::DeviceState::Failed => 120,
        xnm::DeviceState::Prepare => 40,
        xnm::DeviceState::Config => 50,
        xnm::DeviceState::NeedAuth => 60,
        xnm::DeviceState::IpConfig => 70,
        xnm::DeviceState::IpCheck => 80,
        xnm::DeviceState::Secondaries => 90,
        xnm::DeviceState::Disconnected => 30,
        xnm::DeviceState::Unmanaged => 10,
        xnm::DeviceState::Unknown => 0,
    }
}

pub fn apply_shadow_event(shadow: &mut XnmShadowState, event: XnmBridgeEvent) {
    match event {
        XnmBridgeEvent::InitialState(initial) => {
            shadow.wireless_enabled = initial.wireless_enabled;
            shadow.devices = initial.devices;
            shadow.access_points = initial.access_points;
            shadow.saved_connections = initial.saved_connections;
            shadow.active_connections = initial.active_connections;
            shadow.available = true;
        }
        XnmBridgeEvent::Network { event, device } => {
            match event {
                NetworkEvent::NetworkManagerChanged { wireless_enabled } => {
                    shadow.wireless_enabled = wireless_enabled;
                }
                NetworkEvent::DeviceRemoved(id) => {
                    shadow.devices.retain(|current| current.device != id);
                    shadow.access_points.retain(|ap| ap.device != id);
                }
                event => {
                    match event {
                        NetworkEvent::AccessPointAdded(ap)
                        | NetworkEvent::AccessPointChanged(ap) => {
                            shadow.access_points.retain(|current| current.id != ap.id);
                            shadow.access_points.push(ap);
                        }
                        NetworkEvent::AccessPointRemoved { access_point, .. } => {
                            shadow.access_points.retain(|ap| ap.id != access_point);
                        }
                        NetworkEvent::SavedConnectionAdded(profile)
                        | NetworkEvent::SavedConnectionChanged(profile) => {
                            shadow
                                .saved_connections
                                .retain(|current| current.id != profile.id);
                            shadow.saved_connections.push(profile);
                        }
                        NetworkEvent::SavedConnectionRemoved(profile) => {
                            shadow
                                .saved_connections
                                .retain(|current| current.id != profile);
                        }
                        NetworkEvent::ActiveConnectionAdded(connection)
                        | NetworkEvent::ActiveConnectionChanged(connection) => {
                            shadow
                                .active_connections
                                .retain(|current| current.id != connection.id);
                            shadow.active_connections.push(connection);
                        }
                        NetworkEvent::ActiveConnectionRemoved(connection) => {
                            shadow
                                .active_connections
                                .retain(|current| current.id != connection);
                        }
                        _ => {}
                    }
                    if let Some(device) = device {
                        if let Some(current) = shadow
                            .devices
                            .iter_mut()
                            .find(|current| current.device == device.device)
                        {
                            *current = device;
                        } else {
                            shadow.devices.push(device);
                        }
                    }
                }
            }
            shadow.available = true;
        }
        XnmBridgeEvent::BackendFailed(_) => shadow.available = false,
        XnmBridgeEvent::ScanFailed { .. } => {}
    }
}

type EventQueue = Arc<Mutex<VecDeque<XnmBridgeEvent>>>;

enum WaitResult {
    Event(Result<NetworkEvent, xnm::Error>),
    Command(Result<XnmBackendCommand, async_channel::RecvError>),
    Stop,
}

pub struct XnmBridge {
    reader: UnixStream,
    events: EventQueue,
    stop: Option<Sender<()>>,
    commands: Sender<XnmBackendCommand>,
    thread: Option<JoinHandle<()>>,
}

impl XnmBridge {
    pub fn start() -> io::Result<Self> {
        let (reader, mut writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let queue = Arc::clone(&events);
        let (stop, stop_receiver) = async_channel::bounded(1);
        let (commands, command_receiver) = async_channel::unbounded();
        let thread = thread::Builder::new()
            .name("xbar-network".into())
            .spawn(move || {
                zbus::block_on(run(queue, &mut writer, stop_receiver, command_receiver));
            })?;
        Ok(Self {
            reader,
            events,
            stop: Some(stop),
            commands,
            thread: Some(thread),
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
    }

    pub fn request_scan(&self, interface: String) {
        let _ = self
            .commands
            .try_send(XnmBackendCommand::RequestScan { interface });
    }

    pub fn drain_events(&mut self) -> io::Result<Vec<XnmBridgeEvent>> {
        let mut buffer = [0_u8; 128];
        loop {
            match self.reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        let mut queue = self.events.lock().expect("xnm event queue poisoned");
        Ok(queue.drain(..).collect())
    }
}

impl Drop for XnmBridge {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.try_send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn push(queue: &EventQueue, writer: &mut UnixStream, event: XnmBridgeEvent) {
    queue
        .lock()
        .expect("xnm event queue poisoned")
        .push_back(event);
    let _ = writer.write_all(&[1]);
}

async fn run(
    queue: EventQueue,
    writer: &mut UnixStream,
    stop: Receiver<()>,
    commands: Receiver<XnmBackendCommand>,
) {
    let connecting = async { StartupResult::Connected(Box::new(Client::connect().await)) };
    let stopping = async {
        let _ = stop.recv().await;
        StartupResult::Stopped
    };
    let mut client = match or(connecting, stopping).await {
        StartupResult::Connected(result) => match *result {
            Ok(client) => client,
            Err(error) => {
                push(
                    &queue,
                    writer,
                    XnmBridgeEvent::BackendFailed(error.to_string()),
                );
                return;
            }
        },
        StartupResult::Stopped => return,
    };
    let devices = client
        .wifi_devices()
        .into_iter()
        .filter_map(|device| {
            let id = device.id.clone();
            client
                .current_wifi_state(&id)
                .map(|state| XnmShadowDevice { device: id, state })
        })
        .collect();
    let access_points = client
        .wifi_devices()
        .into_iter()
        .flat_map(|device| client.access_points(&device.id))
        .collect();
    let active_connections = client
        .wifi_devices()
        .into_iter()
        .filter_map(|device| client.active_connection(&device.id))
        .collect();
    push(
        &queue,
        writer,
        XnmBridgeEvent::InitialState(XnmInitialState {
            wireless_enabled: client.wireless_enabled(),
            devices,
            access_points,
            saved_connections: client.saved_connections(),
            active_connections,
        }),
    );

    loop {
        let next = async { WaitResult::Event(client.next_event().await) };
        let command = async { WaitResult::Command(commands.recv().await) };
        let stopped = async {
            let _ = stop.recv().await;
            WaitResult::Stop
        };
        match or(or(next, command), stopped).await {
            WaitResult::Command(Ok(XnmBackendCommand::RequestScan { interface })) => {
                let Some(device) = client.device_by_interface(&interface) else {
                    push(
                        &queue,
                        writer,
                        XnmBridgeEvent::ScanFailed {
                            interface,
                            error: "unknown device".into(),
                        },
                    );
                    continue;
                };
                if let Err(error) = client.request_scan(&device.id).await {
                    push(
                        &queue,
                        writer,
                        XnmBridgeEvent::ScanFailed {
                            interface,
                            error: error.to_string(),
                        },
                    );
                }
            }
            WaitResult::Command(Err(_)) => return,
            WaitResult::Event(Ok(event)) => {
                let device = event_device(&event);
                let state = device.as_ref().and_then(|id| {
                    client.current_wifi_state(id).map(|state| XnmShadowDevice {
                        device: id.clone(),
                        state,
                    })
                });
                push(
                    &queue,
                    writer,
                    XnmBridgeEvent::Network {
                        event,
                        device: state,
                    },
                );
            }
            WaitResult::Event(Err(error)) => {
                push(
                    &queue,
                    writer,
                    XnmBridgeEvent::BackendFailed(error.to_string()),
                );
                return;
            }
            WaitResult::Stop => return,
        }
    }
}

enum StartupResult {
    Connected(Box<Result<Client, xnm::Error>>),
    Stopped,
}

fn event_device(event: &NetworkEvent) -> Option<DeviceId> {
    match event {
        NetworkEvent::DeviceAdded(device) | NetworkEvent::DeviceChanged(device) => {
            Some(device.id.clone())
        }
        NetworkEvent::DeviceRemoved(device) => Some(device.clone()),
        NetworkEvent::DeviceStateChanged { device, .. } => Some(device.clone()),
        NetworkEvent::AccessPointAdded(ap) | NetworkEvent::AccessPointChanged(ap) => {
            Some(ap.device.clone())
        }
        NetworkEvent::AccessPointRemoved { device, .. } => Some(device.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xnm::{DeviceState, DeviceStateReason};

    fn state(interface: &str, ssid: Option<&str>) -> XnmShadowDevice {
        let device: DeviceId = "/device/1".parse().unwrap();
        XnmShadowDevice {
            device: device.clone(),
            state: CurrentWifiState {
                interface: interface.into(),
                device_state: DeviceState::Activated,
                active_connection: None,
                active_connection_id: None,
                active_connection_state: Some(2),
                active_ap: None,
                ssid: ssid.map(str::to_owned),
                frequency: Some(5765),
                strength: Some(80),
                device_state_reason: Some(DeviceStateReason {
                    state: 100,
                    reason: 0,
                }),
                active_connection_state_reason: None,
            },
        }
    }

    #[test]
    fn initial_state_updates_shadow_without_authoritative_network_state() {
        let mut shadow = XnmShadowState::default();
        let initial = XnmBridgeEvent::InitialState(XnmInitialState {
            wireless_enabled: true,
            devices: vec![state("wlan0", Some("Foo"))],
            access_points: Vec::new(),
            saved_connections: Vec::new(),
            active_connections: Vec::new(),
        });
        apply(&mut shadow, initial);
        assert!(shadow.available);
        assert!(shadow.wireless_enabled);
        assert_eq!(shadow.devices[0].state.ssid.as_deref(), Some("Foo"));
    }

    #[test]
    fn wireless_enabled_is_global_and_not_derived_from_connection_state() {
        let mut shadow = XnmShadowState {
            wireless_enabled: true,
            available: true,
            devices: vec![state("wlan0", None)],
            ..Default::default()
        };
        assert!(shadow.popup_projection().wireless_enabled);
        apply(
            &mut shadow,
            XnmBridgeEvent::Network {
                event: NetworkEvent::NetworkManagerChanged {
                    wireless_enabled: false,
                },
                device: None,
            },
        );
        assert!(!shadow.wireless_enabled);
        assert!(!shadow.popup_projection().wireless_enabled);
        assert!(!shadow.status().connected);
    }

    #[test]
    fn wireless_enabled_change_does_not_depend_on_ap_or_device_events() {
        let mut shadow = XnmShadowState {
            wireless_enabled: false,
            available: true,
            ..Default::default()
        };
        apply(
            &mut shadow,
            XnmBridgeEvent::Network {
                event: NetworkEvent::NetworkManagerChanged {
                    wireless_enabled: true,
                },
                device: None,
            },
        );
        assert!(shadow.wireless_enabled);
        assert!(shadow.popup_projection().wireless_enabled);
    }

    #[test]
    fn devices_are_isolated_by_identity() {
        let mut shadow = XnmShadowState {
            available: true,
            devices: vec![state("wlan0", Some("Foo")), state("wlan1", Some("Bar"))],
            ..Default::default()
        };
        let mut changed = state("wlan0", Some("Baz"));
        changed.device = "/device/1".parse().unwrap();
        apply(
            &mut shadow,
            XnmBridgeEvent::Network {
                event: NetworkEvent::DeviceChanged(xnm::WifiDevice {
                    id: changed.device.clone(),
                    interface: "wlan0".into(),
                    driver: None,
                    state: DeviceState::Activated,
                    active_connection: None,
                    active_ap: None,
                    last_scan: None,
                    state_reason: None,
                }),
                device: Some(changed),
            },
        );
        assert_eq!(shadow.devices.len(), 2);
        assert_eq!(shadow.devices[1].state.ssid.as_deref(), Some("Bar"));
    }

    #[test]
    fn backend_failure_only_marks_shadow_unavailable() {
        let mut shadow = XnmShadowState {
            available: true,
            devices: vec![state("wlan0", Some("Foo"))],
            ..Default::default()
        };
        apply(&mut shadow, XnmBridgeEvent::BackendFailed("offline".into()));
        assert!(!shadow.available);
        assert_eq!(shadow.devices[0].state.ssid.as_deref(), Some("Foo"));
    }

    #[test]
    fn summary_policy_is_deterministic_without_merging_devices() {
        let wlan0 = state("wlan0", Some("Foo"));
        let wlan1 = state("wlan1", Some("Bar"));
        let shadow = XnmShadowState {
            available: true,
            devices: vec![wlan1, wlan0],
            ..Default::default()
        };
        let status = shadow.status();
        assert_eq!(shadow.devices.len(), 2);
        assert_eq!(status.interface.as_deref(), Some("wlan0"));
        assert_eq!(status.ssid.as_deref(), Some("Foo"));
    }

    fn access_point(
        id: &str,
        device: &str,
        ssid: &str,
        frequency: u32,
        strength: u8,
    ) -> AccessPoint {
        AccessPoint {
            id: id.parse().unwrap(),
            device: device.parse().unwrap(),
            ssid: ssid.into(),
            bssid: format!("02:00:00:00:00:{strength:02x}"),
            frequency,
            strength,
            flags: 0,
            wpa_flags: 0,
            rsn_flags: 0,
        }
    }

    fn profile(id: &str, ssid: &str, interface_name: Option<&str>) -> SavedConnection {
        SavedConnection {
            id: id.parse().unwrap(),
            name: ssid.into(),
            uuid: id.into(),
            connection_type: "802-11-wireless".into(),
            interface_name: interface_name.map(str::to_owned),
            ssid: Some(ssid.into()),
        }
    }

    #[test]
    fn popup_projection_filters_hidden_and_deduplicates_per_device_band() {
        let mut wlan0 = state("wlan0", Some("Foo"));
        wlan0.device = "/device/0".parse().unwrap();
        let mut wlan1 = state("wlan1", None);
        wlan1.device = "/device/1".parse().unwrap();
        let shadow = XnmShadowState {
            available: true,
            devices: vec![wlan0, wlan1],
            access_points: vec![
                access_point("/ap/1", "/device/0", "Foo", 2412, 60),
                access_point("/ap/2", "/device/0", "Foo", 2437, 82),
                access_point("/ap/3", "/device/0", "Foo", 5765, 40),
                access_point("/ap/4", "/device/0", "", 2412, 99),
                access_point("/ap/5", "/device/1", "Foo", 2412, 70),
            ],
            ..Default::default()
        };
        let projection = shadow.popup_projection();
        assert_eq!(projection.wifi_devices.len(), 2);
        assert_eq!(projection.wifi_devices[0].access_points.len(), 2);
        assert_eq!(projection.wifi_devices[0].access_points[0].strength, 82);
        assert_eq!(projection.wifi_devices[1].access_points.len(), 1);
        assert_eq!(projection.access_points.len(), 3);
    }

    #[test]
    fn popup_projection_correlates_saved_profiles_only_on_compatible_interface() {
        let mut wlan0 = state("wlan0", None);
        wlan0.device = "/device/0".parse().unwrap();
        let shadow = XnmShadowState {
            available: true,
            devices: vec![wlan0],
            access_points: vec![access_point("/ap/1", "/device/0", "Foo", 2412, 80)],
            saved_connections: vec![
                profile("/settings/wlan1", "Foo", Some("wlan1")),
                profile("/settings/global", "Foo", None),
            ],
            ..Default::default()
        };
        let rows = &shadow.popup_projection().wifi_devices[0].access_points;
        assert_eq!(rows[0].saved_profile.as_deref(), Some("/settings/global"));
    }

    #[test]
    fn popup_projection_keeps_unsaved_and_active_ap_visible() {
        let mut device = state("wlan0", Some("Foo"));
        device.device = "/device/0".parse().unwrap();
        device.state.active_ap = Some("/ap/2".parse().unwrap());
        let shadow = XnmShadowState {
            available: true,
            devices: vec![device],
            access_points: vec![
                access_point("/ap/1", "/device/0", "Unsaved", 2412, 50),
                access_point("/ap/2", "/device/0", "Foo", 5765, 40),
            ],
            ..Default::default()
        };
        let rows = &shadow.popup_projection().wifi_devices[0].access_points;
        assert_eq!(rows.iter().filter(|row| row.is_active).count(), 1);
        assert!(rows.iter().any(|row| row.ssid == "Unsaved"));
    }

    #[test]
    fn popup_projection_updates_winner_without_duplicate_rows() {
        let mut device = state("wlan0", None);
        device.device = "/device/0".parse().unwrap();
        let mut shadow = XnmShadowState {
            available: true,
            devices: vec![device],
            access_points: vec![
                access_point("/ap/a", "/device/0", "Foo", 2412, 70),
                access_point("/ap/b", "/device/0", "Foo", 2412, 60),
            ],
            ..Default::default()
        };
        let before = shadow.popup_projection();
        shadow.access_points[0].strength = 50;
        shadow.access_points[1].strength = 80;
        let after = shadow.popup_projection();
        assert_eq!(before.wifi_devices[0].access_points.len(), 1);
        assert_eq!(after.wifi_devices[0].access_points.len(), 1);
        assert_eq!(after.wifi_devices[0].access_points[0].path, "/ap/b");
    }

    fn apply(shadow: &mut XnmShadowState, event: XnmBridgeEvent) {
        apply_shadow_event(shadow, event);
    }
}
