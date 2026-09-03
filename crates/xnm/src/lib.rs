#![forbid(unsafe_code)]
mod client;
mod dbus;
mod event;
mod model;

use std::{fmt, str::FromStr};

pub use client::Client;
pub use event::NetworkEvent;
pub use model::{
    AccessPoint, ActiveConnection, Band, CurrentWifiState, DeviceState, SavedConnection, WifiDevice,
};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                if s.starts_with('/') {
                    Ok(Self(s.to_owned()))
                } else {
                    Err(Error::InvalidId(s.to_owned()))
                }
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}
id_type!(DeviceId);
id_type!(AccessPointId);
id_type!(SavedConnectionId);
id_type!(ActiveConnectionId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Dbus(String),
    InvalidId(String),
    NotWifi(DeviceId),
    UnknownDevice(DeviceId),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dbus(x) => write!(f, "D-Bus: {x}"),
            Self::InvalidId(x) => write!(f, "invalid object path: {x}"),
            Self::NotWifi(x) => write!(f, "not a Wi-Fi device: {x}"),
            Self::UnknownDevice(x) => write!(f, "unknown device: {x}"),
        }
    }
}
impl std::error::Error for Error {}
pub(crate) fn id<T: FromStr<Err = Error>>(s: String) -> T {
    T::from_str(&s).expect("NetworkManager returned an invalid object path")
}

#[cfg(test)]
mod send_tests {
    fn assert_send_sync<T: Send + Sync>() {}
    #[test]
    fn public_boundary_is_send_sync() {
        assert_send_sync::<crate::NetworkEvent>();
        assert_send_sync::<crate::WifiDevice>();
        assert_send_sync::<crate::AccessPoint>();
        assert_send_sync::<crate::SavedConnection>();
        assert_send_sync::<crate::ActiveConnection>();
    }
}

#[cfg(test)]
mod model_tests {
    use super::*;
    use crate::model::NetworkGraph;

    fn device(path: &str, iface: &str) -> WifiDevice {
        WifiDevice {
            id: path.parse().unwrap(),
            interface: iface.into(),
            driver: None,
            state: DeviceState::Activated,
            active_connection: None,
            active_ap: None,
            last_scan: None,
        }
    }
    fn ap(path: &str, device: &str, ssid: &str, frequency: u32) -> AccessPoint {
        AccessPoint {
            id: path.parse().unwrap(),
            device: device.parse().unwrap(),
            ssid: ssid.into(),
            bssid: "00:11:22:33:44:55".into(),
            frequency,
            strength: 70,
            flags: 0,
            wpa_flags: 0,
            rsn_flags: 0,
        }
    }

    #[test]
    fn typed_ids_are_distinct_and_validated() {
        let d: DeviceId = "/device/1".parse().unwrap();
        let a: AccessPointId = "/ap/1".parse().unwrap();
        assert_ne!(d.as_str(), a.as_str());
        assert!("not-path".parse::<DeviceId>().is_err());
    }
    #[test]
    fn graph_preserves_device_scope_and_bands() {
        let mut g = NetworkGraph::default();
        let d0 = device("/d0", "wlan0");
        let d1 = device("/d1", "wlan1");
        g.devices.insert(d0.id.clone(), d0);
        g.devices.insert(d1.id.clone(), d1);
        let a0 = ap("/ap/0", "/d0", "same", 2412);
        let a1 = ap("/ap/1", "/d0", "same", 5180);
        let a2 = ap("/ap/2", "/d1", "same", 2412);
        g.access_points.extend([
            (a0.id.clone(), a0.clone()),
            (a1.id.clone(), a1.clone()),
            (a2.id.clone(), a2.clone()),
        ]);
        g.device_aps
            .insert("/d0".parse().unwrap(), vec![a0.id.clone(), a1.id.clone()]);
        g.device_aps
            .insert("/d1".parse().unwrap(), vec![a2.id.clone()]);
        assert_eq!(g.device_aps[&"/d0".parse().unwrap()].len(), 2);
        assert_eq!(a0.band(), Band::Ghz2_4);
        assert_eq!(a1.band(), Band::Ghz5);
    }
    #[test]
    fn current_state_is_graph_only() {
        let mut g = NetworkGraph::default();
        let mut d = device("/d0", "wlan0");
        let a = ap("/ap/0", "/d0", "Foo", 5765);
        d.active_ap = Some(a.id.clone());
        g.devices.insert(d.id.clone(), d);
        g.access_points.insert(a.id.clone(), a);
        let s = g.current(&"/d0".parse().unwrap()).unwrap();
        assert_eq!(
            (s.interface, s.ssid, s.frequency),
            ("wlan0".into(), Some("Foo".into()), Some(5765))
        );
    }
    #[test]
    fn device_removal_removes_associated_aps() {
        let mut g = NetworkGraph::default();
        let d = device("/d0", "wlan0");
        let a = ap("/ap/0", "/d0", "Foo", 2412);
        g.device_aps.insert(d.id.clone(), vec![a.id.clone()]);
        g.access_points.insert(a.id.clone(), a);
        g.devices.insert(d.id.clone(), d);
        g.remove_device(&"/d0".parse().unwrap());
        assert!(g.access_points.is_empty());
    }
}
