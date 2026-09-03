use futures_util::future::{select, Either};
use futures_util::pin_mut;
use std::time::{Duration, Instant};
use xnm::{
    evaluate_activation, ActivationEvaluation, CandidateResolution, Client, DeviceState, Error,
    NetworkEvent,
};

fn usage() {
    eprintln!("usage: xnm status [interface] | aps <interface> | profiles [interface] | candidates <interface> <ssid> | scan <interface> | connect <interface> <ssid>");
}

fn device(client: &Client, interface: &str) -> Result<xnm::WifiDevice, Error> {
    client
        .device_by_interface(interface)
        .ok_or_else(|| Error::InvalidId(format!("unknown interface: {interface}")))
}

fn print_status(client: &Client, only: Option<&str>) -> Result<(), Error> {
    for d in client
        .wifi_devices()
        .into_iter()
        .filter(|d| only.is_none_or(|x| x == d.interface))
    {
        let s = client.current_wifi_state(&d.id).expect("device exists");
        println!("DEVICE {}", s.interface);
        println!("device_path={}", d.id);
        println!(
            "device_state={}({})",
            state_name(s.device_state),
            state_number(s.device_state)
        );
        if let Some(reason) = s.device_state_reason {
            println!(
                "device_reason={}({})",
                device_reason_label(reason.reason),
                reason.reason
            );
        }
        if let Some(ssid) = s.ssid {
            println!("active_ssid={ssid}");
        }
        if let Some(frequency) = s.frequency {
            println!("frequency={frequency}");
        }
        if let Some(strength) = s.strength {
            println!("strength={strength}");
        }
        if let Some(connection) = s.active_connection_id {
            println!("active_connection={connection}");
        }
        if let Some(state) = s.active_connection_state {
            println!(
                "active_connection_state={}({})",
                active_state_name(state),
                state
            );
        }
        if let Some(reason) = s.active_connection_state_reason {
            println!(
                "active_connection_reason={}({})",
                reason_label(reason.reason),
                reason.reason
            );
        }
        println!();
    }
    Ok(())
}

fn reason_label(reason: u32) -> &'static str {
    match reason {
        7 => "NO_SECRETS",
        36 => "DEVICE_REMOVED",
        53 => "SSID_NOT_FOUND",
        60 => "NEW_ACTIVATION",
        _ => "UNKNOWN",
    }
}

fn device_reason_label(reason: u32) -> &'static str {
    if reason == 0 {
        "NONE"
    } else {
        reason_label(reason)
    }
}

fn print_activation_event(event: &NetworkEvent) {
    match event {
        NetworkEvent::DeviceChanged(d) => {
            println!(
                "DEVICE_STATE device={} state={:?} reason={:?}",
                d.interface,
                d.state,
                d.state_reason
                    .map(|r| format!("{}({})", reason_label(r.reason), r.reason))
            );
            if let Some(ap) = &d.active_ap {
                println!("ACTIVE_AP_CHANGED device={} ap={}", d.interface, ap);
            }
        }
        NetworkEvent::DeviceStateChanged {
            device,
            new_state,
            old_state,
            reason,
        } => println!(
            "DEVICE_STATE_CHANGED device={} old_state={} new_state={} reason={}({})",
            device,
            old_state,
            new_state,
            device_reason_label(*reason),
            reason
        ),
        NetworkEvent::ActiveConnectionChanged(c) | NetworkEvent::ActiveConnectionAdded(c) => {
            println!(
                "ACTIVE_CONNECTION_STATE path={} state={}({}) reason={:?}",
                c.id,
                active_state_name(c.state),
                c.state,
                c.state_reason
                    .map(|r| format!("{}({})", reason_label(r.reason), r.reason))
            );
        }
        NetworkEvent::ActiveConnectionRemoved(id) => {
            println!("ACTIVE_CONNECTION_REMOVED path={id}")
        }
        NetworkEvent::DeviceRemoved(id) => println!("DEVICE_REMOVED path={id}"),
        _ => {}
    }
}

async fn run(args: &[String]) -> Result<(), Error> {
    let mut client = Client::connect().await?;
    match args.get(1).map(String::as_str) {
        Some("status") => print_status(&client, args.get(2).map(String::as_str)),
        Some("aps") => {
            let d = device(
                &client,
                args.get(2)
                    .ok_or_else(|| Error::InvalidId("missing interface".into()))?,
            )?;
            for a in client.access_points(&d.id) {
                println!(
                    "AP {} ssid={:?} bssid={} frequency={} strength={}",
                    a.id, a.ssid, a.bssid, a.frequency, a.strength
                );
            }
            Ok(())
        }
        Some("profiles") => {
            let iface = args.get(2).map(String::as_str);
            for p in client
                .saved_connections()
                .into_iter()
                .filter(|p| p.connection_type == "802-11-wireless" || p.connection_type == "wifi")
                .filter(|p| {
                    iface.is_none_or(|i| {
                        p.interface_name.as_deref().is_none()
                            || p.interface_name.as_deref() == Some(i)
                    })
                })
            {
                println!(
                    "PROFILE {} name={:?} uuid={} interface_name={:?} ssid={:?}",
                    p.id, p.name, p.uuid, p.interface_name, p.ssid
                );
            }
            Ok(())
        }
        Some("candidates") => {
            let iface = args
                .get(2)
                .ok_or_else(|| Error::InvalidId("missing interface".into()))?;
            let ssid = args
                .get(3)
                .ok_or_else(|| Error::InvalidId("missing SSID".into()))?;
            let d = device(&client, iface)?;
            match client.activation_candidates(&d.id, ssid) {
                Ok(CandidateResolution::Ready(c)) => println!("CANDIDATE device={} ssid={} profile={} profile_name={:?} profile_uuid={} ap={} frequency={}", d.interface, ssid, c.profile.id, c.profile.name, c.profile.uuid, c.access_point.id, c.access_point.frequency),
                Ok(CandidateResolution::AmbiguousSavedConnections(ps)) => {
                    println!("AMBIGUOUS_PROFILE device={} ssid={}", d.interface, ssid);
                    for p in ps { println!("  {} name={:?} uuid={} interface_name={:?}", p.id, p.name, p.uuid, p.interface_name); }
                }
                Ok(CandidateResolution::UnsavedNetwork) => println!("UNSAVED_NETWORK ssid={ssid}"),
                Err(Error::AccessPointNotFound { .. }) => println!("ACCESS_POINT_NOT_FOUND device={} ssid={}", d.interface, ssid),
                Err(error) => return Err(error),
            }
            Ok(())
        }
        Some("scan") => {
            let d = device(
                &client,
                args.get(2)
                    .ok_or_else(|| Error::InvalidId("missing interface".into()))?,
            )?;
            let before = d.last_scan;
            let bound = client.request_scan(&d.id).await?;
            println!("SCAN_REQUEST_ACCEPTED device={}", d.interface);
            let started = Instant::now();
            println!("WAIT_STARTED timeout_ms=20000");
            let deadline = async_io::Timer::after(Duration::from_secs(20));
            let result = {
                let wait = client.wait_for_scan(&bound, before);
                pin_mut!(wait);
                match select(wait, deadline).await {
                    Either::Left((result, _)) => result,
                    Either::Right(_) => Err(Error::Dbus("SCAN_TIMEOUT".into())),
                }
            };
            match result {
                Ok(last_scan) => println!(
                    "SCAN_COMPLETED device={} last_scan={last_scan} WAIT_FINISHED elapsed_ms={}",
                    d.interface,
                    started.elapsed().as_millis()
                ),
                Err(error) => {
                    if matches!(error, Error::Dbus(ref message) if message == "SCAN_TIMEOUT") {
                        println!(
                            "SCAN_TIMEOUT device={} WAIT_TIMEOUT elapsed_ms={}",
                            d.interface,
                            started.elapsed().as_millis()
                        );
                    }
                    return Err(error);
                }
            }
            Ok(())
        }
        Some("connect") => {
            let iface = args
                .get(2)
                .ok_or_else(|| Error::InvalidId("missing interface".into()))?;
            let ssid = args
                .get(3)
                .ok_or_else(|| Error::InvalidId("missing SSID".into()))?;
            let d = device(&client, iface)?;
            let state = client.current_wifi_state(&d.id).expect("device exists");
            if state.device_state == DeviceState::Activated && state.ssid.as_deref() == Some(ssid) {
                println!(
                    "ALREADY_ACTIVE device={} ssid={} frequency={:?} active_connection={:?}",
                    d.interface, ssid, state.frequency, state.active_connection
                );
                return Ok(());
            }
            let mut bound = d.id.clone();
            let old_active_connection = state.active_connection.clone();
            let mut request =
                client
                    .saved_activation(&d.id, ssid)
                    .map_err(|error| match error {
                        Error::UnsavedNetwork { .. } => {
                            Error::Dbus(format!("UNSAVED_NETWORK ssid={ssid}"))
                        }
                        other => other,
                    })?;
            if request.specific_ap.is_none() {
                println!(
                    "PROFILE_FOUND profile={} profile_uuid={}",
                    request.profile.id, request.profile.uuid
                );
                println!(
                    "ACCESS_POINT_NOT_VISIBLE device={} ssid={}",
                    d.interface, ssid
                );
                let before = d.last_scan;
                bound = client.request_scan(&d.id).await?;
                if bound != d.id {
                    println!("DEVICE_REBOUND old={} new={}", d.id, bound);
                }
                println!("SCAN_REQUEST_ACCEPTED device={}", d.interface);
                let started = Instant::now();
                println!("WAIT_STARTED timeout_ms=20000");
                let deadline = async_io::Timer::after(Duration::from_secs(20));
                let scan = {
                    let wait = client.wait_for_scan(&bound, before);
                    pin_mut!(wait);
                    match select(wait, deadline).await {
                        Either::Left((result, _)) => result,
                        Either::Right(_) => Err(Error::Dbus("SCAN_TIMEOUT".into())),
                    }
                }?;
                println!(
                    "SCAN_COMPLETED device={} last_scan={scan} WAIT_FINISHED elapsed_ms={}",
                    d.interface,
                    started.elapsed().as_millis()
                );
                request = client.saved_activation(&bound, ssid)?;
                if request.specific_ap.is_none() {
                    println!(
                        "ACCESS_POINT_NOT_VISIBLE_AFTER_SCAN device={} ssid={ssid}",
                        d.interface
                    );
                }
            }
            bound = client.late_bind_device(&bound).await?;
            if bound != d.id {
                println!("DEVICE_REBOUND old={} new={}", d.id, bound);
                request = client.saved_activation(&bound, ssid)?;
            }
            match &request.specific_ap {
                Some(ap) => println!("ACTIVATION_MODE explicit-ap ap={ap}"),
                None => println!("ACTIVATION_MODE automatic-ap specific_object=AUTO"),
            }
            println!(
                "RESOLUTION device={} ssid={} profile={} profile_uuid={} specific_object={}",
                d.interface,
                ssid,
                request.profile.id,
                request.profile.uuid,
                request
                    .specific_ap
                    .as_ref()
                    .map_or("AUTO".into(), |ap| ap.to_string())
            );
            let active = client
                .activate_saved_wifi(&bound, &request.profile.id, request.specific_ap.clone())
                .await?;
            println!("ACTIVATION_REQUEST_ACCEPTED active_connection={active}");
            let deadline = async_io::Timer::after(Duration::from_secs(20));
            pin_mut!(deadline);
            let started = Instant::now();
            println!("WAIT_STARTED timeout_ms=20000");
            loop {
                let outcome = {
                    let event = client.next_event();
                    pin_mut!(event);
                    match select(event, &mut deadline).await {
                        Either::Right(_) => None,
                        Either::Left((event, _)) => {
                            let event: NetworkEvent = event?;
                            print_activation_event(&event);
                            Some(event)
                        }
                    }
                };
                let Some(event) = outcome else {
                    println!(
                        "ACTIVATION_TIMEOUT WAIT_TIMEOUT elapsed_ms={}",
                        started.elapsed().as_millis()
                    );
                    print_status(&client, Some(&d.interface))?;
                    return Ok(());
                };
                if matches!(&event, NetworkEvent::DeviceRemoved(id) if id == &bound) {
                    println!("ACTIVATION_FAILED device={} ssid={} reason=DEVICE_REMOVED active_connection={}", d.interface, ssid, active);
                    return Ok(());
                }
                if let Some(s) = client.current_wifi_state(&bound) {
                    match evaluate_activation(Some(&s), ssid) {
                        ActivationEvaluation::Succeeded => {
                            println!(
                                "ACTIVATION_SUCCEEDED device={} ssid={} frequency={:?}",
                                d.interface, ssid, s.frequency
                            );
                            return Ok(());
                        }
                        ActivationEvaluation::Failed(reason) => {
                            println!("ACTIVATION_FAILED device={} ssid={} requested_active_connection={} old_active_connection={:?} failure={reason:?}", d.interface, ssid, active, old_active_connection);
                            return Ok(());
                        }
                        ActivationEvaluation::Pending => {}
                    }
                }
            }
        }
        _ => {
            usage();
            Ok(())
        }
    }
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if let Err(error) = zbus::block_on(run(&args)) {
        eprintln!("xnm: {error}");
        std::process::exit(1);
    }
}

#[allow(dead_code)]
fn _state_name(state: DeviceState) -> &'static str {
    state_name(state)
}

fn state_name(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Unknown => "Unknown",
        DeviceState::Unmanaged => "Unmanaged",
        DeviceState::Disconnected => "Disconnected",
        DeviceState::Prepare => "Preparing",
        DeviceState::Config => "Config",
        DeviceState::NeedAuth => "NeedAuth",
        DeviceState::IpConfig => "IpConfig",
        DeviceState::IpCheck => "IpCheck",
        DeviceState::Secondaries => "Secondaries",
        DeviceState::Activated => "Activated",
        DeviceState::Deactivating => "Deactivating",
        DeviceState::Failed => "Failed",
    }
}

fn state_number(state: DeviceState) -> u32 {
    match state {
        DeviceState::Activated => 100,
        DeviceState::Deactivating => 110,
        DeviceState::Failed => 120,
        _ => 0,
    }
}

fn active_state_name(state: u32) -> &'static str {
    match state {
        0 => "Unknown",
        1 => "Activating",
        2 => "Activated",
        3 => "Deactivating",
        4 => "Deactivated",
        _ => "Unknown",
    }
}
