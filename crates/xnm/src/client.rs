use crate::dbus::*;
use crate::event::NetworkEvent;
use crate::model::{
    AccessPoint, ActiveConnection, CandidateSelection, CurrentWifiState, NetworkGraph,
    SavedConnection, WifiDevice,
};
use crate::{id, AccessPointId, ActiveConnectionId, DeviceId, Error, SavedConnectionId};
use futures_util::TryStreamExt;
use std::collections::{HashMap, HashSet};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

pub struct Client {
    connection: Connection,
    stream: MessageStream,
    graph: NetworkGraph,
    pending: Vec<NetworkEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationCandidate {
    pub profile: SavedConnection,
    pub access_point: AccessPoint,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedActivation {
    pub profile: SavedConnection,
    pub specific_ap: Option<AccessPointId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateResolution {
    Ready(Box<ActivationCandidate>),
    AmbiguousSavedConnections(Vec<SavedConnection>),
    UnsavedNetwork,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationFailure {
    DeviceRemoved,
    DeviceTerminal {
        state: crate::model::DeviceState,
        reason: Option<crate::model::DeviceStateReason>,
    },
    ActiveConnectionTerminal {
        state: u32,
        reason: Option<crate::model::DeviceStateReason>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationEvaluation {
    Pending,
    Succeeded,
    Failed(ActivationFailure),
}

pub fn evaluate_activation(
    state: Option<&CurrentWifiState>,
    target_ssid: &str,
) -> ActivationEvaluation {
    let Some(state) = state else {
        return ActivationEvaluation::Failed(ActivationFailure::DeviceRemoved);
    };
    if state.device_state == crate::model::DeviceState::Activated
        && state.ssid.as_deref() == Some(target_ssid)
    {
        return ActivationEvaluation::Succeeded;
    }
    if matches!(state.device_state, crate::model::DeviceState::Failed) {
        return ActivationEvaluation::Failed(ActivationFailure::DeviceTerminal {
            state: state.device_state,
            reason: state.device_state_reason,
        });
    }
    if state.active_connection_state == Some(4) {
        return ActivationEvaluation::Failed(ActivationFailure::ActiveConnectionTerminal {
            state: 4,
            reason: state.active_connection_state_reason,
        });
    }
    ActivationEvaluation::Pending
}

#[cfg(test)]
pub(crate) fn already_active(state: &CurrentWifiState, ssid: &str) -> bool {
    state.device_state == crate::model::DeviceState::Activated
        && state.ssid.as_deref() == Some(ssid)
}

#[cfg(test)]
pub(crate) fn activation_succeeded(state: &CurrentWifiState, ssid: &str) -> bool {
    state.device_state == crate::model::DeviceState::Activated
        && state.ssid.as_deref() == Some(ssid)
}

#[cfg(test)]
pub(crate) fn scan_event_matches(event: &NetworkEvent, device: &DeviceId, before: i64) -> bool {
    matches!(event, NetworkEvent::ScanCompleted { device: event_device, last_scan } if event_device == device && *last_scan > before)
}

impl Client {
    pub async fn connect() -> Result<Self, Error> {
        let connection = Connection::system()
            .await
            .map_err(|e| Error::Dbus(e.to_string()))?;
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(SERVICE)
            .map_err(e)?
            .build();
        let stream = MessageStream::for_match_rule(rule, &connection, Some(128))
            .await
            .map_err(e)?;
        let mut c = Self {
            connection,
            stream,
            graph: NetworkGraph::default(),
            pending: Vec::new(),
        };
        c.bootstrap().await?;
        Ok(c)
    }
    pub fn wifi_devices(&self) -> Vec<WifiDevice> {
        self.graph.devices.values().cloned().collect()
    }
    pub fn wireless_enabled(&self) -> bool {
        self.graph.wireless_enabled
    }
    pub fn wifi_device(&self, id: &DeviceId) -> Option<WifiDevice> {
        self.graph.devices.get(id).cloned()
    }
    pub fn access_points(&self, id: &DeviceId) -> Vec<AccessPoint> {
        self.graph
            .device_aps
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|p| self.graph.access_points.get(p))
            .cloned()
            .collect()
    }
    pub fn active_access_point(&self, id: &DeviceId) -> Option<AccessPoint> {
        self.graph
            .devices
            .get(id)?
            .active_ap
            .as_ref()
            .and_then(|p| self.graph.access_points.get(p))
            .cloned()
    }
    pub fn active_connection(&self, id: &DeviceId) -> Option<ActiveConnection> {
        self.graph
            .devices
            .get(id)?
            .active_connection
            .as_ref()
            .and_then(|p| self.graph.active_connections.get(p))
            .cloned()
    }
    pub fn saved_connections(&self) -> Vec<SavedConnection> {
        self.graph.saved_connections.values().cloned().collect()
    }
    pub fn networks(&self, id: &DeviceId) -> Vec<(String, Vec<AccessPointId>)> {
        let mut out: HashMap<String, Vec<AccessPointId>> = HashMap::new();
        for ap in self.access_points(id) {
            if !ap.ssid.is_empty() {
                out.entry(ap.ssid.clone()).or_default().push(ap.id);
            }
        }
        out.into_iter().collect()
    }
    pub fn current_wifi_state(&self, id: &DeviceId) -> Option<CurrentWifiState> {
        self.graph.current(id)
    }
    pub fn device_by_interface(&self, interface: &str) -> Option<WifiDevice> {
        self.graph
            .devices
            .values()
            .find(|d| d.interface == interface)
            .cloned()
    }
    pub fn activation_candidates(
        &self,
        device: &DeviceId,
        ssid: &str,
    ) -> Result<CandidateResolution, Error> {
        match self.graph.activation_selection(device, ssid)? {
            CandidateSelection::Ready {
                profile,
                access_point,
            } => Ok(CandidateResolution::Ready(Box::new(ActivationCandidate {
                profile: *profile,
                access_point: *access_point,
            }))),
            CandidateSelection::Ambiguous(profiles) => {
                Ok(CandidateResolution::AmbiguousSavedConnections(profiles))
            }
            CandidateSelection::UnsavedNetwork => Ok(CandidateResolution::UnsavedNetwork),
        }
    }
    pub fn saved_profile(&self, device: &DeviceId, ssid: &str) -> Result<SavedConnection, Error> {
        self.graph.saved_profile(device, ssid)
    }
    pub fn saved_activation(
        &self,
        device: &DeviceId,
        ssid: &str,
    ) -> Result<SavedActivation, Error> {
        let profile = self.graph.saved_profile(device, ssid)?;
        let specific_ap = self.graph.activation_selection(device, ssid).ok().and_then(
            |selection| match selection {
                CandidateSelection::Ready { access_point, .. } => Some(access_point.id.clone()),
                _ => None,
            },
        );
        Ok(SavedActivation {
            profile,
            specific_ap,
        })
    }
    pub async fn activate_saved_wifi(
        &mut self,
        device: &DeviceId,
        profile: &SavedConnectionId,
        specific_ap: Option<AccessPointId>,
    ) -> Result<ActiveConnectionId, Error> {
        let rebound = self.late_bind_device(device).await?;
        if rebound != *device {
            return Err(Error::DeviceDisappeared {
                interface: self.graph.devices[&rebound].interface.clone(),
                expected: device.clone(),
                actual: rebound,
            });
        }
        let d = self
            .graph
            .devices
            .get(device)
            .ok_or_else(|| Error::UnknownDevice(device.clone()))?;
        let p = self
            .graph
            .saved_connections
            .get(profile)
            .ok_or_else(|| Error::UnknownSavedConnection(profile.clone()))?;
        if p.connection_type != "802-11-wireless" && p.connection_type != "wifi" {
            return Err(Error::NotWifiProfile(profile.clone()));
        }
        if let Some(interface) = &p.interface_name {
            if interface != &d.interface {
                return Err(Error::ProfileInterfaceMismatch {
                    profile: profile.clone(),
                    interface: interface.clone(),
                    device: d.interface.clone(),
                });
            }
        }
        if let Some(access_point) = &specific_ap {
            let a = self
                .graph
                .access_points
                .get(access_point)
                .ok_or_else(|| Error::UnknownAccessPoint(access_point.clone()))?;
            if &a.device != device {
                return Err(Error::AccessPointDeviceMismatch {
                    access_point: access_point.clone(),
                    device: device.clone(),
                });
            }
            if let Some(profile_ssid) = &p.ssid {
                if !profile_ssid.is_empty() && profile_ssid != &a.ssid {
                    return Err(Error::ProfileSsidMismatch {
                        profile: profile.clone(),
                        profile_ssid: profile_ssid.clone(),
                        ap_ssid: a.ssid.clone(),
                    });
                }
            }
        }
        let n = ManagerProxy::new(&self.connection, SERVICE, ROOT)
            .await
            .map_err(e)?;
        let path = n
            .activate_connection(
                profile.as_str().try_into().map_err(e)?,
                device.as_str().try_into().map_err(e)?,
                specific_ap.as_ref().map_or_else(
                    || OwnedObjectPath::try_from("/").map_err(e),
                    |ap| ap.as_str().try_into().map_err(e),
                )?,
            )
            .await
            .map_err(e)?;
        Ok(id(path.to_string()))
    }
    pub async fn request_scan(&mut self, id: &DeviceId) -> Result<DeviceId, Error> {
        let current = self.late_bind_device(id).await?;
        let w = WirelessProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(current.as_str())
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        w.request_scan(HashMap::new()).await.map_err(e)?;
        Ok(current)
    }
    pub async fn late_bind_device(&mut self, device_id: &DeviceId) -> Result<DeviceId, Error> {
        let old = self
            .graph
            .devices
            .get(device_id)
            .ok_or_else(|| Error::UnknownDevice(device_id.clone()))?
            .clone();
        let n = ManagerProxy::new(&self.connection, SERVICE, ROOT)
            .await
            .map_err(e)?;
        let path = n.get_device_by_ip_iface(&old.interface).await.map_err(e)?;
        let current = id::<DeviceId>(path.to_string());
        if current != *device_id {
            self.graph.remove_device(device_id);
            self.materialize_device(current.as_str()).await?;
        }
        Ok(current)
    }
    pub async fn wait_for_scan(
        &mut self,
        id: &DeviceId,
        last_scan_before: Option<i64>,
    ) -> Result<i64, Error> {
        let before = last_scan_before.unwrap_or(i64::MIN);
        loop {
            if let NetworkEvent::ScanCompleted { device, last_scan } = self.next_event().await? {
                if &device == id && last_scan > before {
                    return Ok(last_scan);
                }
            }
        }
    }
    pub async fn next_event(&mut self) -> Result<NetworkEvent, Error> {
        if let Some(e) = self.pending.pop() {
            return Ok(e);
        }
        loop {
            let msg = self
                .stream
                .try_next()
                .await
                .map_err(e)?
                .ok_or_else(|| Error::Dbus("signal stream closed".into()))?;
            if let Some(event) = self.handle(msg).await? {
                return Ok(event);
            }
        }
    }
    async fn bootstrap(&mut self) -> Result<(), Error> {
        let n = ManagerProxy::new(&self.connection, SERVICE, ROOT)
            .await
            .map_err(e)?;
        self.graph.wireless_enabled = n.wireless_enabled().await.map_err(e)?;
        for p in n.get_all_devices().await.map_err(e)? {
            self.materialize_device(&p.to_string()).await?;
        }
        for p in n.active_connections().await.map_err(e)? {
            self.insert_active(self.read_active(&p.to_string()).await?);
        }
        let s = SettingsProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(SETTINGS)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        for p in s.list_connections().await.map_err(e)? {
            self.insert_setting(self.read_setting(&p.to_string()).await?);
        }
        Ok(())
    }
    async fn materialize_device(&mut self, p: &str) -> Result<(), Error> {
        if self
            .graph
            .devices
            .contains_key(&id::<DeviceId>(p.to_owned()))
        {
            return Ok(());
        }
        let d = DeviceProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(p)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        if d.device_type().await.map_err(e)? != 2 {
            return Ok(());
        }
        let did = id::<DeviceId>(p.to_owned());
        self.graph.devices.insert(
            did.clone(),
            WifiDevice {
                id: did.clone(),
                interface: d.interface().await.map_err(e)?,
                driver: Some(d.driver().await.map_err(e)?),
                state: d.state().await.map_err(e)?.into(),
                active_connection: obj(d.active_connection().await.map_err(e)?).map(id),
                active_ap: None,
                last_scan: None,
                state_reason: Some(reason_from_tuple(d.state_reason().await.map_err(e)?)),
            },
        );
        let w = WirelessProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(p)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        let (active, last) = (
            obj(w.active_access_point().await.map_err(e)?),
            Some(w.last_scan().await.map_err(e)?),
        );
        if let Some(d) = self.graph.devices.get_mut(&did) {
            d.active_ap = active.map(id);
            d.last_scan = last;
        }
        for ap in w.get_all_access_points().await.map_err(e)? {
            self.insert_ap(
                did.clone(),
                self.read_ap(&ap.to_string(), did.clone()).await?,
            );
        }
        if let Some(ac) = self.graph.devices[&did].active_connection.clone() {
            if !self.graph.active_connections.contains_key(&ac) {
                self.insert_active(self.read_active(ac.as_str()).await?);
            }
        }
        Ok(())
    }
    async fn read_ap(&self, p: &str, device: DeviceId) -> Result<AccessPoint, Error> {
        let a = AccessPointProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(p)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        Ok(AccessPoint {
            id: id(p.to_owned()),
            device,
            ssid: String::from_utf8_lossy(&a.ssid().await.unwrap_or_default())
                .trim_end_matches('\0')
                .into(),
            bssid: a.hw_address().await.unwrap_or_default(),
            frequency: a.frequency().await.unwrap_or_default(),
            strength: a.strength().await.unwrap_or_default(),
            flags: a.flags().await.unwrap_or_default(),
            wpa_flags: a.wpa_flags().await.unwrap_or_default(),
            rsn_flags: a.rsn_flags().await.unwrap_or_default(),
        })
    }
    async fn read_active(&self, p: &str) -> Result<ActiveConnection, Error> {
        let a = ActiveProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(p)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        Ok(ActiveConnection {
            id: id(p.to_owned()),
            name: a.id().await.unwrap_or_default(),
            uuid: Some(a.uuid().await.unwrap_or_default()),
            state: a.state().await.unwrap_or_default(),
            profile: obj(a.connection().await.map_err(e)?).map(id::<SavedConnectionId>),
            devices: a
                .devices()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|x| id::<DeviceId>(x.to_string()))
                .collect(),
            state_reason: Some(reason_from_tuple(
                a.state_reason().await.unwrap_or_default(),
            )),
        })
    }
    async fn read_setting(&self, p: &str) -> Result<SavedConnection, Error> {
        let a = SettingProxy::builder(&self.connection)
            .destination(SERVICE)
            .map_err(e)?
            .path(p)
            .map_err(e)?
            .build()
            .await
            .map_err(e)?;
        let m = a.get_settings().await.map_err(e)?;
        let n = m.get("connection");
        let w = m.get("802-11-wireless");
        let ssid = w
            .and_then(|x| x.get("ssid"))
            .and_then(|v| Vec::<u8>::try_from(v.clone()).ok())
            .map(|x| String::from_utf8_lossy(&x).into());
        Ok(SavedConnection {
            id: id(p.to_owned()),
            name: n.and_then(|x| value_string(x, "id")).unwrap_or_default(),
            uuid: n.and_then(|x| value_string(x, "uuid")).unwrap_or_default(),
            connection_type: n.and_then(|x| value_string(x, "type")).unwrap_or_default(),
            interface_name: n.and_then(|x| value_string(x, "interface-name")),
            ssid,
        })
    }
    fn insert_ap(&mut self, device: DeviceId, ap: AccessPoint) {
        self.graph
            .device_aps
            .entry(device.clone())
            .or_default()
            .retain(|x| x != &ap.id);
        self.graph
            .device_aps
            .entry(device.clone())
            .or_default()
            .push(ap.id.clone());
        self.graph.access_points.insert(ap.id.clone(), ap);
    }
    fn insert_active(&mut self, a: ActiveConnection) {
        self.graph.active_connections.insert(a.id.clone(), a);
    }
    fn insert_setting(&mut self, s: SavedConnection) {
        self.graph.saved_connections.insert(s.id.clone(), s);
    }
    async fn handle(&mut self, msg: zbus::Message) -> Result<Option<NetworkEvent>, Error> {
        let h = msg.header();
        let p = h.path().map(|x| x.to_string()).unwrap_or_default();
        let i = h.interface().map(|x| x.to_string()).unwrap_or_default();
        let member = h.member().map(|x| x.to_string()).unwrap_or_default();
        if i == SERVICE && member == "DeviceAdded" {
            let (x,): (OwnedObjectPath,) = msg.body().deserialize().map_err(e)?;
            let p = x.to_string();
            self.materialize_device(&p).await?;
            return Ok(self
                .graph
                .devices
                .get(&id::<DeviceId>(p))
                .cloned()
                .map(NetworkEvent::DeviceAdded));
        }
        if i == SERVICE && member == "DeviceRemoved" {
            let (x,): (OwnedObjectPath,) = msg.body().deserialize().map_err(e)?;
            let id = id::<DeviceId>(x.to_string());
            self.graph.remove_device(&id);
            return Ok(Some(NetworkEvent::DeviceRemoved(id)));
        }
        if i == "org.freedesktop.NetworkManager.Device" && member == "StateChanged" {
            let (new_state, old_state, reason): (u32, u32, u32) =
                msg.body().deserialize().map_err(e)?;
            let did = id::<DeviceId>(p);
            if let Some(device) = self.graph.devices.get_mut(&did) {
                device.state = new_state.into();
                device.state_reason = Some(crate::model::DeviceStateReason {
                    state: new_state,
                    reason,
                });
            }
            return Ok(Some(NetworkEvent::DeviceStateChanged {
                device: did,
                new_state,
                old_state,
                reason,
            }));
        }
        if i.ends_with("Device.Wireless")
            && (member == "AccessPointAdded" || member == "AccessPointRemoved")
        {
            let (x,): (OwnedObjectPath,) = msg.body().deserialize().map_err(e)?;
            let did = id::<DeviceId>(p);
            let aid = id::<AccessPointId>(x.to_string());
            if member == "AccessPointAdded" {
                let a = self.read_ap(aid.as_str(), did.clone()).await?;
                self.insert_ap(did, a.clone());
                return Ok(Some(NetworkEvent::AccessPointAdded(a)));
            }
            self.graph
                .device_aps
                .entry(did.clone())
                .or_default()
                .retain(|a| a != &aid);
            self.graph.access_points.remove(&aid);
            return Ok(Some(NetworkEvent::AccessPointRemoved {
                device: did,
                access_point: aid,
            }));
        }
        if i == "org.freedesktop.DBus.Properties" && member == "PropertiesChanged" {
            let (changed, vals, _): (String, HashMap<String, OwnedValue>, Vec<String>) =
                msg.body().deserialize().map_err(e)?;
            return self.properties(&p, &changed, &vals).await;
        }
        if i.ends_with("Settings") && member == "NewConnection" {
            let (x,): (OwnedObjectPath,) = msg.body().deserialize().map_err(e)?;
            let s = self.read_setting(&x.to_string()).await?;
            let out = s.clone();
            self.insert_setting(s);
            return Ok(Some(NetworkEvent::SavedConnectionAdded(out)));
        }
        if i.ends_with("Settings") && member == "ConnectionRemoved" {
            let (x,): (OwnedObjectPath,) = msg.body().deserialize().map_err(e)?;
            let id = id::<SavedConnectionId>(x.to_string());
            self.graph.saved_connections.remove(&id);
            return Ok(Some(NetworkEvent::SavedConnectionRemoved(id)));
        }
        if i == "org.freedesktop.NetworkManager.Settings.Connection" && member == "Updated" {
            let s = self.read_setting(&p).await?;
            let out = s.clone();
            self.insert_setting(s);
            return Ok(Some(NetworkEvent::SavedConnectionChanged(out)));
        }
        Ok(None)
    }
    async fn properties(
        &mut self,
        p: &str,
        changed: &str,
        v: &HashMap<String, OwnedValue>,
    ) -> Result<Option<NetworkEvent>, Error> {
        if changed == SERVICE {
            if let Some(enabled) = v.get("WirelessEnabled").and_then(boolv) {
                self.pending.push(set_wireless_enabled(&mut self.graph, enabled));
            }
            if let Some(x) = v.get("ActiveConnections") {
                let now = Vec::<OwnedObjectPath>::try_from(x.clone())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|x| id::<ActiveConnectionId>(x.to_string()))
                    .collect::<HashSet<_>>();
                let old = self
                    .graph
                    .active_connections
                    .keys()
                    .cloned()
                    .collect::<HashSet<_>>();
                for dead in old.difference(&now) {
                    self.graph.active_connections.remove(dead);
                    self.pending
                        .push(NetworkEvent::ActiveConnectionRemoved(dead.clone()));
                }
                for new in now.difference(&old) {
                    let a = self.read_active(new.as_str()).await?;
                    self.insert_active(a.clone());
                    self.pending.push(NetworkEvent::ActiveConnectionAdded(a));
                }
            }
            return Ok(self.pending.pop());
        }
        if changed == "org.freedesktop.NetworkManager.Device" {
            let did = id::<DeviceId>(p.to_owned());
            if !self.graph.devices.contains_key(&did) {
                return Ok(None);
            }
            let mut active = None;
            if let Some(d) = self.graph.devices.get_mut(&did) {
                if let Some(x) = v.get("State").and_then(u32v) {
                    d.state = x.into();
                }
                if v.contains_key("ActiveConnection") {
                    active = v.get("ActiveConnection").and_then(wire_path);
                    d.active_connection = active.clone().map(id);
                }
                if let Some(x) = v.get("StateReason").and_then(state_reason) {
                    d.state_reason = Some(x);
                }
            }
            if let Some(x) = active {
                let a = id::<ActiveConnectionId>(x);
                if !self.graph.active_connections.contains_key(&a) {
                    self.insert_active(self.read_active(a.as_str()).await?);
                }
                return Ok(Some(NetworkEvent::DeviceChanged(
                    self.graph.devices[&did].clone(),
                )));
            }
            return Ok(Some(NetworkEvent::DeviceChanged(
                self.graph.devices[&did].clone(),
            )));
        }
        if changed == "org.freedesktop.NetworkManager.Device.Wireless" {
            let did = id::<DeviceId>(p.to_owned());
            if !self.graph.devices.contains_key(&did) {
                return Ok(None);
            }
            if let Some(x) = v.get("ActiveAccessPoint").and_then(wire_path) {
                let aid = id::<AccessPointId>(x);
                if !self.graph.access_points.contains_key(&aid) {
                    let a = self.read_ap(aid.as_str(), did.clone()).await?;
                    self.insert_ap(did.clone(), a);
                }
                self.graph.devices.get_mut(&did).unwrap().active_ap = Some(aid);
            } else if v.contains_key("ActiveAccessPoint") {
                self.graph.devices.get_mut(&did).unwrap().active_ap = None;
            }
            if let Some(x) = v.get("LastScan").and_then(i64v) {
                self.graph.devices.get_mut(&did).unwrap().last_scan = Some(x);
                return Ok(Some(NetworkEvent::ScanCompleted {
                    device: did,
                    last_scan: x,
                }));
            }
            return Ok(Some(NetworkEvent::DeviceChanged(
                self.graph.devices[&did].clone(),
            )));
        }
        if changed == "org.freedesktop.NetworkManager.AccessPoint" {
            let aid = id::<AccessPointId>(p.to_owned());
            if !self.graph.access_points.contains_key(&aid) {
                return Ok(None);
            }
            if let Some(a) = self.graph.access_points.get_mut(&aid) {
                if let Some(x) = v
                    .get("Ssid")
                    .and_then(|x| Vec::<u8>::try_from(x.clone()).ok())
                {
                    a.ssid = String::from_utf8_lossy(&x).trim_end_matches('\0').into();
                }
                if let Some(x) = v
                    .get("HwAddress")
                    .and_then(|x| x.downcast_ref::<String>().ok())
                {
                    a.bssid = x.clone();
                }
                if let Some(x) = v.get("Frequency").and_then(u32v) {
                    a.frequency = x;
                }
                if let Some(x) = v.get("Strength").and_then(u8v) {
                    a.strength = x;
                }
                if let Some(x) = v.get("Flags").and_then(u32v) {
                    a.flags = x;
                }
                if let Some(x) = v.get("WpaFlags").and_then(u32v) {
                    a.wpa_flags = x;
                }
                if let Some(x) = v.get("RsnFlags").and_then(u32v) {
                    a.rsn_flags = x;
                }
                return Ok(Some(NetworkEvent::AccessPointChanged(a.clone())));
            }
        }
        if changed == "org.freedesktop.NetworkManager.Connection.Active" {
            let aid = id::<ActiveConnectionId>(p.to_owned());
            if !self.graph.active_connections.contains_key(&aid) {
                return Ok(None);
            }
            if let Some(a) = self.graph.active_connections.get_mut(&aid) {
                if let Some(x) = v.get("Id").and_then(|x| x.downcast_ref::<String>().ok()) {
                    a.name = x.clone();
                }
                if let Some(x) = v.get("State").and_then(u32v) {
                    a.state = x;
                }
                if let Some(x) = v.get("StateReason").and_then(state_reason) {
                    a.state_reason = Some(x);
                }
                if let Some(x) = v.get("Connection").and_then(wire_path) {
                    a.profile = Some(id(x));
                }
                if let Some(x) = v
                    .get("Devices")
                    .and_then(|x| Vec::<OwnedObjectPath>::try_from(x.clone()).ok())
                {
                    a.devices = x.into_iter().map(|x| id(x.to_string())).collect();
                }
                return Ok(Some(NetworkEvent::ActiveConnectionChanged(a.clone())));
            }
        }
        Ok(None)
    }
}
fn e<E: std::fmt::Display>(x: E) -> Error {
    Error::Dbus(x.to_string())
}
fn obj(p: OwnedObjectPath) -> Option<String> {
    let s = p.to_string();
    (s != "/").then_some(s)
}
fn wire_path(v: &OwnedValue) -> Option<String> {
    obj(OwnedObjectPath::try_from(v.clone()).ok()?)
}
fn value_string(m: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    m.get(key).and_then(|v| v.downcast_ref::<String>().ok())
}
fn i64v(v: &OwnedValue) -> Option<i64> {
    i64::try_from(v.clone()).ok()
}
fn u32v(v: &OwnedValue) -> Option<u32> {
    u32::try_from(v.clone()).ok()
}
fn u8v(v: &OwnedValue) -> Option<u8> {
    u8::try_from(v.clone()).ok()
}
fn boolv(v: &OwnedValue) -> Option<bool> {
    bool::try_from(v.clone()).ok()
}
fn set_wireless_enabled(graph: &mut NetworkGraph, enabled: bool) -> NetworkEvent {
    graph.wireless_enabled = enabled;
    NetworkEvent::NetworkManagerChanged {
        wireless_enabled: enabled,
    }
}
fn state_reason(v: &OwnedValue) -> Option<crate::model::DeviceStateReason> {
    let (state, reason) = <(u32, u32)>::try_from(v.clone()).ok()?;
    Some(crate::model::DeviceStateReason { state, reason })
}

fn reason_from_tuple((state, reason): (u32, u32)) -> crate::model::DeviceStateReason {
    crate::model::DeviceStateReason { state, reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_wireless_enabled_true_updates_cache_and_event() {
        let mut graph = NetworkGraph::default();
        let event = set_wireless_enabled(&mut graph, true);
        assert!(graph.wireless_enabled);
        assert_eq!(
            event,
            NetworkEvent::NetworkManagerChanged {
                wireless_enabled: true
            }
        );
    }

    #[test]
    fn manager_wireless_enabled_false_updates_cache_and_event() {
        let mut graph = NetworkGraph {
            wireless_enabled: true,
            ..Default::default()
        };
        let event = set_wireless_enabled(&mut graph, false);
        assert!(!graph.wireless_enabled);
        assert_eq!(
            event,
            NetworkEvent::NetworkManagerChanged {
                wireless_enabled: false
            }
        );
    }
}
