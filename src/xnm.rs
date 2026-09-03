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
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct XnmShadowState {
    pub devices: Vec<XnmShadowDevice>,
    pub access_points: Vec<AccessPoint>,
    pub saved_connections: Vec<SavedConnection>,
    pub active_connections: Vec<ActiveConnection>,
    pub available: bool,
}

pub fn apply_shadow_event(shadow: &mut XnmShadowState, event: XnmBridgeEvent) {
    match event {
        XnmBridgeEvent::InitialState(initial) => {
            shadow.devices = initial.devices;
            shadow.access_points = initial.access_points;
            shadow.saved_connections = initial.saved_connections;
            shadow.active_connections = initial.active_connections;
            shadow.available = true;
        }
        XnmBridgeEvent::Network { event, device } => {
            if let NetworkEvent::DeviceRemoved(id) = event {
                shadow.devices.retain(|current| current.device != id);
            } else if let Some(device) = device {
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
            shadow.available = true;
        }
        XnmBridgeEvent::BackendFailed(_) => shadow.available = false,
    }
}

type EventQueue = Arc<Mutex<VecDeque<XnmBridgeEvent>>>;

enum WaitResult {
    Event(Result<NetworkEvent, xnm::Error>),
    Stop,
}

pub struct XnmBridge {
    reader: UnixStream,
    events: EventQueue,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl XnmBridge {
    pub fn start() -> io::Result<Self> {
        let (reader, mut writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let queue = Arc::clone(&events);
        let (stop, stop_receiver) = async_channel::bounded(1);
        let thread = thread::Builder::new()
            .name("xbar-network".into())
            .spawn(move || {
                zbus::block_on(run(queue, &mut writer, stop_receiver));
            })?;
        Ok(Self {
            reader,
            events,
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    pub fn raw_fd(&self) -> RawFd {
        self.reader.as_raw_fd()
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

async fn run(queue: EventQueue, writer: &mut UnixStream, stop: Receiver<()>) {
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
            devices,
            access_points,
            saved_connections: client.saved_connections(),
            active_connections,
        }),
    );

    loop {
        let next = async { WaitResult::Event(client.next_event().await) };
        let stopped = async {
            let _ = stop.recv().await;
            WaitResult::Stop
        };
        match or(next, stopped).await {
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
            devices: vec![state("wlan0", Some("Foo"))],
            access_points: Vec::new(),
            saved_connections: Vec::new(),
            active_connections: Vec::new(),
        });
        apply(&mut shadow, initial);
        assert!(shadow.available);
        assert_eq!(shadow.devices[0].state.ssid.as_deref(), Some("Foo"));
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

    fn apply(shadow: &mut XnmShadowState, event: XnmBridgeEvent) {
        apply_shadow_event(shadow, event);
    }
}
