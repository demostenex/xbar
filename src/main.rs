mod audio;
mod clock;
mod core;
mod dbus;
mod i3;
mod notifications;
mod platform;
mod ui;
mod xnm;

use clock::ClockSource;
use core::{Event, MenuSource, State, StatusNotifierAction};
use i3::I3Client;
use platform::x11::{HitTarget, RenderTarget, X11Platform};
use std::error::Error;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

fn main() -> Result<(), Box<dyn Error>> {
    let mut x11 = X11Platform::connect()?;
    if !x11.acquire_instance()? {
        eprintln!("xbar: another instance already owns _XBAR_INSTANCE");
        return Ok(());
    }
    let socket = i3::socket_path(&x11)?;
    let mut i3 = I3Client::connect(socket)?;
    let clock = ClockSource::new()?;
    let mut audio = audio::AudioBridge::start()?;
    let mut state = State::default();
    let registry = Arc::new(Mutex::new(core::MenuRegistry::default()));
    let mut dbus = dbus::DbusBridge::start(Arc::clone(&registry))?;
    let mut xnm = match xnm::XnmBridge::start() {
        Ok(bridge) => Some(bridge),
        Err(error) => {
            eprintln!("XNM_BACKEND_FAILED error={error}");
            None
        }
    };
    let mut xnm_shadow = xnm::XnmShadowState::default();
    let trace = std::env::var_os("XBAR_TRACE").is_some();
    if trace {
        eprintln!("xbar trace: Xft Xlib connection fd={}", x11.text_raw_fd());
        eprintln!("xbar trace: Xft font={}", x11.text_font_name());
        eprintln!("xbar trace: Xft popup font={}", x11.popup_font_name());
        eprintln!(
            "xbar trace: Xft status icon font={}",
            x11.status_icon_font_name()
        );
        eprintln!("xbar trace: Xft metrics={:?}", x11.text_metrics());
    }
    let mut next_menu_request_id = 1_u64;
    let mut last_audio_command = None;

    i3.subscribe()?;
    i3.request_workspaces()?;
    i3.request_focused_window()?;
    let outputs = x11.outputs()?;
    core::reduce(
        &mut state,
        Event::OutputsChanged(outputs),
        &mut registry.lock().expect("registry poisoned"),
    );
    core::reduce(
        &mut state,
        Event::ClockUpdated(clock.sample()?),
        &mut registry.lock().expect("registry poisoned"),
    );
    if trace {
        eprintln!("xbar trace: initial outputs={:?}", state.outputs);
    }
    let initial_gmenu = x11.discover_gmenu_windows()?;
    if trace {
        eprintln!("xbar trace: initial GMenu windows={}", initial_gmenu.len());
    }
    for (window_id, endpoint) in initial_gmenu {
        core::reduce(
            &mut state,
            Event::GtkMenuDiscovered {
                window_id,
                endpoint,
            },
            &mut registry.lock().expect("registry poisoned"),
        );
    }
    for event in x11.discover_attention_windows()? {
        if let platform::x11::X11Event::WindowAttentionChanged {
            window,
            app_name,
            attention,
        } = event
        {
            dbus.window_attention(window, app_name, attention);
        }
    }
    x11.sync_windows(&state.outputs)?;
    x11.render(&state, RenderTarget::All)?;

    loop {
        let mut fds = [
            libc::pollfd {
                fd: x11.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: i3.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: dbus.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: clock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: audio.raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: dbus.notification_timer_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: xnm.as_ref().map_or(-1, xnm::XnmBridge::raw_fd),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }

        let mut events = Vec::new();
        if fds[0].revents & libc::POLLIN != 0 {
            while let Some(event) = x11.next_event()? {
                events.push(Event::X11(event));
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            events.extend(i3.read_events()?);
        }
        if fds[2].revents & libc::POLLIN != 0 {
            events.extend(dbus.drain_events()?);
        }
        if fds[3].revents & libc::POLLIN != 0 {
            events.push(Event::ClockUpdated(clock.on_readable()?));
        }
        if fds[4].revents & libc::POLLIN != 0 {
            events.extend(audio.drain_events()?);
        }
        if fds[5].revents & libc::POLLIN != 0 {
            dbus.notification_timer_fired();
        }
        if fds[6].revents & libc::POLLIN != 0 {
            if let Some(bridge) = xnm.as_mut() {
                for event in bridge.drain_events()? {
                    if trace {
                        eprintln!("xbar trace: xnm_shadow_event={event:?}");
                    }
                    let previous_status = xnm_shadow.presentation_status();
                    let previous_popup = xnm_shadow.popup_projection();
                    let wireless_action_finished = match &event {
                        xnm::XnmBridgeEvent::WirelessRequestNoop { enabled }
                        | xnm::XnmBridgeEvent::WirelessRequestFailed { enabled, .. } => {
                            Some(*enabled)
                        }
                        _ => None,
                    };
                    xnm::apply_shadow_event(&mut xnm_shadow, event);
                    if let Some(enabled) = wireless_action_finished {
                        events.push(Event::NetworkActionFinished(
                            core::NetworkPendingAction::SetWireless(enabled),
                        ));
                    }
                    let status = xnm_shadow.presentation_status();
                    if status != previous_status {
                        events.push(Event::NetworkStatusChanged(core::NetworkStatus {
                            available: status.available,
                            connected: status.connected,
                            interface: status.interface,
                            ssid: status.ssid,
                            frequency: status.frequency,
                            strength: status.strength,
                        }));
                    }
                    let popup = xnm_shadow.popup_projection();
                    if previous_popup.wireless_enabled != popup.wireless_enabled
                        || previous_popup.wifi_devices != popup.wifi_devices
                        || previous_popup.access_points != popup.access_points
                    {
                        events.push(Event::NetworkPopupProjectionChanged(popup));
                    }
                }
                if trace {
                    for device in &xnm_shadow.devices {
                        eprintln!(
                            "XNM_SHADOW device={} state={:?} ssid={:?} frequency={:?}",
                            device.state.interface,
                            device.state.device_state,
                            device.state.ssid,
                            device.state.frequency
                        );
                    }
                }
            }
        }

        if events
            .iter()
            .any(|event| matches!(event, Event::X11(platform::x11::X11Event::RandrChanged)))
        {
            events.push(Event::OutputsChanged(x11.outputs()?));
        }

        let mut dirty = false;
        let mut outputs_changed = false;
        let mut render_target: Option<RenderTarget> = None;
        let mut event_index = 0;
        while event_index < events.len() {
            let event = events[event_index].clone();
            event_index += 1;
            let event = match event {
                Event::X11(platform::x11::X11Event::WindowAttentionChanged {
                    window,
                    app_name,
                    attention,
                }) => {
                    dbus.window_attention(window, app_name.clone(), attention);
                    Event::WindowAttentionChanged {
                        window,
                        app_name,
                        attention,
                    }
                }
                Event::X11(platform::x11::X11Event::GtkWindowDestroyed(window_id)) => {
                    dbus.window_attention(window_id, String::new(), false);
                    registry
                        .lock()
                        .expect("registry poisoned")
                        .gtk(window_id)
                        .cloned()
                        .map(|endpoint| Event::GtkMenuRemoved {
                            window_id,
                            endpoint,
                        })
                        .unwrap_or(Event::X11(platform::x11::X11Event::GtkWindowDestroyed(
                            window_id,
                        )))
                }
                Event::X11(platform::x11::X11Event::GtkWindowChanged(window_id)) => {
                    match x11.discover_gmenu_window(window_id.0)? {
                        Some(endpoint) => Event::GtkMenuDiscovered {
                            window_id,
                            endpoint,
                        },
                        None => registry
                            .lock()
                            .expect("registry poisoned")
                            .gtk(window_id)
                            .cloned()
                            .map(|endpoint| Event::GtkMenuRemoved {
                                window_id,
                                endpoint,
                            })
                            .unwrap_or(Event::X11(platform::x11::X11Event::GtkWindowChanged(
                                window_id,
                            ))),
                    }
                }
                Event::X11(platform::x11::X11Event::GtkWindowsChanged) => {
                    let discovered = x11.discover_gmenu_windows()?;
                    let focused_endpoint = discovered
                        .iter()
                        .find(|(window_id, _)| Some(*window_id) == state.focused_window)
                        .map(|(_, endpoint)| endpoint.clone());
                    for (window_id, endpoint) in discovered {
                        dirty |= core::reduce(
                            &mut state,
                            Event::GtkMenuDiscovered {
                                window_id,
                                endpoint,
                            },
                            &mut registry.lock().expect("registry poisoned"),
                        );
                    }
                    focused_endpoint
                        .map(|endpoint| Event::GtkMenuDiscovered {
                            window_id: state.focused_window.expect("focused endpoint window"),
                            endpoint,
                        })
                        .unwrap_or(Event::X11(platform::x11::X11Event::GtkWindowsChanged))
                }
                event => event,
            };
            let previous_active_source =
                state.active_menu_endpoint(&registry.lock().expect("registry poisoned"));
            if trace {
                eprintln!("xbar trace: event={event:?}");
            }
            if matches!(event, Event::X11(platform::x11::X11Event::Close)) {
                return Ok(());
            }
            if matches!(event, Event::X11(platform::x11::X11Event::InstanceLost)) {
                eprintln!("xbar: instance ownership lost");
                return Ok(());
            }
            let mouse_target = match &event {
                Event::X11(platform::x11::X11Event::ButtonPress { .. })
                | Event::X11(platform::x11::X11Event::ButtonRelease { .. })
                | Event::X11(platform::x11::X11Event::MotionNotify { .. }) => {
                    Some(x11.hit_test(match &event {
                        Event::X11(e) => e,
                        _ => unreachable!(),
                    }))
                }
                _ => None,
            };
            let activation = match (&event, mouse_target.as_ref()) {
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { timestamp, .. }),
                    Some(platform::x11::HitTarget::Item(path)),
                ) => path.last().copied().and_then(|item_id| {
                    let registry_guard = registry.lock().expect("registry poisoned");
                    let (window_id, endpoint) = match state.current_menu_source(&registry_guard)? {
                        MenuSource::Tray(endpoint) => {
                            (core::WindowId(u32::MAX), MenuSource::Tray(endpoint))
                        }
                        endpoint => (state.focused_window?, endpoint),
                    };
                    let menu_item = state
                        .active_menu_model()
                        .and_then(|model| ui::layout::find_item(&model.root, item_id))?;
                    let actionable = {
                        let item = menu_item;
                        item.visible
                            && item.enabled
                            && !matches!(item.item_type, core::MenuItemType::Separator)
                            && item.children_display.is_none()
                    };
                    actionable.then_some((
                        window_id,
                        endpoint,
                        item_id,
                        *timestamp,
                        menu_item.action.clone(),
                    ))
                }),
                _ => None,
            };
            let translated = match (&event, mouse_target.as_ref()) {
                (Event::X11(platform::x11::X11Event::ButtonRelease { button: 1, .. }), _)
                    if state.audio_dragging =>
                {
                    Event::AudioDragReleased
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress {
                        button,
                        root_x: _,
                        root_y: _,
                        ..
                    }),
                    Some(platform::x11::HitTarget::Tray(endpoint)),
                ) => {
                    let menu = state
                        .status_notifier_items
                        .items()
                        .iter()
                        .find(|item| item.endpoint == *endpoint)
                        .and_then(|item| item.menu.clone());
                    if *button == 1 || *button == 3 {
                        if let Some(endpoint) = menu {
                            Event::TrayMenuOpenRequested { endpoint }
                        } else {
                            tray_action_event(&event, endpoint, &state)
                        }
                    } else {
                        tray_action_event(&event, endpoint, &state)
                    }
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::TopLevel(id)),
                ) => Event::MenuRootClicked(*id),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Audio),
                ) => Event::AudioPopupToggled,
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Bluetooth),
                ) => Event::BluetoothPopupToggled,
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Network),
                ) => {
                    if state.network_popup_open {
                        Event::NetworkPopupToggled
                    } else {
                        Event::NetworkPopupOpenRequested
                    }
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NetworkWifi(target)),
                ) => Event::NetworkConnectSavedWifi(target.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NetworkWireless),
                ) => Event::NetworkSetWireless(!state.network.wireless_enabled),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::BluetoothPower),
                ) => Event::BluetoothSetPowered(!state.bluetooth.powered),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::BluetoothDevice(path)),
                ) => state
                    .bluetooth
                    .devices
                    .iter()
                    .find(|d| d.path == *path)
                    .map_or_else(
                        || event.clone(),
                        |d| {
                            if std::env::var_os("XBAR_TRACE").is_some() {
                                eprintln!(
                                    "xbar trace: bluetooth row ButtonPress path={} connected={}",
                                    path, d.connected
                                );
                            }
                            if d.connected {
                                Event::BluetoothDisconnectDevice(path.clone())
                            } else {
                                Event::BluetoothConnectDevice(path.clone())
                            }
                        },
                    ),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioTrack),
                ) => x11
                    .audio_track_percent(match &event {
                        Event::X11(e) => e,
                        _ => unreachable!(),
                    })
                    .map(|percent| Event::AudioTrackChanged {
                        input: false,
                        percent,
                    })
                    .unwrap_or_else(|| event.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioInputTrack),
                ) => x11
                    .audio_input_track_percent(match &event {
                        Event::X11(e) => e,
                        _ => unreachable!(),
                    })
                    .map(|percent| Event::AudioTrackChanged {
                        input: true,
                        percent,
                    })
                    .unwrap_or_else(|| event.clone()),
                (Event::X11(platform::x11::X11Event::MotionNotify { .. }), _)
                    if state.audio_dragging =>
                {
                    let percent = if state.audio_drag_input {
                        x11.audio_input_track_percent(match &event {
                            Event::X11(e) => e,
                            _ => unreachable!(),
                        })
                    } else {
                        x11.audio_track_percent(match &event {
                            Event::X11(e) => e,
                            _ => unreachable!(),
                        })
                    };
                    percent
                        .map(|percent| Event::AudioTrackChanged {
                            input: state.audio_drag_input,
                            percent,
                        })
                        .unwrap_or_else(|| event.clone())
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioMute),
                ) => Event::AudioMuteToggled { input: false },
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioInputMute),
                ) => Event::AudioMuteToggled { input: true },
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioOutputDevice(name)),
                ) => Event::AudioSelectOutput(name.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioInputDevice(name)),
                ) => Event::AudioSelectInput(name.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonRelease { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioTrack),
                ) => Event::AudioDragReleased,
                (
                    Event::X11(platform::x11::X11Event::ButtonRelease { button: 1, .. }),
                    Some(platform::x11::HitTarget::AudioInputTrack),
                ) => Event::AudioDragReleased,
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::Item(_)),
                ) => {
                    if let Some((window_id, endpoint, item_id, timestamp, _)) = &activation {
                        Event::MenuItemActivateRequested {
                            window_id: *window_id,
                            endpoint: endpoint.clone(),
                            item_id: *item_id,
                            timestamp: *timestamp,
                        }
                    } else {
                        event.clone()
                    }
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::Outside),
                ) => Event::MenuClickedOutside,
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::AudioInside),
                ) => event.clone(),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::BluetoothInside),
                ) => event.clone(),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::NetworkInside),
                )
                | (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::Network),
                ) => event.clone(),
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::Item(path)),
                ) => Event::MenuItemHovered { path: path.clone() },
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::TopLevel(id)),
                ) => Event::MenuItemHovered { path: vec![*id] },
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::Outside),
                ) => Event::MenuItemHovered { path: vec![] },
                _ => event.clone(),
            };
            let hovered_before = if matches!(
                &event,
                Event::X11(platform::x11::X11Event::MotionNotify { .. })
            ) {
                Some(state.menu_interaction.hovered_path.clone())
            } else {
                None
            };
            if trace {
                match (&event, mouse_target.as_ref()) {
                    (
                        Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                        Some(platform::x11::HitTarget::TopLevel(id)),
                    ) => eprintln!("xbar trace: top-level hit item={}", id.0),
                    (
                        Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                        Some(platform::x11::HitTarget::Tray(endpoint)),
                    ) => eprintln!("xbar trace: tray hit endpoint={endpoint:?}"),
                    (
                        Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                        Some(platform::x11::HitTarget::Outside),
                    ) => eprintln!("xbar trace: click outside"),
                    (
                        Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                        Some(platform::x11::HitTarget::Item(path)),
                    ) => eprintln!("xbar trace: pointer hit item path={path:?}"),
                    _ => {}
                }
            }
            if let (true, Some((_, _, item_id, timestamp, _))) = (trace, activation.as_ref()) {
                eprintln!(
                    "xbar trace: menu item activation item={} timestamp={}",
                    item_id.0, timestamp
                );
            }
            outputs_changed |= matches!(&event, Event::OutputsChanged(_));
            let stale_layout = matches!(
                &event,
                Event::MenuLayoutInvalidated {
                    endpoint,
                    revision: Some(revision),
                } if state.active_menu_endpoint(&registry.lock().expect("registry poisoned")) == Some(endpoint.clone())
                    && state.active_menu_model().is_some_and(|model| model.revision >= *revision)
            );
            let request_menu = !stale_layout
                && (matches!(
                    &translated,
                    Event::WindowFocused(_)
                        | Event::WindowFocusedWithApp { .. }
                        | Event::MenuRegistered { .. }
                        | Event::GtkMenuDiscovered { .. }
                        | Event::GtkMenuRemoved { .. }
                        | Event::MenuUnregistered { .. }
                        | Event::MenuOwnerVanished { .. }
                ) || matches!(
                    &translated,
                    Event::MenuLayoutInvalidated { endpoint, .. }
                        if state.active_menu_endpoint(&registry.lock().expect("registry poisoned")) == Some(endpoint.clone())
                ) || (matches!(&translated, Event::MenuRootClicked(_))
                    && matches!(state.menu, core::MenuState::TrayLoaded { .. })));
            if trace {
                match &translated {
                    Event::WindowFocused(new_window) => eprintln!(
                        "xbar trace: focus transition old_window={:?} new_window={:?} old_workspace={:?} new_workspace={:?} pointer_grabbed={} popup_count={} open_root={:?}",
                        state.focused_window,
                        new_window,
                        state.focused_workspace,
                        state.focused_workspace,
                        x11.pointer_grabbed(),
                        x11.popup_count(),
                        state.menu_interaction.open_root
                    ),
                    Event::WindowFocusedWithApp { window: new_window, .. } => eprintln!(
                        "xbar trace: focus transition old_window={:?} new_window={:?} old_workspace={:?} new_workspace={:?} pointer_grabbed={} popup_count={} open_root={:?}",
                        state.focused_window,
                        new_window,
                        state.focused_workspace,
                        state.focused_workspace,
                        x11.pointer_grabbed(),
                        x11.popup_count(),
                        state.menu_interaction.open_root
                    ),
                    Event::WorkspaceFocused { name: new_workspace } => eprintln!(
                        "xbar trace: focus transition old_window={:?} new_window={:?} old_workspace={:?} new_workspace={:?} pointer_grabbed={} popup_count={} open_root={:?}",
                        state.focused_window,
                        state.focused_window,
                        state.focused_workspace,
                        new_workspace,
                        x11.pointer_grabbed(),
                        x11.popup_count(),
                        state.menu_interaction.open_root
                    ),
                    _ => {}
                }
            }
            let previous_audio_glyph = (state.audio.available, ui::view::audio_glyph(&state.audio));
            let mut event_render_target = render_target_for(&translated, &mouse_target, &x11);
            let tray_menu_open = match &translated {
                Event::TrayMenuOpenRequested { endpoint } => Some(endpoint.clone()),
                _ => None,
            };
            let sni_action = match &translated {
                Event::StatusNotifierActionRequested {
                    endpoint,
                    action,
                    root_x,
                    root_y,
                } => Some((endpoint.clone(), *action, *root_x, *root_y)),
                _ => None,
            };
            let reduced = core::reduce(
                &mut state,
                translated.clone(),
                &mut registry.lock().expect("registry poisoned"),
            );
            if trace && matches!(translated, Event::NetworkPopupSnapshotReceived(_)) && reduced {
                let active = state
                    .network
                    .wifi_devices
                    .iter()
                    .filter(|device| device.active_connection.is_some())
                    .map(|device| {
                        let active = device
                            .access_points
                            .iter()
                            .find(|access_point| access_point.is_active)
                            .map(|access_point| {
                                format!(
                                    "{} ({})",
                                    access_point.ssid,
                                    crate::core::wifi_band(access_point.frequency)
                                )
                            })
                            .unwrap_or_else(|| "-".to_owned());
                        format!("{}={active}", device.interface)
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                eprintln!("xbar trace: NETWORK_POPUP_OPEN active_device={active}");
            }
            if matches!(translated, Event::AudioSnapshotReceived(_)) && reduced {
                let current_audio_glyph =
                    (state.audio.available, ui::view::audio_glyph(&state.audio));
                if previous_audio_glyph == current_audio_glyph {
                    event_render_target = None;
                }
            }
            dirty |= reduced;
            match translated {
                Event::AudioTrackChanged { input, percent }
                    if last_audio_command != Some((input, percent)) =>
                {
                    if input {
                        audio.set_input_volume(percent);
                    } else {
                        audio.set_volume(percent);
                    }
                    last_audio_command = Some((input, percent));
                }
                Event::AudioTrackChanged { .. } => {}
                Event::AudioMuteToggled { input } => {
                    if input {
                        audio.toggle_input_mute()
                    } else {
                        audio.toggle_mute()
                    }
                }
                Event::AudioSelectOutput(ref name) => audio.set_default_output(name),
                Event::AudioSelectInput(ref name) => audio.set_default_input(name),
                Event::AudioDragReleased => last_audio_command = None,
                Event::BluetoothSetPowered(powered) => {
                    if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: BluetoothCommand SetPowered powered={powered}");
                    }
                    dbus.bluetooth_set_powered(powered)
                }
                Event::BluetoothConnectDevice(ref path) => {
                    if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: BluetoothCommand ConnectDevice path={path}");
                    }
                    dbus.bluetooth_connect_device(path.clone())
                }
                Event::BluetoothDisconnectDevice(ref path) => {
                    if std::env::var_os("XBAR_TRACE").is_some() {
                        eprintln!("xbar trace: BluetoothCommand DisconnectDevice path={path}");
                    }
                    dbus.bluetooth_disconnect_device(path.clone())
                }
                Event::NetworkSetWireless(enabled) => {
                    if trace {
                        eprintln!("xbar trace: NetworkCommand SetWireless enabled={enabled}");
                    }
                    if let Some(bridge) = xnm.as_ref() {
                        if !bridge.set_wireless_enabled(enabled) {
                            events.push(Event::NetworkActionFinished(
                                core::NetworkPendingAction::SetWireless(enabled),
                            ));
                            if trace {
                                eprintln!(
                                    "xbar trace: NETWORK_WIRELESS_ENABLE_REQUEST_REJECTED reason=xnm-unavailable"
                                );
                            }
                        }
                    } else {
                        events.push(Event::NetworkActionFinished(
                            core::NetworkPendingAction::SetWireless(enabled),
                        ));
                        if trace {
                            eprintln!(
                                "xbar trace: NETWORK_WIRELESS_ENABLE_REQUEST_REJECTED reason=xnm-unavailable"
                            );
                        }
                    }
                }
                Event::NetworkConnectSavedWifi(ref target) if reduced => {
                    if trace {
                        eprintln!(
                            "xbar trace: ConnectSavedWifi interface={} ssid={} band={} saved={} active={}",
                            target.interface, target.ssid, target.band, target.saved, target.active
                        );
                    }
                    if let Some(bridge) = xnm.as_ref() {
                        bridge.connect_saved_wifi(target.clone());
                    } else if trace {
                        eprintln!("xbar trace: NETWORK_ACTION_REJECTED reason=xnm-unavailable");
                    }
                }
                _ => {}
            }
            if matches!(translated, Event::NetworkPopupOpenRequested) && reduced {
                if trace {
                    eprintln!("xbar trace: NETWORK_POPUP_OPEN_REQUEST");
                }
                if let Some(bridge) = xnm.as_ref() {
                    for interface in xnm_shadow.interfaces() {
                        bridge.request_scan(interface);
                    }
                }
            }
            if matches!(
                translated,
                Event::AudioSnapshotReceived(_)
                    | Event::AudioInventoryReceived { .. }
                    | Event::AudioUnavailable
            ) && state.audio_popup_open
            {
                render_target = Some(match render_target {
                    Some(current) => current.merge(RenderTarget::Popup),
                    None => RenderTarget::Popup,
                });
            }
            if matches!(
                translated,
                Event::BluetoothSnapshotReceived(_) | Event::BluetoothUnavailable
            ) && state.bluetooth_popup_open
            {
                render_target = Some(match render_target {
                    Some(current) => current.merge(RenderTarget::Popup),
                    None => RenderTarget::Popup,
                });
            }
            if matches!(
                translated,
                Event::NetworkStatusChanged(_)
                    | Event::NetworkSnapshotReceived(_)
                    | Event::NetworkPopupProjectionChanged(_)
                    | Event::NetworkPopupSnapshotReceived(_)
                    | Event::NetworkActionFinished(_)
            ) && state.network_popup_open
            {
                render_target = Some(match render_target {
                    Some(current) => current.merge(RenderTarget::Popup),
                    None => RenderTarget::Popup,
                });
            }
            if let Some((endpoint, action, root_x, root_y)) = sni_action {
                if trace {
                    eprintln!("xbar trace: SNI action={action:?} endpoint={endpoint:?} root=({root_x},{root_y})");
                }
                dbus.request_status_notifier_action(endpoint, action, root_x, root_y);
            }
            if let Some(endpoint) = tray_menu_open {
                let request_id = next_menu_request_id;
                next_menu_request_id += 1;
                let source = MenuSource::Tray(endpoint.clone());
                let request = Event::MenuLoadRequested {
                    window_id: core::WindowId(u32::MAX),
                    endpoint: source.clone(),
                    request_id,
                };
                if core::reduce(
                    &mut state,
                    request,
                    &mut registry.lock().expect("registry poisoned"),
                ) {
                    dirty = true;
                    render_target = Some(RenderTarget::Popup);
                }
                dbus.request_layout(core::WindowId(u32::MAX), endpoint, request_id);
            }
            if reduced {
                if let Some(target) = event_render_target {
                    render_target = Some(match render_target {
                        Some(current) => current.merge(target),
                        None => target,
                    });
                }
            }
            let current_active_source =
                state.active_menu_endpoint(&registry.lock().expect("registry poisoned"));
            if previous_active_source != current_active_source {
                if let Some(MenuSource::GtkGMenu(endpoint)) = previous_active_source {
                    dbus.end_gtk_menu(endpoint);
                }
            }
            if reduced {
                if let Some((window_id, endpoint, item_id, timestamp, action)) = activation {
                    if trace {
                        eprintln!(
                            "xbar trace: activation command queued item={} timestamp={}",
                            item_id.0, timestamp
                        );
                    }
                    match endpoint {
                        MenuSource::DbusMenu(endpoint) => {
                            dbus.request_activation(window_id, endpoint, item_id, timestamp);
                        }
                        MenuSource::GtkGMenu(endpoint) => {
                            if let Some(action) = action {
                                dbus.request_gtk_activation(
                                    window_id,
                                    endpoint,
                                    action.name,
                                    action.target,
                                );
                            }
                        }
                        MenuSource::Tray(endpoint) => {
                            dbus.request_activation(window_id, endpoint, item_id, timestamp);
                        }
                    }
                }
            }
            if trace && hovered_before.is_some() {
                eprintln!(
                    "xbar trace: hover transition old={:?} new={:?}",
                    hovered_before.as_deref().unwrap_or_default(),
                    state.menu_interaction.hovered_path
                );
            }
            if let Some(platform::x11::HitTarget::Item(path)) = mouse_target {
                if let Some(item_id) = path.last().copied() {
                    let candidate = state
                        .active_menu_model()
                        .and_then(|model| ui::layout::find_item(&model.root, item_id));
                    if trace {
                        eprintln!(
                            "xbar trace: submenu candidate item={} found={} enabled={} visible={} children_display={} children={}",
                            item_id.0,
                            candidate.is_some(),
                            candidate.is_some_and(|item| item.enabled),
                            candidate.is_some_and(|item| item.visible),
                            candidate.is_some_and(|item| item.children_display.is_some()),
                            candidate.map_or(0, |item| item.children.len())
                        );
                    }
                    let should_request =
                        matches!(
                            &event,
                            Event::X11(platform::x11::X11Event::MotionNotify { .. })
                        ) && state.menu_interaction.pending_about_to_show.is_none()
                            && state.menu_interaction.about_to_show_item != Some(item_id)
                            && !state.menu_interaction.open_path.contains(&item_id)
                            && candidate.is_some_and(|item| {
                                item.enabled && item.visible && item.children_display.is_some()
                            });
                    if trace {
                        eprintln!(
                            "xbar trace: about-to-show decision item={} request={}",
                            item_id.0, should_request
                        );
                    }
                    if should_request {
                        let active_endpoint =
                            state.current_menu_source(&registry.lock().expect("registry poisoned"));
                        let focused_window = active_endpoint.as_ref().and_then(|source| {
                            if matches!(source, MenuSource::Tray(_)) {
                                Some(core::WindowId(u32::MAX))
                            } else {
                                state.focused_window
                            }
                        });
                        if trace {
                            let registry_endpoint = focused_window.and_then(|window_id| {
                                registry
                                    .lock()
                                    .expect("registry poisoned")
                                    .get(window_id)
                                    .cloned()
                            });
                            eprintln!(
                                "xbar trace: about-to-show gate focused_window={:?} active_endpoint={:?} registry_lookup={:?} menu_state={:?} open_root={:?} hovered_path={:?} pending_about_to_show={:?}",
                                focused_window,
                                active_endpoint,
                                registry_endpoint,
                                state.menu,
                                state.menu_interaction.open_root,
                                state.menu_interaction.hovered_path,
                                state.menu_interaction.pending_about_to_show
                            );
                        }
                        if let (Some(window_id), Some(endpoint)) = (focused_window, active_endpoint)
                        {
                            let request_id = next_menu_request_id;
                            next_menu_request_id += 1;
                            let about = Event::MenuAboutToShowRequested {
                                window_id,
                                endpoint: endpoint.clone(),
                                item_id,
                                request_id,
                            };
                            if core::reduce(
                                &mut state,
                                about,
                                &mut registry.lock().expect("registry poisoned"),
                            ) {
                                if trace {
                                    eprintln!(
                                        "xbar trace: about-to-show request created request_id={} item={}",
                                        request_id, item_id.0
                                    );
                                }
                                if trace {
                                    eprintln!(
                                        "xbar trace: AboutToShow requested item={} request_id={}",
                                        item_id.0, request_id
                                    );
                                }
                                if let MenuSource::DbusMenu(endpoint) | MenuSource::Tray(endpoint) =
                                    endpoint
                                {
                                    dbus.request_about_to_show(
                                        window_id, endpoint, item_id, request_id,
                                    );
                                }
                                if trace {
                                    eprintln!(
                                        "xbar trace: about-to-show command queued request_id={}",
                                        request_id
                                    );
                                }
                                dirty = true;
                                render_target = Some(match render_target {
                                    Some(current) => current.merge(RenderTarget::Popup),
                                    None => RenderTarget::Popup,
                                });
                            }
                        }
                    }
                }
            }
            if request_menu {
                let mut registry_guard = registry.lock().expect("registry poisoned");
                if let (Some(window_id), Some(endpoint)) = (
                    state.focused_window,
                    state.active_menu_endpoint(&registry_guard),
                ) {
                    let request_id = next_menu_request_id;
                    next_menu_request_id += 1;
                    let request_event = Event::MenuLoadRequested {
                        window_id,
                        endpoint: endpoint.clone(),
                        request_id,
                    };
                    let request_dirty =
                        core::reduce(&mut state, request_event, &mut registry_guard);
                    dirty |= request_dirty;
                    if request_dirty {
                        render_target = Some(match render_target {
                            Some(current) => current.merge(RenderTarget::All),
                            None => RenderTarget::All,
                        });
                    }
                    match endpoint {
                        MenuSource::DbusMenu(endpoint) => {
                            dbus.request_layout(window_id, endpoint, request_id)
                        }
                        MenuSource::GtkGMenu(endpoint) => {
                            dbus.request_gtk_layout(window_id, endpoint, request_id)
                        }
                        MenuSource::Tray(endpoint) => {
                            dbus.request_layout(window_id, endpoint, request_id)
                        }
                    }
                }
            }
            if trace {
                let registry_guard = registry.lock().expect("registry poisoned");
                let active_menu = state.active_menu_endpoint(&registry_guard);
                eprintln!(
                    "xbar trace: focused_workspace={:?} focused_window={:?} active_menu_endpoint={active_menu:?} menu_state={:?} dirty={dirty}",
                    state.focused_workspace, state.focused_window, state.menu
                );
            }
        }
        if dirty {
            if outputs_changed {
                x11.sync_windows(&state.outputs)?;
            }
            x11.render(&state, render_target.unwrap_or(RenderTarget::All))?;
        }
    }
}

fn render_target_for(
    event: &Event,
    mouse_target: &Option<HitTarget>,
    x11: &X11Platform,
) -> Option<RenderTarget> {
    match event {
        Event::X11(platform::x11::X11Event::Expose(window)) => {
            if x11.is_dock_window(*window) {
                Some(RenderTarget::Dock)
            } else if x11.is_popup_window(*window) {
                Some(RenderTarget::Popup)
            } else {
                None
            }
        }
        Event::WindowFocusedWithApp { .. } => Some(RenderTarget::DockContext),
        Event::MenuLoaded { .. } | Event::MenuLoadFailed { .. } => Some(RenderTarget::DockContext),
        Event::X11(platform::x11::X11Event::MotionNotify { .. })
        | Event::MenuItemHovered { .. } => match mouse_target {
            Some(HitTarget::Item(_)) => Some(RenderTarget::Popup),
            Some(HitTarget::TopLevel(_)) => Some(RenderTarget::Dock),
            Some(HitTarget::Outside) | None => None,
            Some(HitTarget::Tray(_)) => None,
            Some(HitTarget::AudioTrack) | Some(HitTarget::AudioInputTrack) => {
                Some(RenderTarget::Popup)
            }
            Some(HitTarget::Audio)
            | Some(HitTarget::AudioMute)
            | Some(HitTarget::AudioInputMute)
            | Some(HitTarget::AudioInside) => None,
            Some(HitTarget::AudioOutputDevice(_)) | Some(HitTarget::AudioInputDevice(_)) => {
                Some(RenderTarget::Popup)
            }
            Some(HitTarget::BluetoothDevice(_)) => Some(RenderTarget::Popup),
            Some(HitTarget::BluetoothPower)
            | Some(HitTarget::BluetoothInside)
            | Some(HitTarget::Bluetooth) => None,
            Some(HitTarget::NetworkWireless)
            | Some(HitTarget::NetworkInside)
            | Some(HitTarget::NetworkWifi(_))
            | Some(HitTarget::Network) => None,
        },
        Event::MenuClickedOutside => Some(RenderTarget::Popup),
        Event::MenuAboutToShowRequested { .. } => Some(RenderTarget::Popup),
        Event::MenuAboutToShowCompleted { need_update, .. } => Some(if *need_update {
            RenderTarget::DockContext
        } else {
            RenderTarget::Popup
        }),
        Event::ClockUpdated(_) => Some(RenderTarget::Dock),
        Event::AudioSnapshotReceived(_)
        | Event::AudioUnavailable
        | Event::NetworkStatusChanged(_)
        | Event::NetworkSnapshotReceived(_)
        | Event::BluetoothSnapshotReceived(_)
        | Event::BluetoothUnavailable => Some(RenderTarget::DockRight),
        Event::AudioInventoryReceived { .. } => Some(RenderTarget::Popup),
        Event::AudioSelectOutput(_) | Event::AudioSelectInput(_) => Some(RenderTarget::Popup),
        Event::AudioPopupToggled => Some(RenderTarget::Popup),
        Event::AudioTrackChanged { .. }
        | Event::AudioDragReleased
        | Event::AudioMuteToggled { .. } => Some(RenderTarget::Popup),
        Event::BluetoothPopupToggled
        | Event::BluetoothSetPowered(_)
        | Event::BluetoothConnectDevice(_)
        | Event::BluetoothDisconnectDevice(_)
        | Event::BluetoothActionFinished(_) => Some(RenderTarget::Popup),
        Event::NetworkPopupToggled
        | Event::NetworkPopupOpenRequested
        | Event::NetworkPopupSnapshotReceived(_)
        | Event::NetworkPopupSnapshotFailed
        | Event::NetworkSetWireless(_)
        | Event::NetworkActionFinished(_) => Some(RenderTarget::Popup),
        Event::NotificationsSnapshot(_) => Some(RenderTarget::Notification),
        Event::WindowAttentionChanged { .. } => None,
        Event::StatusNotifierActionRequested { .. } => None,
        _ => Some(RenderTarget::All),
    }
}

fn tray_action_event(
    event: &Event,
    endpoint: &core::StatusNotifierEndpoint,
    state: &State,
) -> Event {
    let Event::X11(platform::x11::X11Event::ButtonPress {
        button,
        root_x,
        root_y,
        ..
    }) = event
    else {
        return event.clone();
    };
    let item_is_menu = state
        .status_notifier_items
        .items()
        .iter()
        .find(|item| item.endpoint == *endpoint)
        .is_some_and(|item| item.item_is_menu);
    StatusNotifierAction::for_button(*button, item_is_menu)
        .map(|action| Event::StatusNotifierActionRequested {
            endpoint: endpoint.clone(),
            action,
            root_x: *root_x,
            root_y: *root_y,
        })
        .unwrap_or_else(|| event.clone())
}
