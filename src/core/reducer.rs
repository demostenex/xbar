use super::state::MenuInteractionState;
use super::{Event, MenuItemId, MenuRegistry, MenuSource, MenuState, State};

fn item(model: &super::MenuModel, id: MenuItemId) -> Option<&super::MenuItem> {
    fn walk(node: &super::MenuItem, id: MenuItemId) -> Option<&super::MenuItem> {
        if node.id == id {
            return Some(node);
        }
        node.children.iter().find_map(|child| walk(child, id))
    }
    walk(&model.root, id)
}

fn begin_bluetooth_action(state: &mut State, action: super::BluetoothPendingAction) -> bool {
    if state.bluetooth_pending.contains(&action) {
        false
    } else {
        state.bluetooth_pending.push(action);
        true
    }
}

fn normalize_interaction(state: &mut State) {
    let Some(model) = state.active_menu_model().cloned() else {
        state.menu_interaction = Default::default();
        return;
    };
    let valid = |id| item(&model, id).is_some_and(|i| i.visible && i.enabled);
    if state.menu_interaction.open_root.is_none_or(|id| !valid(id)) {
        state.menu_interaction = Default::default();
        return;
    }
    let root_id = state.menu_interaction.open_root.unwrap();
    let mut path = vec![root_id];
    for id in state.menu_interaction.open_path.iter().copied().skip(1) {
        let Some(parent) = path.last().and_then(|parent_id| item(&model, *parent_id)) else {
            break;
        };
        if parent
            .children
            .iter()
            .any(|child| child.id == id && child.visible && child.enabled)
        {
            path.push(id);
        } else {
            break;
        }
    }
    state.menu_interaction.open_path = path;
    state
        .menu_interaction
        .hovered_path
        .retain(|id| item(&model, *id).is_some_and(|i| i.visible));
    if state.menu_interaction.open_path.first() != state.menu_interaction.open_root.as_ref() {
        state.menu_interaction.open_path.clear();
        state
            .menu_interaction
            .open_path
            .push(state.menu_interaction.open_root.unwrap());
    }
}

fn patch_item(node: &mut super::MenuItem, update: &super::MenuItemPropertiesUpdate) -> bool {
    if node.id == update.item_id {
        let mut changed = false;
        for property in &update.properties {
            changed |= match property {
                super::MenuPropertyUpdate::Label(value) => {
                    if node.label != *value {
                        node.label = value.clone();
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::Enabled(value) => {
                    if node.enabled != *value {
                        node.enabled = *value;
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::Visible(value) => {
                    if node.visible != *value {
                        node.visible = *value;
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::ItemType(value) => {
                    if node.item_type != *value {
                        node.item_type = value.clone();
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::ChildrenDisplay(value) => {
                    if node.children_display != *value {
                        node.children_display = value.clone();
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::Shortcut(value) => {
                    if node.shortcut != *value {
                        node.shortcut = value.clone();
                        true
                    } else {
                        false
                    }
                }
                super::MenuPropertyUpdate::IconName(value) => {
                    if node.icon_name != *value {
                        node.icon_name = value.clone();
                        true
                    } else {
                        false
                    }
                }
            };
        }
        return changed;
    }
    node.children
        .iter_mut()
        .any(|child| patch_item(child, update))
}

pub fn reduce(state: &mut State, event: Event, registry: &mut MenuRegistry) -> bool {
    match event {
        Event::WorkspacesSnapshot(workspaces) => {
            state.focused_workspace = workspaces
                .iter()
                .find(|w| w.focused)
                .map(|w| w.name.clone());
            state.workspaces = workspaces;
            true
        }
        Event::WorkspaceFocused { name } => {
            if state.focused_workspace == name {
                return false;
            }
            if let Some(name) = &name {
                if !state
                    .workspaces
                    .iter()
                    .any(|workspace| &workspace.name == name)
                {
                    let output = state
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.focused)
                        .and_then(|workspace| workspace.output.clone());
                    state.workspaces.push(super::WorkspaceState {
                        name: name.clone(),
                        output,
                        focused: false,
                    });
                }
            }
            state.focused_workspace = name.clone();
            for workspace in &mut state.workspaces {
                workspace.focused = Some(&workspace.name) == name.as_ref();
            }
            true
        }
        Event::WindowFocused(window) => {
            if state.focused_window == window {
                return false;
            }
            state.focused_window = window;
            state.focused_app_name = None;
            state.menu = MenuState::NoMenu;
            state.menu_interaction = Default::default();
            state.global_menu_model = None;
            state.audio_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            state.audio_drag_input = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            true
        }
        Event::WindowFocusedWithApp { window, app_name } => {
            if state.focused_window == window && state.focused_app_name == app_name {
                return false;
            }
            state.focused_window = window;
            state.focused_app_name = app_name;
            state.menu = MenuState::NoMenu;
            state.menu_interaction = Default::default();
            state.global_menu_model = None;
            state.audio_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            true
        }
        Event::MenuRegistered {
            window_id,
            endpoint,
        } => {
            let MenuSource::DbusMenu(endpoint) = endpoint else {
                return false;
            };
            registry.register(window_id, endpoint.service, endpoint.object_path);
            if state.focused_window == Some(window_id) {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.global_menu_model = None;
            }
            true
        }
        Event::GtkMenuDiscovered {
            window_id,
            endpoint,
        } => {
            let changed = registry.gtk(window_id) != Some(&endpoint);
            registry.register_gtk(window_id, endpoint);
            if changed && state.focused_window == Some(window_id) {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.global_menu_model = None;
            }
            changed
        }
        Event::GtkMenuRemoved {
            window_id,
            endpoint,
        } => {
            let removed = registry.remove_gtk_if_matches(window_id, &endpoint);
            if removed
                && state.focused_window == Some(window_id)
                && registry.get(window_id).is_none()
            {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.global_menu_model = None;
            }
            removed
        }
        Event::MenuUnregistered { window_id } => {
            let removed = registry.unregister(window_id).is_some();
            if removed && state.focused_window == Some(window_id) {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.global_menu_model = None;
            }
            removed
        }
        Event::MenuOwnerVanished { sender } => {
            let removed = registry.remove_sender(&sender);
            if state
                .focused_window
                .is_some_and(|window| removed.contains(&window))
            {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.global_menu_model = None;
            }
            !removed.is_empty()
        }
        Event::MenuLoadRequested {
            window_id,
            endpoint,
            request_id,
        } => {
            if (matches!(endpoint, MenuSource::Tray(_)) && window_id == super::WindowId(u32::MAX))
                || (state.focused_window == Some(window_id)
                    && registry.source_matches(window_id, &endpoint))
            {
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                state.audio_drag_input = false;
                state.menu = if let MenuSource::Tray(endpoint) = endpoint {
                    MenuState::TrayLoading {
                        endpoint,
                        request_id,
                    }
                } else {
                    MenuState::Loading {
                        window_id,
                        endpoint,
                        request_id,
                    }
                };
                true
            } else {
                false
            }
        }
        Event::MenuLoaded {
            window_id,
            endpoint,
            request_id,
            model,
        } => {
            let accepted = matches!(&state.menu,
                MenuState::Loading { window_id: w, endpoint: e, request_id: r }
                if *w == window_id && *e == endpoint && *r == request_id
                    && ((matches!(endpoint, MenuSource::Tray(_)) && window_id == super::WindowId(u32::MAX))
                        || (state.focused_window == Some(window_id) && registry.source_matches(window_id, &endpoint))));
            if accepted {
                state.menu = MenuState::Loaded {
                    window_id,
                    endpoint: endpoint.clone(),
                    model,
                };
                if let MenuState::Loaded {
                    window_id,
                    endpoint,
                    model,
                } = &state.menu
                {
                    state.global_menu_model = Some((*window_id, endpoint.clone(), model.clone()));
                }
                if matches!(endpoint, MenuSource::Tray(_)) {
                    state.menu_interaction.open_root = Some(MenuItemId(0));
                    state.menu_interaction.open_path = vec![MenuItemId(0)];
                }
                normalize_interaction(state);
            }
            accepted
        }
        Event::MenuLoadFailed {
            window_id,
            endpoint,
            request_id,
            error,
        } => {
            let accepted = matches!(&state.menu,
                MenuState::Loading { window_id: w, endpoint: e, request_id: r }
                if *w == window_id && *e == endpoint && *r == request_id
                    && ((matches!(endpoint, MenuSource::Tray(_)) && window_id == super::WindowId(u32::MAX))
                        || (state.focused_window == Some(window_id) && registry.source_matches(window_id, &endpoint))));
            if accepted {
                state.menu = MenuState::Error {
                    window_id,
                    endpoint,
                    request_id,
                    error,
                };
                state.menu_interaction = Default::default();
            }
            accepted
        }
        Event::TrayMenuLoaded {
            endpoint,
            request_id,
            model,
        } => {
            if matches!(&state.menu, MenuState::TrayLoading { endpoint: current, request_id: current_id }
                if *current == endpoint && *current_id == request_id)
            {
                state.menu = MenuState::TrayLoaded { endpoint, model };
                state.menu_interaction.open_root = Some(MenuItemId(0));
                state.menu_interaction.open_path = vec![MenuItemId(0)];
                true
            } else {
                false
            }
        }
        Event::TrayMenuLoadFailed {
            endpoint,
            request_id,
            error,
        } => {
            if matches!(&state.menu, MenuState::TrayLoading { endpoint: current, request_id: current_id }
                if *current == endpoint && *current_id == request_id)
            {
                state.menu = MenuState::TrayError {
                    endpoint,
                    request_id,
                    error,
                };
                state.menu_interaction = Default::default();
                true
            } else {
                false
            }
        }
        Event::MenuLayoutInvalidated { .. } => false,
        Event::MenuPropertiesUpdated { endpoint, updates } => {
            if !matches!(&state.menu, MenuState::Loaded { endpoint: current, .. } if current == &endpoint)
                || !state
                    .focused_window
                    .is_some_and(|window| registry.source_matches(window, &endpoint))
            {
                return false;
            }
            let MenuState::Loaded { model, .. } = &mut state.menu else {
                unreachable!()
            };
            let mut changed = false;
            for update in &updates {
                changed |= patch_item(&mut model.root, update);
            }
            if changed {
                normalize_interaction(state);
            }
            changed
        }
        Event::MenuRootClicked(id) => {
            state.audio_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            if matches!(state.menu, MenuState::TrayLoaded { .. }) {
                let Some((window_id, endpoint, model)) = state.global_menu_model.clone() else {
                    state.menu = MenuState::NoMenu;
                    state.menu_interaction = Default::default();
                    return true;
                };
                state.menu = MenuState::Loaded {
                    window_id,
                    endpoint,
                    model,
                };
                state.menu_interaction = MenuInteractionState {
                    open_root: Some(id),
                    open_path: vec![id],
                    ..Default::default()
                };
                normalize_interaction(state);
                return true;
            }
            let Some(model) = state.active_menu_model() else {
                return false;
            };
            let Some(menu_item) = model.root.children.iter().find(|item| item.id == id) else {
                return false;
            };
            if !menu_item.visible || !menu_item.enabled {
                return false;
            }
            if menu_item.children_display.is_none() || menu_item.children.is_empty() {
                if state.menu_interaction.open_root.is_some() {
                    state.menu_interaction = Default::default();
                    return true;
                }
                return false;
            }
            if state.menu_interaction.open_root == Some(id) {
                state.menu_interaction = Default::default();
            } else {
                state.menu_interaction.open_root = Some(id);
                state.menu_interaction.open_path = vec![id];
                state.menu_interaction.hovered_path.clear();
                state.menu_interaction.pending_about_to_show = None;
                state.menu_interaction.about_to_show_item = None;
            }
            true
        }
        Event::MenuItemActivateRequested {
            window_id,
            endpoint,
            item_id,
            timestamp: _,
        } => {
            let valid_context = ((matches!(endpoint, MenuSource::Tray(_))
                && window_id == super::WindowId(u32::MAX))
                || (state.focused_window == Some(window_id)
                    && registry.source_matches(window_id, &endpoint)))
                && (matches!(&state.menu, MenuState::Loaded { window_id: current_window, endpoint: current_endpoint, .. }
                    if *current_window == window_id && *current_endpoint == endpoint)
                    || matches!(&state.menu, MenuState::TrayLoaded { endpoint: current_endpoint, .. }
                        if window_id == super::WindowId(u32::MAX)
                            && MenuSource::Tray(current_endpoint.clone()) == endpoint))
                && state.menu_interaction.open_root.is_some();
            let actionable = state
                .active_menu_model()
                .and_then(|model| item(model, item_id))
                .is_some_and(|menu_item| {
                    menu_item.visible
                        && menu_item.enabled
                        && !matches!(menu_item.item_type, super::MenuItemType::Separator)
                        && menu_item.children_display.is_none()
                });
            if valid_context && actionable {
                state.menu_interaction = Default::default();
                true
            } else {
                false
            }
        }
        Event::MenuItemHovered { path } => {
            if state.menu_interaction.open_root.is_none() {
                return false;
            }
            if state.menu_interaction.hovered_path == path {
                return false;
            }
            state.menu_interaction.hovered_path = path;
            if state.menu_interaction.about_to_show_item
                != state.menu_interaction.hovered_path.last().copied()
            {
                state.menu_interaction.about_to_show_item = None;
            }
            if state
                .menu_interaction
                .pending_about_to_show
                .as_ref()
                .is_some_and(|pending| {
                    state.menu_interaction.hovered_path.last() != Some(&pending.item_id)
                })
            {
                state.menu_interaction.pending_about_to_show = None;
            }
            true
        }
        Event::MenuClickedOutside => {
            if state.menu_interaction.open_root.is_some()
                || state.audio_popup_open
                || state.bluetooth_popup_open
                || state.network_popup_open
            {
                state.menu_interaction = Default::default();
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                state.bluetooth_popup_open = false;
                state.network_popup_open = false;
                true
            } else {
                false
            }
        }
        Event::TrayMenuOpenRequested { .. } => {
            let changed = state.audio_popup_open
                || state.audio_dragging
                || state.bluetooth_popup_open
                || state.network_popup_open;
            state.audio_popup_open = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            changed
        }
        Event::MenuAboutToShowRequested {
            window_id,
            endpoint,
            item_id,
            request_id,
        } => {
            let valid = ((matches!(endpoint, MenuSource::Tray(_))
                && window_id == super::WindowId(u32::MAX))
                || (state.focused_window == Some(window_id)
                    && registry.source_matches(window_id, &endpoint)))
                && (matches!(&state.menu, MenuState::Loaded { endpoint: current, .. } if current == &endpoint)
                    || matches!(&state.menu, MenuState::TrayLoaded { endpoint: current, .. }
                        if MenuSource::Tray(current.clone()) == endpoint))
                && state.menu_interaction.open_root.is_some()
                && state.menu_interaction.hovered_path.last() == Some(&item_id);
            if valid {
                state.menu_interaction.pending_about_to_show = Some(super::AboutToShowPending {
                    window_id,
                    endpoint,
                    item_id,
                    request_id,
                });
                state.menu_interaction.about_to_show_item = Some(item_id);
                true
            } else {
                false
            }
        }
        Event::MenuAboutToShowCompleted {
            window_id,
            endpoint,
            item_id,
            request_id,
            need_update,
            model,
            error,
        } => {
            let accepted = matches!(&state.menu_interaction.pending_about_to_show,
                Some(p) if p.window_id == window_id && p.endpoint == endpoint && p.item_id == item_id && p.request_id == request_id)
                && ((matches!(endpoint, MenuSource::Tray(_))
                    && window_id == super::WindowId(u32::MAX))
                    || (state.focused_window == Some(window_id)
                        && registry.source_matches(window_id, &endpoint)))
                && state.menu_interaction.hovered_path.last() == Some(&item_id);
            if !accepted {
                return false;
            }
            state.menu_interaction.pending_about_to_show = None;
            if error.is_some() {
                return true;
            }
            if need_update {
                if let Some(model) = model {
                    state.menu = if let MenuSource::Tray(tray_endpoint) = endpoint {
                        MenuState::TrayLoaded {
                            endpoint: tray_endpoint,
                            model,
                        }
                    } else {
                        MenuState::Loaded {
                            window_id,
                            endpoint,
                            model,
                        }
                    };
                    normalize_interaction(state);
                } else {
                    return true;
                }
            }
            if state.active_menu_model().is_some_and(|model| {
                item(model, item_id).is_some_and(|item| {
                    item.visible
                        && item.enabled
                        && item.children_display.is_some()
                        && !item.children.is_empty()
                })
            }) {
                let root = state.menu_interaction.open_root;
                let mut path = state.menu_interaction.hovered_path.clone();
                if root.is_some_and(|id| path.first() != Some(&id)) {
                    path.insert(0, root.unwrap());
                }
                state.menu_interaction.open_path = path;
            }
            true
        }
        Event::OutputsChanged(outputs) => {
            if state.outputs == outputs {
                false
            } else {
                state.outputs = outputs;
                true
            }
        }
        Event::ClockUpdated(clock) => {
            if state.clock == Some(clock) {
                false
            } else {
                state.clock = Some(clock);
                true
            }
        }
        Event::AudioSnapshotReceived(audio) => {
            if state.audio == audio {
                false
            } else {
                state.audio = audio;
                true
            }
        }
        Event::AudioInventoryReceived { outputs, inputs } => {
            if state.audio.outputs == outputs && state.audio.inputs == inputs {
                false
            } else {
                state.audio.outputs = outputs;
                state.audio.inputs = inputs;
                true
            }
        }
        Event::AudioSelectOutput(_) | Event::AudioSelectInput(_) => false,
        Event::NetworkSnapshotReceived(network) => {
            let visual_before = network_visual_state(&state.network);
            let visual_after = network_visual_state(&network);
            if state.network == network {
                false
            } else {
                state.network = network;
                visual_before != visual_after || state.network_popup_open
            }
        }
        Event::NetworkPopupToggled => {
            state.network_popup_open = !state.network_popup_open;
            if state.network_popup_open {
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                state.bluetooth_popup_open = false;
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            true
        }
        Event::NetworkSetWireless(enabled) => {
            let action = super::NetworkPendingAction::SetWireless(enabled);
            if state.network_pending.contains(&action) {
                false
            } else {
                state.network_pending.push(action);
                true
            }
        }
        Event::NetworkActionFinished(action) => {
            let before = state.network_pending.len();
            state.network_pending.retain(|pending| pending != &action);
            before != state.network_pending.len()
        }
        Event::BluetoothSnapshotReceived(bluetooth) => {
            let before = bluetooth_visual_state(&state.bluetooth);
            let after = bluetooth_visual_state(&bluetooth);
            if state.bluetooth == bluetooth {
                false
            } else {
                state.bluetooth = bluetooth;
                before != after || state.bluetooth_popup_open
            }
        }
        Event::BluetoothUnavailable => {
            let before = bluetooth_visual_state(&state.bluetooth);
            state.bluetooth = Default::default();
            state.bluetooth_pending.clear();
            state.bluetooth_popup_open = false;
            before != bluetooth_visual_state(&state.bluetooth)
        }
        Event::BluetoothPopupToggled => {
            state.bluetooth_popup_open = !state.bluetooth_popup_open;
            if state.bluetooth_popup_open {
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
                state.network_popup_open = false;
            }
            true
        }
        Event::BluetoothSetPowered(powered) => {
            begin_bluetooth_action(state, super::BluetoothPendingAction::SetPowered(powered))
        }
        Event::BluetoothConnectDevice(path) => {
            begin_bluetooth_action(state, super::BluetoothPendingAction::ConnectDevice(path))
        }
        Event::BluetoothDisconnectDevice(path) => {
            begin_bluetooth_action(state, super::BluetoothPendingAction::DisconnectDevice(path))
        }
        Event::BluetoothActionFinished(action) => {
            let before = state.bluetooth_pending.len();
            state.bluetooth_pending.retain(|pending| pending != &action);
            before != state.bluetooth_pending.len()
        }
        Event::NotificationsSnapshot(notifications) => {
            if state.notifications == notifications {
                false
            } else {
                state.notifications = notifications;
                true
            }
        }
        Event::WindowAttentionChanged { .. } => false,
        Event::AudioUnavailable => {
            let audio = super::AudioState::default();
            let popup_changed = state.audio_popup_open || state.audio_dragging;
            if state.audio == audio && !popup_changed {
                false
            } else {
                state.audio = audio;
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                true
            }
        }
        Event::AudioPopupToggled => {
            state.audio_popup_open = !state.audio_popup_open;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            if state.audio_popup_open {
                state.bluetooth_popup_open = false;
                state.network_popup_open = false;
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            true
        }
        Event::AudioTrackChanged { input, .. } => {
            if !state.audio_popup_open {
                false
            } else {
                state.audio_dragging = true;
                state.audio_drag_input = input;
                true
            }
        }
        Event::AudioDragReleased => {
            let changed = state.audio_dragging;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            changed
        }
        Event::AudioMuteToggled { .. } => state.audio_popup_open,
        Event::StatusNotifierRegistered(endpoint) => {
            state.status_notifiers.register(endpoint);
            false
        }
        Event::StatusNotifierUnregistered(endpoint) => {
            state.status_notifiers.unregister(&endpoint);
            let removed = state.status_notifier_items.remove(&endpoint);
            if matches!(&state.menu, MenuState::TrayLoaded { endpoint: current, .. } | MenuState::TrayLoading { endpoint: current, .. }
                if current.service == endpoint.service)
            {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            removed
        }
        Event::StatusNotifierOwnerVanished(service) => {
            state.status_notifiers.remove_service(&service);
            let removed = state.status_notifier_items.remove_service(&service) > 0;
            if matches!(&state.menu, MenuState::TrayLoaded { endpoint, .. } | MenuState::TrayLoading { endpoint, .. }
                if endpoint.service == service)
            {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            removed
        }
        Event::StatusNotifierItemUpdated(item) => {
            let closes = matches!(&state.menu, MenuState::TrayLoaded { endpoint, .. } | MenuState::TrayLoading { endpoint, .. }
                if endpoint.service == item.endpoint.service && endpoint.object_path != item.menu.as_ref().map(|menu| menu.object_path.clone()).unwrap_or_default());
            let changed = state.status_notifier_items.upsert(item);
            if closes {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            changed || closes
        }
        Event::StatusNotifierHostRegistered => {
            if state.status_notifier_host_registered {
                false
            } else {
                state.status_notifier_host_registered = true;
                false
            }
        }
        Event::StatusNotifierActionRequested { .. } => false,
        Event::X11(crate::platform::x11::X11Event::RandrChanged) => true,
        Event::X11(crate::platform::x11::X11Event::Expose(_)) => true,
        Event::X11(crate::platform::x11::X11Event::ButtonPress { .. })
        | Event::X11(crate::platform::x11::X11Event::ButtonRelease { .. })
        | Event::X11(crate::platform::x11::X11Event::MotionNotify { .. }) => false,
        Event::X11(crate::platform::x11::X11Event::GtkWindowChanged(_))
        | Event::X11(crate::platform::x11::X11Event::GtkWindowsChanged)
        | Event::X11(crate::platform::x11::X11Event::GtkWindowDestroyed(_))
        | Event::X11(crate::platform::x11::X11Event::InstanceLost) => false,
        Event::X11(crate::platform::x11::X11Event::Close) => false,
        Event::X11(crate::platform::x11::X11Event::WindowAttentionChanged { .. }) => false,
    }
}

fn network_visual_state(network: &super::NetworkState) -> (bool, u8) {
    if !network.available
        || matches!(
            network.connectivity,
            super::NetworkConnectivity::Disconnected
        )
    {
        return (false, 0);
    }
    if matches!(network.link_kind, super::NetworkLinkKind::Ethernet) {
        return (true, 4);
    }
    let band = match network.signal_percent.unwrap_or(0) {
        0..=33 => 1,
        34..=66 => 2,
        _ => 3,
    };
    (true, band)
}

fn bluetooth_visual_state(bluetooth: &super::BluetoothState) -> u8 {
    if !bluetooth.available {
        0
    } else if !bluetooth.powered {
        1
    } else if bluetooth.devices.iter().any(|device| device.connected) {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{MenuEndpoint, OutputId, OutputState, WindowId, WorkspaceState};
    fn ep() -> super::super::MenuEndpoint {
        super::super::MenuEndpoint {
            service: ":1.9".into(),
            object_path: "/menu".into(),
        }
    }
    fn model() -> super::super::MenuModel {
        super::super::MenuModel {
            revision: 1,
            root: super::super::MenuItem {
                id: super::super::MenuItemId(0),
                label: None,
                enabled: true,
                visible: true,
                item_type: super::super::MenuItemType::Standard,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![],
            },
        }
    }
    fn ws(name: &str, focused: bool) -> WorkspaceState {
        WorkspaceState {
            name: name.into(),
            output: Some("HDMI-1".into()),
            focused,
        }
    }

    #[test]
    fn network_signal_within_same_band_does_not_dirty() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        state.network = super::super::NetworkState {
            available: true,
            connectivity: super::super::NetworkConnectivity::Connected,
            link_kind: super::super::NetworkLinkKind::Wifi,
            signal_percent: Some(80),
            ..Default::default()
        };
        let mut updated = state.network.clone();
        updated.signal_percent = Some(75);
        assert!(!reduce(
            &mut state,
            Event::NetworkSnapshotReceived(updated),
            &mut registry,
        ));
        assert_eq!(state.network.signal_percent, Some(75));
    }

    #[test]
    fn bluetooth_visual_states_and_deduplication() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let adapter = |powered| super::super::BluetoothState {
            available: true,
            powered,
            devices: Vec::new(),
        };
        assert!(!reduce(
            &mut state,
            Event::BluetoothSnapshotReceived(Default::default()),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::BluetoothSnapshotReceived(adapter(false)),
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::BluetoothSnapshotReceived(adapter(false)),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::BluetoothSnapshotReceived(adapter(true)),
            &mut registry
        ));
        let connected = super::super::BluetoothState {
            available: true,
            powered: true,
            devices: vec![super::super::BluetoothDevice {
                path: "/org/bluez/hci0/dev_C01".into(),
                address: "55:FB:BA:A6:E7:D2".into(),
                alias: "C01".into(),
                name: "C01".into(),
                paired: true,
                trusted: true,
                connected: true,
            }],
        };
        assert!(reduce(
            &mut state,
            Event::BluetoothSnapshotReceived(connected),
            &mut registry
        ));
        assert!(state.bluetooth.devices[0].connected);
        assert!(reduce(
            &mut state,
            Event::BluetoothUnavailable,
            &mut registry
        ));
    }

    #[test]
    fn bluetooth_popup_is_exclusive_and_commands_are_not_optimistic() {
        let mut state = State {
            audio_popup_open: true,
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::BluetoothPopupToggled,
            &mut registry
        ));
        assert!(state.bluetooth_popup_open);
        assert!(!state.audio_popup_open);
        assert!(reduce(
            &mut state,
            Event::BluetoothDisconnectDevice("/org/bluez/hci0/dev_C01".into()),
            &mut registry
        ));
        assert_eq!(state.bluetooth_pending.len(), 1);
        assert!(state.bluetooth_popup_open);
        assert!(reduce(&mut state, Event::MenuClickedOutside, &mut registry));
        assert!(!state.bluetooth_popup_open);
    }

    #[test]
    fn bluetooth_pending_action_blocks_duplicates_without_changing_backend_state() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let path = "/org/bluez/hci0/dev_C01".to_owned();
        assert!(reduce(
            &mut state,
            Event::BluetoothConnectDevice(path.clone()),
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::BluetoothConnectDevice(path.clone()),
            &mut registry
        ));
        assert!(!state
            .bluetooth
            .devices
            .iter()
            .any(|device| device.connected));
        assert!(reduce(
            &mut state,
            Event::BluetoothActionFinished(super::super::BluetoothPendingAction::ConnectDevice(
                path
            ),),
            &mut registry
        ));
        assert!(state.bluetooth_pending.is_empty());
    }

    #[test]
    fn network_popup_is_exclusive_and_wireless_pending_is_not_authoritative() {
        let mut state = State {
            audio_popup_open: true,
            bluetooth_popup_open: true,
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::NetworkPopupToggled,
            &mut registry
        ));
        assert!(state.network_popup_open);
        assert!(!state.audio_popup_open);
        assert!(!state.bluetooth_popup_open);

        state.network.wireless_enabled = true;
        assert!(reduce(
            &mut state,
            Event::NetworkSetWireless(false),
            &mut registry
        ));
        assert_eq!(state.network_pending.len(), 1);
        assert!(state.network.wireless_enabled);
        assert!(!reduce(
            &mut state,
            Event::NetworkSetWireless(false),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::NetworkActionFinished(super::super::NetworkPendingAction::SetWireless(false)),
            &mut registry
        ));
        assert!(state.network_pending.is_empty());
    }

    #[test]
    fn every_interactive_popup_transition_leaves_one_owner() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let count = |state: &State| {
            [
                state.network_popup_open,
                state.bluetooth_popup_open,
                state.audio_popup_open,
            ]
            .into_iter()
            .filter(|open| *open)
            .count()
        };

        reduce(&mut state, Event::NetworkPopupToggled, &mut registry);
        assert_eq!(count(&state), 1);
        reduce(&mut state, Event::BluetoothPopupToggled, &mut registry);
        assert_eq!(count(&state), 1);
        reduce(&mut state, Event::AudioPopupToggled, &mut registry);
        assert_eq!(count(&state), 1);
        reduce(&mut state, Event::NetworkPopupToggled, &mut registry);
        assert_eq!(count(&state), 1);
        reduce(&mut state, Event::NetworkPopupToggled, &mut registry);
        assert_eq!(count(&state), 0);
    }

    #[test]
    fn bluetooth_unavailable_closes_popup_but_power_off_keeps_slot_state() {
        let mut state = State {
            bluetooth_popup_open: true,
            bluetooth: super::super::BluetoothState {
                available: true,
                powered: false,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::BluetoothUnavailable,
            &mut registry
        ));
        assert!(!state.bluetooth_popup_open);
        assert!(!state.bluetooth.available);
    }

    #[test]
    fn audio_drag_release_clears_drag_without_closing_popup() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        state.audio_popup_open = true;
        state.audio_dragging = true;
        state.audio_drag_input = true;
        assert!(reduce(&mut state, Event::AudioDragReleased, &mut registry,));
        assert!(!state.audio_dragging);
        assert!(!state.audio_drag_input);
        assert!(state.audio_popup_open);
        assert!(!reduce(&mut state, Event::AudioDragReleased, &mut registry,));
    }

    #[test]
    fn audio_track_action_records_input_drag_kind() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        state.audio_popup_open = true;
        assert!(reduce(
            &mut state,
            Event::AudioTrackChanged {
                input: true,
                percent: 60,
            },
            &mut registry,
        ));
        assert!(state.audio_dragging);
        assert!(state.audio_drag_input);
    }

    #[test]
    fn audio_mute_action_keeps_popup_open() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        state.audio_popup_open = true;
        assert!(reduce(
            &mut state,
            Event::AudioMuteToggled { input: false },
            &mut registry,
        ));
        assert!(state.audio_popup_open);
    }

    #[test]
    fn snapshot_sets_focus() {
        let mut s = State::default();
        assert!(reduce(
            &mut s,
            Event::WorkspacesSnapshot(vec![ws("1", true), ws("2", false)]),
            &mut MenuRegistry::default()
        ));
        assert_eq!(s.focused_workspace, Some("1".into()));
    }
    #[test]
    fn workspace_focus_changes() {
        let mut s = State {
            workspaces: vec![ws("1", true), ws("2", false)],
            ..Default::default()
        };
        reduce(
            &mut s,
            Event::WorkspaceFocused {
                name: Some("2".into()),
            },
            &mut MenuRegistry::default(),
        );
        assert_eq!(s.focused_workspace, Some("2".into()));
        assert!(s.workspaces[1].focused);
    }

    #[test]
    fn workspace_focus_adds_new_workspace_and_converges_back() {
        let mut s = State {
            workspaces: vec![ws("1", true)],
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();

        reduce(
            &mut s,
            Event::WorkspaceFocused {
                name: Some("2".into()),
            },
            &mut registry,
        );
        assert_eq!(s.focused_workspace, Some("2".into()));
        assert!(!s.workspaces[0].focused);
        assert!(s.workspaces[1].focused);

        reduce(
            &mut s,
            Event::WorkspaceFocused {
                name: Some("1".into()),
            },
            &mut registry,
        );
        assert_eq!(s.focused_workspace, Some("1".into()));
        assert!(s.workspaces[0].focused);
        assert!(!s.workspaces[1].focused);
    }

    #[test]
    fn audio_snapshot_updates_and_deduplicates() {
        let mut s = State::default();
        let audio = super::super::AudioState {
            available: true,
            default_output: Some("auto_null".into()),
            volume_percent: 42,
            muted: false,
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut s,
            Event::AudioSnapshotReceived(audio.clone()),
            &mut registry
        ));
        assert_eq!(s.audio, audio);
        assert!(!reduce(
            &mut s,
            Event::AudioSnapshotReceived(audio),
            &mut registry
        ));
    }

    #[test]
    fn audio_default_output_replacement_is_atomic() {
        let mut s = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut s,
            Event::AudioSnapshotReceived(super::super::AudioState {
                available: true,
                default_output: Some("speakers".into()),
                volume_percent: 80,
                muted: false,
                ..Default::default()
            }),
            &mut registry,
        );
        reduce(
            &mut s,
            Event::AudioSnapshotReceived(super::super::AudioState {
                available: true,
                default_output: Some("headset".into()),
                volume_percent: 25,
                muted: true,
                ..Default::default()
            }),
            &mut registry,
        );
        assert_eq!(s.audio.default_output.as_deref(), Some("headset"));
        assert_eq!(s.audio.volume_percent, 25);
        assert!(s.audio.muted);
    }

    #[test]
    fn audio_inventory_is_filtered_by_domain_and_deduplicated() {
        let mut state = State::default();
        let devices = vec![super::super::AudioDevice {
            name: "sink.a".into(),
            display_name: "Speakers".into(),
        }];
        let inputs = vec![super::super::AudioDevice {
            name: "source.a".into(),
            display_name: "Microphone".into(),
        }];
        let event = Event::AudioInventoryReceived {
            outputs: devices.clone(),
            inputs: inputs.clone(),
        };
        assert!(reduce(
            &mut state,
            event.clone(),
            &mut MenuRegistry::default()
        ));
        assert_eq!(state.audio.outputs, devices);
        assert_eq!(state.audio.inputs, inputs);
        assert!(!reduce(&mut state, event, &mut MenuRegistry::default()));
    }

    #[test]
    fn focused_window_xid_changes() {
        let mut s = State::default();
        reduce(
            &mut s,
            Event::WindowFocused(Some(WindowId(10))),
            &mut MenuRegistry::default(),
        );
        assert_eq!(s.focused_window, Some(WindowId(10)));
        reduce(
            &mut s,
            Event::WindowFocused(Some(WindowId(20))),
            &mut MenuRegistry::default(),
        );
        assert_eq!(s.focused_window, Some(WindowId(20)));
    }

    #[test]
    fn focused_application_changes_without_touching_workspace_or_clock() {
        let mut state = State {
            focused_workspace: Some("1".into()),
            workspaces: vec![ws("1", true)],
            clock: Some(super::super::ClockState {
                hour: 12,
                minute: 1,
                day: 1,
                month: 9,
            }),
            ..Default::default()
        };
        let before = (state.workspaces.clone(), state.clock);
        assert!(reduce(
            &mut state,
            Event::WindowFocusedWithApp {
                window: Some(WindowId(10)),
                app_name: Some("Alacritty".into()),
            },
            &mut MenuRegistry::default(),
        ));
        assert_eq!(state.focused_app_name, Some("Alacritty".into()));
        assert_eq!((state.workspaces, state.clock), before);
    }

    #[test]
    fn tray_menu_load_uses_endpoint_and_closes_on_owner_vanish() {
        let mut s = State::default();
        let source = MenuSource::Tray(ep());
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut s,
            Event::MenuLoadRequested {
                window_id: WindowId(u32::MAX),
                endpoint: source,
                request_id: 1,
            },
            &mut registry,
        ));
        assert!(reduce(
            &mut s,
            Event::TrayMenuLoaded {
                endpoint: ep(),
                request_id: 1,
                model: model(),
            },
            &mut registry,
        ));
        assert_eq!(s.menu_interaction.open_root, Some(MenuItemId(0)));
        assert!(
            reduce(
                &mut s,
                Event::StatusNotifierOwnerVanished(":1.9".into()),
                &mut registry,
            ) || matches!(s.menu, MenuState::NoMenu)
        );
        assert!(matches!(s.menu, MenuState::NoMenu));
    }

    #[test]
    fn tray_and_global_menu_owners_are_mutually_exclusive() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let tray = ep();
        registry.register(WindowId(7), ":1.7".into(), "/global-menu".into());
        state.focused_window = Some(WindowId(7));
        let global_endpoint = MenuEndpoint {
            service: ":1.7".into(),
            object_path: "/global-menu".into(),
        };
        state.global_menu_model = Some((
            WindowId(7),
            MenuSource::DbusMenu(global_endpoint.clone()),
            interactive_model(),
        ));
        state.menu = MenuState::TrayLoaded {
            endpoint: tray.clone(),
            model: model(),
        };
        state.menu_interaction.open_root = Some(MenuItemId(0));
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(matches!(state.menu, MenuState::Loaded { .. }));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
    }

    #[test]
    fn tray_menu_lifecycle_clears_interaction_without_unregistering_item() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let item_endpoint = super::super::StatusNotifierEndpoint {
            service: ":1.9".into(),
            object_path: "/StatusNotifierItem".into(),
        };
        let menu_endpoint = ep();
        state
            .status_notifier_items
            .upsert(super::super::StatusNotifierItem {
                endpoint: item_endpoint.clone(),
                status: super::super::StatusNotifierStatus::Active,
                icon: None,
                item_is_menu: false,
                menu: Some(menu_endpoint.clone()),
            });
        state.menu = MenuState::TrayLoaded {
            endpoint: menu_endpoint.clone(),
            model: model(),
        };
        state.menu_interaction.open_root = Some(MenuItemId(0));
        assert!(reduce(
            &mut state,
            Event::StatusNotifierItemUpdated(super::super::StatusNotifierItem {
                endpoint: item_endpoint.clone(),
                status: super::super::StatusNotifierStatus::Active,
                icon: None,
                item_is_menu: false,
                menu: None,
            }),
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::NoMenu));
        assert!(state.menu_interaction.open_root.is_none());
        assert_eq!(state.status_notifier_items.items().len(), 1);

        state.menu = MenuState::TrayLoaded {
            endpoint: MenuEndpoint {
                service: ":1.9".into(),
                object_path: "/MenuA".into(),
            },
            model: model(),
        };
        state.menu_interaction.open_root = Some(MenuItemId(0));
        assert!(reduce(
            &mut state,
            Event::StatusNotifierItemUpdated(super::super::StatusNotifierItem {
                endpoint: item_endpoint,
                status: super::super::StatusNotifierStatus::Active,
                icon: None,
                item_is_menu: false,
                menu: Some(MenuEndpoint {
                    service: ":1.9".into(),
                    object_path: "/MenuB".into()
                }),
            }),
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::NoMenu));
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn tray_scroll_action_does_not_dirty_state() {
        let mut state = State::default();
        assert!(!reduce(
            &mut state,
            Event::StatusNotifierActionRequested {
                endpoint: super::super::StatusNotifierEndpoint {
                    service: ":1.9".into(),
                    object_path: "/StatusNotifierItem".into(),
                },
                action: super::super::StatusNotifierAction::Scroll {
                    delta: 1,
                    orientation: "vertical",
                },
                root_x: 100,
                root_y: 12,
            },
            &mut MenuRegistry::default(),
        ));
    }
    #[test]
    fn outputs_update() {
        let mut s = State::default();
        let o = OutputState {
            id: OutputId(1),
            name: "HDMI-1".into(),
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(reduce(
            &mut s,
            Event::OutputsChanged(vec![o.clone()]),
            &mut MenuRegistry::default()
        ));
        assert_eq!(s.outputs, vec![o]);
    }
    #[test]
    fn irrelevant_duplicate_does_not_dirty() {
        let mut s = State::default();
        assert!(!reduce(
            &mut s,
            Event::WindowFocused(None),
            &mut MenuRegistry::default()
        ));
    }

    #[test]
    fn identical_clock_update_does_not_dirty() {
        let clock = super::super::ClockState {
            hour: 18,
            minute: 42,
            day: 31,
            month: 8,
        };
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::ClockUpdated(clock),
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::ClockUpdated(clock),
            &mut registry
        ));
        assert_eq!(state.clock, Some(clock));
    }

    #[test]
    fn menu_registration_and_focus_resolve_endpoint() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(42),
                endpoint: MenuSource::DbusMenu(super::super::MenuEndpoint {
                    service: ":1.42".into(),
                    object_path: "/com/example/Menu".into(),
                }),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(42))),
            &mut registry,
        );
        assert_eq!(
            state.active_menu_endpoint(&registry),
            Some(MenuSource::DbusMenu(super::super::MenuEndpoint {
                service: ":1.42".into(),
                object_path: "/com/example/Menu".into(),
            }))
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(43))),
            &mut registry,
        );
        assert_eq!(state.active_menu_endpoint(&registry), None);
    }

    #[test]
    fn owner_vanishing_removes_registrations() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(42),
                endpoint: MenuSource::DbusMenu(super::super::MenuEndpoint {
                    service: ":1.42".into(),
                    object_path: "/com/example/Menu".into(),
                }),
            },
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::MenuOwnerVanished {
                sender: ":1.42".into()
            },
            &mut registry,
        ));
        assert_eq!(registry.get(WindowId(42)), None);
    }

    #[test]
    fn menu_load_lifecycle_and_stale_response() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 2
            },
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 1,
                model: model()
            },
            &mut registry
        ));
        assert!(matches!(
            state.menu,
            super::super::MenuState::Loading { request_id: 2, .. }
        ));
        assert!(reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 2,
                model: model()
            },
            &mut registry
        ));
        assert!(matches!(state.menu, super::super::MenuState::Loaded { .. }));
        reduce(&mut state, Event::WindowFocused(None), &mut registry);
        assert!(matches!(state.menu, super::super::MenuState::NoMenu));
    }

    #[test]
    fn unregister_and_owner_vanished_clear_active_model() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 1,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 1,
                model: model(),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuOwnerVanished {
                sender: ":1.9".into(),
            },
            &mut registry,
        );
        assert!(matches!(state.menu, super::super::MenuState::NoMenu));
    }

    #[test]
    fn stale_response_from_window_a_is_ignored_after_focus_moves_to_b() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let endpoint_a = ep();
        let endpoint_b = super::super::MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/menu-b".into(),
        };
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(endpoint_a.clone()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(8),
                endpoint: MenuSource::DbusMenu(endpoint_b.clone()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(endpoint_a.clone()),
                request_id: 10,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(8),
                endpoint: MenuSource::DbusMenu(endpoint_b.clone()),
                request_id: 11,
            },
            &mut registry,
        );
        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(endpoint_a),
                request_id: 10,
                model: model(),
            },
            &mut registry,
        ));
        assert!(matches!(
            state.menu,
            super::super::MenuState::Loading {
                window_id: WindowId(8),
                request_id: 11,
                ..
            }
        ));
        assert!(reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(8),
                endpoint: MenuSource::DbusMenu(endpoint_b),
                request_id: 11,
                model: model(),
            },
            &mut registry,
        ));
    }

    #[test]
    fn load_failure_and_stale_signals_do_not_corrupt_active_menu() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 1,
            },
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::MenuLoadFailed {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 1,
                error: "gone".into()
            },
            &mut registry
        ));
        assert!(matches!(state.menu, super::super::MenuState::Error { .. }));
        let other = super::super::MenuEndpoint {
            service: ":1.11".into(),
            object_path: "/other".into(),
        };
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(8),
                endpoint: MenuSource::DbusMenu(other.clone()),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry,
        );
        assert!(!reduce(
            &mut state,
            Event::MenuLayoutInvalidated {
                endpoint: MenuSource::DbusMenu(ep()),
                revision: None,
            },
            &mut registry,
        ));
        assert!(matches!(state.menu, super::super::MenuState::NoMenu));
    }

    fn interactive_model() -> super::super::MenuModel {
        let child = super::super::MenuItem {
            id: MenuItemId(2),
            label: Some("Recentes".into()),
            enabled: true,
            visible: true,
            item_type: super::super::MenuItemType::Standard,
            children_display: Some(super::super::ChildrenDisplay::Submenu),
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![super::super::MenuItem {
                id: MenuItemId(3),
                label: Some("a".into()),
                enabled: true,
                visible: true,
                item_type: super::super::MenuItemType::Standard,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![],
            }],
        };
        super::super::MenuModel {
            revision: 1,
            root: super::super::MenuItem {
                id: MenuItemId(0),
                label: None,
                enabled: true,
                visible: true,
                item_type: super::super::MenuItemType::Standard,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![super::super::MenuItem {
                    id: MenuItemId(1),
                    label: Some("Arquivo".into()),
                    enabled: true,
                    visible: true,
                    item_type: super::super::MenuItemType::Standard,
                    children_display: Some(super::super::ChildrenDisplay::Submenu),
                    shortcut: None,
                    icon_name: None,
                    action: None,
                    children: vec![child],
                }],
            },
        }
    }

    #[test]
    fn interaction_opens_toggles_and_rejects_stale_about_to_show() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
        state.focused_window = Some(WindowId(7));
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);
        assert!(reduce(
            &mut state,
            Event::MenuItemHovered {
                path: vec![MenuItemId(1), MenuItemId(2)]
            },
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuItemHovered {
                path: vec![MenuItemId(1), MenuItemId(2)]
            },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 41
            },
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 40,
                need_update: false,
                model: None,
                error: None
            },
            &mut registry
        ));
        assert!(state.menu_interaction.open_path == vec![MenuItemId(1)]);
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 41,
                need_update: false,
                model: None,
                error: None
            },
            &mut registry
        ));
        assert_eq!(
            state.menu_interaction.open_path,
            vec![MenuItemId(1), MenuItemId(2)]
        );
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(state.menu_interaction.open_root.is_none());
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(reduce(&mut state, Event::MenuClickedOutside, &mut registry));
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn empty_dynamic_submenu_requests_about_to_show_and_only_opens_after_children_arrive() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
        state.focused_window = Some(WindowId(7));
        let mut initial = interactive_model();
        initial.root.children[0].children[0].children.clear();
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: initial,
        };
        reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuItemHovered {
                path: vec![MenuItemId(1), MenuItemId(2)],
            },
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 50,
            },
            &mut registry,
        ));
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 50,
                need_update: false,
                model: None,
                error: None,
            },
            &mut registry,
        ));
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);

        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 51,
            },
            &mut registry,
        ));
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 51,
                need_update: true,
                model: Some(interactive_model()),
                error: None,
            },
            &mut registry,
        ));
        assert_eq!(
            state.menu_interaction.open_path,
            vec![MenuItemId(1), MenuItemId(2)]
        );
    }

    #[test]
    fn lifecycle_close_events_converge_to_empty_interaction() {
        fn open_state() -> (super::super::State, super::super::MenuRegistry) {
            let mut state = State::default();
            let mut registry = MenuRegistry::default();
            registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
            state.focused_window = Some(WindowId(7));
            state.menu = MenuState::Loaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                model: interactive_model(),
            };
            assert!(reduce(
                &mut state,
                Event::MenuRootClicked(MenuItemId(1)),
                &mut registry
            ));
            state.menu_interaction.open_path = vec![MenuItemId(1), MenuItemId(2)];
            state.menu_interaction.hovered_path = vec![MenuItemId(1), MenuItemId(2)];
            (state, registry)
        }

        let close_events = [
            Event::MenuClickedOutside,
            Event::WindowFocused(Some(WindowId(8))),
            Event::MenuUnregistered {
                window_id: WindowId(7),
            },
            Event::MenuOwnerVanished {
                sender: ":1.9".into(),
            },
        ];
        for event in close_events {
            let (mut state, mut registry) = open_state();
            assert!(reduce(&mut state, event, &mut registry));
            assert_eq!(state.menu_interaction, Default::default());
        }

        let (mut state, mut registry) = open_state();
        state.menu = MenuState::Loading {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            request_id: 99,
        };
        assert!(reduce(
            &mut state,
            Event::MenuLoadFailed {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                request_id: 99,
                error: "gone".into(),
            },
            &mut registry,
        ));
        assert_eq!(state.menu_interaction, Default::default());
    }

    #[test]
    fn activation_accepts_leaf_and_rejects_disabled_separator_submenu_and_wrong_endpoint() {
        fn open_state(model: super::super::MenuModel) -> (State, MenuRegistry) {
            let mut state = State::default();
            let mut registry = MenuRegistry::default();
            registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
            state.focused_window = Some(WindowId(7));
            state.menu = MenuState::Loaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                model,
            };
            assert!(reduce(
                &mut state,
                Event::MenuRootClicked(MenuItemId(1)),
                &mut registry
            ));
            (state, registry)
        }

        let (mut state, mut registry) = open_state(interactive_model());
        assert!(reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(3),
                timestamp: 123,
            },
            &mut registry
        ));
        assert_eq!(state.menu_interaction, Default::default());

        let mut disabled = interactive_model();
        disabled.root.children[0]
            .children
            .push(super::super::MenuItem {
                id: MenuItemId(4),
                label: Some("Disabled".into()),
                enabled: false,
                visible: true,
                item_type: super::super::MenuItemType::Standard,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![],
            });
        let (mut state, mut registry) = open_state(disabled);
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(4),
                timestamp: 123,
            },
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));

        let mut separator = interactive_model();
        separator.root.children[0]
            .children
            .push(super::super::MenuItem {
                id: MenuItemId(5),
                label: None,
                enabled: true,
                visible: true,
                item_type: super::super::MenuItemType::Separator,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![],
            });
        let (mut state, mut registry) = open_state(separator);
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(5),
                timestamp: 123,
            },
            &mut registry
        ));

        let (mut state, mut registry) = open_state(interactive_model());
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                timestamp: 123,
            },
            &mut registry
        ));
        let wrong_endpoint = super::super::MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/other".into(),
        };
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(wrong_endpoint),
                item_id: MenuItemId(3),
                timestamp: 123,
            },
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));

        let mut invisible = interactive_model();
        invisible.root.children[0]
            .children
            .push(super::super::MenuItem {
                id: MenuItemId(6),
                label: Some("Invisible".into()),
                enabled: true,
                visible: false,
                item_type: super::super::MenuItemType::Standard,
                children_display: None,
                shortcut: None,
                icon_name: None,
                action: None,
                children: vec![],
            });
        let (mut state, mut registry) = open_state(invisible);
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(6),
                timestamp: 123,
            },
            &mut registry
        ));

        let mut empty_submenu = interactive_model();
        empty_submenu.root.children[0].children[0].children.clear();
        let (mut state, mut registry) = open_state(empty_submenu);
        assert!(!reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                timestamp: 123,
            },
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
    }

    #[test]
    fn property_updates_patch_by_id_and_reconcile_open_path() {
        let (mut state, mut registry) = {
            let mut state = State::default();
            let mut registry = MenuRegistry::default();
            registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
            state.focused_window = Some(WindowId(7));
            state.menu = MenuState::Loaded {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                model: interactive_model(),
            };
            reduce(
                &mut state,
                Event::MenuRootClicked(MenuItemId(1)),
                &mut registry,
            );
            state.menu_interaction.open_path = vec![MenuItemId(1), MenuItemId(2)];
            state.menu_interaction.hovered_path = vec![MenuItemId(1), MenuItemId(2)];
            (state, registry)
        };

        assert!(reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(ep()),
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(2),
                    properties: vec![super::super::MenuPropertyUpdate::Enabled(false)],
                }],
            },
            &mut registry,
        ));
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);

        assert!(reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(ep()),
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(2),
                    properties: vec![super::super::MenuPropertyUpdate::Label(Some(
                        "Recentes…".into(),
                    ))],
                }],
            },
            &mut registry,
        ));
        assert_eq!(
            state.active_menu_model().unwrap().root.children[0].children[0].label,
            Some("Recentes…".into())
        );
        assert!(!reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(ep()),
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(2),
                    properties: vec![super::super::MenuPropertyUpdate::Label(Some(
                        "Recentes…".into(),
                    ))],
                }],
            },
            &mut registry,
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(ep()),
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(999),
                    properties: vec![super::super::MenuPropertyUpdate::Enabled(false)],
                }],
            },
            &mut registry,
        ));
    }

    #[test]
    fn removed_properties_restore_defaults() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
        state.focused_window = Some(WindowId(7));
        let mut model = interactive_model();
        model.root.children[0].children[0].enabled = false;
        model.root.children[0].children[0].visible = false;
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model,
        };
        assert!(reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(ep()),
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(2),
                    properties: vec![
                        super::super::MenuPropertyUpdate::Enabled(true),
                        super::super::MenuPropertyUpdate::Visible(true),
                    ],
                }],
            },
            &mut registry,
        ));
        let item = &state.active_menu_model().unwrap().root.children[0].children[0];
        assert!(item.enabled && item.visible);
    }
}
