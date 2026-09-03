use crate::model::{AccessPoint, ActiveConnection, SavedConnection, WifiDevice};
use crate::{AccessPointId, ActiveConnectionId, DeviceId, SavedConnectionId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkEvent {
    DeviceAdded(WifiDevice),
    DeviceRemoved(DeviceId),
    DeviceChanged(WifiDevice),
    AccessPointAdded(AccessPoint),
    AccessPointRemoved {
        device: DeviceId,
        access_point: AccessPointId,
    },
    AccessPointChanged(AccessPoint),
    ActiveConnectionAdded(ActiveConnection),
    ActiveConnectionRemoved(ActiveConnectionId),
    ActiveConnectionChanged(ActiveConnection),
    SavedConnectionAdded(SavedConnection),
    SavedConnectionRemoved(SavedConnectionId),
    SavedConnectionChanged(SavedConnection),
    ScanCompleted {
        device: DeviceId,
        last_scan: i64,
    },
}
