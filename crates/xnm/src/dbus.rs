use std::collections::HashMap;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

pub(crate) const SERVICE: &str = "org.freedesktop.NetworkManager";
pub(crate) const ROOT: &str = "/org/freedesktop/NetworkManager";
pub(crate) const SETTINGS: &str = "/org/freedesktop/NetworkManager/Settings";

#[zbus::proxy(interface = "org.freedesktop.NetworkManager")]
pub(crate) trait Manager {
    fn get_all_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn activate_connection(
        &self,
        connection: OwnedObjectPath,
        device: OwnedObjectPath,
        specific_object: OwnedObjectPath,
    ) -> zbus::Result<OwnedObjectPath>;
    fn get_device_by_ip_iface(&self, interface: &str) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn active_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Device")]
pub(crate) trait Device {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn driver(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn state_reason(&self) -> zbus::Result<(u32, u32)>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Device.Wireless")]
pub(crate) trait Wireless {
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn request_scan(&self, options: HashMap<&str, OwnedValue>) -> zbus::Result<()>;
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn last_scan(&self) -> zbus::Result<i64>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.AccessPoint")]
pub(crate) trait AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Settings")]
pub(crate) trait Settings {
    fn list_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Settings.Connection")]
pub(crate) trait Setting {
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
}
#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Connection.Active")]
pub(crate) trait Active {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state_reason(&self) -> zbus::Result<(u32, u32)>;
    #[zbus(property)]
    fn connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}
