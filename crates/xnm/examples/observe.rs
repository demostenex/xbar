use std::{env, fs};
use xnm::{Client, NetworkEvent};

fn resources() {
    let pid = std::process::id();
    let s = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let t = s
        .lines()
        .find(|x| x.starts_with("Threads:"))
        .unwrap_or("Threads: ?");
    let r = s
        .lines()
        .find(|x| x.starts_with("VmRSS:"))
        .unwrap_or("VmRSS: ?");
    let f = fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|x| x.count())
        .unwrap_or(0);
    eprintln!(
        "RESOURCE {t} {r} fds={f} system_dbus_connections=1 timers=0 polling=0 subprocesses=0"
    );
}
fn print_states(client: &Client) {
    for d in client.wifi_devices() {
        println!("CURRENT_WIFI_STATE {:?}", client.current_wifi_state(&d.id));
    }
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = Client::connect().await?;
    resources();
    let args: Vec<String> = env::args().collect();
    if let Some(pos) = args.iter().position(|x| x == "--scan") {
        if let Some(iface) = args.get(pos + 1) {
            if let Some(d) = client
                .wifi_devices()
                .into_iter()
                .find(|d| d.interface == *iface)
            {
                client.request_scan(&d.id).await?;
                println!("SCAN_REQUEST_ACCEPTED device={iface}");
            }
        }
    }
    for d in client.wifi_devices() {
        println!("CURRENT_WIFI_STATE {:?}", client.current_wifi_state(&d.id));
    }
    loop {
        match client.next_event().await? {
            NetworkEvent::DeviceAdded(d) => println!("DEVICE_ADDED {}", d.interface),
            NetworkEvent::DeviceRemoved(id) => println!("DEVICE_REMOVED {id}"),
            NetworkEvent::DeviceStateChanged {
                device,
                new_state,
                old_state,
                reason,
            } => println!(
                "DEVICE_STATE_CHANGED device={device} old_state={old_state} new_state={new_state} reason={reason}"
            ),
            NetworkEvent::DeviceChanged(d) => println!(
                "DEVICE_CHANGED {} state={:?} active={:?}",
                d.interface, d.state, d.active_connection
            ),
            NetworkEvent::AccessPointAdded(a) => println!(
                "AP_ADDED {} ssid={} frequency={}",
                a.id, a.ssid, a.frequency
            ),
            NetworkEvent::AccessPointRemoved {
                device,
                access_point,
            } => println!("AP_REMOVED device={device} path={access_point}"),
            NetworkEvent::AccessPointChanged(a) => println!(
                "AP_CHANGED {} ssid={} frequency={} strength={}",
                a.id, a.ssid, a.frequency, a.strength
            ),
            NetworkEvent::ActiveConnectionAdded(a) => {
                println!("ACTIVE_CONNECTION_ADDED {} id={}", a.id, a.name)
            }
            NetworkEvent::ActiveConnectionRemoved(id) => println!("ACTIVE_CONNECTION_REMOVED {id}"),
            NetworkEvent::ActiveConnectionChanged(a) => println!(
                "ACTIVE_CONNECTION_CHANGED {} id={} state={}",
                a.id, a.name, a.state
            ),
            NetworkEvent::SavedConnectionAdded(s) => println!("SETTING_ADDED {}", s.name),
            NetworkEvent::SavedConnectionRemoved(id) => println!("SETTING_REMOVED {id}"),
            NetworkEvent::SavedConnectionChanged(s) => println!("SETTING_CHANGED {}", s.name),
            NetworkEvent::ScanCompleted { device, last_scan } => {
                println!("SCAN_COMPLETED device={device} last_scan={last_scan}")
            }
        }
        print_states(&client);
    }
}
fn main() {
    zbus::block_on(run()).unwrap_or_else(|e| {
        eprintln!("xnm observe: {e}");
        std::process::exit(1)
    });
}
