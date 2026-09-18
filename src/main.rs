mod audio;
mod clock;
mod config;
mod core;
mod dbus;
mod external;
mod i3;
mod logging;
mod notification_icons;
mod notification_persistence;
mod notification_sound;
mod notifications;
mod platform;
mod ui;
mod xnm;

use clock::ClockSource;
use core::{Event, MenuLayoutReloadTracker, MenuSource, State, StatusNotifierAction};
use i3::I3Client;
use platform::x11::{HitTarget, RenderTarget, X11Platform};
use std::collections::HashMap;
use std::error::Error;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

fn should_schedule_invalidation(
    stale_layout: bool,
    accepted_layout_invalidation: bool,
    layout_in_flight: bool,
    lazy_about_to_show_pending: bool,
) -> bool {
    !stale_layout
        && accepted_layout_invalidation
        && !layout_in_flight
        && !lazy_about_to_show_pending
}

fn same_lazy_root_request(
    left: &core::LazyRootOpenPending,
    right: &core::LazyRootOpenPending,
) -> bool {
    left.window_id == right.window_id
        && left.endpoint == right.endpoint
        && left.item_id == right.item_id
        && left.intent_id == right.intent_id
        && left.watcher_generation == right.watcher_generation
}

fn pending_lazy_root_to_schedule<'a>(
    before: Option<&core::LazyRootOpenPending>,
    after: Option<&'a core::LazyRootOpenPending>,
) -> Option<&'a core::LazyRootOpenPending> {
    let after = after?;
    if before.is_some_and(|before| same_lazy_root_request(before, after)) {
        None
    } else {
        Some(after)
    }
}

fn main() {
    logging::init();
    logging::install_panic_hook();
    logging::info("START", &format!("pid={}", std::process::id()));
    match run() {
        Ok(()) => logging::info("EXIT", "normal"),
        Err(error) => {
            logging::error("ERROR", &format!("{error:?}"));
            eprintln!("Error: {error:?}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let config = match config::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("xbar: failed to load config: {error}");
            config::Config::default()
        }
    };
    let mut x11 = X11Platform::connect()?;
    if !x11.acquire_instance()? {
        eprintln!("xbar: another instance already owns _XBAR_INSTANCE");
        return Ok(());
    }
    let socket = i3::socket_path(&x11)?;
    let mut i3 = I3Client::connect(socket)?;
    let clock = ClockSource::new()?;
    let mut audio = audio::AudioBridge::start()?;
    let mut state = State {
        bluetooth_manager_command: config.bluetooth.manager_command(),
        external_floating_terminal: config.external.floating_terminal(),
        ..State::default()
    };
    let mut notification_icon_resolver = notification_icons::NotificationIconResolver::new();
    let external_launcher =
        external::ExternalLauncher::new(state.external_floating_terminal.clone());
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
    let mut menu_layout_reloads = MenuLayoutReloadTracker::default();
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

        prepare_passive_batch(&mut events, &state, &x11);

        if events
            .iter()
            .any(|event| matches!(event, Event::X11(platform::x11::X11Event::RandrChanged)))
        {
            events.push(Event::OutputsChanged(x11.outputs()?));
        }

        let mut dirty = false;
        let mut outputs_changed = false;
        let mut render_target: Option<RenderTarget> = None;
        let mut render_cause: Option<&'static str> = None;
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
            if let Event::EnsureNotificationCenterOpen { target, .. } = &event {
                x11.request_notification_center_target(*target);
            }
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
            let notification_scrolled = match &event {
                Event::X11(platform::x11::X11Event::ButtonPress { window, button, .. })
                    if (*button == 4 || *button == 5)
                        && x11.is_notification_center_window(*window) =>
                {
                    x11.scroll_notification_center(*button, &state)
                }
                _ => false,
            };
            if let (
                Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                Some(target),
            ) = (&event, mouse_target.as_ref())
            {
                if matches!(
                    target,
                    platform::x11::HitTarget::NotificationCenterCard(_)
                        | platform::x11::HitTarget::NotificationCenterDismiss(_)
                        | platform::x11::HitTarget::NotificationCenterAction(_, _)
                        | platform::x11::HitTarget::NotificationCenterActionPagePrev(_)
                        | platform::x11::HitTarget::NotificationCenterActionPageNext(_)
                        | platform::x11::HitTarget::NotificationCenterClearAll
                        | platform::x11::HitTarget::NotificationCenterGroupBody(_)
                        | platform::x11::HitTarget::NotificationCenterGroupHeader(_)
                ) {
                    x11.clear_notification_center_highlight();
                }
                let notification_action_page_changed =
                    match platform::x11::notification_center_button_action(1, target) {
                        Some(platform::x11::NotificationCenterButtonAction::Dismiss(id)) => {
                            if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                                eprintln!("notification-center dismiss-request: history_id={} enqueue=attempt", id.0);
                            }
                            dbus.dismiss_notification_history_entry(id);
                            false
                        }
                        Some(platform::x11::NotificationCenterButtonAction::ClearAll) => {
                            if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                                eprintln!("notification-center clear-request: enqueue=attempt");
                            }
                            dbus.clear_notification_history();
                            false
                        }
                        Some(platform::x11::NotificationCenterButtonAction::InvokeDefault(id)) => {
                            if std::env::var_os("XBAR_TRACE_NOTIFICATION_UI").is_some() {
                                eprintln!(
                                "notification-center default-action-request: history_id={} enqueue=attempt",
                                id.0
                            );
                            }
                            dbus.invoke_notification_default(id);
                            false
                        }
                        Some(platform::x11::NotificationCenterButtonAction::InvokeAction(
                            id,
                            key,
                        )) => {
                            dbus.invoke_notification_action(id, key);
                            false
                        }
                        Some(platform::x11::NotificationCenterButtonAction::ActionPagePrev(id)) => {
                            x11.previous_notification_action_page(id)
                        }
                        Some(platform::x11::NotificationCenterButtonAction::ActionPageNext(id)) => {
                            x11.next_notification_action_page(id)
                        }
                        Some(platform::x11::NotificationCenterButtonAction::ExpandGroup(_))
                        | Some(platform::x11::NotificationCenterButtonAction::CollapseGroup(_)) => {
                            false
                        }
                        None => false,
                    };
                if notification_action_page_changed {
                    dirty = true;
                    render_target = merge_render_target(
                        render_target,
                        notification_action_page_render_target(true),
                    );
                }
            }
            let popup_hover_changed = matches!(
                event,
                Event::X11(platform::x11::X11Event::MotionNotify { .. })
            ) && x11.update_popup_hover(mouse_target.as_ref());
            let notification_hover_changed = matches!(
                event,
                Event::X11(platform::x11::X11Event::MotionNotify { .. })
            ) && x11
                .update_notification_center_hover(mouse_target.as_ref());
            let popup_exposed = if let Event::X11(platform::x11::X11Event::Expose(window)) = &event
            {
                x11.note_menu_popup_exposed(*window)
            } else {
                false
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
                        endpoint => (state.menu_presentation_window()?, endpoint),
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
                (Event::X11(x11_event @ platform::x11::X11Event::KeyPress { .. }), _) => x11
                    .global_pin_shortcut_event(x11_event)
                    .or_else(|| x11.global_navigation_shortcut_event(x11_event))
                    .unwrap_or(keyboard_event(
                        &event,
                        &state,
                        &registry.lock().expect("registry poisoned"),
                        &x11,
                    )?),
                (Event::X11(x11_event @ platform::x11::X11Event::KeyRelease { .. }), _) => {
                    let _ = x11.global_pin_shortcut_event(x11_event);
                    let _ = x11.global_navigation_shortcut_event(x11_event);
                    event.clone()
                }
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
                    Some(platform::x11::HitTarget::Tray(endpoint, output)),
                ) => {
                    let menu = state
                        .status_notifier_items
                        .items()
                        .iter()
                        .find(|item| item.endpoint == *endpoint)
                        .and_then(|item| item.menu.clone());
                    if *button == 1 || *button == 3 {
                        if let Some(endpoint) = menu {
                            Event::TrayMenuOpenRequestedAt {
                                endpoint,
                                output: *output,
                            }
                        } else {
                            tray_action_event(&event, endpoint, &state)
                        }
                    } else {
                        tray_action_event(&event, endpoint, &state)
                    }
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::TopLevel(id, output)),
                ) => Event::MenuRootClickedAt {
                    id: *id,
                    output: *output,
                },
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::AiUsage(plugin, output)),
                ) => Event::AiUsagePopupToggled {
                    plugin: plugin.clone(),
                    output: *output,
                },
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                    Some(platform::x11::HitTarget::AiUsageInside),
                ) => event.clone(),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NotificationBody(output, history_id)),
                ) => {
                    x11.consume_notification_toast(*history_id);
                    toast_navigation_event(*output, *history_id, &state.notification_history)
                }
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NotificationCenter(output)),
                ) => Event::ToggleNotificationCenter(*output),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NotificationCenterGroupBody(key)),
                ) => Event::ExpandNotificationGroup(key.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::NotificationCenterGroupHeader(key)),
                ) => Event::CollapseNotificationGroup(key.clone()),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Audio(output)),
                ) => Event::AudioPopupToggledAt(*output),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Bluetooth(output)),
                ) => Event::BluetoothPopupToggledAt(*output),
                (
                    Event::X11(platform::x11::X11Event::ButtonPress { button: 1, .. }),
                    Some(platform::x11::HitTarget::Network(output)),
                ) => {
                    if state.network_popup_open {
                        Event::NetworkPopupToggledAt(*output)
                    } else {
                        Event::NetworkPopupOpenRequestedAt(*output)
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
                    Some(platform::x11::HitTarget::BluetoothManager),
                ) => Event::BluetoothManagerRequested,
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
                    Some(platform::x11::HitTarget::Network(_)),
                ) => event.clone(),
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::Item(path)),
                ) => Event::MenuItemHovered { path: path.clone() },
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::TopLevel(id, _)),
                ) => Event::MenuItemHovered { path: vec![*id] },
                (
                    Event::X11(platform::x11::X11Event::MotionNotify { .. }),
                    Some(platform::x11::HitTarget::Outside),
                ) => Event::MenuItemHovered { path: vec![] },
                _ => event.clone(),
            };
            let watcher_to_end = match &translated {
                Event::MenuUnregistered { window_id } => registry
                    .lock()
                    .expect("registry poisoned")
                    .get(*window_id)
                    .cloned(),
                Event::MenuRegistered {
                    window_id,
                    endpoint: MenuSource::DbusMenu(new_endpoint),
                } => registry
                    .lock()
                    .expect("registry poisoned")
                    .get(*window_id)
                    .filter(|old_endpoint| *old_endpoint != new_endpoint)
                    .cloned(),
                _ => None,
            };
            if let Event::MenuWatcherReady {
                endpoint,
                watcher_generation,
                request_id,
            } = &translated
            {
                menu_layout_reloads.watcher_ready(
                    endpoint.clone(),
                    *watcher_generation,
                    *request_id,
                );
            }
            match &translated {
                Event::WindowFocused(_)
                | Event::WindowFocusedWithApp { .. }
                | Event::MenuRegistered { .. }
                | Event::MenuUnregistered { .. }
                | Event::GtkMenuDiscovered { .. }
                | Event::GtkMenuRemoved { .. } => menu_layout_reloads.clear_active(),
                Event::MenuOwnerVanished { sender } => {
                    menu_layout_reloads.remove_watchers_for_owner(sender);
                    menu_layout_reloads.clear_active();
                }
                _ => {}
            }
            if let Some(endpoint) = watcher_to_end.as_ref() {
                menu_layout_reloads.remove_watcher(&MenuSource::DbusMenu(endpoint.clone()));
            }
            let stale_menu_watcher_event = match &translated {
                Event::MenuLayoutInvalidated {
                    endpoint,
                    watcher_generation: Some(watcher_generation),
                    ..
                }
                | Event::MenuPropertiesUpdated {
                    endpoint,
                    watcher_generation: Some(watcher_generation),
                    ..
                } => !menu_layout_reloads.accepts_watcher(endpoint, *watcher_generation),
                _ => false,
            };
            if stale_menu_watcher_event {
                continue;
            }
            let active_menu_endpoint =
                state.active_menu_endpoint(&registry.lock().expect("registry poisoned"));
            let stale_layout = matches!(
                &translated,
                Event::MenuLayoutInvalidated {
                    endpoint,
                    revision: Some(revision),
                    ..
                } if active_menu_endpoint.as_ref() == Some(endpoint)
                    && state.active_menu_model().is_some_and(|model| model.revision >= *revision)
            );
            let accepted_layout_invalidation = match &translated {
                Event::MenuLayoutInvalidated {
                    endpoint,
                    watcher_generation: Some(watcher_generation),
                    revision,
                } if !stale_layout && active_menu_endpoint.as_ref() == Some(endpoint) => {
                    menu_layout_reloads.record(endpoint.clone(), *watcher_generation, *revision)
                }
                Event::MenuLayoutInvalidated {
                    endpoint,
                    watcher_generation: None,
                    ..
                } => active_menu_endpoint.as_ref() == Some(endpoint),
                _ => false,
            };
            if let Some(endpoint) = watcher_to_end {
                dbus.end_menu_watcher(endpoint);
            }
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
                        Some(platform::x11::HitTarget::TopLevel(id, _)),
                    ) => eprintln!("xbar trace: top-level hit item={}", id.0),
                    (
                        Event::X11(platform::x11::X11Event::ButtonPress { .. }),
                        Some(platform::x11::HitTarget::Tray(endpoint, _)),
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
            let layout_in_flight = matches!(
                &state.menu,
                core::MenuState::Loading { endpoint: current, .. }
                    if state.active_menu_endpoint(&registry.lock().expect("registry poisoned"))
                        == Some(current.clone())
            );
            let mut request_menu = !stale_layout
                && ((matches!(
                    &translated,
                    Event::WindowFocused(_)
                        | Event::WindowFocusedWithApp { .. }
                        | Event::MenuRegistered { .. }
                        | Event::GtkMenuDiscovered { .. }
                        | Event::GtkMenuRemoved { .. }
                        | Event::MenuUnregistered { .. }
                        | Event::MenuOwnerVanished { .. }
                ) && state.menu_navigation.is_none()
                    && matches!(
                        state.menu_presentation_policy,
                        core::MenuPresentationPolicy::FollowFocus
                    ))
                    || should_schedule_invalidation(
                        stale_layout,
                        accepted_layout_invalidation,
                        layout_in_flight,
                        state
                            .menu_interaction
                            .pending_about_to_show
                            .as_ref()
                            .is_some_and(|pending| pending.lazy_root),
                    )
                    || (matches!(
                        &translated,
                        Event::MenuNavigateEscape
                            | Event::MenuItemActivateRequested { .. }
                            | Event::MenuClickedOutside
                    ) && matches!(
                        state.menu_presentation_policy,
                        core::MenuPresentationPolicy::FollowFocus
                    ))
                    || (matches!(
                        &translated,
                        Event::MenuRootClicked(_) | Event::MenuRootClickedAt { .. }
                    ) && matches!(state.menu, core::MenuState::TrayLoaded { .. })));
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
            let mut semantic_render_target = render_target_for(&translated, &mouse_target, &x11);
            let tray_menu_open = match &translated {
                Event::TrayMenuOpenRequested { endpoint }
                | Event::TrayMenuOpenRequestedAt { endpoint, .. } => Some(endpoint.clone()),
                _ => None,
            };
            let tray_menu_reclick = tray_menu_open.as_ref().is_some_and(|endpoint| {
                matches!(
                    &state.menu,
                    core::MenuState::TrayLoaded {
                        endpoint: current,
                        ..
                    } if current == endpoint
                ) && state.menu_interaction.open_root.is_some()
            });
            let sni_action = match &translated {
                Event::StatusNotifierActionRequested {
                    endpoint,
                    action,
                    root_x,
                    root_y,
                } => Some((endpoint.clone(), *action, *root_x, *root_y)),
                _ => None,
            };
            if trace && matches!(translated, Event::WindowFocusedWithApp { .. }) {
                eprintln!(
                    "xbar trace: PLUGINZONE_STATE before_reducer items={}",
                    state.plugin_zone.plugins.len()
                );
            }
            if let Event::StatusNotifierItemUpdated(item) = &translated {
                x11.note_status_notifier_item_update(item);
            }
            let presentation_before = state.menu_presentation.clone();
            let menu_interaction_before = (
                state.menu_interaction.open_root,
                state.menu_interaction.open_path.clone(),
                state.menu_interaction.hovered_path.clone(),
            );
            let workspace_event = matches!(
                translated,
                Event::WorkspaceFocused { .. } | Event::WorkspacesSnapshot(_)
            );
            let explicit_pin_before = matches!(
                state.menu_presentation_policy,
                core::MenuPresentationPolicy::Pinned { .. }
            );
            let pending_lazy_root_before = state.menu_interaction.pending_lazy_root.clone();
            let notification_center_before = state.notification_center_open;
            let notification_history_before = state.notification_history.clone();
            let ai_usage_popup_before = state.ai_usage_popup.is_some();
            let reduced = core::reduce(
                &mut state,
                translated.clone(),
                &mut registry.lock().expect("registry poisoned"),
            );
            if matches!(&translated, Event::NotificationsState { .. }) {
                notification_icon_resolver.resolve_history(&state.notification_history);
                x11.set_notification_icons(notification_icon_resolver.resolved_history_icons());
            }
            render_target = merge_render_target(
                render_target,
                ai_usage_update_render_target(
                    &translated,
                    reduced,
                    ai_usage_popup_before,
                    state.ai_usage_popup.is_some(),
                ),
            );
            if notification_center_toggle_opened(
                &translated,
                notification_center_before,
                state.notification_center_open,
            ) {
                x11.consume_notification_toast_stack();
            }
            if state.notification_center_open.is_some()
                && matches!(translated, Event::NotificationsState { .. })
            {
                x11.mark_notification_toast_known(notification_history_ids_added(
                    &notification_history_before,
                    &state.notification_history,
                ));
            }
            if reduced {
                x11.note_menu_interaction_change(
                    menu_interaction_before.0,
                    &menu_interaction_before.1,
                    &menu_interaction_before.2,
                    state.menu_interaction.open_root,
                    &state.menu_interaction.open_path,
                    &state.menu_interaction.hovered_path,
                );
                if matches!(
                    &translated,
                    Event::MenuAboutToShowRequested { .. }
                        | Event::MenuAboutToShowCompleted { .. }
                        | Event::MenuLoaded { .. }
                        | Event::MenuPropertiesUpdated { .. }
                ) {
                    // Provider/lazy-menu completion can alter visible rows or
                    // popup topology beyond the local hover owner.
                    x11.mark_all_menu_popups_dirty();
                }
            }
            if trace && matches!(translated, Event::WindowFocusedWithApp { .. }) {
                eprintln!(
                    "xbar trace: PLUGINZONE_STATE after_reducer items={}",
                    state.plugin_zone.plugins.len()
                );
            }
            if reduced {
                render_cause = Some(match &translated {
                    Event::ActiveAiUsageChanged(_) => "ActiveAiUsageChanged",
                    Event::WindowFocusedWithApp { .. } => "WindowFocusedWithApp",
                    _ => "other",
                });
            }
            if workspace_event
                && explicit_pin_before
                && matches!(
                    state.menu_presentation_policy,
                    core::MenuPresentationPolicy::FollowFocus
                )
            {
                i3.request_focused_window()?;
            }
            if reduced
                && state.menu_navigation.is_none()
                && state.menu_presentation != presentation_before
                && state.focused_window.is_some()
                && !workspace_event
            {
                request_menu = true;
            }
            let requested_grab = state
                .menu_navigation
                .as_ref()
                .filter(|session| session.grab_state == core::KeyboardGrabState::Requested)
                .map(|session| session.id);
            if let Some(session_id) = requested_grab {
                let acquired = x11.acquire_keyboard_grab(session_id)?;
                core::reduce(
                    &mut state,
                    if acquired {
                        Event::KeyboardGrabAcquired { session_id }
                    } else {
                        Event::KeyboardGrabFailed { session_id }
                    },
                    &mut registry.lock().expect("registry poisoned"),
                );
                dirty = true;
            }
            if x11.keyboard_grab_session().is_some_and(|session_id| {
                state
                    .menu_navigation
                    .as_ref()
                    .is_none_or(|session| session.id != session_id)
            }) {
                x11.release_keyboard_grab(None)?;
            }
            if reduced {
                if let Some(pending) = pending_lazy_root_to_schedule(
                    pending_lazy_root_before.as_ref(),
                    state.menu_interaction.pending_lazy_root.as_ref(),
                )
                .cloned()
                {
                    let request_id = next_menu_request_id;
                    next_menu_request_id += 1;
                    let about = Event::MenuAboutToShowRequested {
                        window_id: pending.window_id,
                        endpoint: pending.endpoint.clone(),
                        item_id: pending.item_id,
                        request_id,
                        lazy_root: true,
                        intent_id: Some(pending.intent_id),
                        watcher_generation: Some(pending.watcher_generation),
                    };
                    if core::reduce(
                        &mut state,
                        about,
                        &mut registry.lock().expect("registry poisoned"),
                    ) {
                        dirty = true;
                        render_target = Some(RenderTarget::DockContext);
                        if let MenuSource::DbusMenu(endpoint) = pending.endpoint {
                            dbus.request_about_to_show(
                                pending.window_id,
                                endpoint,
                                pending.item_id,
                                request_id,
                                true,
                                Some(pending.intent_id),
                                Some(pending.watcher_generation),
                            );
                        }
                    }
                }
            }
            if let Event::MenuLoaded {
                window_id,
                endpoint,
                request_id,
                model,
                ..
            } = &translated
            {
                let follow_up =
                    menu_layout_reloads.complete_load(endpoint, *request_id, model.revision);
                if follow_up
                    || state
                        .menu_interaction
                        .pending_lazy_root
                        .as_ref()
                        .is_some_and(|pending| pending.layout_request_id == Some(*request_id))
                {
                    let next_request_id = if follow_up {
                        let id = next_menu_request_id;
                        next_menu_request_id += 1;
                        Some(id)
                    } else {
                        None
                    };
                    let convergence = Event::MenuLazyRootLoadConvergence {
                        window_id: *window_id,
                        endpoint: endpoint.clone(),
                        request_id: *request_id,
                        follow_up_request_id: next_request_id,
                    };
                    core::reduce(
                        &mut state,
                        convergence,
                        &mut registry.lock().expect("registry poisoned"),
                    );
                }
                if follow_up {
                    request_menu = true;
                }
            }
            if reduced
                && matches!(
                    &translated,
                    Event::MenuAboutToShowCompleted {
                        lazy_root: true,
                        error: None,
                        intent_id: Some(_),
                        watcher_generation: Some(_),
                        ..
                    }
                )
            {
                if let Event::MenuAboutToShowCompleted {
                    window_id,
                    endpoint: MenuSource::DbusMenu(endpoint),
                    item_id: _,
                    request_id: _,
                    intent_id: Some(intent_id),
                    watcher_generation: Some(watcher_generation),
                    ..
                } = &translated
                {
                    let layout_request_id = next_menu_request_id;
                    next_menu_request_id += 1;
                    let layout_event = Event::MenuLazyRootLayoutRequested {
                        window_id: *window_id,
                        endpoint: MenuSource::DbusMenu(endpoint.clone()),
                        request_id: layout_request_id,
                        intent_id: *intent_id,
                        watcher_generation: *watcher_generation,
                    };
                    if core::reduce(
                        &mut state,
                        layout_event,
                        &mut registry.lock().expect("registry poisoned"),
                    ) {
                        dirty = true;
                        render_target = Some(RenderTarget::DockContext);
                        let source = MenuSource::DbusMenu(endpoint.clone());
                        menu_layout_reloads.begin_load(source, layout_request_id);
                        dbus.request_layout(*window_id, endpoint.clone(), layout_request_id);
                    }
                }
            }
            if trace && matches!(translated, Event::ActiveAiUsageChanged(_)) {
                eprintln!(
                    "xbar trace: AI_MAIN_EVENT_RECEIVED agents={} AI_REDUCER_CHANGED dirty={reduced}",
                    state.ai_usage.len()
                );
            }
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
                    semantic_render_target = None;
                }
            }
            dirty |= reduced || popup_hover_changed || popup_exposed || notification_scrolled;
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
                Event::BluetoothManagerRequested => {
                    if let Some(argv) = state.bluetooth_manager_command.clone() {
                        let request = external::ExternalLaunchRequest {
                            argv,
                            presentation: external::ExternalPresentation::FloatingTerminal,
                        };
                        if let Err(error) = external_launcher.launch(&request) {
                            eprintln!("xbar: {error}");
                        }
                    }
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
            if matches!(
                translated,
                Event::NetworkPopupOpenRequested | Event::NetworkPopupOpenRequestedAt(_)
            ) && reduced
            {
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
            if let Some(endpoint) = tray_menu_open.filter(|_| !tray_menu_reclick) {
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
            let event_render_target =
                render_target_for_changes(reduced, semantic_render_target, popup_hover_changed);
            if let Some(target) = event_render_target {
                render_target = merge_render_target(render_target, Some(target));
            }
            if notification_hover_changed {
                render_target =
                    merge_render_target(render_target, Some(RenderTarget::Notification));
            }
            if notification_scrolled {
                render_target =
                    merge_render_target(render_target, Some(RenderTarget::Notification));
            }
            if notification_scrolled && std::env::var_os("XBAR_TRACE_NOTIFICATION_SCROLL").is_some()
            {
                eprintln!(
                    "notification-center scroll-redraw: dirty={} render_target={} scheduled=true",
                    dirty,
                    render_target
                        .map_or_else(|| "NONE".to_owned(), |target| target.debug_regions())
                );
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
                                    timestamp,
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
                                state.menu_presentation_window()
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
                                lazy_root: false,
                                intent_id: None,
                                watcher_generation: None,
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
                                        window_id, endpoint, item_id, request_id, false, None, None,
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
                    state.menu_presentation_window(),
                    state.active_menu_endpoint(&registry_guard),
                ) {
                    let request_id = state
                        .menu_interaction
                        .pending_lazy_root
                        .as_ref()
                        .filter(|pending| {
                            pending.window_id == window_id && pending.endpoint == endpoint
                        })
                        .and_then(|pending| pending.layout_request_id)
                        .unwrap_or_else(|| {
                            let id = next_menu_request_id;
                            next_menu_request_id += 1;
                            id
                        });
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
                            Some(current) => current.merge(RenderTarget::DockContext),
                            None => RenderTarget::DockContext,
                        });
                    }
                    match endpoint {
                        MenuSource::DbusMenu(endpoint) => {
                            menu_layout_reloads
                                .begin_load(MenuSource::DbusMenu(endpoint.clone()), request_id);
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
        let render_target = promote_menu_popup_target(render_target, x11.has_menu_popup_dirty());
        if dirty {
            if trace {
                eprintln!(
                    "xbar trace: RENDER_REQUESTED cause={}",
                    render_cause.unwrap_or("unknown")
                );
                let target = render_target.unwrap_or(RenderTarget::All);
                eprintln!("xbar trace: DIRTY_REGIONS={}", target.debug_regions());
            }
            if outputs_changed {
                x11.sync_windows(&state.outputs)?;
            }
            x11.render(&state, render_target.unwrap_or(RenderTarget::All))?;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PassiveCoreTarget {
    AiOpener,
    AiPopupInside,
    NotificationCenter,
    OtherXbar,
    External,
}

fn passive_core_target(target: &HitTarget) -> PassiveCoreTarget {
    match target {
        HitTarget::AiUsage(_, _) => PassiveCoreTarget::AiOpener,
        HitTarget::AiUsageInside => PassiveCoreTarget::AiPopupInside,
        HitTarget::NotificationCenter(_)
        | HitTarget::NotificationCenterCard(_)
        | HitTarget::NotificationCenterDismiss(_)
        | HitTarget::NotificationCenterGroupBody(_)
        | HitTarget::NotificationCenterGroupHeader(_)
        | HitTarget::NotificationCenterAction(_, _)
        | HitTarget::NotificationCenterActionPagePrev(_)
        | HitTarget::NotificationCenterActionPageNext(_)
        | HitTarget::NotificationCenterClearAll
        | HitTarget::NotificationCenterEmpty => PassiveCoreTarget::NotificationCenter,
        HitTarget::Outside => PassiveCoreTarget::External,
        _ => PassiveCoreTarget::OtherXbar,
    }
}

fn eligible_passive_raw_button(detail: u32) -> bool {
    matches!(detail, 1..=3)
}

fn passive_dismiss_for_targets(
    ai_open: bool,
    notification_open: bool,
    core_targets: &[PassiveCoreTarget],
) -> (bool, bool) {
    let ai_protected = core_targets.iter().any(|target| {
        matches!(
            target,
            PassiveCoreTarget::AiOpener | PassiveCoreTarget::AiPopupInside
        )
    });
    let notification_protected = core_targets.contains(&PassiveCoreTarget::NotificationCenter);
    (
        ai_open && !ai_protected,
        notification_open && !notification_protected,
    )
}

#[derive(Clone, Debug)]
struct PassiveCoreCandidate {
    index: usize,
    timestamp: u32,
    target: PassiveCoreTarget,
}

fn passive_core_targets_for_timestamp(
    cores: &[PassiveCoreCandidate],
    timestamp: u32,
) -> Vec<PassiveCoreTarget> {
    cores
        .iter()
        .filter(|core| core.timestamp == timestamp)
        .map(|core| core.target)
        .collect()
}

fn passive_dismiss_event(ai_usage: bool, notification_center: bool) -> Option<Event> {
    (ai_usage || notification_center).then_some(Event::PassivePopupDismissRequested {
        ai_usage,
        notification_center,
    })
}

fn prepare_passive_batch(events: &mut Vec<Event>, state: &State, x11: &X11Platform) {
    let cores = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let Event::X11(platform::x11::X11Event::ButtonPress {
                timestamp, button, ..
            }) = event
            else {
                return None;
            };
            if !eligible_passive_raw_button(u32::from(*button)) {
                return None;
            }
            let target = x11.hit_test(match event {
                Event::X11(event) => event,
                _ => unreachable!(),
            });
            Some(PassiveCoreCandidate {
                index,
                timestamp: *timestamp,
                target: passive_core_target(&target),
            })
        })
        .collect::<Vec<_>>();

    let raw_presses = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let Event::X11(platform::x11::X11Event::RawButtonPress { time, detail, .. }) = event
            else {
                return None;
            };
            eligible_passive_raw_button(*detail).then_some((index, *time))
        })
        .collect::<Vec<_>>();

    let ai_open = state.ai_usage_popup.is_some();
    let notification_open = state.notification_center_open.is_some();
    let mut insertions = HashMap::<usize, (bool, bool)>::new();
    for (raw_index, timestamp) in &raw_presses {
        let targets = passive_core_targets_for_timestamp(&cores, *timestamp);
        if targets.is_empty() {
            let (dismiss_ai, dismiss_notification) =
                passive_dismiss_for_targets(ai_open, notification_open, &[]);
            if dismiss_ai || dismiss_notification {
                insertions.insert(*raw_index, (dismiss_ai, dismiss_notification));
            }
            continue;
        }
        let (dismiss_ai, dismiss_notification) =
            passive_dismiss_for_targets(ai_open, notification_open, &targets);
        if let Some(index) = cores
            .iter()
            .filter(|core| core.timestamp == *timestamp)
            .map(|core| core.index)
            .min()
        {
            if dismiss_ai || dismiss_notification {
                insertions
                    .entry(index)
                    .and_modify(|flags| {
                        flags.0 |= dismiss_ai;
                        flags.1 |= dismiss_notification;
                    })
                    .or_insert((dismiss_ai, dismiss_notification));
            }
        }
    }

    let mut insertions = insertions.into_iter().collect::<Vec<_>>();
    insertions.sort_by_key(|(index, _)| std::cmp::Reverse(*index));
    for (index, (dismiss_ai, dismiss_notification)) in insertions {
        if let Some(dismiss) = passive_dismiss_event(dismiss_ai, dismiss_notification) {
            events.insert(index, dismiss);
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
            } else if x11.is_notification_center_window(*window) {
                Some(RenderTarget::Notification)
            } else {
                None
            }
        }
        Event::X11(platform::x11::X11Event::RandrChanged) => Some(RenderTarget::Dock),
        Event::X11(
            platform::x11::X11Event::GtkWindowChanged(_)
            | platform::x11::X11Event::GtkWindowsChanged
            | platform::x11::X11Event::GtkWindowDestroyed(_),
        ) => Some(RenderTarget::DockContext),
        Event::X11(platform::x11::X11Event::WindowAttentionChanged { .. }) => None,
        Event::X11(platform::x11::X11Event::InstanceLost)
        | Event::X11(platform::x11::X11Event::Close) => None,
        Event::WindowFocusedWithApp { .. } => Some(RenderTarget::DockContext),
        Event::MenuWatcherReady { .. } => None,
        Event::MenuRegistered { .. }
        | Event::GtkMenuDiscovered { .. }
        | Event::GtkMenuRemoved { .. }
        | Event::MenuUnregistered { .. }
        | Event::MenuOwnerVanished { .. }
        | Event::MenuLoadRequested { .. }
        | Event::MenuLazyRootLayoutRequested { .. }
        | Event::MenuLazyRootLoadConvergence { .. }
        | Event::MenuLoaded { .. }
        | Event::MenuLoadFailed { .. }
        | Event::MenuLayoutInvalidated { .. }
        | Event::MenuPropertiesUpdated { .. } => Some(RenderTarget::DockContext),
        Event::WorkspacesSnapshot(_) | Event::WorkspaceFocused { .. } => {
            Some(RenderTarget::Workspaces)
        }
        Event::OutputsChanged(_) => Some(RenderTarget::Dock.merge(RenderTarget::Notification)),
        Event::X11(platform::x11::X11Event::MotionNotify { .. }) => {
            hover_render_target_for(mouse_target.as_ref(), None)
        }
        Event::X11(platform::x11::X11Event::ButtonPress { button: 4 | 5, .. })
            if matches!(
                mouse_target,
                Some(
                    HitTarget::NotificationCenterCard(_)
                        | HitTarget::NotificationCenterDismiss(_)
                        | HitTarget::NotificationCenterClearAll
                        | HitTarget::NotificationCenterEmpty,
                )
            ) =>
        {
            Some(RenderTarget::Notification)
        }
        Event::MenuItemHovered { .. } => {
            hover_render_target_for(mouse_target.as_ref(), Some(RenderTarget::DockContext))
        }
        Event::MenuClickedOutside => Some(RenderTarget::Popup),
        Event::PassivePopupDismissRequested {
            ai_usage,
            notification_center,
        } => {
            let mut target = None;
            if *ai_usage {
                target = Some(RenderTarget::Popup);
            }
            if *notification_center {
                target = merge_render_target(target, Some(RenderTarget::Notification));
            }
            target
        }
        Event::MenuAboutToShowRequested { .. } => Some(RenderTarget::Popup),
        Event::MenuAboutToShowCompleted { need_update, .. } => Some(if *need_update {
            RenderTarget::DockContext
        } else {
            RenderTarget::Popup
        }),
        Event::ClockUpdated(_) => Some(RenderTarget::DateTime),
        Event::AudioSnapshotReceived(_) | Event::AudioUnavailable => Some(RenderTarget::Audio),
        Event::NetworkStatusChanged(_) | Event::NetworkSnapshotReceived(_) => {
            Some(RenderTarget::Network)
        }
        Event::BluetoothSnapshotReceived(_) | Event::BluetoothUnavailable => {
            Some(RenderTarget::Bluetooth)
        }
        Event::ActiveAiUsageChanged(_) => Some(RenderTarget::PluginZone),
        Event::AiUsagePopupToggled { .. } => Some(RenderTarget::Popup),
        Event::StatusNotifierRegistered(_)
        | Event::StatusNotifierUnregistered(_)
        | Event::StatusNotifierOwnerVanished(_)
        | Event::StatusNotifierWatcherUnavailable
        | Event::StatusNotifierItemUpdated(_) => Some(RenderTarget::Tray),
        Event::StatusNotifierHostRegistered => Some(RenderTarget::Tray),
        Event::MenuRootClicked(_)
        | Event::MenuRootClickedAt { .. }
        | Event::MenuItemActivateRequested { .. }
        | Event::TrayMenuOpenRequested { .. }
        | Event::TrayMenuOpenRequestedAt { .. }
        | Event::TrayMenuLoaded { .. }
        | Event::TrayMenuLoadFailed { .. }
        | Event::NetworkPopupProjectionChanged(_)
        | Event::NetworkConnectSavedWifi(_)
        | Event::NetworkPopupOpenRequested
        | Event::NetworkPopupOpenRequestedAt(_)
        | Event::NetworkPopupSnapshotReceived(_)
        | Event::NetworkPopupSnapshotFailed
        | Event::NetworkPopupToggled
        | Event::NetworkPopupToggledAt(_)
        | Event::NetworkSetWireless(_)
        | Event::NetworkActionFinished(_)
        | Event::BluetoothPopupToggled
        | Event::BluetoothPopupToggledAt(_)
        | Event::BluetoothSetPowered(_)
        | Event::BluetoothConnectDevice(_)
        | Event::BluetoothDisconnectDevice(_)
        | Event::BluetoothManagerRequested
        | Event::BluetoothActionFinished(_)
        | Event::AudioInventoryReceived { .. }
        | Event::AudioSelectOutput(_)
        | Event::AudioSelectInput(_)
        | Event::AudioPopupToggled
        | Event::AudioPopupToggledAt(_)
        | Event::AudioTrackChanged { .. }
        | Event::AudioDragReleased
        | Event::AudioMuteToggled { .. } => Some(RenderTarget::Popup),
        Event::WindowFocused(_) => Some(RenderTarget::DockContext),
        Event::WindowAttentionChanged { .. } | Event::StatusNotifierActionRequested { .. } => None,
        Event::NotificationsSnapshot(_) | Event::NotificationsState { .. } => {
            Some(RenderTarget::Dock.merge(RenderTarget::Notification))
        }
        Event::ToggleNotificationCenter(_) => Some(RenderTarget::Notification),
        Event::ExpandNotificationGroup(_) | Event::CollapseNotificationGroup(_) => {
            Some(RenderTarget::Notification)
        }
        Event::EnsureNotificationCenterOpen { .. } | Event::NotificationToastConsumed => {
            Some(RenderTarget::Notification)
        }
        _ => Some(RenderTarget::All),
    }
}

fn toast_navigation_event(
    output: crate::core::OutputId,
    history_id: crate::core::HistoryEntryId,
    history: &[crate::core::NotificationHistoryEntry],
) -> Event {
    if history.is_empty() {
        Event::NotificationToastConsumed
    } else {
        Event::EnsureNotificationCenterOpen {
            output,
            target: history
                .iter()
                .any(|entry| entry.id == history_id)
                .then_some(history_id),
        }
    }
}

fn hover_render_target_for(
    mouse_target: Option<&HitTarget>,
    outside_target: Option<RenderTarget>,
) -> Option<RenderTarget> {
    match mouse_target {
        Some(HitTarget::Item(_)) => Some(RenderTarget::Popup),
        Some(HitTarget::TopLevel(_, _)) => Some(RenderTarget::DockContext),
        Some(HitTarget::AiUsage(_, _)) => None,
        Some(HitTarget::AiUsageInside) => None,
        Some(HitTarget::Outside) | None => outside_target,
        Some(HitTarget::Tray(_, _)) => None,
        Some(HitTarget::AudioTrack) | Some(HitTarget::AudioInputTrack) => Some(RenderTarget::Popup),
        Some(HitTarget::Audio(_))
        | Some(HitTarget::AudioMute)
        | Some(HitTarget::AudioInputMute)
        | Some(HitTarget::AudioInside) => None,
        Some(HitTarget::AudioOutputDevice(_)) | Some(HitTarget::AudioInputDevice(_)) => {
            Some(RenderTarget::Popup)
        }
        Some(HitTarget::BluetoothDevice(_)) | Some(HitTarget::BluetoothManager) => {
            Some(RenderTarget::Popup)
        }
        Some(HitTarget::BluetoothPower)
        | Some(HitTarget::BluetoothInside)
        | Some(HitTarget::Bluetooth(_)) => None,
        Some(HitTarget::NetworkWireless)
        | Some(HitTarget::NetworkInside)
        | Some(HitTarget::NetworkWifi(_))
        | Some(HitTarget::Network(_)) => None,
        Some(HitTarget::NotificationCenter(_)) => None,
        Some(HitTarget::NotificationCenterCard(_))
        | Some(HitTarget::NotificationCenterDismiss(_))
        | Some(HitTarget::NotificationCenterAction(_, _))
        | Some(HitTarget::NotificationCenterActionPagePrev(_))
        | Some(HitTarget::NotificationCenterActionPageNext(_))
        | Some(HitTarget::NotificationBody(_, _))
        | Some(HitTarget::NotificationCenterClearAll)
        | Some(HitTarget::NotificationCenterGroupBody(_))
        | Some(HitTarget::NotificationCenterGroupHeader(_))
        | Some(HitTarget::NotificationCenterEmpty) => None,
    }
}

fn merge_render_target(
    current: Option<RenderTarget>,
    required: Option<RenderTarget>,
) -> Option<RenderTarget> {
    match (current, required) {
        (Some(current), Some(required)) => Some(current.merge(required)),
        (Some(current), None) => Some(current),
        (None, Some(required)) => Some(required),
        (None, None) => None,
    }
}

fn promote_menu_popup_target(
    target: Option<RenderTarget>,
    menu_popup_dirty: bool,
) -> Option<RenderTarget> {
    if menu_popup_dirty {
        merge_render_target(target, Some(RenderTarget::Popup))
    } else {
        target
    }
}

fn render_target_for_changes(
    reduced: bool,
    semantic_target: Option<RenderTarget>,
    popup_hover_changed: bool,
) -> Option<RenderTarget> {
    let semantic_target = reduced.then_some(semantic_target).flatten();
    let popup_hover_target = popup_hover_changed.then_some(RenderTarget::Popup);
    merge_render_target(semantic_target, popup_hover_target)
}

fn ai_usage_update_render_target(
    event: &Event,
    reduced: bool,
    popup_was_open: bool,
    popup_is_open: bool,
) -> Option<RenderTarget> {
    (reduced
        && matches!(event, Event::ActiveAiUsageChanged(_))
        && (popup_was_open || popup_is_open))
        .then_some(RenderTarget::Popup)
}

fn notification_action_page_render_target(changed: bool) -> Option<RenderTarget> {
    changed.then_some(RenderTarget::Notification)
}

fn notification_center_toggle_opened(
    event: &Event,
    previous: Option<core::OutputId>,
    current: Option<core::OutputId>,
) -> bool {
    matches!(event, Event::ToggleNotificationCenter(output) if previous.is_none() && current == Some(*output))
}

fn notification_history_ids_added(
    before: &[core::NotificationHistoryEntry],
    after: &[core::NotificationHistoryEntry],
) -> Vec<core::HistoryEntryId> {
    after
        .iter()
        .filter(|entry| !before.iter().any(|previous| previous.id == entry.id))
        .map(|entry| entry.id)
        .collect()
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

fn keyboard_event(
    event: &Event,
    state: &State,
    registry: &core::MenuRegistry,
    x11: &platform::x11::X11Platform,
) -> Result<Event, Box<dyn Error>> {
    let semantic = x11
        .navigation_event(match event {
            Event::X11(event) => event,
            _ => return Ok(event.clone()),
        })?
        .unwrap_or_else(|| event.clone());
    if !matches!(semantic, Event::MenuNavigateEnter) {
        return Ok(semantic);
    }
    let Some(item_id) = state
        .menu_navigation
        .as_ref()
        .and_then(|session| session.selected_path.as_ref())
        .and_then(|path| path.last())
        .copied()
    else {
        return Ok(semantic);
    };
    let Some(model) = state.active_menu_model() else {
        return Ok(semantic);
    };
    let Some(menu_item) = ui::layout::find_item(&model.root, item_id) else {
        return Ok(semantic);
    };
    if !menu_item.visible
        || !menu_item.enabled
        || !matches!(menu_item.item_type, core::MenuItemType::Standard)
        || menu_item.children_display.is_some()
    {
        return Ok(semantic);
    }
    let Some(presentation) = state.menu_presentation.as_ref() else {
        return Ok(semantic);
    };
    let timestamp = match event {
        Event::X11(platform::x11::X11Event::KeyPress { timestamp, .. }) => *timestamp,
        _ => 0,
    };
    let endpoint = presentation.endpoint.clone();
    if !registry.source_matches(presentation.window_id, &endpoint) {
        return Ok(semantic);
    }
    Ok(Event::MenuItemActivateRequested {
        window_id: presentation.window_id,
        endpoint,
        item_id,
        timestamp,
    })
}

#[cfg(test)]
mod scheduler_tests {
    use super::{
        ai_usage_update_render_target, eligible_passive_raw_button, hover_render_target_for,
        merge_render_target, notification_action_page_render_target,
        notification_center_toggle_opened, notification_history_ids_added, passive_core_target,
        passive_core_targets_for_timestamp, passive_dismiss_for_targets,
        pending_lazy_root_to_schedule, promote_menu_popup_target, render_target_for_changes,
        should_schedule_invalidation, toast_navigation_event, PassiveCoreCandidate,
        PassiveCoreTarget,
    };
    use crate::core::{LazyRootOpenPending, MenuEndpoint, MenuItemId, MenuSource, WindowId};
    use crate::platform::x11::{HitTarget, RenderTarget};

    #[test]
    fn passive_dismiss_classification_respects_protected_targets() {
        assert_eq!(
            passive_dismiss_for_targets(true, false, &[PassiveCoreTarget::External]),
            (true, false)
        );
        assert_eq!(
            passive_dismiss_for_targets(false, true, &[PassiveCoreTarget::External]),
            (false, true)
        );
        assert_eq!(
            passive_dismiss_for_targets(true, true, &[PassiveCoreTarget::External]),
            (true, true)
        );
        assert_eq!(
            passive_dismiss_for_targets(true, false, &[PassiveCoreTarget::AiOpener]),
            (false, false)
        );
        assert_eq!(
            passive_dismiss_for_targets(false, true, &[PassiveCoreTarget::NotificationCenter]),
            (false, false)
        );
        assert_eq!(
            passive_dismiss_for_targets(true, true, &[PassiveCoreTarget::AiPopupInside]),
            (false, true)
        );
        assert_eq!(
            passive_dismiss_for_targets(true, true, &[PassiveCoreTarget::NotificationCenter]),
            (true, false)
        );
        assert_eq!(passive_dismiss_for_targets(true, false, &[]), (true, false));
    }

    #[test]
    fn batch_core_matching_is_independent_of_raw_event_position() {
        let raw_then_core = vec![PassiveCoreCandidate {
            index: 1,
            timestamp: 77,
            target: PassiveCoreTarget::OtherXbar,
        }];
        let core_then_raw = raw_then_core.clone();
        assert_eq!(
            passive_core_targets_for_timestamp(&raw_then_core, 77),
            passive_core_targets_for_timestamp(&core_then_raw, 77)
        );
        assert_eq!(
            passive_dismiss_for_targets(
                true,
                false,
                &passive_core_targets_for_timestamp(&raw_then_core, 77)
            ),
            (true, false)
        );
    }

    #[test]
    fn passive_targets_and_buttons_keep_opener_inside_and_wheel_distinct() {
        assert_eq!(
            passive_core_target(&HitTarget::AiUsageInside),
            PassiveCoreTarget::AiPopupInside
        );
        assert_eq!(
            passive_core_target(&HitTarget::AiUsage(
                crate::core::PluginId("ai".into()),
                crate::core::OutputId(1),
            )),
            PassiveCoreTarget::AiOpener
        );
        assert_eq!(
            passive_core_target(&HitTarget::NotificationCenter(crate::core::OutputId(1))),
            PassiveCoreTarget::NotificationCenter
        );
        assert!(eligible_passive_raw_button(1));
        assert!(eligible_passive_raw_button(2));
        assert!(eligible_passive_raw_button(3));
        assert!(!eligible_passive_raw_button(4));
        assert!(!eligible_passive_raw_button(5));
    }

    fn pending(
        window_id: u32,
        service: &str,
        item_id: i32,
        intent_id: u64,
        watcher_generation: u64,
        layout_request_id: Option<u64>,
    ) -> LazyRootOpenPending {
        LazyRootOpenPending {
            window_id: WindowId(window_id),
            endpoint: MenuSource::DbusMenu(MenuEndpoint {
                service: service.into(),
                object_path: "/Menu".into(),
            }),
            item_id: MenuItemId(item_id),
            intent_id,
            watcher_generation,
            layout_request_id,
        }
    }

    fn history_entry(id: u64) -> crate::core::NotificationHistoryEntry {
        crate::core::NotificationHistoryEntry {
            id: crate::core::HistoryEntryId(id),
            live_notification_id: None,
            source: crate::core::NotificationSource::Freedesktop,
            app_name: "app".into(),
            summary: "summary".into(),
            body: "body".into(),
            icon_metadata: Default::default(),
            order: id,
            received_at: id,
            updated_at: id,
        }
    }

    #[test]
    fn toast_navigation_targets_the_exact_history_entry() {
        let history = vec![history_entry(7), history_entry(8)];
        assert!(matches!(
            toast_navigation_event(
                crate::core::OutputId(3),
                crate::core::HistoryEntryId(8),
                &history
            ),
            crate::core::Event::EnsureNotificationCenterOpen {
                output: crate::core::OutputId(3),
                target: Some(crate::core::HistoryEntryId(8)),
            }
        ));
    }

    #[test]
    fn ai_usage_updates_redraw_only_an_open_or_closing_popup() {
        let event = crate::core::Event::ActiveAiUsageChanged(Vec::new());
        assert_eq!(
            ai_usage_update_render_target(&event, true, false, false),
            None
        );
        assert_eq!(
            ai_usage_update_render_target(&event, true, true, true),
            Some(RenderTarget::Popup)
        );
        assert_eq!(
            ai_usage_update_render_target(&event, true, true, false),
            Some(RenderTarget::Popup)
        );
        assert_eq!(
            ai_usage_update_render_target(&event, false, true, true),
            None
        );
    }

    #[test]
    fn bell_open_transition_consumes_toasts_only_when_center_was_closed() {
        let event = crate::core::Event::ToggleNotificationCenter(crate::core::OutputId(3));
        assert!(notification_center_toggle_opened(
            &event,
            None,
            Some(crate::core::OutputId(3))
        ));
        assert!(!notification_center_toggle_opened(
            &event,
            Some(crate::core::OutputId(3)),
            None
        ));
        assert!(!notification_center_toggle_opened(
            &event,
            Some(crate::core::OutputId(2)),
            Some(crate::core::OutputId(3))
        ));
    }

    #[test]
    fn center_open_arrivals_are_distinguished_from_existing_history() {
        let before = [history_entry(3), history_entry(2), history_entry(1)];
        let after = [
            history_entry(4),
            history_entry(3),
            history_entry(2),
            history_entry(1),
        ];
        assert_eq!(
            notification_history_ids_added(&before, &after),
            vec![crate::core::HistoryEntryId(4)]
        );
    }

    #[test]
    fn stale_toast_navigation_opens_without_a_target_when_history_remains() {
        let history = vec![history_entry(7)];
        assert!(matches!(
            toast_navigation_event(
                crate::core::OutputId(3),
                crate::core::HistoryEntryId(8),
                &history
            ),
            crate::core::Event::EnsureNotificationCenterOpen { target: None, .. }
        ));
    }

    #[test]
    fn stale_toast_navigation_consumes_only_when_history_is_empty() {
        assert!(matches!(
            toast_navigation_event(
                crate::core::OutputId(3),
                crate::core::HistoryEntryId(8),
                &[]
            ),
            crate::core::Event::NotificationToastConsumed
        ));
    }

    #[test]
    fn lazy_about_to_show_suppresses_invalidation_load() {
        assert!(!should_schedule_invalidation(false, true, false, true));
    }

    #[test]
    fn completed_lazy_about_to_show_allows_one_canonical_load() {
        assert!(should_schedule_invalidation(false, true, false, false));
    }

    #[test]
    fn in_flight_load_blocks_parallel_invalidation_load() {
        assert!(!should_schedule_invalidation(false, true, true, false));
    }

    #[test]
    fn stale_invalidation_never_schedules_a_load() {
        assert!(!should_schedule_invalidation(true, true, false, false));
    }

    #[test]
    fn new_lazy_pending_request_is_scheduled_once() {
        let pending = pending(7, ":1.7", 1, 10, 20, None);
        assert_eq!(
            pending_lazy_root_to_schedule(None, Some(&pending)),
            Some(&pending)
        );
    }

    #[test]
    fn unchanged_lazy_pending_request_is_not_rescheduled() {
        let before = pending(7, ":1.7", 1, 10, 20, None);
        let after = pending(7, ":1.7", 1, 10, 20, Some(30));
        assert_eq!(
            pending_lazy_root_to_schedule(Some(&before), Some(&after)),
            None
        );
    }

    #[test]
    fn changed_notification_action_page_requests_notification_render() {
        assert_eq!(
            notification_action_page_render_target(true),
            Some(RenderTarget::Notification)
        );
        assert_eq!(notification_action_page_render_target(false), None);
    }

    #[test]
    fn replaced_lazy_pending_request_is_scheduled_once() {
        let before = pending(7, ":1.7", 1, 10, 20, None);
        let after = pending(7, ":1.7", 2, 11, 20, None);
        assert_eq!(
            pending_lazy_root_to_schedule(Some(&before), Some(&after)),
            Some(&after)
        );
    }

    #[test]
    fn cleared_lazy_pending_request_is_not_scheduled() {
        let before = pending(7, ":1.7", 1, 10, 20, None);
        assert_eq!(pending_lazy_root_to_schedule(Some(&before), None), None);
    }

    #[test]
    fn same_numeric_root_from_a_different_endpoint_is_new_request() {
        let before = pending(7, ":1.7", 1, 10, 20, None);
        let after = pending(7, ":1.8", 1, 10, 20, None);
        assert_eq!(
            pending_lazy_root_to_schedule(Some(&before), Some(&after)),
            Some(&after)
        );
    }

    #[test]
    fn changed_local_popup_hover_requires_popup_without_reducer_change() {
        assert_eq!(
            render_target_for_changes(false, None, true),
            Some(RenderTarget::Popup)
        );
    }

    #[test]
    fn unchanged_local_popup_hover_requires_no_render_target() {
        assert_eq!(
            render_target_for_changes(false, Some(RenderTarget::Popup), false),
            None
        );
    }

    #[test]
    fn semantic_and_auxiliary_popup_targets_stay_popup_scoped() {
        assert_eq!(
            render_target_for_changes(true, Some(RenderTarget::Popup), true),
            Some(RenderTarget::Popup)
        );
    }

    #[test]
    fn auxiliary_popup_target_cannot_replace_dock_context_target() {
        assert_eq!(
            render_target_for_changes(true, Some(RenderTarget::DockContext), true),
            Some(RenderTarget::DockContext.merge(RenderTarget::Popup))
        );
    }

    #[test]
    fn pending_menu_popup_work_promotes_only_the_popup_render_class() {
        assert_eq!(
            promote_menu_popup_target(Some(RenderTarget::DockContext), false),
            Some(RenderTarget::DockContext)
        );
        assert_eq!(
            promote_menu_popup_target(Some(RenderTarget::DockContext), true),
            Some(RenderTarget::DockContext.merge(RenderTarget::Popup))
        );
        assert_eq!(
            promote_menu_popup_target(Some(RenderTarget::Popup), true),
            Some(RenderTarget::Popup)
        );
        let combined = RenderTarget::DockContext.merge(RenderTarget::Popup);
        assert_eq!(
            promote_menu_popup_target(Some(combined), true),
            Some(combined)
        );
    }

    #[test]
    fn global_menu_popup_item_hover_is_popup_scoped() {
        assert_eq!(
            hover_render_target_for(
                Some(&HitTarget::Item(vec![MenuItemId(1)])),
                Some(RenderTarget::DockContext),
            ),
            Some(RenderTarget::Popup)
        );
    }

    #[test]
    fn global_menu_hover_outside_clears_dock_context_without_all_fallback() {
        let semantic_target =
            hover_render_target_for(Some(&HitTarget::Outside), Some(RenderTarget::DockContext));
        assert_eq!(semantic_target, Some(RenderTarget::DockContext));
        assert_eq!(
            render_target_for_changes(true, semantic_target, false),
            Some(RenderTarget::DockContext)
        );
    }

    #[test]
    fn menu_popup_exit_merges_popup_and_dock_context_requirements() {
        let semantic_target =
            hover_render_target_for(Some(&HitTarget::Outside), Some(RenderTarget::DockContext));
        assert_eq!(
            render_target_for_changes(true, semantic_target, true),
            Some(RenderTarget::DockContext.merge(RenderTarget::Popup))
        );
    }

    #[test]
    fn genuine_global_invalidation_remains_all_after_target_merge() {
        assert_eq!(
            merge_render_target(Some(RenderTarget::All), Some(RenderTarget::Popup)),
            Some(RenderTarget::All)
        );
    }
}
