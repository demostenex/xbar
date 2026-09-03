use crate::{AccessPointId, ActiveConnectionId, DeviceId, SavedConnectionId};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceState {
    Unknown,
    Unmanaged,
    Disconnected,
    Prepare,
    Config,
    NeedAuth,
    IpConfig,
    IpCheck,
    Secondaries,
    Activated,
    Deactivating,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceStateReason {
    pub state: u32,
    pub reason: u32,
}
impl From<u32> for DeviceState {
    fn from(v: u32) -> Self {
        match v {
            10 => Self::Unmanaged,
            30 => Self::Disconnected,
            40 => Self::Prepare,
            50 => Self::Config,
            60 => Self::NeedAuth,
            70 => Self::IpConfig,
            80 => Self::IpCheck,
            90 => Self::Secondaries,
            100 => Self::Activated,
            110 => Self::Deactivating,
            120 => Self::Failed,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Ghz2_4,
    Ghz5,
    Ghz6,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPoint {
    pub id: AccessPointId,
    pub device: DeviceId,
    pub ssid: String,
    pub bssid: String,
    pub frequency: u32,
    pub strength: u8,
    pub flags: u32,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
}
impl AccessPoint {
    pub fn band(&self) -> Band {
        match self.frequency {
            2412..=2999 => Band::Ghz2_4,
            3000..=5999 => Band::Ghz5,
            6000.. => Band::Ghz6,
            _ => Band::Unknown,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiDevice {
    pub id: DeviceId,
    pub interface: String,
    pub driver: Option<String>,
    pub state: DeviceState,
    pub active_connection: Option<ActiveConnectionId>,
    pub active_ap: Option<AccessPointId>,
    pub last_scan: Option<i64>,
    pub state_reason: Option<DeviceStateReason>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedConnection {
    pub id: SavedConnectionId,
    pub name: String,
    pub uuid: String,
    pub connection_type: String,
    pub interface_name: Option<String>,
    pub ssid: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveConnection {
    pub id: ActiveConnectionId,
    pub name: String,
    pub uuid: Option<String>,
    pub state: u32,
    pub profile: Option<SavedConnectionId>,
    pub devices: Vec<DeviceId>,
    pub state_reason: Option<DeviceStateReason>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentWifiState {
    pub interface: String,
    pub device_state: DeviceState,
    pub active_connection: Option<ActiveConnectionId>,
    pub active_connection_id: Option<String>,
    pub active_connection_state: Option<u32>,
    pub active_ap: Option<AccessPointId>,
    pub ssid: Option<String>,
    pub frequency: Option<u32>,
    pub strength: Option<u8>,
    pub device_state_reason: Option<DeviceStateReason>,
    pub active_connection_state_reason: Option<DeviceStateReason>,
}

#[derive(Debug, Default)]
pub(crate) struct NetworkGraph {
    pub devices: HashMap<DeviceId, WifiDevice>,
    pub access_points: HashMap<AccessPointId, AccessPoint>,
    pub saved_connections: HashMap<SavedConnectionId, SavedConnection>,
    pub active_connections: HashMap<ActiveConnectionId, ActiveConnection>,
    pub device_aps: HashMap<DeviceId, Vec<AccessPointId>>,
}
impl NetworkGraph {
    pub(crate) fn saved_profile(
        &self,
        device: &DeviceId,
        ssid: &str,
    ) -> Result<SavedConnection, crate::Error> {
        let d = self
            .devices
            .get(device)
            .ok_or_else(|| crate::Error::UnknownDevice(device.clone()))?;
        let wifi = |p: &&SavedConnection| {
            (p.connection_type == "802-11-wireless" || p.connection_type == "wifi")
                && p.ssid.as_deref() == Some(ssid)
        };
        let specific: Vec<_> = self
            .saved_connections
            .values()
            .filter(|p| p.interface_name.as_deref() == Some(d.interface.as_str()))
            .filter(wifi)
            .cloned()
            .collect();
        let profiles = if specific.is_empty() {
            self.saved_connections
                .values()
                .filter(|p| p.interface_name.is_none())
                .filter(wifi)
                .cloned()
                .collect()
        } else {
            specific
        };
        match profiles.as_slice() {
            [profile] => Ok(profile.clone()),
            [] => Err(crate::Error::UnsavedNetwork {
                device: device.clone(),
                ssid: ssid.to_owned(),
            }),
            _ => Err(crate::Error::AmbiguousSavedConnections(profiles)),
        }
    }
    pub(crate) fn activation_selection(
        &self,
        device: &DeviceId,
        ssid: &str,
    ) -> Result<CandidateSelection, crate::Error> {
        let _d = self
            .devices
            .get(device)
            .ok_or_else(|| crate::Error::UnknownDevice(device.clone()))?;
        let access_point = self
            .device_aps
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|id| self.access_points.get(id))
            .filter(|a| a.ssid == ssid)
            .max_by(|a, b| a.strength.cmp(&b.strength).then_with(|| b.id.cmp(&a.id)))
            .cloned();
        let Some(access_point) = access_point else {
            return Err(crate::Error::AccessPointNotFound {
                device: device.clone(),
                ssid: ssid.to_owned(),
            });
        };
        match self.saved_profile(device, ssid) {
            Ok(profile) => Ok(CandidateSelection::Ready {
                profile: Box::new(profile),
                access_point: Box::new(access_point),
            }),
            Err(crate::Error::UnsavedNetwork { .. }) => Ok(CandidateSelection::UnsavedNetwork),
            Err(crate::Error::AmbiguousSavedConnections(profiles)) => {
                Ok(CandidateSelection::Ambiguous(profiles))
            }
            Err(error) => Err(error),
        }
    }
    pub(crate) fn current(&self, id: &DeviceId) -> Option<CurrentWifiState> {
        let d = self.devices.get(id)?;
        let c = d
            .active_connection
            .as_ref()
            .and_then(|x| self.active_connections.get(x));
        let a = d.active_ap.as_ref().and_then(|x| self.access_points.get(x));
        Some(CurrentWifiState {
            interface: d.interface.clone(),
            device_state: d.state,
            active_connection: d.active_connection.clone(),
            active_connection_id: c.map(|x| x.name.clone()),
            active_connection_state: c.map(|x| x.state),
            active_ap: d.active_ap.clone(),
            ssid: a.map(|x| x.ssid.clone()),
            frequency: a.map(|x| x.frequency),
            strength: a.map(|x| x.strength),
            device_state_reason: d.state_reason,
            active_connection_state_reason: c.and_then(|x| x.state_reason),
        })
    }
    pub(crate) fn remove_device(&mut self, id: &DeviceId) {
        self.devices.remove(id);
        if let Some(aps) = self.device_aps.remove(id) {
            for ap in aps {
                self.access_points.remove(&ap);
            }
        }
        self.active_connections
            .retain(|_, c| !c.devices.iter().any(|d| d == id));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CandidateSelection {
    Ready {
        profile: Box<SavedConnection>,
        access_point: Box<AccessPoint>,
    },
    Ambiguous(Vec<SavedConnection>),
    UnsavedNetwork,
}
