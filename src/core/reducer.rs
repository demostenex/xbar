use super::state::{
    KeyboardGrabState, LazyRootOpenPending, MenuInteractionState, MenuNavigationSession,
    MenuPresentation, MenuPresentationPolicy,
};
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

fn selectable(item: &super::MenuItem) -> bool {
    item.visible && item.enabled && !matches!(item.item_type, super::MenuItemType::Separator)
}

fn selectable_children(item: &super::MenuItem) -> Vec<MenuItemId> {
    item.children
        .iter()
        .filter(|child| selectable(child))
        .map(|child| child.id)
        .collect()
}

fn source_presentation(state: &State) -> Option<(super::WindowId, MenuSource)> {
    state
        .menu_presentation
        .as_ref()
        .map(|presentation| (presentation.window_id, presentation.endpoint.clone()))
}

fn current_presentation_model(state: &State) -> Option<&super::MenuModel> {
    let presentation = state.menu_presentation.as_ref()?;
    match &state.menu {
        MenuState::Loaded {
            window_id,
            endpoint,
            model,
        } if *window_id == presentation.window_id && *endpoint == presentation.endpoint => {
            Some(model)
        }
        _ => None,
    }
}

fn eligible_root(model: &super::MenuModel, id: MenuItemId) -> bool {
    model.root.children.iter().any(|item| {
        item.id == id
            && selectable(item)
            && item.children_display == Some(super::ChildrenDisplay::Submenu)
    })
}

fn first_eligible_root(state: &State) -> Option<MenuItemId> {
    current_presentation_model(state)?
        .root
        .children
        .iter()
        .find(|item| {
            selectable(item) && item.children_display == Some(super::ChildrenDisplay::Submenu)
        })
        .map(|item| item.id)
}

fn current_pending_lazy_root_matches(state: &State, item_id: MenuItemId) -> bool {
    let Some(pending) = state.menu_interaction.pending_lazy_root.as_ref() else {
        return false;
    };
    pending.item_id == item_id
        && matches!(
            &state.menu,
            MenuState::Loaded {
                window_id,
                endpoint,
                ..
            } if *window_id == pending.window_id && *endpoint == pending.endpoint
        )
        && state.watcher_generations.get(&pending.endpoint) == Some(&pending.watcher_generation)
}

fn start_navigation_session(state: &mut State, root: MenuItemId) -> bool {
    let Some((source_window, endpoint)) = source_presentation(state) else {
        return false;
    };
    if !current_presentation_model(state).is_some_and(|model| eligible_root(model, root)) {
        return false;
    }
    state.next_menu_navigation_session = state.next_menu_navigation_session.wrapping_add(1);
    let id = state.next_menu_navigation_session;
    state.menu_navigation = Some(MenuNavigationSession {
        id,
        source_window,
        endpoint,
        selected_path: Some(vec![root]),
        grab_state: KeyboardGrabState::Requested,
    });
    true
}

fn teardown_navigation_waiting_for_lazy_root(
    state: &mut State,
    registry: &MenuRegistry,
    item_id: MenuItemId,
) {
    let waiting_for_item = state.menu_navigation.as_ref().is_some_and(|session| {
        session.selected_path.as_ref().and_then(|path| path.last()) == Some(&item_id)
    });
    if waiting_for_item && state.menu_interaction.open_root.is_none() {
        teardown_navigation(state, registry);
    }
}

fn teardown_navigation(state: &mut State, registry: &MenuRegistry) {
    state.menu_navigation = None;
    if matches!(
        state.menu_presentation_policy,
        MenuPresentationPolicy::Pinned { .. }
    ) {
        state.menu_interaction = Default::default();
        return;
    }
    let still_current = state
        .focused_window
        .zip(state.menu_presentation.as_ref())
        .is_some_and(|(window, presentation)| {
            window == presentation.window_id
                && registry.active(Some(window)).as_ref() == Some(&presentation.endpoint)
        });
    if !still_current {
        reconcile_menu_presentation_to_focus(state, registry);
    } else {
        state.menu_interaction = Default::default();
    }
}

fn normalize_keyboard_selection(state: &mut State) {
    let Some(model) = state.active_menu_model().cloned() else {
        if let Some(session) = &mut state.menu_navigation {
            session.selected_path = None;
        }
        return;
    };
    let Some(session) = &mut state.menu_navigation else {
        return;
    };
    let valid = session.selected_path.as_ref().is_some_and(|path| {
        !path.is_empty()
            && path.starts_with(&state.menu_interaction.open_path)
            && path.windows(2).all(|pair| {
                item(&model, pair[0]).is_some_and(|parent| {
                    parent
                        .children
                        .iter()
                        .any(|child| child.id == pair[1] && selectable(child))
                })
            })
            && path
                .last()
                .and_then(|id| item(&model, *id))
                .is_some_and(selectable)
    });
    if !valid {
        session.selected_path = None;
    }
    state.menu_interaction.hovered_path = session.selected_path.clone().unwrap_or_default();
}

fn navigate_list(state: &mut State, direction: i32) -> bool {
    let Some(model) = state.active_menu_model() else {
        return false;
    };
    let Some(root) = state.menu_interaction.open_root else {
        return false;
    };
    let parent_id = state
        .menu_interaction
        .open_path
        .last()
        .copied()
        .unwrap_or(root);
    let Some(parent) = item(model, parent_id) else {
        return false;
    };
    let children = selectable_children(parent);
    if children.is_empty() {
        return false;
    }
    let current = state
        .menu_navigation
        .as_ref()
        .and_then(|session| session.selected_path.as_ref())
        .and_then(|path| path.last().copied());
    let index = current.and_then(|id| children.iter().position(|candidate| *candidate == id));
    let next = match (index, direction) {
        (None, 1) => children[0],
        (None, -1) => *children.last().unwrap(),
        (Some(index), 1) => children.get(index + 1).copied().unwrap_or(children[index]),
        (Some(index), -1) => index
            .checked_sub(1)
            .map_or(children[index], |i| children[i]),
        _ => return false,
    };
    let mut path = state.menu_interaction.open_path.clone();
    path.push(next);
    let Some(session) = &mut state.menu_navigation else {
        return false;
    };
    let changed = session.selected_path.as_deref() != Some(path.as_slice());
    session.selected_path = Some(path.clone());
    state.menu_interaction.hovered_path = path;
    changed
}

fn navigate_root(state: &mut State, direction: i32) -> bool {
    let Some(model) = state.active_menu_model() else {
        return false;
    };
    let roots = selectable_children(&model.root);
    let Some(current) = state.menu_interaction.open_root else {
        return false;
    };
    let Some(index) = roots.iter().position(|id| *id == current) else {
        return false;
    };
    let next = if direction < 0 {
        index.checked_sub(1).map(|i| roots[i])
    } else {
        roots.get(index + 1).copied()
    };
    let Some(next) = next else { return false };
    state.menu_interaction.open_root = Some(next);
    state.menu_interaction.open_path = vec![next];
    state.menu_interaction.hovered_path.clear();
    if let Some(session) = &mut state.menu_navigation {
        session.selected_path = Some(vec![next]);
    }
    true
}

fn begin_bluetooth_action(state: &mut State, action: super::BluetoothPendingAction) -> bool {
    if state.bluetooth_pending.contains(&action) {
        false
    } else {
        state.bluetooth_pending.push(action);
        true
    }
}

// ActiveAiUsageChanged is expected to contain collector-canonical semantic identities.
fn canonicalize_ai_usage_order(
    mut usage: Vec<super::ActiveAgentUsage>,
) -> Vec<super::ActiveAgentUsage> {
    for agent in &mut usage {
        agent.meters.sort_by(|left, right| left.id.cmp(&right.id));
    }
    usage.sort_by(|left, right| {
        left.provider_id
            .cmp(&right.provider_id)
            .then_with(|| left.agent_id.cmp(&right.agent_id))
            .then_with(|| left.account_id.cmp(&right.account_id))
    });
    usage
}

fn plugin_visual_key(plugin: &super::PluginSummary) -> (&super::PluginId, &str) {
    (&plugin.id, &plugin.text)
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
    normalize_keyboard_selection(state);
}

/// Interactive popup ownership is separate from the focused application's
/// loaded menu model. Opening a different popup dismisses only the menu
/// presentation; focus/endpoint lifecycle events remain responsible for
/// invalidating the model itself.
fn dismiss_menu_presentation(state: &mut State) -> bool {
    let changed = state.menu_interaction != MenuInteractionState::default();
    state.menu_interaction = Default::default();
    changed
}

fn reconcile_menu_presentation_to_focus(state: &mut State, registry: &MenuRegistry) {
    state.menu_presentation = state.focused_window.and_then(|window_id| {
        registry
            .active(Some(window_id))
            .map(|endpoint| MenuPresentation {
                window_id,
                endpoint,
            })
    });
    state.menu = MenuState::NoMenu;
    state.menu_interaction = Default::default();
    state.global_menu_model = None;
    state.menu_presentation_needs_focus_reconciliation = false;
}

fn presentation_follows_focus(state: &State) -> bool {
    matches!(
        state.menu_presentation_policy,
        MenuPresentationPolicy::FollowFocus
    )
}

fn end_pinned_presentation(state: &mut State, registry: &MenuRegistry) {
    state.menu_navigation = None;
    state.menu_presentation_policy = MenuPresentationPolicy::FollowFocus;
    reconcile_menu_presentation_to_focus(state, registry);
}

fn end_pinned_presentation_for_workspace_change(state: &mut State) {
    state.menu_navigation = None;
    state.menu_presentation_policy = MenuPresentationPolicy::FollowFocus;
    state.menu_presentation = None;
    state.menu = MenuState::NoMenu;
    state.menu_interaction = Default::default();
    state.global_menu_model = None;
    state.menu_presentation_needs_focus_reconciliation = true;
}

fn pinned_workspace(state: &State) -> Option<&str> {
    match &state.menu_presentation_policy {
        MenuPresentationPolicy::FollowFocus => None,
        MenuPresentationPolicy::Pinned { workspace } => Some(workspace),
    }
}

fn presentation_matches_registry(
    state: &State,
    registry: &MenuRegistry,
    window_id: super::WindowId,
    endpoint: &MenuSource,
) -> bool {
    state.menu_presentation_matches(window_id, endpoint)
        && registry.source_matches(window_id, endpoint)
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
            let focused_workspace = workspaces
                .iter()
                .find(|w| w.focused)
                .map(|w| w.name.clone());
            let workspace_changed = state.focused_workspace != focused_workspace;
            state.focused_workspace = focused_workspace;
            state.workspaces = workspaces;
            if workspace_changed
                && !presentation_follows_focus(state)
                && pinned_workspace(state) != state.focused_workspace.as_deref()
            {
                end_pinned_presentation_for_workspace_change(state);
            }
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
            if pinned_workspace(state) != state.focused_workspace.as_deref()
                && !presentation_follows_focus(state)
            {
                end_pinned_presentation_for_workspace_change(state);
            }
            true
        }
        Event::WindowFocused(window) => {
            if state.focused_window == window
                && !(presentation_follows_focus(state)
                    && state.menu_navigation.is_none()
                    && state.menu_presentation_needs_focus_reconciliation)
            {
                return false;
            }
            state.focused_window = window;
            state.focused_app_name = None;
            if presentation_follows_focus(state) && state.menu_navigation.is_none() {
                reconcile_menu_presentation_to_focus(state, registry);
            }
            state.audio_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            state.audio_drag_input = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            state.network_popup_open_pending = false;
            true
        }
        Event::WindowFocusedWithApp { window, app_name } => {
            if state.focused_window == window
                && state.focused_app_name == app_name
                && !(presentation_follows_focus(state)
                    && state.menu_navigation.is_none()
                    && state.menu_presentation_needs_focus_reconciliation)
            {
                return false;
            }
            state.focused_window = window;
            state.focused_app_name = app_name;
            if presentation_follows_focus(state) && state.menu_navigation.is_none() {
                reconcile_menu_presentation_to_focus(state, registry);
            }
            state.audio_popup_open = false;
            state.audio_dragging = false;
            state.audio_drag_input = false;
            state.bluetooth_popup_open = false;
            state.network_popup_open = false;
            state.network_popup_open_pending = false;
            true
        }
        Event::PinCurrentMenuPresentation => {
            let Some(presentation) = state.menu_presentation.clone() else {
                return false;
            };
            if !registry.source_matches(presentation.window_id, &presentation.endpoint) {
                return false;
            }
            let Some(workspace) = state
                .focused_workspace
                .clone()
                .filter(|workspace| !workspace.is_empty())
            else {
                return false;
            };
            if matches!(
                state.menu_presentation_policy,
                MenuPresentationPolicy::Pinned { .. }
            ) {
                return false;
            }
            state.menu_presentation_policy = MenuPresentationPolicy::Pinned { workspace };
            true
        }
        Event::UnpinMenuPresentation => {
            if !matches!(
                state.menu_presentation_policy,
                MenuPresentationPolicy::Pinned { .. }
            ) {
                return false;
            }
            // A global unpin may arrive while navigation owns XGrabKeyboard.
            // End that session first; the platform observes the missing
            // session and releases its physical grab exactly once.
            teardown_navigation(state, registry);
            state.menu_presentation_policy = MenuPresentationPolicy::FollowFocus;
            reconcile_menu_presentation_to_focus(state, registry);
            true
        }
        Event::ToggleMenuPresentationPin => {
            if presentation_follows_focus(state) {
                reduce(state, Event::PinCurrentMenuPresentation, registry)
            } else {
                reduce(state, Event::UnpinMenuPresentation, registry)
            }
        }
        Event::MenuRegistered {
            window_id,
            endpoint,
        } => {
            let MenuSource::DbusMenu(endpoint) = endpoint else {
                return false;
            };
            registry.register(window_id, endpoint.service, endpoint.object_path);
            if presentation_follows_focus(state) && state.focused_window == Some(window_id) {
                reconcile_menu_presentation_to_focus(state, registry);
            }
            true
        }
        Event::GtkMenuDiscovered {
            window_id,
            endpoint,
        } => {
            let changed = registry.gtk(window_id) != Some(&endpoint);
            registry.register_gtk(window_id, endpoint);
            if changed
                && presentation_follows_focus(state)
                && state.focused_window == Some(window_id)
            {
                reconcile_menu_presentation_to_focus(state, registry);
            }
            changed
        }
        Event::GtkMenuRemoved {
            window_id,
            endpoint,
        } => {
            let removed = registry.remove_gtk_if_matches(window_id, &endpoint);
            if removed
                && state.menu_presentation_window() == Some(window_id)
                && registry.get(window_id).is_none()
            {
                if matches!(
                    state.menu_presentation_policy,
                    MenuPresentationPolicy::Pinned { .. }
                ) {
                    end_pinned_presentation(state, registry);
                } else {
                    state.menu_navigation = None;
                    state.menu = MenuState::NoMenu;
                    state.menu_interaction = Default::default();
                    state.global_menu_model = None;
                    state.menu_presentation = None;
                }
            }
            removed
        }
        Event::MenuUnregistered { window_id } => {
            let previous = registry.unregister(window_id);
            let removed = previous.is_some();
            if let Some(endpoint) = previous {
                state
                    .watcher_generations
                    .remove(&MenuSource::DbusMenu(endpoint));
            }
            if removed && state.menu_presentation_window() == Some(window_id) {
                if matches!(
                    state.menu_presentation_policy,
                    MenuPresentationPolicy::Pinned { .. }
                ) {
                    end_pinned_presentation(state, registry);
                } else {
                    state.menu_navigation = None;
                    state.menu = MenuState::NoMenu;
                    state.menu_interaction = Default::default();
                    state.global_menu_model = None;
                    state.menu_presentation = None;
                }
            }
            removed
        }
        Event::MenuOwnerVanished { sender } => {
            let removed = registry.remove_sender(&sender);
            state.watcher_generations.retain(|endpoint, _| {
                !matches!(endpoint, MenuSource::DbusMenu(endpoint) if endpoint.service == sender)
            });
            if state
                .menu_presentation_window()
                .is_some_and(|window| removed.contains(&window))
            {
                if matches!(
                    state.menu_presentation_policy,
                    MenuPresentationPolicy::Pinned { .. }
                ) {
                    end_pinned_presentation(state, registry);
                } else {
                    state.menu_navigation = None;
                    state.menu = MenuState::NoMenu;
                    state.menu_interaction = Default::default();
                    state.global_menu_model = None;
                    state.menu_presentation = None;
                }
            }
            !removed.is_empty()
        }
        Event::MenuLoadRequested {
            window_id,
            endpoint,
            request_id,
        } => {
            if (matches!(endpoint, MenuSource::Tray(_)) && window_id == super::WindowId(u32::MAX))
                || presentation_matches_registry(state, registry, window_id, &endpoint)
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
        Event::MenuLazyRootLayoutRequested {
            window_id,
            endpoint,
            request_id,
            intent_id,
            watcher_generation,
        } => {
            let valid = state
                .menu_interaction
                .pending_lazy_root
                .as_ref()
                .is_some_and(|pending| {
                    pending.window_id == window_id
                        && pending.endpoint == endpoint
                        && pending.intent_id == intent_id
                        && pending.watcher_generation == watcher_generation
                        && state.watcher_generations.get(&endpoint) == Some(&watcher_generation)
                })
                && presentation_matches_registry(state, registry, window_id, &endpoint);
            if !valid {
                return false;
            }
            state
                .menu_interaction
                .pending_lazy_root
                .as_mut()
                .expect("validated lazy root intent")
                .layout_request_id = Some(request_id);
            state.menu = MenuState::Loading {
                window_id,
                endpoint,
                request_id,
            };
            true
        }
        Event::MenuLazyRootLoadConvergence {
            window_id,
            endpoint,
            request_id,
            follow_up_request_id,
        } => {
            let Some(pending) = state.menu_interaction.pending_lazy_root.as_mut() else {
                return false;
            };
            if pending.window_id != window_id
                || pending.endpoint != endpoint
                || pending.layout_request_id != Some(request_id)
            {
                return false;
            }
            if let Some(next_request_id) = follow_up_request_id {
                pending.layout_request_id = Some(next_request_id);
            } else {
                let item_id = pending.item_id;
                state.menu_interaction.pending_lazy_root = None;
                teardown_navigation_waiting_for_lazy_root(state, registry, item_id);
            }
            true
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
                        || presentation_matches_registry(state, registry, window_id, &endpoint)));
            if accepted {
                let pending_lazy_root = state.menu_interaction.pending_lazy_root.clone();
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
                if let Some(pending) = pending_lazy_root {
                    if pending.endpoint == endpoint
                        && pending.window_id == window_id
                        && pending.layout_request_id == Some(request_id)
                        && state.watcher_generations.get(&endpoint)
                            == Some(&pending.watcher_generation)
                    {
                        let can_open = state.active_menu_model().is_some_and(|model| {
                            model.root.children.iter().any(|item| {
                                item.id == pending.item_id
                                    && item.visible
                                    && item.enabled
                                    && item.children_display
                                        == Some(super::ChildrenDisplay::Submenu)
                                    && !item.children.is_empty()
                            })
                        });
                        if can_open {
                            state.menu_interaction.open_root = Some(pending.item_id);
                            state.menu_interaction.open_path = vec![pending.item_id];
                            state.menu_interaction.pending_lazy_root = None;
                        } else {
                            state.menu_interaction.pending_lazy_root = Some(pending);
                        }
                    }
                }
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
                        || presentation_matches_registry(state, registry, window_id, &endpoint)));
            if accepted {
                let lazy_root = state.menu_interaction.pending_lazy_root.clone();
                state.menu = MenuState::Error {
                    window_id,
                    endpoint,
                    request_id,
                    error,
                };
                state.menu_interaction = Default::default();
                if let Some(pending) = lazy_root {
                    teardown_navigation_waiting_for_lazy_root(state, registry, pending.item_id);
                }
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
        Event::MenuWatcherReady {
            endpoint,
            watcher_generation,
            ..
        } => {
            let previous = state
                .watcher_generations
                .insert(endpoint.clone(), watcher_generation);
            if previous.is_some_and(|old| old != watcher_generation)
                && state
                    .menu_interaction
                    .pending_lazy_root
                    .as_ref()
                    .is_some_and(|pending| pending.endpoint == endpoint)
            {
                let item_id = state
                    .menu_interaction
                    .pending_lazy_root
                    .as_ref()
                    .expect("checked pending lazy root")
                    .item_id;
                state.menu_interaction.pending_lazy_root = None;
                state.menu_interaction.pending_about_to_show = None;
                teardown_navigation_waiting_for_lazy_root(state, registry, item_id);
            }
            false
        }
        Event::MenuLayoutInvalidated { .. } => false,
        Event::MenuPropertiesUpdated {
            endpoint, updates, ..
        } => {
            if !matches!(&state.menu, MenuState::Loaded { endpoint: current, .. } if current == &endpoint)
                || state.active_menu_endpoint(registry).as_ref() != Some(&endpoint)
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
            if menu_item.children_display == Some(super::ChildrenDisplay::Submenu)
                && menu_item.children.is_empty()
            {
                if let MenuState::Loaded {
                    window_id,
                    endpoint: MenuSource::DbusMenu(endpoint),
                    ..
                } = &state.menu
                {
                    let watcher_generation = state
                        .watcher_generations
                        .get(&MenuSource::DbusMenu(endpoint.clone()))
                        .copied();
                    let Some(watcher_generation) = watcher_generation else {
                        return false;
                    };
                    state.next_lazy_root_intent = state.next_lazy_root_intent.wrapping_add(1);
                    state.menu_interaction.pending_lazy_root = Some(LazyRootOpenPending {
                        window_id: *window_id,
                        endpoint: MenuSource::DbusMenu(endpoint.clone()),
                        item_id: id,
                        intent_id: state.next_lazy_root_intent,
                        watcher_generation,
                        layout_request_id: None,
                    });
                    state.menu_interaction.pending_about_to_show = None;
                    state.menu_interaction.open_root = None;
                    state.menu_interaction.open_path.clear();
                    return true;
                }
            }
            if menu_item.children_display.is_none() || menu_item.children.is_empty() {
                if state.menu_interaction.open_root.is_some() {
                    if state.menu_navigation.is_some() {
                        teardown_navigation(state, registry);
                    }
                    state.menu_interaction = Default::default();
                    return true;
                }
                return false;
            }
            if state.menu_interaction.open_root == Some(id) {
                if state.menu_navigation.is_some() {
                    teardown_navigation(state, registry);
                } else {
                    state.menu_interaction = Default::default();
                }
            } else {
                state.menu_interaction.pending_lazy_root = None;
                state.menu_interaction.open_root = Some(id);
                state.menu_interaction.open_path = vec![id];
                state.menu_interaction.hovered_path.clear();
                state.menu_interaction.pending_about_to_show = None;
                state.menu_interaction.about_to_show_item = None;
                if let Some(session) = &mut state.menu_navigation {
                    session.selected_path = Some(vec![id]);
                }
            }
            true
        }
        Event::MenuNavigationStarted => {
            if state.menu_navigation.is_some() {
                false
            } else {
                let root = match state.menu_interaction.open_root {
                    Some(root)
                        if current_presentation_model(state)
                            .is_some_and(|model| eligible_root(model, root)) =>
                    {
                        root
                    }
                    Some(_) => return false,
                    None => {
                        let Some(root) = first_eligible_root(state) else {
                            return false;
                        };
                        if !reduce(state, Event::MenuRootClicked(root), registry) {
                            return false;
                        }
                        root
                    }
                };
                start_navigation_session(state, root)
            }
        }
        Event::KeyboardGrabAcquired { session_id } => {
            let Some(session) = &mut state.menu_navigation else {
                return false;
            };
            if session.id != session_id {
                return false;
            }
            session.grab_state = KeyboardGrabState::Active;
            true
        }
        Event::KeyboardGrabFailed { session_id } => {
            let Some(session) = &mut state.menu_navigation else {
                return false;
            };
            if session.id != session_id {
                return false;
            }
            session.grab_state = KeyboardGrabState::Failed;
            true
        }
        Event::MenuNavigateLeft => {
            let Some(session) = state.menu_navigation.as_ref() else {
                return false;
            };
            if state.menu_interaction.open_path.len() > 1 {
                state.menu_interaction.open_path.pop();
                if let Some(session) = &mut state.menu_navigation {
                    session.selected_path = Some(state.menu_interaction.open_path.clone());
                }
                state.menu_interaction.hovered_path = state.menu_interaction.open_path.clone();
                true
            } else {
                let _ = session;
                navigate_root(state, -1)
            }
        }
        Event::MenuNavigateRight => {
            let Some(session) = state.menu_navigation.as_ref() else {
                return false;
            };
            let selected = session
                .selected_path
                .as_ref()
                .and_then(|path| path.last())
                .copied();
            let Some(selected) = selected else {
                return navigate_root(state, 1);
            };
            if state.menu_interaction.open_root == Some(selected) {
                return navigate_root(state, 1);
            };
            let Some(model) = state.active_menu_model() else {
                return false;
            };
            let Some(selected_item) = item(model, selected) else {
                return false;
            };
            if selected_item.children_display == Some(super::ChildrenDisplay::Submenu) {
                if selected_item.children.is_empty() {
                    if let MenuState::Loaded {
                        window_id,
                        endpoint: MenuSource::DbusMenu(endpoint),
                        ..
                    } = &state.menu
                    {
                        let source = MenuSource::DbusMenu(endpoint.clone());
                        if let Some(watcher_generation) =
                            state.watcher_generations.get(&source).copied()
                        {
                            if current_pending_lazy_root_matches(state, selected) {
                                return false;
                            }
                            state.next_lazy_root_intent =
                                state.next_lazy_root_intent.wrapping_add(1);
                            state.menu_interaction.pending_lazy_root = Some(LazyRootOpenPending {
                                window_id: *window_id,
                                endpoint: source,
                                item_id: selected,
                                intent_id: state.next_lazy_root_intent,
                                watcher_generation,
                                layout_request_id: None,
                            });
                            state.menu_interaction.open_root = None;
                            state.menu_interaction.open_path.clear();
                            return true;
                        }
                    }
                    return false;
                }
                state.menu_interaction.open_path.push(selected);
                let path = state.menu_interaction.open_path.clone();
                if let Some(session) = &mut state.menu_navigation {
                    session.selected_path = Some(path.clone());
                }
                state.menu_interaction.hovered_path = path;
                return true;
            }
            navigate_root(state, 1)
        }
        Event::MenuNavigateDown => navigate_list(state, 1),
        Event::MenuNavigateUp => navigate_list(state, -1),
        Event::MenuNavigateEnter => {
            let Some(model) = state.active_menu_model() else {
                return false;
            };
            let Some(selected) = state
                .menu_navigation
                .as_ref()
                .and_then(|session| session.selected_path.as_ref())
                .and_then(|path| path.last())
                .copied()
            else {
                return false;
            };
            let Some(selected_item) = item(model, selected) else {
                return false;
            };
            if selected_item.children_display == Some(super::ChildrenDisplay::Submenu) {
                if state.menu_interaction.open_path.len() <= 1 {
                    if selected_item.children.is_empty() {
                        if current_pending_lazy_root_matches(state, selected) {
                            return false;
                        }
                        // A lazy top-level root must use the existing
                        // AboutToShow/GetLayout lifecycle.
                        reduce(state, Event::MenuRootClicked(selected), registry)
                    } else {
                        // The root is already open; Enter selects its first
                        // visible child just like Down.
                        navigate_list(state, 1)
                    }
                } else {
                    reduce(state, Event::MenuNavigateRight, registry)
                }
            } else {
                false
            }
        }
        Event::MenuNavigateEscape => {
            if state.menu_interaction.open_path.len() > 1 {
                state.menu_interaction.open_path.pop();
                if let Some(session) = &mut state.menu_navigation {
                    session.selected_path = Some(state.menu_interaction.open_path.clone());
                }
                state.menu_interaction.hovered_path = state.menu_interaction.open_path.clone();
                true
            } else if state.menu_navigation.is_some() {
                teardown_navigation(state, registry);
                true
            } else {
                false
            }
        }
        Event::MenuItemActivateRequested {
            window_id,
            endpoint,
            item_id,
            timestamp: _,
        } => {
            let valid_context = ((matches!(endpoint, MenuSource::Tray(_))
                && window_id == super::WindowId(u32::MAX))
                || presentation_matches_registry(state, registry, window_id, &endpoint))
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
                if state.menu_navigation.is_some() {
                    teardown_navigation(state, registry);
                } else {
                    state.menu_interaction = Default::default();
                }
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
            if let Some(session) = &mut state.menu_navigation {
                session.selected_path = Some(state.menu_interaction.hovered_path.clone());
            }
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
                if state.menu_navigation.is_some() {
                    teardown_navigation(state, registry);
                } else {
                    state.menu_interaction = Default::default();
                }
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
                || state.network_popup_open
                || dismiss_menu_presentation(state);
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
            lazy_root,
            intent_id,
            watcher_generation,
        } => {
            let lazy_root_match = state
                .menu_interaction
                .pending_lazy_root
                .as_ref()
                .is_some_and(|pending| {
                    pending.window_id == window_id
                        && pending.endpoint == endpoint
                        && pending.item_id == item_id
                });
            let lazy_authority = if lazy_root {
                state
                    .menu_interaction
                    .pending_lazy_root
                    .as_ref()
                    .is_some_and(|pending| {
                        Some(pending.intent_id) == intent_id
                            && Some(pending.watcher_generation) == watcher_generation
                            && pending.window_id == window_id
                            && pending.endpoint == endpoint
                            && state.watcher_generations.get(&endpoint)
                                == watcher_generation.as_ref()
                    })
            } else {
                false
            };
            let valid = ((matches!(endpoint, MenuSource::Tray(_))
                && window_id == super::WindowId(u32::MAX))
                || presentation_matches_registry(state, registry, window_id, &endpoint))
                && (matches!(&state.menu, MenuState::Loaded { endpoint: current, .. } if current == &endpoint)
                    || matches!(&state.menu, MenuState::TrayLoaded { endpoint: current, .. }
                        if MenuSource::Tray(current.clone()) == endpoint))
                && ((state.menu_interaction.open_root.is_some()
                    && state.menu_interaction.hovered_path.last() == Some(&item_id))
                    || (lazy_root && lazy_root_match && lazy_authority));
            if valid {
                state.menu_interaction.pending_about_to_show = Some(super::AboutToShowPending {
                    window_id,
                    endpoint,
                    item_id,
                    request_id,
                    lazy_root,
                    intent_id,
                    watcher_generation,
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
            lazy_root,
            intent_id,
            watcher_generation,
            need_update,
            model,
            error,
        } => {
            let accepted = matches!(&state.menu_interaction.pending_about_to_show,
                Some(p) if p.window_id == window_id && p.endpoint == endpoint && p.item_id == item_id && p.request_id == request_id && p.lazy_root == lazy_root && p.intent_id == intent_id && p.watcher_generation == watcher_generation)
                && ((matches!(endpoint, MenuSource::Tray(_))
                    && window_id == super::WindowId(u32::MAX))
                    || presentation_matches_registry(state, registry, window_id, &endpoint))
                && ((lazy_root
                    && state
                        .menu_interaction
                        .pending_lazy_root
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.window_id == window_id
                                && pending.endpoint == endpoint
                                && pending.item_id == item_id
                                && pending.intent_id == intent_id.unwrap_or_default()
                                && pending.watcher_generation
                                    == watcher_generation.unwrap_or_default()
                        }))
                    || (!lazy_root
                        && state.menu_interaction.hovered_path.last() == Some(&item_id)));
            if !accepted {
                return false;
            }
            state.menu_interaction.pending_about_to_show = None;
            if error.is_some() {
                if lazy_root {
                    state.menu_interaction.pending_lazy_root = None;
                    teardown_navigation_waiting_for_lazy_root(state, registry, item_id);
                }
                return true;
            }
            if lazy_root {
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
                if let Some(open) = state.notification_center_open {
                    if !outputs.iter().any(|output| output.id == open) {
                        state.notification_center_open = None;
                    }
                }
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
            if state.network_status_authoritative {
                return false;
            }
            let visual_before = network_visual_state(&state.network);
            let mut network = network;
            if state.network_status_authoritative {
                network.available = state.network.available;
                network.connectivity = state.network.connectivity.clone();
                network.link_kind = state.network.link_kind.clone();
                network.interface = state.network.interface.clone();
                network.display_name = state.network.display_name.clone();
                network.signal_percent = state.network.signal_percent;
            }
            if state.network == network {
                false
            } else {
                state.network = network;
                visual_before != network_visual_state(&state.network) || state.network_popup_open
            }
        }
        Event::NetworkStatusChanged(status) => {
            let visual_before = network_visual_state(&state.network);
            let changed = state.network_status != status;
            state.network_status = status.clone();
            state.network_status_authoritative = true;
            state.network.available = status.available;
            state.network.connectivity = if !status.available || !status.connected {
                super::NetworkConnectivity::Disconnected
            } else {
                super::NetworkConnectivity::Connected
            };
            state.network.link_kind = if status.connected {
                super::NetworkLinkKind::Wifi
            } else {
                super::NetworkLinkKind::Other
            };
            state.network.interface = status.interface;
            state.network.display_name = status.ssid;
            state.network.signal_percent = status.strength;
            changed && visual_before != network_visual_state(&state.network)
        }
        Event::NetworkPopupProjectionChanged(network) => {
            if state.network.wireless_enabled == network.wireless_enabled
                && state.network.wifi_devices == network.wifi_devices
                && state.network.access_points == network.access_points
            {
                false
            } else {
                state.network.wireless_enabled = network.wireless_enabled;
                state.network.wifi_devices = network.wifi_devices;
                state.network.access_points = network.access_points;
                state.network_popup_open
            }
        }
        Event::NetworkConnectSavedWifi(target) => state.network.access_points.iter().any(|ap| {
            ap.interface == target.interface
                && ap.ssid == target.ssid
                && super::wifi_band(ap.frequency) == target.band
                && ap.saved_profile.is_some()
                && !ap.is_active
        }),
        Event::NetworkPopupOpenRequested => {
            if state.network_popup_open || state.network_popup_open_pending {
                false
            } else {
                if state.network_status_authoritative {
                    state.network_popup_open = true;
                } else {
                    state.network_popup_open_pending = true;
                }
                state.audio_popup_open = false;
                state.audio_dragging = false;
                state.audio_drag_input = false;
                state.bluetooth_popup_open = false;
                dismiss_menu_presentation(state);
                true
            }
        }
        Event::NetworkPopupSnapshotReceived(network) => {
            if state.network_status_authoritative {
                return false;
            }
            if !state.network_popup_open_pending {
                false
            } else {
                state.network_popup_open_pending = false;
                let mut network = network;
                if state.network_status_authoritative {
                    network.available = state.network.available;
                    network.connectivity = state.network.connectivity.clone();
                    network.link_kind = state.network.link_kind.clone();
                    network.interface = state.network.interface.clone();
                    network.display_name = state.network.display_name.clone();
                    network.signal_percent = state.network.signal_percent;
                }
                state.network = network;
                state.network_popup_open = true;
                true
            }
        }
        Event::NetworkPopupSnapshotFailed => {
            let pending = state.network_popup_open_pending;
            state.network_popup_open_pending = false;
            pending
        }
        Event::NetworkPopupToggled => {
            state.network_popup_open = !state.network_popup_open;
            state.network_popup_open_pending = false;
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
        Event::ActiveAiUsageChanged(usage) => {
            let usage = canonicalize_ai_usage_order(usage);
            let plugins = usage
                .iter()
                .map(super::ActiveAgentUsage::plugin_summary)
                .collect::<Vec<_>>();
            let visual_changed = state
                .plugin_zone
                .plugins
                .iter()
                .map(plugin_visual_key)
                .ne(plugins.iter().map(plugin_visual_key));
            state.ai_usage = usage;
            state.plugin_zone.plugins = plugins;
            visual_changed
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
                dismiss_menu_presentation(state);
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
        Event::NotificationsState {
            active,
            history,
            action_projections,
        } => {
            let changed = state.notifications != active
                || state.notification_history != history
                || state.notification_action_projections != action_projections;
            if changed {
                state.notifications = active;
                state.notification_history = history;
                state.notification_action_projections = action_projections;
            }
            changed
        }
        Event::ToggleNotificationCenter(output) => {
            state.notification_center_open = match state.notification_center_open {
                None => Some(output),
                Some(current) if current == output => None,
                Some(_) => Some(output),
            };
            true
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
                dismiss_menu_presentation(state);
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
        Event::StatusNotifierWatcherUnavailable => {
            let changed = !state.status_notifiers.is_empty()
                || !state.status_notifier_items.items().is_empty()
                || state.status_notifier_host_registered;
            state.status_notifiers = super::StatusNotifierRegistry::default();
            state.status_notifier_items = super::StatusNotifierItemRegistry::default();
            state.status_notifier_host_registered = false;
            if matches!(
                state.menu,
                MenuState::TrayLoaded { .. } | MenuState::TrayLoading { .. }
            ) {
                state.menu = MenuState::NoMenu;
                state.menu_interaction = Default::default();
            }
            changed
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
        | Event::X11(crate::platform::x11::X11Event::MotionNotify { .. })
        | Event::X11(crate::platform::x11::X11Event::KeyPress { .. })
        | Event::X11(crate::platform::x11::X11Event::KeyRelease { .. }) => false,
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
    fn saved_wifi_intent_accepts_only_saved_inactive_matching_row() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        state.network.access_points = vec![super::super::NetworkAccessPoint {
            interface: "wlan0".into(),
            ssid: "Foo".into(),
            frequency: 2412,
            saved_profile: Some("/settings/1".into()),
            ..Default::default()
        }];
        let target = super::super::NetworkWifiTarget {
            interface: "wlan0".into(),
            ssid: "Foo".into(),
            band: "2.4 GHz".into(),
            ..Default::default()
        };
        assert!(reduce(
            &mut state,
            Event::NetworkConnectSavedWifi(target.clone()),
            &mut registry,
        ));
        state.network.access_points[0].is_active = true;
        assert!(!reduce(
            &mut state,
            Event::NetworkConnectSavedWifi(target.clone()),
            &mut registry,
        ));
        state.network.access_points[0].is_active = false;
        state.network.access_points[0].saved_profile = None;
        assert!(!reduce(
            &mut state,
            Event::NetworkConnectSavedWifi(target),
            &mut registry,
        ));
    }

    #[test]
    fn wifi_inventory_is_device_scoped_and_duplicate_snapshots_are_quiet() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let access_point = |device: &str, interface: &str| super::super::NetworkAccessPoint {
            path: format!("{device}/ap"),
            device_path: device.into(),
            interface: interface.into(),
            ssid: "SAME-SSID".into(),
            strength: 80,
            frequency: 2412,
            is_active: false,
            saved_profile: None,
        };
        let inventory = super::super::NetworkState {
            available: true,
            wireless_enabled: true,
            wifi_devices: vec![
                super::super::WifiDevice {
                    path: "/device/0".into(),
                    interface: "wlan0".into(),
                    access_points: vec![access_point("/device/0", "wlan0")],
                    ..Default::default()
                },
                super::super::WifiDevice {
                    path: "/device/1".into(),
                    interface: "wlan1".into(),
                    access_points: vec![access_point("/device/1", "wlan1")],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(!reduce(
            &mut state,
            Event::NetworkSnapshotReceived(inventory.clone()),
            &mut registry,
        ));
        assert_eq!(state.network.wifi_devices.len(), 2);
        assert_eq!(
            state.network.wifi_devices[0].access_points[0].saved_profile,
            None
        );
        assert_eq!(super::super::wifi_band(5765), "5 GHz");
        assert!(!reduce(
            &mut state,
            Event::NetworkSnapshotReceived(inventory),
            &mut registry,
        ));
    }

    fn wifi_ap(
        device: &str,
        interface: &str,
        ssid: &str,
        frequency: u32,
        strength: u8,
        is_active: bool,
        saved_profile: Option<&str>,
    ) -> super::super::NetworkAccessPoint {
        super::super::NetworkAccessPoint {
            path: format!("{device}/{ssid}/{frequency}/{strength}"),
            device_path: device.into(),
            interface: interface.into(),
            ssid: ssid.into(),
            strength,
            frequency,
            is_active,
            saved_profile: saved_profile.map(str::to_owned),
        }
    }

    fn wifi_snapshot(
        devices: Vec<super::super::WifiDevice>,
        enabled: bool,
    ) -> super::super::NetworkState {
        super::super::NetworkState {
            available: true,
            wireless_enabled: enabled,
            wifi_devices: devices,
            ..Default::default()
        }
    }

    #[test]
    fn wifi_inventory_keeps_unsaved_and_saved_candidates_and_filters_empty_ssid() {
        let device = super::super::WifiDevice {
            path: "/device/0".into(),
            interface: "wlan0".into(),
            raw_access_points: 4,
            named_access_points: 3,
            access_points: vec![
                wifi_ap(
                    "/device/0",
                    "wlan0",
                    "A",
                    2412,
                    50,
                    false,
                    Some("profile-a"),
                ),
                wifi_ap("/device/0", "wlan0", "B", 2412, 60, false, None),
                wifi_ap("/device/0", "wlan0", "C", 5180, 70, false, None),
            ],
            ..Default::default()
        };
        let snapshot = wifi_snapshot(vec![device], true);
        assert_eq!(snapshot.wifi_devices[0].raw_access_points, 4);
        assert_eq!(snapshot.wifi_devices[0].named_access_points, 3);
        assert_eq!(snapshot.wifi_devices[0].access_points.len(), 3);
        assert!(snapshot.wifi_devices[0]
            .access_points
            .iter()
            .any(|ap| ap.ssid == "B" && ap.saved_profile.is_none()));
        assert!(snapshot.wifi_devices[0]
            .access_points
            .iter()
            .all(|ap| !ap.ssid.is_empty() && !ap.ssid.eq_ignore_ascii_case("hidden")));
    }

    #[test]
    fn wifi_inventory_preserves_band_split_and_selects_strongest_same_band_ap() {
        let device = super::super::WifiDevice {
            path: "/device/0".into(),
            interface: "wlan0".into(),
            access_points: vec![
                wifi_ap("/device/0", "wlan0", "Foo", 5180, 40, false, None),
                wifi_ap("/device/0", "wlan0", "Foo", 5200, 75, false, None),
                wifi_ap("/device/0", "wlan0", "Foo", 2412, 55, false, None),
            ],
            ..Default::default()
        };
        let candidates = &wifi_snapshot(vec![device], true).wifi_devices[0].access_points;
        assert_eq!(candidates.len(), 3);
        assert_eq!(super::super::wifi_band(2412), "2.4 GHz");
        assert_eq!(super::super::wifi_band(2462), "2.4 GHz");
        assert_eq!(super::super::wifi_band(5180), "5 GHz");
        assert_eq!(super::super::wifi_band(5765), "5 GHz");
        assert_eq!(
            candidates
                .iter()
                .filter(|ap| ap.ssid == "Foo" && super::super::wifi_band(ap.frequency) == "5 GHz")
                .count(),
            2
        );
    }

    #[test]
    fn wifi_inventory_is_independent_across_devices_and_active_is_device_scoped() {
        let wlan0 = super::super::WifiDevice {
            path: "/device/0".into(),
            interface: "wlan0".into(),
            active_connection: Some("/active/foo".into()),
            access_points: vec![wifi_ap("/device/0", "wlan0", "Foo", 2412, 80, true, None)],
            ..Default::default()
        };
        let wlan1 = super::super::WifiDevice {
            path: "/device/1".into(),
            interface: "wlan1".into(),
            access_points: vec![wifi_ap("/device/1", "wlan1", "Foo", 2412, 70, false, None)],
            ..Default::default()
        };
        let snapshot = wifi_snapshot(vec![wlan0, wlan1], true);
        assert_eq!(snapshot.wifi_devices.len(), 2);
        assert!(snapshot.wifi_devices[0].access_points[0].is_active);
        assert!(!snapshot.wifi_devices[1].access_points[0].is_active);
        assert_eq!(
            snapshot
                .wifi_devices
                .iter()
                .filter(|device| device.active_connection.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn wifi_inventory_supports_two_active_devices_and_global_wireless_state() {
        let devices = vec![
            super::super::WifiDevice {
                path: "/device/0".into(),
                interface: "wlan0".into(),
                state: 100,
                active_connection: Some("/active/foo".into()),
                access_points: vec![wifi_ap("/device/0", "wlan0", "Foo", 5180, 80, true, None)],
                ..Default::default()
            },
            super::super::WifiDevice {
                path: "/device/1".into(),
                interface: "wlan1".into(),
                state: 100,
                active_connection: Some("/active/bar".into()),
                access_points: vec![wifi_ap("/device/1", "wlan1", "Bar", 2412, 70, true, None)],
                ..Default::default()
            },
        ];
        let on = wifi_snapshot(devices.clone(), true);
        let off = wifi_snapshot(devices, false);
        assert!(on.wireless_enabled);
        assert!(!off.wireless_enabled);
        assert_eq!(
            on.wifi_devices
                .iter()
                .filter(|d| d.active_connection.is_some())
                .count(),
            2
        );
        assert!(on
            .wifi_devices
            .iter()
            .all(|d| d.access_points.iter().any(|ap| ap.is_active)));
        assert_eq!(on.wifi_devices.len(), off.wifi_devices.len());
    }

    #[test]
    fn wifi_device_states_have_semantic_labels_and_grouping_is_structural() {
        assert_eq!(super::super::wifi_device_state_label(10), "Não gerenciada");
        assert_eq!(super::super::wifi_device_state_label(20), "Indisponível");
        assert_eq!(super::super::wifi_device_state_label(30), "Desconectada");
        assert_eq!(super::super::wifi_device_state_label(40), "Conectando");
        assert_eq!(super::super::wifi_device_state_label(100), "Conectada");
        assert_eq!(super::super::wifi_device_state_label(110), "Desconectando");
        assert_eq!(super::super::wifi_device_state_label(120), "Falha");

        let snapshot = wifi_snapshot(
            vec![
                super::super::WifiDevice {
                    interface: "wlan0".into(),
                    ..Default::default()
                },
                super::super::WifiDevice {
                    interface: "wlan1".into(),
                    ..Default::default()
                },
            ],
            true,
        );
        assert_eq!(snapshot.wifi_devices.len(), 2);
        assert_eq!(snapshot.wifi_devices[0].interface, "wlan0");
        assert_eq!(snapshot.wifi_devices[1].interface, "wlan1");
    }

    #[test]
    fn external_wifi_membership_changes_update_open_popup_and_active_count() {
        let mut state = State {
            network_popup_open: true,
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();
        let wlan0 = super::super::WifiDevice {
            interface: "wlan0".into(),
            active_connection: Some("/active/foo".into()),
            access_points: vec![wifi_ap("/device/0", "wlan0", "Foo", 5180, 80, true, None)],
            ..Default::default()
        };
        let wlan1 = super::super::WifiDevice {
            interface: "wlan1".into(),
            active_connection: Some("/active/bar".into()),
            access_points: vec![wifi_ap("/device/1", "wlan1", "Bar", 2412, 70, true, None)],
            ..Default::default()
        };
        assert!(reduce(
            &mut state,
            Event::NetworkSnapshotReceived(wifi_snapshot(vec![wlan0.clone()], true)),
            &mut registry,
        ));
        assert_eq!(state.network.wifi_devices.len(), 1);
        assert!(reduce(
            &mut state,
            Event::NetworkSnapshotReceived(wifi_snapshot(vec![wlan0, wlan1], true)),
            &mut registry,
        ));
        assert_eq!(
            state
                .network
                .wifi_devices
                .iter()
                .filter(|device| device.active_connection.is_some())
                .count(),
            2
        );
        assert!(reduce(
            &mut state,
            Event::NetworkSnapshotReceived(wifi_snapshot(
                vec![super::super::WifiDevice {
                    interface: "wlan0".into(),
                    active_connection: Some("/active/foo".into()),
                    access_points: vec![
                        wifi_ap("/device/0", "wlan0", "Foo", 5180, 80, true, None,)
                    ],
                    ..Default::default()
                }],
                true,
            )),
            &mut registry,
        ));
        assert_eq!(state.network.wifi_devices.len(), 1);
    }

    #[test]
    fn reopening_network_popup_uses_each_fresh_same_device_snapshot() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let snapshot = |active_connection: &str, active_ap: &str, five_ghz: bool| {
            let five = wifi_ap(
                "/device/0",
                "wlan0",
                "DEMOSTENES-5G",
                5765,
                80,
                five_ghz,
                Some("profile-5g"),
            );
            let two_four = wifi_ap(
                "/device/0",
                "wlan0",
                "DEMOSTENES-2.4G",
                2417,
                75,
                !five_ghz,
                Some("profile-2.4g"),
            );
            wifi_snapshot(
                vec![super::super::WifiDevice {
                    path: "/device/0".into(),
                    interface: "wlan0".into(),
                    active_connection: Some(active_connection.into()),
                    active_ap: Some(active_ap.into()),
                    access_points: vec![five, two_four],
                    ..Default::default()
                }],
                true,
            )
        };
        let assert_active = |state: &State, ssid: &str| {
            let device = &state.network.wifi_devices[0];
            assert_eq!(
                device
                    .access_points
                    .iter()
                    .filter(|access_point| access_point.is_active)
                    .map(|access_point| access_point.ssid.as_str())
                    .collect::<Vec<_>>(),
                vec![ssid]
            );
        };

        for (connection, ap, five_ghz, expected) in [
            ("/active/5g", "/ap/5g", true, "DEMOSTENES-5G"),
            ("/active/2.4g", "/ap/2.4g", false, "DEMOSTENES-2.4G"),
            ("/active/5g-again", "/ap/5g-again", true, "DEMOSTENES-5G"),
            (
                "/active/2.4g-again",
                "/ap/2.4g-again",
                false,
                "DEMOSTENES-2.4G",
            ),
            ("/active/5g-final", "/ap/5g-final", true, "DEMOSTENES-5G"),
        ] {
            assert!(reduce(
                &mut state,
                Event::NetworkPopupToggled,
                &mut registry,
            ));
            assert!(state.network_popup_open);
            assert!(reduce(
                &mut state,
                Event::NetworkSnapshotReceived(snapshot(connection, ap, five_ghz)),
                &mut registry,
            ));
            assert_active(&state, expected);
            assert!(reduce(
                &mut state,
                Event::NetworkPopupToggled,
                &mut registry,
            ));
            assert!(!state.network_popup_open);
        }
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
    fn xnm_wireless_projection_updates_only_after_authoritative_event() {
        let mut state = State {
            network: wifi_snapshot(Vec::new(), true),
            ..Default::default()
        };
        state.network.wireless_enabled = true;
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::NetworkSetWireless(false),
            &mut registry
        ));
        assert!(state.network.wireless_enabled);
        let mut projection = state.network.clone();
        projection.wireless_enabled = false;
        reduce(
            &mut state,
            Event::NetworkPopupProjectionChanged(projection),
            &mut registry,
        );
        assert!(!state.network.wireless_enabled);
    }

    #[test]
    fn network_popup_fetch_first_never_maps_stale_state() {
        let mut state = State {
            network: wifi_snapshot(
                vec![super::super::WifiDevice {
                    interface: "wlan0".into(),
                    active_connection: Some("/active/5g".into()),
                    access_points: vec![wifi_ap(
                        "/device/0",
                        "wlan0",
                        "DEMOSTENES-5G",
                        5765,
                        80,
                        true,
                        None,
                    )],
                    ..Default::default()
                }],
                true,
            ),
            ..Default::default()
        };
        let mut registry = MenuRegistry::default();

        assert!(reduce(
            &mut state,
            Event::NetworkPopupOpenRequested,
            &mut registry,
        ));
        assert!(state.network_popup_open_pending);
        assert!(!state.network_popup_open);

        let fresh = wifi_snapshot(
            vec![super::super::WifiDevice {
                interface: "wlan0".into(),
                active_connection: Some("/active/2.4g".into()),
                access_points: vec![
                    wifi_ap("/device/0", "wlan0", "DEMOSTENES-5G", 5765, 80, false, None),
                    wifi_ap(
                        "/device/0",
                        "wlan0",
                        "DEMOSTENES-2.4G",
                        2417,
                        75,
                        true,
                        None,
                    ),
                ],
                ..Default::default()
            }],
            true,
        );
        assert!(reduce(
            &mut state,
            Event::NetworkPopupSnapshotReceived(fresh),
            &mut registry,
        ));
        assert!(!state.network_popup_open_pending);
        assert!(state.network_popup_open);
        let active = &state.network.wifi_devices[0].access_points;
        assert!(!active
            .iter()
            .any(|ap| ap.ssid == "DEMOSTENES-5G" && ap.is_active));
        assert!(active
            .iter()
            .any(|ap| ap.ssid == "DEMOSTENES-2.4G" && ap.is_active));
    }

    #[test]
    fn network_popup_snapshot_failure_releases_pending_for_next_click() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();

        assert!(reduce(
            &mut state,
            Event::NetworkPopupOpenRequested,
            &mut registry,
        ));
        assert!(state.network_popup_open_pending);
        assert!(reduce(
            &mut state,
            Event::NetworkPopupSnapshotFailed,
            &mut registry,
        ));
        assert!(!state.network_popup_open_pending);
        assert!(!state.network_popup_open);

        assert!(reduce(
            &mut state,
            Event::NetworkPopupOpenRequested,
            &mut registry,
        ));
        assert!(state.network_popup_open_pending);
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
                icon_name: None,
                attention_icon_name: None,
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
                icon_name: None,
                attention_icon_name: None,
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
                icon_name: None,
                attention_icon_name: None,
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
    fn watcher_loss_clears_only_tray_projection() {
        let mut state = State {
            focused_app_name: Some("keep-me".into()),
            ..Default::default()
        };
        state
            .status_notifiers
            .register(super::super::StatusNotifierEndpoint {
                service: ":1.9".into(),
                object_path: "/StatusNotifierItem".into(),
            });
        assert!(reduce(
            &mut state,
            Event::StatusNotifierWatcherUnavailable,
            &mut MenuRegistry::default(),
        ));
        assert!(state.status_notifiers.is_empty());
        assert!(state.status_notifier_items.items().is_empty());
        assert_eq!(state.focused_app_name.as_deref(), Some("keep-me"));
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
    fn notification_center_toggle_is_output_owned() {
        let mut state = State::default();
        assert!(reduce(
            &mut state,
            Event::ToggleNotificationCenter(OutputId(1)),
            &mut MenuRegistry::default()
        ));
        assert_eq!(state.notification_center_open, Some(OutputId(1)));
        assert!(reduce(
            &mut state,
            Event::ToggleNotificationCenter(OutputId(1)),
            &mut MenuRegistry::default()
        ));
        assert_eq!(state.notification_center_open, None);
        reduce(
            &mut state,
            Event::ToggleNotificationCenter(OutputId(1)),
            &mut MenuRegistry::default(),
        );
        reduce(
            &mut state,
            Event::ToggleNotificationCenter(OutputId(2)),
            &mut MenuRegistry::default(),
        );
        assert_eq!(state.notification_center_open, Some(OutputId(2)));
    }

    #[test]
    fn removed_output_closes_notification_center_without_migration() {
        let mut state = State {
            notification_center_open: Some(OutputId(1)),
            outputs: vec![OutputState {
                id: OutputId(1),
                name: "A".into(),
                x: 0,
                y: 0,
                width: 100,
                height: 100,
            }],
            ..Default::default()
        };
        reduce(
            &mut state,
            Event::OutputsChanged(vec![OutputState {
                id: OutputId(2),
                name: "B".into(),
                x: 100,
                y: 0,
                width: 100,
                height: 100,
            }]),
            &mut MenuRegistry::default(),
        );
        assert_eq!(state.notification_center_open, None);
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
    fn presentation_identity_can_remain_a_while_real_focus_is_b() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source_a = MenuSource::DbusMenu(ep());
        let source_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/menu-b".into(),
        });
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: source_a.clone(),
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(8),
                endpoint: source_b,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        let presented_model = model();
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: source_a.clone(),
            model: presented_model.clone(),
        };

        state.focused_window = Some(WindowId(8));

        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert_eq!(state.active_menu_endpoint(&registry), Some(source_a));
        assert_eq!(state.active_menu_model(), Some(&presented_model));
    }

    #[test]
    fn endpoint_scoped_properties_update_the_current_presentation_not_focus() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source = MenuSource::DbusMenu(ep());
        for window_id in [WindowId(7), WindowId(8)] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint: source.clone(),
                },
                &mut registry,
            );
        }
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: source.clone(),
            model: model(),
        };
        state.focused_window = Some(WindowId(8));

        assert!(reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: source.clone(),
                watcher_generation: None,
                updates: vec![super::super::MenuItemPropertiesUpdate {
                    item_id: MenuItemId(0),
                    properties: vec![super::super::MenuPropertyUpdate::Label(Some(
                        "updated".into(),
                    ))],
                }],
            },
            &mut registry,
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuPropertiesUpdated {
                endpoint: MenuSource::DbusMenu(MenuEndpoint {
                    service: ":1.other".into(),
                    object_path: "/other".into(),
                }),
                watcher_generation: None,
                updates: Vec::new(),
            },
            &mut registry,
        ));
    }

    #[test]
    fn endpoint_invalidation_reloads_the_presentation_window_not_real_focus() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source = MenuSource::DbusMenu(ep());
        for window_id in [WindowId(7), WindowId(8)] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint: source.clone(),
                },
                &mut registry,
            );
        }
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        state.focused_window = Some(WindowId(8));

        assert_eq!(state.active_menu_endpoint(&registry), Some(source.clone()));
        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert!(reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: source.clone(),
                request_id: 71,
            },
            &mut registry,
        ));
        assert!(matches!(
            state.menu,
            MenuState::Loading {
                window_id: WindowId(7),
                endpoint: ref current,
                request_id: 71,
            } if current == &source
        ));
    }

    #[test]
    fn presentation_rejects_same_endpoint_load_for_non_presented_window() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source = MenuSource::DbusMenu(ep());
        for window_id in [WindowId(7), WindowId(8)] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint: source.clone(),
                },
                &mut registry,
            );
        }
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        state.focused_window = Some(WindowId(8));

        assert!(!reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(8),
                endpoint: source,
                request_id: 72,
            },
            &mut registry,
        ));
    }

    #[test]
    fn pinned_presentation_accepts_only_its_fenced_load_result() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source = MenuSource::DbusMenu(ep());
        reduce(
            &mut state,
            Event::MenuRegistered {
                window_id: WindowId(7),
                endpoint: source.clone(),
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
                endpoint: source.clone(),
                request_id: 73,
            },
            &mut registry,
        ));
        state.focused_window = Some(WindowId(8));

        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: source.clone(),
                request_id: 74,
                model: model(),
            },
            &mut registry,
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(8),
                endpoint: source.clone(),
                request_id: 73,
                model: model(),
            },
            &mut registry,
        ));
        assert!(reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint: source,
                request_id: 73,
                model: model(),
            },
            &mut registry,
        ));
    }

    #[test]
    fn follow_focus_replaces_presentation_and_no_menu_clears_it() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source_a = MenuSource::DbusMenu(ep());
        let source_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/menu-b".into(),
        });
        for (window_id, endpoint) in [
            (WindowId(7), source_a.clone()),
            (WindowId(8), source_b.clone()),
        ] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint,
                },
                &mut registry,
            );
        }
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        assert_eq!(state.active_menu_endpoint(&registry), Some(source_a));
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry,
        );
        assert_eq!(state.active_menu_endpoint(&registry), Some(source_b));
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(9))),
            &mut registry,
        );
        assert!(state.menu_presentation.is_none());
        assert!(matches!(state.menu, MenuState::NoMenu));
    }

    #[test]
    fn presentation_lifecycle_is_scoped_to_the_presented_window() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source_a = MenuSource::DbusMenu(ep());
        let source_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/menu-b".into(),
        });
        for (window_id, endpoint) in [(WindowId(7), source_a.clone()), (WindowId(8), source_b)] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint,
                },
                &mut registry,
            );
        }
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(7))),
            &mut registry,
        );
        state.focused_window = Some(WindowId(8));

        assert!(reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(8),
            },
            &mut registry,
        ));
        assert_eq!(
            state.active_menu_endpoint(&registry),
            Some(source_a.clone())
        );
        assert!(reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(7),
            },
            &mut registry,
        ));
        assert!(state.menu_presentation.is_none());
    }

    #[test]
    fn reconciliation_reads_current_focus_when_it_is_invoked_later() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let source_a = MenuSource::DbusMenu(ep());
        let source_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/menu-b".into(),
        });
        for (window_id, endpoint) in [(WindowId(7), source_a), (WindowId(8), source_b.clone())] {
            reduce(
                &mut state,
                Event::MenuRegistered {
                    window_id,
                    endpoint,
                },
                &mut registry,
            );
        }
        state.focused_window = Some(WindowId(8));
        reconcile_menu_presentation_to_focus(&mut state, &registry);
        assert_eq!(state.menu_presentation_window(), Some(WindowId(8)));
        assert_eq!(state.active_menu_endpoint(&registry), Some(source_b));
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
                watcher_generation: None,
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

    fn loaded_menu_with_open_presentation() -> (State, MenuRegistry) {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
        state.focused_window = Some(WindowId(7));
        state.focused_workspace = Some("1".into());
        state.workspaces = vec![ws("1", true), ws("2", false)];
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        state.menu_interaction = MenuInteractionState {
            open_root: Some(MenuItemId(1)),
            open_path: vec![MenuItemId(1), MenuItemId(2)],
            hovered_path: vec![MenuItemId(1), MenuItemId(2)],
            ..Default::default()
        };
        (state, registry)
    }

    fn open_keyboard_navigation(state: &mut State, registry: &mut MenuRegistry) {
        assert!(reduce(
            state,
            Event::MenuRootClicked(MenuItemId(1)),
            registry
        ));
        assert!(reduce(state, Event::MenuNavigationStarted, registry));
    }

    #[test]
    fn keyboard_entry_from_closed_menu_opens_first_eligible_root_and_requests_grab() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();

        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);
        let session = state.menu_navigation.as_ref().expect("navigation session");
        assert_eq!(session.selected_path, Some(vec![MenuItemId(1)]));
        assert_eq!(session.grab_state, KeyboardGrabState::Requested);
    }

    #[test]
    fn keyboard_entry_from_open_root_keeps_that_root_selected() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();

        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().selected_path,
            Some(vec![MenuItemId(1)])
        );
    }

    #[test]
    fn keyboard_entry_rejects_no_menu_and_an_active_session() {
        let mut empty = State::default();
        assert!(!reduce(
            &mut empty,
            Event::MenuNavigationStarted,
            &mut MenuRegistry::default()
        ));
        assert!(empty.menu_navigation.is_none());

        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        let session = state.menu_navigation.clone();
        assert!(!reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert_eq!(state.menu_navigation, session);
    }

    #[test]
    fn keyboard_entry_targets_the_pinned_presentation_not_live_focus() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));

        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert_eq!(state.focused_window, Some(WindowId(8)));
        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().source_window,
            WindowId(7)
        );
    }

    #[test]
    fn non_menu_popup_opening_preserves_loaded_model_and_dismisses_menu_presentation() {
        let cases = [
            ("network", Event::NetworkPopupOpenRequested),
            ("audio", Event::AudioPopupToggled),
            ("bluetooth", Event::BluetoothPopupToggled),
        ];

        for (name, event) in cases {
            let (mut state, mut registry) = loaded_menu_with_open_presentation();
            state.network_status_authoritative = true;

            assert!(reduce(&mut state, event, &mut registry), "{name}");
            assert!(matches!(state.menu, MenuState::Loaded { .. }), "{name}");
            assert_eq!(
                state.menu_interaction,
                MenuInteractionState::default(),
                "{name}"
            );
            match name {
                "network" => assert!(state.network_popup_open),
                "audio" => assert!(state.audio_popup_open),
                "bluetooth" => assert!(state.bluetooth_popup_open),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn tray_menu_open_dismisses_global_menu_presentation_without_discarding_model() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        let global_model = match &state.menu {
            MenuState::Loaded {
                window_id,
                endpoint,
                model,
            } => (*window_id, endpoint.clone(), model.clone()),
            _ => unreachable!(),
        };
        state.global_menu_model = Some(global_model.clone());

        assert!(reduce(
            &mut state,
            Event::TrayMenuOpenRequested { endpoint: ep() },
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::Loaded { .. }));
        assert_eq!(state.menu_interaction, MenuInteractionState::default());
        assert_eq!(state.global_menu_model, Some(global_model.clone()));

        let tray = MenuEndpoint {
            service: ":1.10".into(),
            object_path: "/tray-menu".into(),
        };
        assert!(reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(u32::MAX),
                endpoint: MenuSource::Tray(tray),
                request_id: 9,
            },
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::TrayLoading { .. }));
        assert_eq!(state.global_menu_model, Some(global_model));
    }

    #[test]
    fn focus_and_menu_owner_lifecycle_still_discard_loaded_model() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::NoMenu));

        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        assert!(reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(7),
            },
            &mut registry,
        ));
        assert!(matches!(state.menu, MenuState::NoMenu));
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
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
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
                request_id: 41,
                lazy_root: false,
                intent_id: None,
                watcher_generation: None
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
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

    fn lazy_root_state() -> (State, MenuRegistry) {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        registry.register(WindowId(7), ep().service.clone(), ep().object_path.clone());
        state.focused_window = Some(WindowId(7));
        let mut model = interactive_model();
        model.root.children[0].children.clear();
        let mut second_root = model.root.children[0].clone();
        second_root.id = MenuItemId(4);
        second_root.label = Some("View".into());
        model.root.children.push(second_root);
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model,
        };
        state
            .watcher_generations
            .insert(MenuSource::DbusMenu(ep()), 10);
        (state, registry)
    }

    #[test]
    fn keyboard_entry_reuses_lazy_root_about_to_show_lifecycle() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu_presentation = Some(MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });

        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().selected_path,
            Some(vec![MenuItemId(1)])
        );
        assert!(state.menu_interaction.pending_lazy_root.is_some());
        assert!(state.menu_interaction.open_root.is_none());

        request_lazy_root(&mut state, &mut registry, 401);
        assert!(state.menu_interaction.pending_about_to_show.is_some());
    }

    #[test]
    fn keyboard_lazy_root_navigation_keys_keep_the_existing_pending_request() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu_presentation = Some(MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        let pending = state.menu_interaction.pending_lazy_root.clone();
        let session = state.menu_navigation.clone();

        assert!(!reduce(&mut state, Event::MenuNavigateRight, &mut registry));
        assert_eq!(state.menu_interaction.pending_lazy_root, pending);
        assert_eq!(state.menu_navigation, session);

        assert!(!reduce(&mut state, Event::MenuNavigateEnter, &mut registry));
        assert_eq!(state.menu_interaction.pending_lazy_root, pending);
        assert_eq!(state.menu_navigation, session);
    }

    #[test]
    fn keyboard_entry_lazy_root_error_ends_the_pending_navigation_session() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu_presentation = Some(MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        request_lazy_root(&mut state, &mut registry, 402);
        let pending = state
            .menu_interaction
            .pending_about_to_show
            .clone()
            .unwrap();

        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                item_id: pending.item_id,
                request_id: pending.request_id,
                lazy_root: true,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
                need_update: false,
                model: None,
                error: Some("failed".into()),
            },
            &mut registry,
        ));
        assert!(state.menu_navigation.is_none());
    }

    fn click_lazy_root(state: &mut State, registry: &mut MenuRegistry) {
        assert!(reduce(
            state,
            Event::MenuRootClicked(MenuItemId(1)),
            registry
        ));
    }

    fn request_lazy_root(state: &mut State, registry: &mut MenuRegistry, request_id: u64) {
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        assert!(reduce(
            state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id,
                lazy_root: true,
                intent_id: Some(pending.intent_id),
                watcher_generation: Some(pending.watcher_generation),
            },
            registry,
        ));
    }

    fn complete_lazy_root(
        state: &mut State,
        registry: &mut MenuRegistry,
        request_id: u64,
        model: Option<super::super::MenuModel>,
        need_update: bool,
    ) {
        let pending = state
            .menu_interaction
            .pending_about_to_show
            .clone()
            .unwrap();
        assert!(reduce(
            state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id,
                lazy_root: true,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
                need_update,
                model: model.clone(),
                error: None,
            },
            registry,
        ));
        if let Some(model) = model {
            let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
            let layout_request_id = request_id + 1000;
            assert!(reduce(
                state,
                Event::MenuLazyRootLayoutRequested {
                    window_id: pending.window_id,
                    endpoint: pending.endpoint.clone(),
                    request_id: layout_request_id,
                    intent_id: pending.intent_id,
                    watcher_generation: pending.watcher_generation,
                },
                registry,
            ));
            assert!(reduce(
                state,
                Event::MenuLoaded {
                    window_id: pending.window_id,
                    endpoint: pending.endpoint,
                    request_id: layout_request_id,
                    model: model.clone(),
                },
                registry,
            ));
        }
    }

    #[test]
    fn lazy_root_flow_accepts_presented_a_while_real_focus_is_b() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu_presentation = Some(super::super::state::MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        state.focused_window = Some(WindowId(8));

        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 91);
        complete_lazy_root(
            &mut state,
            &mut registry,
            91,
            Some(interactive_model()),
            true,
        );

        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert!(state.menu_interaction.open_root.is_some());
    }

    #[test]
    fn lazy_root_click_creates_intent_without_empty_popup() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        assert!(state.menu_interaction.open_root.is_none());
        assert_eq!(
            state
                .menu_interaction
                .pending_lazy_root
                .as_ref()
                .unwrap()
                .item_id,
            MenuItemId(1)
        );
    }

    #[test]
    fn lazy_root_click_accepts_about_to_show_request() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 80);
        assert!(state.menu_interaction.pending_about_to_show.is_some());
    }

    #[test]
    fn lazy_root_about_false_keeps_popup_closed_and_cleans_attempt() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 81);
        complete_lazy_root(&mut state, &mut registry, 81, None, false);
        assert!(state.menu_interaction.open_root.is_none());
        assert!(state.menu_interaction.pending_lazy_root.is_some());
    }

    #[test]
    fn lazy_root_about_true_opens_after_single_fresh_model() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 82);
        complete_lazy_root(
            &mut state,
            &mut registry,
            82,
            Some(interactive_model()),
            true,
        );
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);
    }

    #[test]
    fn lazy_root_later_model_with_children_opens_automatically() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 83);
        complete_lazy_root(
            &mut state,
            &mut registry,
            83,
            Some(interactive_model()),
            true,
        );
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
    }

    #[test]
    fn lazy_root_still_empty_never_opens_popup() {
        let (mut state, mut registry) = lazy_root_state();
        let empty = match &state.menu {
            MenuState::Loaded { model, .. } => model.clone(),
            _ => unreachable!(),
        };
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 84);
        complete_lazy_root(&mut state, &mut registry, 84, Some(empty), true);
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn lazy_root_second_click_replaces_first_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(4)),
            &mut registry
        ));
        assert_eq!(
            state
                .menu_interaction
                .pending_lazy_root
                .as_ref()
                .unwrap()
                .item_id,
            MenuItemId(4)
        );
    }

    #[test]
    fn lazy_root_focus_change_clears_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_unregister_clears_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(7),
            },
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_owner_vanish_clears_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        reduce(
            &mut state,
            Event::MenuOwnerVanished {
                sender: ep().service,
            },
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_stale_about_completion_is_rejected() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 85);
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id: 84,
                lazy_root: true,
                intent_id: None,
                watcher_generation: None,
                need_update: true,
                model: Some(interactive_model()),
                error: None,
            },
            &mut registry
        ));
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn lazy_root_old_intent_cannot_open_after_new_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 86);
        click_lazy_root(&mut state, &mut registry);
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id: 86,
                lazy_root: true,
                intent_id: None,
                watcher_generation: None,
                need_update: true,
                model: Some(interactive_model()),
                error: None,
            },
            &mut registry
        ));
    }

    #[test]
    fn populated_root_still_opens_immediately() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert_eq!(state.menu_interaction.open_root, Some(MenuItemId(1)));
    }

    #[test]
    fn non_submenu_empty_root_is_not_lazy() {
        let (mut state, mut registry) = lazy_root_state();
        if let MenuState::Loaded { model, .. } = &mut state.menu {
            model.root.children[0].children_display = None;
        }
        assert!(!reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn nested_lazy_submenu_flow_remains_supported() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction.hovered_path = vec![MenuItemId(1), MenuItemId(2)];
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(2),
                request_id: 87,
                lazy_root: false,
                intent_id: None,
                watcher_generation: None,
            },
            &mut registry
        ));
        assert!(state.menu_interaction.pending_about_to_show.is_some());
    }

    #[test]
    fn lazy_root_endpoint_identity_is_required() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(MenuEndpoint {
                    service: ":1.other".into(),
                    object_path: ep().object_path
                }),
                item_id: MenuItemId(1),
                request_id: 88,
                lazy_root: true,
                intent_id: None,
                watcher_generation: None,
            },
            &mut registry
        ));
    }

    #[test]
    fn lazy_root_window_identity_is_required() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: WindowId(8),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id: 89,
                lazy_root: true,
                intent_id: None,
                watcher_generation: None,
            },
            &mut registry
        ));
    }

    #[test]
    fn lazy_root_error_clears_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 90);
        let pending = state
            .menu_interaction
            .pending_about_to_show
            .clone()
            .unwrap();
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(1),
                request_id: 90,
                lazy_root: true,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
                need_update: false,
                model: None,
                error: Some("failed".into()),
            },
            &mut registry
        ));
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_completion_does_not_open_leaf_model() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 91);
        let mut leaf = interactive_model();
        leaf.root.children[0].children_display = None;
        complete_lazy_root(&mut state, &mut registry, 91, Some(leaf), true);
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn lazy_root_completion_reuses_current_endpoint_model() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 92);
        complete_lazy_root(
            &mut state,
            &mut registry,
            92,
            Some(interactive_model()),
            true,
        );
        assert!(matches!(
            state.menu,
            MenuState::Loaded {
                window_id: WindowId(7),
                ..
            }
        ));
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
                watcher_generation: None,
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
                watcher_generation: None,
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
                watcher_generation: None,
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
                watcher_generation: None,
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
                watcher_generation: None,
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

    fn xnm_status(
        connected: bool,
        interface: Option<&str>,
        ssid: Option<&str>,
    ) -> super::super::NetworkStatus {
        super::super::NetworkStatus {
            available: true,
            connected,
            interface: interface.map(str::to_owned),
            ssid: ssid.map(str::to_owned),
            frequency: connected.then_some(5765),
            strength: connected.then_some(74),
        }
    }

    #[test]
    fn xnm_status_is_authoritative_and_updates_summary() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::NetworkStatusChanged(xnm_status(true, Some("wlan0"), Some("Foo"))),
            &mut registry,
        ));
        assert!(state.network_status_authoritative);
        assert_eq!(state.network.display_name.as_deref(), Some("Foo"));
        assert_eq!(state.network.interface.as_deref(), Some("wlan0"));
        assert_eq!(state.network.signal_percent, Some(74));
    }

    #[test]
    fn identical_xnm_status_has_zero_delta() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let status = xnm_status(true, Some("wlan0"), Some("Foo"));
        assert!(reduce(
            &mut state,
            Event::NetworkStatusChanged(status.clone()),
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::NetworkStatusChanged(status),
            &mut registry
        ));
    }

    #[test]
    fn legacy_popup_snapshot_cannot_overwrite_xnm_status() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::NetworkStatusChanged(xnm_status(true, Some("wlan0"), Some("Foo"))),
            &mut registry,
        ));
        assert!(!reduce(
            &mut state,
            Event::NetworkSnapshotReceived(super::super::NetworkState {
                available: false,
                display_name: Some("legacy".into()),
                ..Default::default()
            }),
            &mut registry,
        ));
        assert_eq!(state.network.display_name.as_deref(), Some("Foo"));
        assert!(state.network.available);
    }

    #[test]
    fn xnm_unavailable_is_explicit_disconnected_status() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::NetworkStatusChanged(xnm_status(true, Some("wlan0"), Some("Foo"))),
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::NetworkStatusChanged(super::super::NetworkStatus::default()),
            &mut registry,
        ));
        assert!(!state.network.available);
        assert_eq!(
            state.network.connectivity,
            super::super::NetworkConnectivity::Disconnected
        );
    }

    fn ai_usage(
        provider: &str,
        agent: &str,
        account: super::super::AccountIdentity,
        remaining: Option<u16>,
        status: super::super::UsageStatus,
    ) -> super::super::ActiveAgentUsage {
        super::super::ActiveAgentUsage {
            agent_id: agent.into(),
            provider_id: provider.into(),
            account_id: account,
            display_name: agent.into(),
            active_instances: 1,
            meters: vec![super::super::UsageMeter {
                id: "primary".into(),
                label: "primary".into(),
                remaining_pct: remaining,
                used_pct: remaining.map(|value| 100 - value),
                value: Some(super::super::UsageValue::Percentage {
                    remaining_pct: remaining,
                    used_pct: remaining.map(|value| 100 - value),
                }),
                reset_at: None,
            }],
            summary: super::super::UsageSummary {
                label: "primary".into(),
                remaining_pct: remaining,
            },
            status,
            fetched_at: None,
            cache_age_secs: None,
        }
    }

    #[test]
    fn ai_usage_is_canonicalized_before_dirtying() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let mut usage = vec![
            ai_usage(
                "anthropic",
                "claude",
                super::super::AccountIdentity::Unknown,
                Some(41),
                super::super::UsageStatus::Fresh,
            ),
            ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Default,
                Some(72),
                super::super::UsageStatus::Unavailable,
            ),
        ];
        usage[1].active_instances = 2;
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(usage.clone()),
            &mut registry,
        ));
        assert_eq!(state.ai_usage.len(), 2);
        assert_eq!(state.ai_usage[0].agent_id, "claude");
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 claude 41%");
        assert_eq!(state.plugin_zone.plugins[1].text, "󰚩 codex ?");
        assert_eq!(
            state.plugin_zone.plugins[1].status,
            super::super::PluginStatus::Unavailable
        );
        assert!(!reduce(
            &mut state,
            Event::ActiveAiUsageChanged(usage.clone()),
            &mut registry,
        ));
        usage[1].status = super::super::UsageStatus::Fresh;
        usage[1].summary.remaining_pct = Some(71);
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(usage),
            &mut registry,
        ));
    }

    #[test]
    fn ai_usage_unknown_and_non_percentage_values_are_not_invented() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let mut unknown = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Named("work".into()),
            None,
            super::super::UsageStatus::Unknown,
        );
        unknown.meters.extend([
            super::super::UsageMeter {
                id: "balance".into(),
                label: "balance".into(),
                remaining_pct: None,
                used_pct: None,
                value: Some(super::super::UsageValue::Amount {
                    value: "12.00".into(),
                    unit: Some("USD".into()),
                }),
                reset_at: None,
            },
            super::super::UsageMeter {
                id: "requests".into(),
                label: "requests".into(),
                remaining_pct: None,
                used_pct: None,
                value: Some(super::super::UsageValue::Count {
                    value: 3,
                    unit: Some("requests".into()),
                }),
                reset_at: None,
            },
            super::super::UsageMeter {
                id: "note".into(),
                label: "note".into(),
                remaining_pct: None,
                used_pct: None,
                value: Some(super::super::UsageValue::Text {
                    value: "unknown".into(),
                    unit: None,
                }),
                reset_at: None,
            },
        ]);
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![unknown]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex ?");
        assert_eq!(state.ai_usage[0].meters[0].reset_at, None);
        assert!(matches!(
            state.ai_usage[0]
                .meters
                .iter()
                .find(|meter| meter.id == "balance")
                .and_then(|meter| meter.value.as_ref()),
            Some(super::super::UsageValue::Amount { .. })
        ));
        assert!(matches!(
            state.ai_usage[0]
                .meters
                .iter()
                .find(|meter| meter.id == "requests")
                .and_then(|meter| meter.value.as_ref()),
            Some(super::super::UsageValue::Count { .. })
        ));
        assert!(matches!(
            state.ai_usage[0]
                .meters
                .iter()
                .find(|meter| meter.id == "note")
                .and_then(|meter| meter.value.as_ref()),
            Some(super::super::UsageValue::Text { .. })
        ));
    }

    #[test]
    fn ai_usage_supports_stale_status_without_removing_agent() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Default,
                None,
                super::super::UsageStatus::Stale,
            )]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins.len(), 1);
        assert_eq!(
            state.plugin_zone.plugins[0].status,
            super::super::PluginStatus::Stale
        );
    }

    #[test]
    fn one_canonical_ai_agent_creates_one_namespaced_plugin() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let mut agent = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(72),
            super::super::UsageStatus::Fresh,
        );
        agent.active_instances = 2;
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![agent]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins.len(), 1);
        assert_eq!(state.ai_usage[0].active_instances, 2);
        assert_eq!(
            state.plugin_zone.plugins[0].id.0,
            "ai-usage:openai:codex:default"
        );
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 72%");
    }

    #[test]
    fn empty_ai_usage_clears_state_and_plugin_zone() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Default,
                Some(72),
                super::super::UsageStatus::Fresh,
            )]),
            &mut registry,
        );
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(Vec::new()),
            &mut registry,
        ));
        assert!(state.ai_usage.is_empty());
        assert!(state.plugin_zone.plugins.is_empty());
    }

    #[test]
    fn non_visual_ai_state_changes_update_state_without_dirtying() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let mut initial = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(72),
            super::super::UsageStatus::Fresh,
        );
        initial.fetched_at = Some(100);
        initial.active_instances = 1;
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![initial.clone()]),
            &mut registry,
        ));

        let mut updated = initial;
        updated.fetched_at = Some(200);
        updated.active_instances = 2;
        updated.status = super::super::UsageStatus::Stale;
        assert!(!reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![updated]),
            &mut registry,
        ));
        assert_eq!(state.ai_usage[0].fetched_at, Some(200));
        assert_eq!(state.ai_usage[0].active_instances, 2);
        assert_eq!(state.ai_usage[0].status, super::super::UsageStatus::Stale);
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 72%");
    }

    #[test]
    fn ai_usage_remaining_change_marks_visual_dirty() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let first = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(99),
            super::super::UsageStatus::Fresh,
        );
        reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![first]),
            &mut registry,
        );
        let mut second = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(98),
            super::super::UsageStatus::Fresh,
        );
        second.active_instances = 2;
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![second]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 98%");
    }

    #[test]
    fn ai_usage_fetched_at_only_change_is_not_visual_dirty() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let mut first = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(99),
            super::super::UsageStatus::Fresh,
        );
        first.fetched_at = Some(100);
        reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![first.clone()]),
            &mut registry,
        );
        first.fetched_at = Some(200);
        assert!(!reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![first]),
            &mut registry,
        ));
        assert_eq!(state.ai_usage[0].fetched_at, Some(200));
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 99%");
    }

    #[test]
    fn ai_usage_dirty_event_renders_without_followup_focus_event() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let codex = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(81),
            super::super::UsageStatus::Fresh,
        );
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![codex]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins.len(), 1);
    }

    #[test]
    fn two_agents_produce_two_plugin_entries() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let agents = vec![
            ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Default,
                Some(81),
                super::super::UsageStatus::Fresh,
            ),
            ai_usage(
                "anthropic",
                "claude-code",
                super::super::AccountIdentity::Default,
                None,
                super::super::UsageStatus::Unavailable,
            ),
        ];
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(agents),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins.len(), 2);
        assert!(state
            .plugin_zone
            .plugins
            .iter()
            .any(|plugin| plugin.id.0 == "ai-usage:openai:codex:default"));
        assert!(state
            .plugin_zone
            .plugins
            .iter()
            .any(|plugin| plugin.id.0 == "ai-usage:anthropic:claude-code:default"));
    }

    #[test]
    fn codex_and_claude_unavailable_both_remain_visible() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let agents = vec![
            ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Default,
                None,
                super::super::UsageStatus::Unavailable,
            ),
            ai_usage(
                "anthropic",
                "claude-code",
                super::super::AccountIdentity::Unknown,
                None,
                super::super::UsageStatus::Unknown,
            ),
        ];
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(agents),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins.len(), 2);
        assert!(state
            .plugin_zone
            .plugins
            .iter()
            .all(|plugin| plugin.text.ends_with(" ?")));
    }

    #[test]
    fn ai_state_persists_across_unrelated_focus_window_events() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let codex = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(81),
            super::super::UsageStatus::Fresh,
        );
        reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![codex]),
            &mut registry,
        );
        reduce(
            &mut state,
            Event::WindowFocusedWithApp {
                window: Some(WindowId(42)),
                app_name: Some("unrelated-window".into()),
            },
            &mut registry,
        );
        assert_eq!(state.ai_usage.len(), 1);
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 81%");
    }

    #[test]
    fn unavailable_and_unknown_never_present_accidental_percentages() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        for status in [
            super::super::UsageStatus::Unavailable,
            super::super::UsageStatus::Unknown,
        ] {
            let unavailable = matches!(&status, super::super::UsageStatus::Unavailable);
            let dirty = reduce(
                &mut state,
                Event::ActiveAiUsageChanged(vec![ai_usage(
                    "openai",
                    "codex",
                    super::super::AccountIdentity::Default,
                    Some(72),
                    status,
                )]),
                &mut registry,
            );
            assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex ?");
            if unavailable {
                assert!(dirty);
            } else {
                assert!(!dirty);
            }
        }
    }

    #[test]
    fn account_identity_change_is_dirty_even_when_text_is_equal() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();
        let first = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Default,
            Some(72),
            super::super::UsageStatus::Fresh,
        );
        reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![first]),
            &mut registry,
        );
        let second = ai_usage(
            "openai",
            "codex",
            super::super::AccountIdentity::Named("work".into()),
            Some(72),
            super::super::UsageStatus::Fresh,
        );
        assert!(reduce(
            &mut state,
            Event::ActiveAiUsageChanged(vec![second]),
            &mut registry,
        ));
        assert_eq!(state.plugin_zone.plugins[0].text, "󰚩 codex 72%");
        assert_eq!(
            state.plugin_zone.plugins[0].id.0,
            "ai-usage:openai:codex:named:work"
        );
    }

    #[test]
    fn canonical_order_is_independent_of_unique_input_order() {
        let records = vec![
            ai_usage(
                "openai",
                "codex",
                super::super::AccountIdentity::Named("work".into()),
                Some(72),
                super::super::UsageStatus::Fresh,
            ),
            ai_usage(
                "anthropic",
                "claude",
                super::super::AccountIdentity::Default,
                Some(41),
                super::super::UsageStatus::Fresh,
            ),
        ];
        let mut left = State::default();
        let mut right = State::default();
        let mut left_registry = MenuRegistry::default();
        let mut right_registry = MenuRegistry::default();
        reduce(
            &mut left,
            Event::ActiveAiUsageChanged(records.clone()),
            &mut left_registry,
        );
        reduce(
            &mut right,
            Event::ActiveAiUsageChanged(records.into_iter().rev().collect()),
            &mut right_registry,
        );
        assert_eq!(left.ai_usage, right.ai_usage);
        assert_eq!(left.plugin_zone.plugins, right.plugin_zone.plugins);
    }

    #[test]
    fn lazy_root_canonical_layout_request_binds_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 120);
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        assert!(reduce(
            &mut state,
            Event::MenuLazyRootLayoutRequested {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                request_id: 121,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
            },
            &mut registry,
        ));
        assert_eq!(
            state
                .menu_interaction
                .pending_lazy_root
                .unwrap()
                .layout_request_id,
            Some(121)
        );
    }

    #[test]
    fn stale_canonical_layout_cannot_open_lazy_root() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 122);
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        reduce(
            &mut state,
            Event::MenuLazyRootLayoutRequested {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                request_id: 123,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
            },
            &mut registry,
        );
        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                request_id: 122,
                model: interactive_model(),
            },
            &mut registry
        ));
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn lazy_root_watcher_replacement_invalidates_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        assert!(state.menu_interaction.pending_lazy_root.is_some());
        reduce(
            &mut state,
            Event::MenuWatcherReady {
                endpoint: MenuSource::DbusMenu(ep()),
                watcher_generation: 11,
                request_id: 124,
            },
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_does_not_enter_loading_before_about_completion() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 125);
        assert!(matches!(state.menu, MenuState::Loaded { .. }));
    }

    #[test]
    fn lazy_root_about_error_leaves_layout_authority_unarmed() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 126);
        let pending = state
            .menu_interaction
            .pending_about_to_show
            .clone()
            .unwrap();
        reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                item_id: pending.item_id,
                request_id: pending.request_id,
                lazy_root: true,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
                need_update: false,
                model: None,
                error: Some("failure".into()),
            },
            &mut registry,
        );
        assert!(matches!(state.menu, MenuState::Loaded { .. }));
        assert!(state.menu_interaction.pending_lazy_root.is_none());
    }

    #[test]
    fn lazy_root_same_generation_ready_keeps_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        reduce(
            &mut state,
            Event::MenuWatcherReady {
                endpoint: MenuSource::DbusMenu(ep()),
                watcher_generation: 10,
                request_id: 127,
            },
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_some());
    }

    #[test]
    fn lazy_root_layout_request_is_single_canonical_transition() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 128);
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        assert!(reduce(
            &mut state,
            Event::MenuLazyRootLayoutRequested {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                request_id: 129,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
            },
            &mut registry
        ));
        assert!(matches!(
            state.menu,
            MenuState::Loading {
                request_id: 129,
                ..
            }
        ));
    }

    #[test]
    fn lazy_root_empty_snapshot_can_finish_attempt_without_opening() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 130);
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        let empty = match &state.menu {
            MenuState::Loaded { model, .. } => model.clone(),
            _ => unreachable!(),
        };
        reduce(
            &mut state,
            Event::MenuLazyRootLayoutRequested {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                request_id: 131,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                request_id: 131,
                model: empty,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLazyRootLoadConvergence {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                request_id: 131,
                follow_up_request_id: None,
            },
            &mut registry,
        );
        assert!(state.menu_interaction.pending_lazy_root.is_none());
        assert!(state.menu_interaction.open_root.is_none());
    }

    #[test]
    fn lazy_root_empty_snapshot_rebinds_follow_up_without_new_intent() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 132);
        let pending = state.menu_interaction.pending_lazy_root.clone().unwrap();
        let empty = match &state.menu {
            MenuState::Loaded { model, .. } => model.clone(),
            _ => unreachable!(),
        };
        reduce(
            &mut state,
            Event::MenuLazyRootLayoutRequested {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                request_id: 133,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                request_id: 133,
                model: empty,
            },
            &mut registry,
        );
        reduce(
            &mut state,
            Event::MenuLazyRootLoadConvergence {
                window_id: pending.window_id,
                endpoint: pending.endpoint,
                request_id: 133,
                follow_up_request_id: Some(134),
            },
            &mut registry,
        );
        let rebound = state.menu_interaction.pending_lazy_root.unwrap();
        assert_eq!(rebound.intent_id, pending.intent_id);
        assert_eq!(rebound.layout_request_id, Some(134));
    }

    #[test]
    fn lazy_root_tools_view_tools_rejects_both_older_about_completions() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        let first = state.menu_interaction.pending_lazy_root.clone().unwrap();
        request_lazy_root(&mut state, &mut registry, 140);
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(4)),
            &mut registry
        ));
        assert!(state.menu_interaction.pending_lazy_root.is_some());
        let middle = state.menu_interaction.pending_lazy_root.clone().unwrap();
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: middle.window_id,
                endpoint: middle.endpoint.clone(),
                item_id: middle.item_id,
                request_id: 141,
                lazy_root: true,
                intent_id: Some(middle.intent_id),
                watcher_generation: Some(middle.watcher_generation),
            },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        let second = state.menu_interaction.pending_lazy_root.clone().unwrap();
        assert_ne!(first.intent_id, middle.intent_id);
        assert_ne!(middle.intent_id, second.intent_id);
        assert!(!reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: first.window_id,
                endpoint: first.endpoint.clone(),
                item_id: first.item_id,
                request_id: 140,
                lazy_root: true,
                intent_id: Some(first.intent_id),
                watcher_generation: Some(first.watcher_generation),
                need_update: false,
                model: None,
                error: None,
            },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowRequested {
                window_id: second.window_id,
                endpoint: second.endpoint.clone(),
                item_id: second.item_id,
                request_id: 142,
                lazy_root: true,
                intent_id: Some(second.intent_id),
                watcher_generation: Some(second.watcher_generation),
            },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: second.window_id,
                endpoint: second.endpoint,
                item_id: second.item_id,
                request_id: 142,
                lazy_root: true,
                intent_id: Some(second.intent_id),
                watcher_generation: Some(second.watcher_generation),
                need_update: false,
                model: None,
                error: None,
            },
            &mut registry
        ));
    }

    #[test]
    fn lazy_root_error_then_invalidation_keeps_normal_model_authority_available() {
        let (mut state, mut registry) = lazy_root_state();
        click_lazy_root(&mut state, &mut registry);
        request_lazy_root(&mut state, &mut registry, 142);
        let pending = state
            .menu_interaction
            .pending_about_to_show
            .clone()
            .unwrap();
        assert!(reduce(
            &mut state,
            Event::MenuAboutToShowCompleted {
                window_id: pending.window_id,
                endpoint: pending.endpoint.clone(),
                item_id: pending.item_id,
                request_id: pending.request_id,
                lazy_root: true,
                intent_id: pending.intent_id,
                watcher_generation: pending.watcher_generation,
                need_update: false,
                model: None,
                error: Some("failed".into()),
            },
            &mut registry
        ));
        assert!(state.menu_interaction.pending_about_to_show.is_none());
        assert!(state.menu_interaction.pending_lazy_root.is_none());
        assert!(matches!(state.menu, MenuState::Loaded { .. }));
        assert!(!reduce(
            &mut state,
            Event::MenuLayoutInvalidated {
                endpoint: pending.endpoint,
                watcher_generation: Some(10),
                revision: Some(9),
            },
            &mut registry
        ));
    }

    #[test]
    fn keyboard_session_pins_presentation_and_keeps_focus_live() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        let session = state.menu_navigation.clone().unwrap();
        assert_eq!(session.source_window, super::super::WindowId(7));
        assert!(session.selected_path.is_some());
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(super::super::WindowId(8))),
            &mut registry
        ));
        assert_eq!(state.focused_window, Some(super::super::WindowId(8)));
        assert_eq!(
            state.menu_presentation_window(),
            Some(super::super::WindowId(7))
        );
        assert_eq!(state.menu_navigation.as_ref().unwrap().id, session.id);
    }

    #[test]
    fn keyboard_session_ids_are_monotonic_and_grab_results_are_fenced() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        let first = state.menu_navigation.as_ref().unwrap().id;
        assert!(reduce(
            &mut state,
            Event::KeyboardGrabAcquired { session_id: first },
            &mut registry
        ));
        reduce(&mut state, Event::MenuNavigateEscape, &mut registry);
        open_keyboard_navigation(&mut state, &mut registry);
        let second = state.menu_navigation.as_ref().unwrap().id;
        assert!(second > first);
        assert!(!reduce(
            &mut state,
            Event::KeyboardGrabAcquired { session_id: first },
            &mut registry
        ));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().grab_state,
            KeyboardGrabState::Requested
        );
    }

    #[test]
    fn keyboard_navigation_skips_nonselectable_items_without_wrapping() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        assert!(reduce(&mut state, Event::MenuNavigateDown, &mut registry));
        let selected = state
            .menu_navigation
            .as_ref()
            .unwrap()
            .selected_path
            .clone();
        assert!(selected.is_some());
        assert!(!reduce(&mut state, Event::MenuNavigateUp, &mut registry));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().selected_path,
            Some(vec![MenuItemId(1), MenuItemId(2)])
        );
    }

    #[test]
    fn keyboard_escape_nested_unwinds_then_root_tears_down() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: super::super::WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        state.menu_interaction.open_path = vec![MenuItemId(1), MenuItemId(2)];
        state.menu_navigation.as_mut().unwrap().selected_path =
            Some(vec![MenuItemId(1), MenuItemId(2)]);
        assert!(reduce(&mut state, Event::MenuNavigateEscape, &mut registry));
        assert!(state.menu_navigation.is_some());
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);
        assert!(reduce(&mut state, Event::MenuNavigateEscape, &mut registry));
        assert!(state.menu_navigation.is_none());
    }

    #[test]
    fn keyboard_grab_failure_keeps_menu_mouse_operable() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        let session_id = state.menu_navigation.as_ref().unwrap().id;
        assert!(reduce(
            &mut state,
            Event::KeyboardGrabFailed { session_id },
            &mut registry
        ));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().grab_state,
            KeyboardGrabState::Failed
        );
        assert!(reduce(&mut state, Event::MenuNavigateDown, &mut registry));
        assert!(state.menu_navigation.is_some());
    }

    #[test]
    fn keyboard_nested_right_and_left_share_open_path() {
        let (mut state, mut registry) = lazy_root_state();
        state.menu = MenuState::Loaded {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
            model: interactive_model(),
        };
        state.menu_presentation = Some(MenuPresentation {
            window_id: WindowId(7),
            endpoint: MenuSource::DbusMenu(ep()),
        });
        open_keyboard_navigation(&mut state, &mut registry);
        assert!(reduce(&mut state, Event::MenuNavigateDown, &mut registry));
        assert!(!reduce(
            &mut state,
            Event::MenuItemHovered {
                path: vec![MenuItemId(1), MenuItemId(2)],
            },
            &mut registry
        ));
        assert_eq!(
            state.menu_navigation.as_ref().unwrap().selected_path,
            Some(vec![MenuItemId(1), MenuItemId(2)])
        );
        assert!(reduce(&mut state, Event::MenuNavigateRight, &mut registry));
        assert_eq!(
            state.menu_interaction.open_path,
            vec![MenuItemId(1), MenuItemId(2)]
        );
        assert!(reduce(&mut state, Event::MenuNavigateLeft, &mut registry));
        assert_eq!(state.menu_interaction.open_path, vec![MenuItemId(1)]);
    }

    #[test]
    fn keyboard_teardown_reconciles_to_latest_focus() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        let source_c = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.30".into(),
            object_path: "/menu-c".into(),
        });
        registry.register(WindowId(9), ":1.30".into(), "/menu-c".into());
        open_keyboard_navigation(&mut state, &mut registry);
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(9))),
            &mut registry
        ));
        assert_eq!(state.focused_window, Some(WindowId(9)));
        assert!(reduce(&mut state, Event::MenuNavigateEscape, &mut registry));
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation,
            Some(MenuPresentation {
                window_id: WindowId(9),
                endpoint: source_c,
            })
        );
    }

    #[test]
    fn unrelated_window_destruction_does_not_end_pinned_session() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        open_keyboard_navigation(&mut state, &mut registry);
        assert!(!reduce(
            &mut state,
            Event::X11(crate::platform::x11::X11Event::GtkWindowDestroyed(
                WindowId(9),
            )),
            &mut registry
        ));
        assert!(state.menu_navigation.is_some());
        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
    }

    #[test]
    fn explicit_pin_keeps_presentation_without_navigation_and_unpins_to_current_focus() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        let endpoint_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.20".into(),
            object_path: "/menu-b".into(),
        });
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(matches!(
            &state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned { workspace } if workspace == "1"
        ));
        assert!(state.menu_navigation.is_none());
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert_eq!(state.focused_window, Some(WindowId(8)));
        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert!(reduce(
            &mut state,
            Event::UnpinMenuPresentation,
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation,
            Some(MenuPresentation {
                window_id: WindowId(8),
                endpoint: endpoint_b,
            })
        );
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
    }

    #[test]
    fn navigation_end_keeps_explicit_pin_but_reconciles_without_one() {
        let (mut pinned, mut registry) = loaded_menu_with_open_presentation();
        pinned.menu_interaction = MenuInteractionState::default();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        assert!(reduce(
            &mut pinned,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        open_keyboard_navigation(&mut pinned, &mut registry);
        assert!(reduce(
            &mut pinned,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert!(reduce(
            &mut pinned,
            Event::MenuNavigateEscape,
            &mut registry
        ));
        assert_eq!(pinned.menu_presentation_window(), Some(WindowId(7)));
        assert!(matches!(
            pinned.menu_presentation_policy,
            MenuPresentationPolicy::Pinned { .. }
        ));

        let (mut temporary, mut registry) = loaded_menu_with_open_presentation();
        temporary.menu_interaction = MenuInteractionState::default();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        open_keyboard_navigation(&mut temporary, &mut registry);
        assert!(reduce(
            &mut temporary,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert!(reduce(
            &mut temporary,
            Event::MenuNavigateEscape,
            &mut registry
        ));
        assert_eq!(temporary.menu_presentation_window(), Some(WindowId(8)));
        assert_eq!(
            temporary.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
    }

    #[test]
    fn pinned_source_invalidation_unpins_but_unrelated_source_does_not() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(9),
            },
            &mut registry
        ));
        assert!(matches!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned { .. }
        ));
        assert!(reduce(
            &mut state,
            Event::MenuUnregistered {
                window_id: WindowId(7),
            },
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert_eq!(state.menu_presentation_window(), Some(WindowId(8)));
    }

    #[test]
    fn presentation_pin_toggle_switches_between_pin_and_follow_focus() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::ToggleMenuPresentationPin,
            &mut registry
        ));
        assert!(matches!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned { .. }
        ));
        assert!(reduce(
            &mut state,
            Event::ToggleMenuPresentationPin,
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
    }

    #[test]
    fn pin_toggle_ends_active_navigation_only_when_disarming_an_explicit_pin() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::ToggleMenuPresentationPin,
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert!(state.menu_navigation.is_some());

        assert!(reduce(
            &mut state,
            Event::ToggleMenuPresentationPin,
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert!(state.menu_navigation.is_none());
    }

    #[test]
    fn pin_without_a_current_presentation_is_a_noop() {
        let mut state = State::default();
        let mut registry = MenuRegistry::default();

        assert!(!reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::ToggleMenuPresentationPin,
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert!(state.menu_navigation.is_none());
    }

    #[test]
    fn pin_requires_a_named_current_workspace() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.focused_workspace = None;
        assert!(!reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        state.focused_workspace = Some(String::new());
        assert!(!reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
    }

    #[test]
    fn idle_pin_keeps_one_presentation_across_focus_changes() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        let presentation = state.menu_presentation.clone();
        let model = state.active_menu_model().cloned();
        state.menu_interaction = MenuInteractionState::default();

        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        for window_id in [WindowId(6), WindowId(8), WindowId(9)] {
            assert!(reduce(
                &mut state,
                Event::WindowFocused(Some(window_id)),
                &mut registry
            ));
        }

        assert_eq!(state.focused_window, Some(WindowId(9)));
        assert_eq!(state.menu_presentation, presentation);
        assert_eq!(state.active_menu_model(), model.as_ref());
        assert!(state.menu_navigation.is_none());
    }

    #[test]
    fn mouse_popup_open_and_close_do_not_start_navigation_or_unpin() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));

        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(state.menu_interaction.open_root.is_some());
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned {
                workspace: "1".into()
            }
        );

        assert!(reduce(&mut state, Event::MenuClickedOutside, &mut registry));
        assert!(state.menu_interaction.open_root.is_none());
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned {
                workspace: "1".into()
            }
        );
    }

    #[test]
    fn explicit_pin_survives_terminal_action_and_outside_click() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        open_keyboard_navigation(&mut state, &mut registry);
        state.menu_interaction.open_path = vec![MenuItemId(1), MenuItemId(2)];
        state.menu_navigation.as_mut().unwrap().selected_path =
            Some(vec![MenuItemId(1), MenuItemId(2), MenuItemId(3)]);
        assert!(reduce(
            &mut state,
            Event::MenuItemActivateRequested {
                window_id: WindowId(7),
                endpoint: MenuSource::DbusMenu(ep()),
                item_id: MenuItemId(3),
                timestamp: 0,
            },
            &mut registry
        ));
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned {
                workspace: "1".into()
            }
        );

        assert!(reduce(
            &mut state,
            Event::MenuRootClicked(MenuItemId(1)),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuNavigationStarted,
            &mut registry
        ));
        assert!(reduce(&mut state, Event::MenuClickedOutside, &mut registry));
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned {
                workspace: "1".into()
            }
        );
    }

    #[test]
    fn idle_pin_accepts_only_the_presented_source_load_result() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        let endpoint = MenuSource::DbusMenu(ep());
        assert!(reduce(
            &mut state,
            Event::MenuLoadRequested {
                window_id: WindowId(7),
                endpoint: endpoint.clone(),
                request_id: 91,
            },
            &mut registry
        ));
        assert!(!reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(8),
                endpoint: endpoint.clone(),
                request_id: 91,
                model: model(),
            },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuLoaded {
                window_id: WindowId(7),
                endpoint,
                request_id: 91,
                model: interactive_model(),
            },
            &mut registry
        ));
        assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::Pinned {
                workspace: "1".into()
            }
        );
        assert!(state.menu_navigation.is_none());
    }

    #[test]
    fn endpoint_owner_vanish_invalidates_only_the_explicit_pin() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::MenuOwnerVanished {
                sender: ":1.9".into(),
            },
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert_eq!(state.menu_presentation_window(), Some(WindowId(8)));
    }

    #[test]
    fn workspace_change_ends_idle_pin_without_reactivating_or_moving_its_source() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        let endpoint_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.20".into(),
            object_path: "/menu-b".into(),
        });
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));

        for window_id in [WindowId(6), WindowId(8)] {
            assert!(reduce(
                &mut state,
                Event::WindowFocused(Some(window_id)),
                &mut registry
            ));
            assert!(matches!(
                &state.menu_presentation_policy,
                MenuPresentationPolicy::Pinned { workspace } if workspace == "1"
            ));
            assert_eq!(state.menu_presentation_window(), Some(WindowId(7)));
        }

        assert!(reduce(
            &mut state,
            Event::WorkspaceFocused {
                name: Some("2".into()),
            },
            &mut registry
        ));
        assert_eq!(state.focused_workspace.as_deref(), Some("2"));
        assert_eq!(state.focused_window, Some(WindowId(8)));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert_eq!(state.menu_presentation, None);
        assert!(state.menu_navigation.is_none());
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation,
            Some(MenuPresentation {
                window_id: WindowId(8),
                endpoint: endpoint_b,
            })
        );

        assert!(reduce(
            &mut state,
            Event::WorkspaceFocused {
                name: Some("1".into()),
            },
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
    }

    #[test]
    fn workspace_change_ends_active_navigation_and_workspace_snapshot_does_the_same() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));
        open_keyboard_navigation(&mut state, &mut registry);
        let session_id = state.menu_navigation.as_ref().unwrap().id;
        assert!(reduce(
            &mut state,
            Event::KeyboardGrabAcquired { session_id },
            &mut registry
        ));
        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));

        assert!(reduce(
            &mut state,
            Event::WorkspacesSnapshot(vec![ws("1", false), ws("2", true)]),
            &mut registry
        ));
        assert!(state.menu_navigation.is_none());
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );
        assert_eq!(state.focused_workspace.as_deref(), Some("2"));
    }

    #[test]
    fn workspace_event_before_focus_clears_old_presentation_then_converges_to_new_focus() {
        let (mut state, mut registry) = loaded_menu_with_open_presentation();
        let endpoint_b = MenuSource::DbusMenu(MenuEndpoint {
            service: ":1.20".into(),
            object_path: "/menu-b".into(),
        });
        registry.register(WindowId(8), ":1.20".into(), "/menu-b".into());
        state.menu_interaction = MenuInteractionState::default();
        assert!(reduce(
            &mut state,
            Event::PinCurrentMenuPresentation,
            &mut registry
        ));

        assert!(reduce(
            &mut state,
            Event::WorkspaceFocused {
                name: Some("2".into()),
            },
            &mut registry
        ));
        assert_eq!(state.menu_presentation, None);
        assert_eq!(
            state.menu_presentation_policy,
            MenuPresentationPolicy::FollowFocus
        );

        assert!(reduce(
            &mut state,
            Event::WindowFocused(Some(WindowId(8))),
            &mut registry
        ));
        assert_eq!(
            state.menu_presentation,
            Some(MenuPresentation {
                window_id: WindowId(8),
                endpoint: endpoint_b,
            })
        );
    }
}
