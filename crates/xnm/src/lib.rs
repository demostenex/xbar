#![forbid(unsafe_code)]
mod client;
mod dbus;
mod event;
mod model;

use std::{fmt, str::FromStr};

pub use client::{
    evaluate_activation, ActivationCandidate, ActivationEvaluation, ActivationFailure,
    CandidateResolution, Client, SavedActivation,
};
pub use event::NetworkEvent;
pub use model::{
    AccessPoint, ActiveConnection, Band, CurrentWifiState, DeviceState, DeviceStateReason,
    SavedConnection, WifiDevice,
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
    WirelessDisabled,
    NotWifi(DeviceId),
    UnknownDevice(DeviceId),
    DeviceDisappeared {
        interface: String,
        expected: DeviceId,
        actual: DeviceId,
    },
    UnsavedNetwork {
        device: DeviceId,
        ssid: String,
    },
    AmbiguousSavedConnections(Vec<SavedConnection>),
    AccessPointNotFound {
        device: DeviceId,
        ssid: String,
    },
    UnknownAccessPoint(AccessPointId),
    UnknownSavedConnection(SavedConnectionId),
    NotWifiProfile(SavedConnectionId),
    AccessPointDeviceMismatch {
        access_point: AccessPointId,
        device: DeviceId,
    },
    ProfileInterfaceMismatch {
        profile: SavedConnectionId,
        interface: String,
        device: String,
    },
    ProfileSsidMismatch {
        profile: SavedConnectionId,
        profile_ssid: String,
        ap_ssid: String,
    },
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dbus(x) => write!(f, "D-Bus: {x}"),
            Self::InvalidId(x) => write!(f, "invalid object path: {x}"),
            Self::WirelessDisabled => write!(f, "wireless is disabled"),
            Self::NotWifi(x) => write!(f, "not a Wi-Fi device: {x}"),
            Self::UnknownDevice(x) => write!(f, "unknown device: {x}"),
            Self::DeviceDisappeared {
                interface,
                expected,
                actual,
            } => write!(f, "device {interface} rebound from {expected} to {actual}"),
            Self::UnsavedNetwork { device, ssid } => {
                write!(f, "no saved Wi-Fi profile for {ssid} on {device}")
            }
            Self::AmbiguousSavedConnections(_) => write!(f, "ambiguous saved Wi-Fi profiles"),
            Self::AccessPointNotFound { device, ssid } => {
                write!(f, "access point not found on {device}: {ssid}")
            }
            Self::UnknownAccessPoint(x) => write!(f, "unknown access point: {x}"),
            Self::UnknownSavedConnection(x) => write!(f, "unknown saved connection: {x}"),
            Self::NotWifiProfile(x) => write!(f, "saved connection is not Wi-Fi: {x}"),
            Self::AccessPointDeviceMismatch {
                access_point,
                device,
            } => {
                write!(
                    f,
                    "access point {access_point} does not belong to device {device}"
                )
            }
            Self::ProfileInterfaceMismatch {
                profile,
                interface,
                device,
            } => write!(
                f,
                "profile {profile} is bound to {interface}, device interface is {device}"
            ),
            Self::ProfileSsidMismatch {
                profile,
                profile_ssid,
                ap_ssid,
            } => write!(
                f,
                "profile {profile} SSID {profile_ssid:?} does not match AP SSID {ap_ssid:?}"
            ),
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
    use crate::client::{
        activation_succeeded, already_active, evaluate_activation, scan_event_matches,
        ActivationEvaluation,
    };
    use crate::model::{CandidateSelection, NetworkGraph};

    fn device(path: &str, iface: &str) -> WifiDevice {
        WifiDevice {
            id: path.parse().unwrap(),
            interface: iface.into(),
            driver: None,
            state: DeviceState::Activated,
            active_connection: None,
            active_ap: None,
            last_scan: None,
            state_reason: None,
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
    fn profile(path: &str, name: &str, iface: Option<&str>, ssid: &str) -> SavedConnection {
        SavedConnection {
            id: path.parse().unwrap(),
            name: name.into(),
            uuid: format!("uuid-{name}"),
            connection_type: "802-11-wireless".into(),
            interface_name: iface.map(str::to_owned),
            ssid: Some(ssid.into()),
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
        let did = d.id.clone();
        g.devices.insert(did.clone(), d);
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

    #[test]
    fn resolver_rejects_other_interface_and_prefers_specific_profile() {
        let mut g = NetworkGraph::default();
        let d0 = device("/d0", "wlan0");
        let d1 = device("/d1", "wlan1");
        let a0 = ap("/ap/0", "/d0", "Foo", 2412);
        let a1 = ap("/ap/1", "/d1", "Foo", 5180);
        g.devices.extend([(d0.id.clone(), d0), (d1.id.clone(), d1)]);
        g.access_points
            .extend([(a0.id.clone(), a0), (a1.id.clone(), a1)]);
        g.device_aps
            .insert("/d0".parse().unwrap(), vec!["/ap/0".parse().unwrap()]);
        g.device_aps
            .insert("/d1".parse().unwrap(), vec!["/ap/1".parse().unwrap()]);
        let wrong = profile("/s/1", "wrong", Some("wlan1"), "Foo");
        let global = profile("/s/2", "global", None, "Foo");
        let right = profile("/s/3", "right", Some("wlan0"), "Foo");
        g.saved_connections.extend([
            (wrong.id.clone(), wrong),
            (global.id.clone(), global),
            (right.id.clone(), right),
        ]);
        let CandidateSelection::Ready {
            profile,
            access_point,
        } = g
            .activation_selection(&"/d0".parse().unwrap(), "Foo")
            .unwrap()
        else {
            panic!("expected candidate")
        };
        assert_eq!(profile.name, "right");
        assert_eq!(access_point.device, "/d0".parse().unwrap());
    }

    #[test]
    fn resolver_falls_back_to_one_global_and_reports_ambiguity() {
        let mut g = NetworkGraph::default();
        let d = device("/d0", "wlan0");
        let a = ap("/ap/0", "/d0", "Foo", 2412);
        g.device_aps.insert(d.id.clone(), vec![a.id.clone()]);
        g.access_points.insert(a.id.clone(), a);
        g.devices.insert(d.id.clone(), d);
        let p = profile("/s/1", "global", None, "Foo");
        g.saved_connections.insert(p.id.clone(), p);
        assert!(matches!(
            g.activation_selection(&"/d0".parse().unwrap(), "Foo")
                .unwrap(),
            CandidateSelection::Ready { .. }
        ));
        let p2 = profile("/s/2", "global2", None, "Foo");
        g.saved_connections.insert(p2.id.clone(), p2);
        assert!(matches!(
            g.activation_selection(&"/d0".parse().unwrap(), "Foo")
                .unwrap(),
            CandidateSelection::Ambiguous(_)
        ));
        assert!(matches!(
            g.activation_selection(&"/d0".parse().unwrap(), "Missing")
                .unwrap_err(),
            Error::AccessPointNotFound { .. }
        ));
    }

    #[test]
    fn resolver_selects_strongest_ap_deterministically() {
        let mut g = NetworkGraph::default();
        let d = device("/d0", "wlan0");
        let mut weak = ap("/ap/a", "/d0", "Foo", 2412);
        weak.strength = 40;
        let mut strong = ap("/ap/b", "/d0", "Foo", 5180);
        strong.strength = 80;
        let did = d.id.clone();
        g.devices.insert(did.clone(), d);
        g.access_points
            .extend([(weak.id.clone(), weak), (strong.id.clone(), strong)]);
        g.device_aps.insert(
            did,
            vec!["/ap/a".parse().unwrap(), "/ap/b".parse().unwrap()],
        );
        g.saved_connections.insert(
            "/s/1".parse().unwrap(),
            profile("/s/1", "global", None, "Foo"),
        );
        let CandidateSelection::Ready { access_point, .. } = g
            .activation_selection(&"/d0".parse().unwrap(), "Foo")
            .unwrap()
        else {
            panic!("expected candidate")
        };
        assert_eq!(access_point.id, "/ap/b".parse().unwrap());
    }

    #[test]
    fn resolver_for_band_does_not_choose_stronger_ap_from_other_band() {
        let mut g = NetworkGraph::default();
        let d = device("/d0", "wlan0");
        let mut two_four = ap("/ap/2", "/d0", "Foo", 2412);
        two_four.strength = 40;
        let mut five = ap("/ap/5", "/d0", "Foo", 5180);
        five.strength = 90;
        let did = d.id.clone();
        g.devices.insert(did.clone(), d);
        g.access_points.extend([
            (two_four.id.clone(), two_four),
            (five.id.clone(), five),
        ]);
        g.device_aps.insert(
            did.clone(),
            vec!["/ap/2".parse().unwrap(), "/ap/5".parse().unwrap()],
        );
        g.saved_connections.insert(
            "/s/1".parse().unwrap(),
            profile("/s/1", "global", None, "Foo"),
        );
        let CandidateSelection::Ready { access_point, .. } = g
            .activation_selection_for_band(&did, "Foo", Some(Band::Ghz2_4))
            .unwrap()
        else {
            panic!("expected band-specific candidate")
        };
        assert_eq!(access_point.id, "/ap/2".parse().unwrap());
    }

    #[test]
    fn resolver_distinguishes_visible_unsaved_from_invisible_saved_network() {
        let mut g = NetworkGraph::default();
        let d = device("/d0", "wlan0");
        let visible = ap("/ap/0", "/d0", "Visible", 2412);
        let did = d.id.clone();
        g.devices.insert(did.clone(), d);
        g.access_points.insert(visible.id.clone(), visible);
        g.device_aps
            .insert(did.clone(), vec!["/ap/0".parse().unwrap()]);
        assert!(matches!(
            g.activation_selection(&did, "Visible").unwrap(),
            CandidateSelection::UnsavedNetwork
        ));
        g.saved_connections.insert(
            "/s/5g".parse().unwrap(),
            profile("/s/5g", "saved", Some("wlan0"), "Invisible"),
        );
        assert!(matches!(
            g.activation_selection(&did, "Invisible").unwrap_err(),
            Error::AccessPointNotFound { .. }
        ));
    }

    #[test]
    fn activation_preflight_is_idempotent_by_device_state_and_ssid() {
        let state = CurrentWifiState {
            interface: "wlan0".into(),
            device_state: DeviceState::Activated,
            active_connection: Some("/active/1".parse().unwrap()),
            active_connection_id: Some("DEMOSTENES-2.4G 1".into()),
            active_connection_state: Some(2),
            active_ap: Some("/ap/1".parse().unwrap()),
            ssid: Some("DEMOSTENES-2.4G".into()),
            frequency: Some(2447),
            strength: Some(74),
            device_state_reason: None,
            active_connection_state_reason: None,
        };
        assert!(already_active(&state, "DEMOSTENES-2.4G"));
        assert!(activation_succeeded(&state, "DEMOSTENES-2.4G"));
        assert!(!already_active(&state, "DEMOSTENES-5G"));
        assert!(!activation_succeeded(&state, "DEMOSTENES-5G"));
    }

    #[test]
    fn scan_completion_requires_same_device_and_new_timestamp() {
        let device: DeviceId = "/d0".parse().unwrap();
        assert!(!scan_event_matches(
            &NetworkEvent::ScanCompleted {
                device: "/d1".parse().unwrap(),
                last_scan: 200,
            },
            &device,
            100
        ));
        assert!(!scan_event_matches(
            &NetworkEvent::ScanCompleted {
                device: device.clone(),
                last_scan: 100,
            },
            &device,
            100
        ));
        assert!(scan_event_matches(
            &NetworkEvent::ScanCompleted {
                device,
                last_scan: 200,
            },
            &"/d0".parse().unwrap(),
            100
        ));
    }

    #[test]
    fn activation_evaluation_is_semantic_and_reports_terminal_reason() {
        let mut state = CurrentWifiState {
            interface: "wlan0".into(),
            device_state: DeviceState::Activated,
            active_connection: None,
            active_connection_id: None,
            active_connection_state: Some(2),
            active_ap: None,
            ssid: Some("Foo".into()),
            frequency: Some(5765),
            strength: Some(80),
            device_state_reason: None,
            active_connection_state_reason: None,
        };
        assert_eq!(
            evaluate_activation(Some(&state), "Foo"),
            ActivationEvaluation::Succeeded
        );
        assert_eq!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Pending
        );
        state.device_state = DeviceState::Failed;
        state.device_state_reason = Some(DeviceStateReason {
            state: 120,
            reason: 53,
        });
        assert!(matches!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Failed(_)
        ));
        assert!(matches!(
            evaluate_activation(None, "Bar"),
            ActivationEvaluation::Failed(_)
        ));
    }

    fn handover_state(device_state: DeviceState, ssid: Option<&str>) -> CurrentWifiState {
        CurrentWifiState {
            interface: "wlan0".into(),
            device_state,
            active_connection: Some("/active/b".parse().unwrap()),
            active_connection_id: Some("B".into()),
            active_connection_state: Some(1),
            active_ap: ssid.map(|_| "/ap/bar".parse().unwrap()),
            ssid: ssid.map(str::to_owned),
            frequency: Some(5180),
            strength: Some(70),
            device_state_reason: None,
            active_connection_state_reason: None,
        }
    }

    #[test]
    fn device_deactivating_during_handover_is_pending() {
        let state = handover_state(DeviceState::Deactivating, Some("Foo"));
        assert_eq!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Pending
        );
    }

    #[test]
    fn real_handover_trace_never_fails_before_target_converges() {
        let states = [
            handover_state(DeviceState::Deactivating, Some("Foo")),
            handover_state(DeviceState::Deactivating, Some("Foo")),
            handover_state(DeviceState::Deactivating, Some("Foo")),
            handover_state(DeviceState::Deactivating, Some("Foo")),
            handover_state(DeviceState::Prepare, Some("Foo")),
            handover_state(DeviceState::Prepare, Some("Foo")),
            handover_state(DeviceState::Activated, Some("Bar")),
        ];
        for (index, state) in states.iter().enumerate() {
            let result = evaluate_activation(Some(state), "Bar");
            if index < states.len() - 1 {
                assert_eq!(result, ActivationEvaluation::Pending, "cycle {index}");
            } else {
                assert_eq!(result, ActivationEvaluation::Succeeded);
            }
        }
    }

    #[test]
    fn returned_active_connection_activated_while_device_transient_is_pending() {
        let mut state = handover_state(DeviceState::Prepare, Some("Foo"));
        state.active_connection_state = Some(2);
        assert_eq!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Pending
        );
    }

    #[test]
    fn semantic_target_state_wins_even_if_requested_connection_disappeared() {
        let state = handover_state(DeviceState::Activated, Some("Bar"));
        assert_eq!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Succeeded
        );
    }

    #[test]
    fn device_failed_is_immediate_failure_with_state_reason() {
        let mut state = handover_state(DeviceState::Failed, None);
        state.device_state_reason = Some(DeviceStateReason {
            state: 120,
            reason: 7,
        });
        assert!(matches!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Failed(ActivationFailure::DeviceTerminal {
                reason: Some(DeviceStateReason {
                    state: 120,
                    reason: 7
                }),
                ..
            })
        ));
    }

    #[test]
    fn activated_active_connection_state_two_is_not_failure() {
        let mut state = handover_state(DeviceState::Prepare, Some("Foo"));
        state.active_connection_state = Some(2);
        assert!(!matches!(
            evaluate_activation(Some(&state), "Bar"),
            ActivationEvaluation::Failed(_)
        ));
    }
}
