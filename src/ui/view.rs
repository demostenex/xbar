use crate::core::{
    ClockState, MenuItemId, MenuModel, NetworkConnectivity, NetworkLinkKind, StatusNotifierIcon,
    StatusNotifierItem, StatusNotifierStatus,
};
use crate::ui::layout::{allocate_tray, MenuRect, WorkspaceRect};
use crate::ui::style::{TextMeasurer, BAR_STYLE, STATUS_ITEM_GAP};

#[derive(Clone, Debug, PartialEq)]
pub struct MenuVisualItem {
    pub id: MenuItemId,
    pub rect: MenuRect,
    pub label: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextView {
    pub workspaces: Vec<WorkspaceRect>,
    pub menu: Vec<MenuVisualItem>,
    pub future: MenuRect,
    pub app_name: Option<AppNameVisual>,
    pub audio: Option<AudioVisual>,
    pub network: Option<NetworkVisual>,
    pub bluetooth: Option<BluetoothVisual>,
    pub tray: Vec<TrayVisualItem>,
    pub plugins: Vec<PluginVisualItem>,
    pub datetime: Option<DateTimeVisual>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PluginVisualItem {
    pub id: crate::core::PluginId,
    pub text: String,
    pub rect: MenuRect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrayVisualItem {
    pub endpoint: crate::core::StatusNotifierEndpoint,
    pub icon: StatusNotifierIcon,
    pub rect: MenuRect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DateTimeVisual {
    pub text: String,
    pub rect: MenuRect,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AppNameVisual {
    pub text: String,
    pub rect: MenuRect,
}
#[derive(Clone, Debug, PartialEq)]
pub struct AudioVisual {
    pub text: String,
    pub rect: MenuRect,
}
#[derive(Clone, Debug, PartialEq)]
pub struct NetworkVisual {
    pub text: String,
    pub rect: MenuRect,
}
#[derive(Clone, Debug, PartialEq)]
pub struct BluetoothVisual {
    pub text: String,
    pub rect: MenuRect,
}

pub fn audio_glyph(audio: &crate::core::AudioState) -> &'static str {
    if audio.muted || audio.volume_percent == 0 {
        "󰖁"
    } else if audio.volume_percent <= 33 {
        "󰕿"
    } else if audio.volume_percent <= 66 {
        "󰖀"
    } else {
        "󰕾"
    }
}

pub fn microphone_glyph(audio: &crate::core::AudioState) -> &'static str {
    if audio.input_muted || audio.input_volume_percent == 0 {
        "󰍭"
    } else {
        "󰍬"
    }
}

pub fn network_glyph(network: &crate::core::NetworkState) -> &'static str {
    if !network.available || matches!(network.connectivity, NetworkConnectivity::Disconnected) {
        return "󰤮";
    }
    if matches!(network.link_kind, NetworkLinkKind::Ethernet) {
        return "󰈀";
    }
    match network.signal_percent.unwrap_or(0) {
        0..=33 => "󰤟",
        34..=66 => "󰤢",
        _ => "󰤨",
    }
}

pub fn bluetooth_glyph(bluetooth: &crate::core::BluetoothState) -> Option<&'static str> {
    if !bluetooth.available {
        None
    } else if !bluetooth.powered {
        Some("󰂲")
    } else if bluetooth.devices.iter().any(|device| device.connected) {
        Some("󰂱")
    } else {
        Some("󰂯")
    }
}

pub fn capitalize_app_name(name: &str) -> String {
    let trimmed = name.trim();
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    first.to_uppercase().chain(chars).collect()
}

#[allow(dead_code)]
pub fn context_view(
    output: &crate::core::OutputState,
    workspaces: &[crate::core::WorkspaceState],
    model: Option<&MenuModel>,
    clock: Option<&ClockState>,
    tray_items: &[StatusNotifierItem],
) -> ContextView {
    context_view_with_app_name(
        output, workspaces, model, clock, tray_items, None, &BAR_STYLE,
    )
}

pub fn context_view_with_app_name<M: TextMeasurer>(
    output: &crate::core::OutputState,
    workspaces: &[crate::core::WorkspaceState],
    model: Option<&MenuModel>,
    clock: Option<&ClockState>,
    tray_items: &[StatusNotifierItem],
    app_name: Option<&str>,
    measurer: &M,
) -> ContextView {
    context_view_with_app_name_and_audio(
        output, workspaces, model, clock, tray_items, app_name, None, None, measurer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn context_view_with_app_name_and_audio<M: TextMeasurer>(
    output: &crate::core::OutputState,
    workspaces: &[crate::core::WorkspaceState],
    model: Option<&MenuModel>,
    clock: Option<&ClockState>,
    tray_items: &[StatusNotifierItem],
    app_name: Option<&str>,
    audio: Option<&crate::core::AudioState>,
    network: Option<&crate::core::NetworkState>,
    measurer: &M,
) -> ContextView {
    context_view_with_app_name_and_audio_and_bluetooth(
        output, workspaces, model, clock, tray_items, app_name, audio, network, None, measurer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn context_view_with_app_name_and_audio_and_bluetooth<M: TextMeasurer>(
    output: &crate::core::OutputState,
    workspaces: &[crate::core::WorkspaceState],
    model: Option<&MenuModel>,
    clock: Option<&ClockState>,
    tray_items: &[StatusNotifierItem],
    app_name: Option<&str>,
    audio: Option<&crate::core::AudioState>,
    network: Option<&crate::core::NetworkState>,
    bluetooth: Option<&crate::core::BluetoothState>,
    measurer: &M,
) -> ContextView {
    context_view_with_app_name_and_audio_and_bluetooth_and_plugins(
        output,
        workspaces,
        model,
        clock,
        tray_items,
        app_name,
        audio,
        network,
        bluetooth,
        &[],
        measurer,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn context_view_with_app_name_and_audio_and_bluetooth_and_plugins<M: TextMeasurer>(
    output: &crate::core::OutputState,
    workspaces: &[crate::core::WorkspaceState],
    model: Option<&MenuModel>,
    clock: Option<&ClockState>,
    tray_items: &[StatusNotifierItem],
    app_name: Option<&str>,
    audio: Option<&crate::core::AudioState>,
    network: Option<&crate::core::NetworkState>,
    bluetooth: Option<&crate::core::BluetoothState>,
    plugins: &[crate::core::PluginSummary],
    measurer: &M,
) -> ContextView {
    let focused_workspace = workspaces
        .iter()
        .filter(|workspace| workspace.focused)
        .take(1)
        .cloned()
        .collect::<Vec<_>>();
    let visible = match model {
        Some(model) => model
            .root
            .children
            .iter()
            .filter(|item| item.visible)
            .filter_map(|item| {
                item.label
                    .as_ref()
                    .map(|label| (item, present_label(label)))
            })
            .map(|(item, label)| (item.id, label, item.enabled))
            .collect::<Vec<_>>(),
        None => Vec::new(),
    };
    let datetime = clock.map(format_clock);
    let (workspace_rects, mut menu_rects, datetime_rect, mut future) =
        crate::ui::layout::allocate_context_with_measurer(
            output,
            &focused_workspace,
            &visible,
            datetime.as_deref(),
            measurer,
        );
    let tray_items = tray_items
        .iter()
        .filter(|item| !matches!(item.status, StatusNotifierStatus::Passive))
        .filter_map(|item| item.icon.as_ref().map(|icon| (item, icon.clone())))
        .collect::<Vec<_>>();
    let app_name = app_name
        .filter(|text| !text.is_empty())
        .map(capitalize_app_name)
        .filter(|text| !text.is_empty())
        .and_then(|text| truncate_text(&text, future.width.saturating_sub(8), measurer));
    let app_visual = app_name.map(|text| {
        let width = (measurer.measure_width(&text) + 12).min(future.width);
        let app_left = workspace_rects
            .first()
            .map(|rect| rect.x + rect.width as i16 + 8)
            .unwrap_or(future.x);
        let rect = MenuRect {
            x: app_left,
            y: future.y,
            width,
            height: future.height,
        };
        let shift = width as i16 + 16;
        for menu in &mut menu_rects {
            menu.x += shift;
        }
        future.x += shift;
        future.width = future.width.saturating_sub(shift as u16);
        AppNameVisual { text, rect }
    });
    let audio_enabled = audio.is_some_and(|audio| audio.available);
    let glyphs = ["󰖁", "󰕿", "󰖀", "󰕾"];
    let audio_width = if audio_enabled {
        glyphs
            .iter()
            .map(|glyph| measurer.measure_status_icon_width(glyph))
            .max()
            .unwrap_or(0)
            .max(12)
            + 12
    } else {
        0
    };
    let network_enabled = network.is_some_and(|network| network.available);
    let network_width = if network_enabled { 24 } else { 0 };
    let bluetooth_enabled = bluetooth.and_then(bluetooth_glyph).is_some();
    let bluetooth_width = if bluetooth_enabled { 24 } else { 0 };
    let status_right = future.x + future.width as i16;
    let audio_left = status_right - audio_width as i16;
    let bluetooth_left = audio_left
        - if bluetooth_enabled && audio_enabled {
            STATUS_ITEM_GAP
        } else {
            0
        }
        - bluetooth_width as i16;
    let network_right = if bluetooth_enabled {
        bluetooth_left
    } else {
        audio_left
    };
    let network_left = network_right
        - if network_enabled && (bluetooth_enabled || audio_enabled) {
            STATUS_ITEM_GAP
        } else {
            0
        }
        - network_width as i16;
    let tray_left = network_left
        - if !tray_items.is_empty() && (network_enabled || audio_enabled) {
            STATUS_ITEM_GAP
        } else {
            0
        };
    let tray_future = MenuRect {
        x: future.x,
        y: future.y,
        width: tray_left.saturating_sub(future.x) as u16,
        height: future.height,
    };
    let tray_rects = allocate_tray(tray_future, tray_items.len());
    let plugin_labels = plugins
        .iter()
        .map(|plugin| plugin.text.clone())
        .collect::<Vec<_>>();
    let plugin_right = tray_rects.first().map(|rect| rect.x).unwrap_or(tray_left);
    let plugin_rects =
        crate::ui::layout::allocate_plugins(future.x, plugin_right, &plugin_labels, measurer);
    let plugin_left = plugin_rects
        .first()
        .map(|rect| rect.x)
        .unwrap_or(plugin_right);
    future.width = plugin_left.saturating_sub(future.x) as u16;
    let plugin_start = plugins.len().saturating_sub(plugin_rects.len());
    let plugins = plugins
        .iter()
        .skip(plugin_start)
        .zip(plugin_rects)
        .map(|(plugin, mut rect)| {
            rect.y = future.y;
            PluginVisualItem {
                id: plugin.id.clone(),
                text: plugin.text.clone(),
                rect,
            }
        })
        .collect();
    let audio = audio.filter(|audio| audio.available).and_then(|audio| {
        (audio_width > 0).then(|| AudioVisual {
            text: audio_glyph(audio).to_owned(),
            rect: MenuRect {
                x: audio_left,
                y: future.y,
                width: audio_width.max(24),
                height: future.height,
            },
        })
    });
    let network = network
        .filter(|network| network.available)
        .map(|network| NetworkVisual {
            text: network_glyph(network).to_owned(),
            rect: MenuRect {
                x: network_left,
                y: future.y,
                width: network_width,
                height: future.height,
            },
        });
    let bluetooth = bluetooth.and_then(|bluetooth| {
        bluetooth_glyph(bluetooth).map(|text| BluetoothVisual {
            text: text.to_owned(),
            rect: MenuRect {
                x: bluetooth_left,
                y: future.y,
                width: bluetooth_width,
                height: future.height,
            },
        })
    });
    ContextView {
        workspaces: workspace_rects,
        menu: visible
            .into_iter()
            .zip(menu_rects)
            .map(|((id, label, enabled), rect)| MenuVisualItem {
                id,
                rect,
                label,
                enabled,
            })
            .collect(),
        future,
        app_name: app_visual,
        audio,
        network,
        bluetooth,
        plugins,
        tray: tray_items
            .into_iter()
            .zip(tray_rects)
            .map(|((item, icon), rect)| TrayVisualItem {
                endpoint: item.endpoint.clone(),
                icon,
                rect,
            })
            .collect(),
        datetime: datetime_rect.map(|rect| DateTimeVisual {
            text: datetime.expect("datetime text exists when rect exists"),
            rect,
        }),
    }
}

fn truncate_text<M: TextMeasurer>(text: &str, width: u16, measurer: &M) -> Option<String> {
    if measurer.measure_width(text) <= width {
        return Some(text.to_owned());
    }
    let ellipsis = "…";
    if measurer.measure_width(ellipsis) > width {
        return None;
    }
    let mut result = String::new();
    for ch in text.chars() {
        let candidate = format!("{result}{ch}{ellipsis}");
        if measurer.measure_width(&candidate) > width {
            break;
        }
        result.push(ch);
    }
    Some(format!("{result}{ellipsis}"))
}

pub fn format_clock(clock: &ClockState) -> String {
    format!(
        "{:02}:{:02} {:02}/{:02}",
        clock.hour, clock.minute, clock.day, clock.month
    )
}

/// DBusMenu uses `_x` for a mnemonic and `__` for a literal underscore.
/// Mnemonic presentation belongs here, never in the domain model.
pub fn present_label(label: &str) -> String {
    let mut result = String::with_capacity(label.len());
    let mut chars = label.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '_' {
            match chars.peek() {
                Some('_') => {
                    result.push('_');
                    chars.next();
                }
                Some(next) if !next.is_whitespace() => {}
                _ => result.push('_'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_glyph_bands_follow_volume() {
        let mut audio = crate::core::AudioState {
            available: true,
            ..Default::default()
        };
        assert_eq!(audio_glyph(&audio), "󰖁");
        audio.volume_percent = 10;
        assert_eq!(audio_glyph(&audio), "󰕿");
        audio.volume_percent = 50;
        assert_eq!(audio_glyph(&audio), "󰖀");
        audio.volume_percent = 90;
        assert_eq!(audio_glyph(&audio), "󰕾");
        audio.muted = true;
        assert_eq!(audio_glyph(&audio), "󰖁");
    }

    #[test]
    fn microphone_glyph_reflects_confirmed_mute_state() {
        let mut audio = crate::core::AudioState {
            available: true,
            input_volume_percent: 40,
            ..Default::default()
        };
        assert_eq!(microphone_glyph(&audio), "󰍬");
        audio.input_muted = true;
        assert_eq!(microphone_glyph(&audio), "󰍭");
        audio.input_muted = false;
        audio.input_volume_percent = 0;
        assert_eq!(microphone_glyph(&audio), "󰍭");
    }

    #[test]
    fn network_glyph_distinguishes_connection_and_signal_bands() {
        let mut network = crate::core::NetworkState {
            available: true,
            connectivity: NetworkConnectivity::Connected,
            link_kind: NetworkLinkKind::Wifi,
            signal_percent: Some(20),
            ..Default::default()
        };
        assert_eq!(network_glyph(&network), "󰤟");
        network.signal_percent = Some(50);
        assert_eq!(network_glyph(&network), "󰤢");
        network.signal_percent = Some(80);
        assert_eq!(network_glyph(&network), "󰤨");
        network.link_kind = NetworkLinkKind::Ethernet;
        assert_eq!(network_glyph(&network), "󰈀");
        network.connectivity = NetworkConnectivity::Disconnected;
        assert_eq!(network_glyph(&network), "󰤮");
    }

    #[test]
    fn network_slot_is_before_audio_and_datetime() {
        let audio = crate::core::AudioState {
            available: true,
            ..Default::default()
        };
        let network = crate::core::NetworkState {
            available: true,
            connectivity: NetworkConnectivity::Connected,
            link_kind: NetworkLinkKind::Wifi,
            signal_percent: Some(80),
            ..Default::default()
        };
        let view = context_view_with_app_name_and_audio(
            &output(640),
            &workspace(),
            None,
            Some(&ClockState {
                hour: 12,
                minute: 34,
                day: 1,
                month: 9,
            }),
            &[],
            None,
            Some(&audio),
            Some(&network),
            &BAR_STYLE,
        );
        let network_rect = view.network.unwrap().rect;
        let audio_rect = view.audio.unwrap().rect;
        let datetime_rect = view.datetime.unwrap().rect;
        assert_eq!(
            network_rect.x + network_rect.width as i16 + STATUS_ITEM_GAP,
            audio_rect.x
        );
        assert_eq!(audio_rect.x + audio_rect.width as i16, datetime_rect.x - 8);
    }
    use crate::core::{
        ChildrenDisplay, MenuItem, MenuItemType, MenuModel, OutputId, OutputState,
        StatusNotifierEndpoint, StatusNotifierIcon, StatusNotifierItem, StatusNotifierStatus,
        WorkspaceState,
    };

    fn output(width: u16) -> OutputState {
        OutputState {
            id: OutputId(1),
            name: "HDMI-1".into(),
            x: 0,
            y: 0,
            width,
            height: 600,
        }
    }
    fn workspace() -> Vec<WorkspaceState> {
        vec![WorkspaceState {
            name: "1".into(),
            output: Some("HDMI-1".into()),
            focused: true,
        }]
    }
    fn item(id: i32, label: Option<&str>, visible: bool) -> MenuItem {
        MenuItem {
            id: MenuItemId(id),
            label: label.map(str::to_string),
            enabled: true,
            visible,
            item_type: MenuItemType::Standard,
            children_display: None,
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![],
        }
    }
    fn loaded(items: Vec<MenuItem>) -> MenuModel {
        MenuModel {
            revision: 1,
            root: MenuItem {
                id: MenuItemId(0),
                label: None,
                enabled: true,
                visible: true,
                item_type: MenuItemType::Standard,
                children_display: Some(ChildrenDisplay::Submenu),
                shortcut: None,
                icon_name: None,
                action: None,
                children: items,
            },
        }
    }

    fn tray_item(status: StatusNotifierStatus) -> StatusNotifierItem {
        StatusNotifierItem {
            endpoint: StatusNotifierEndpoint {
                service: ":1.50".into(),
                object_path: "/StatusNotifierItem".into(),
            },
            status,
            icon: Some(StatusNotifierIcon::Pixmap {
                width: 16,
                height: 16,
                argb: vec![0xffff_0000; 16 * 16],
            }),
            item_is_menu: false,
            menu: None,
        }
    }

    #[test]
    fn renders_only_visible_top_level_and_keeps_ids() {
        let model = loaded(vec![
            item(1, Some("Arquivo"), true),
            item(2, Some("Novo"), false),
            item(3, Some("Editar"), true),
        ]);
        let view = context_view(&output(600), &workspace(), Some(&model), None, &[]);
        assert_eq!(
            view.menu.iter().map(|item| item.id).collect::<Vec<_>>(),
            vec![MenuItemId(1), MenuItemId(3)]
        );
        assert!(view
            .menu
            .iter()
            .all(|item| item.rect.x >= 0 && item.rect.x + item.rect.width as i16 <= 600));
    }

    #[test]
    fn loading_error_and_no_menu_have_no_visual_menu() {
        assert!(context_view(&output(600), &workspace(), None, None, &[])
            .menu
            .is_empty());
    }

    #[test]
    fn mnemonic_presentation_does_not_mutate_domain() {
        let model = loaded(vec![
            item(1, Some("_Arquivo"), true),
            item(2, Some("A__B"), true),
        ]);
        let view = context_view(&output(600), &workspace(), Some(&model), None, &[]);
        assert_eq!(view.menu[0].label, "Arquivo");
        assert_eq!(view.menu[1].label, "A_B");
        assert_eq!(model.root.children[0].label.as_deref(), Some("_Arquivo"));
    }

    #[test]
    fn overflow_is_clipped_without_invalid_rectangles() {
        let items = (0..30)
            .map(|id| item(id, Some("Long menu item"), true))
            .collect();
        let model = loaded(items);
        let view = context_view(&output(120), &workspace(), Some(&model), None, &[]);
        assert!(view.menu.iter().all(|item| item.rect.x >= 0
            && item.rect.width > 0
            && item.rect.x + item.rect.width as i16 <= 120));
    }

    #[test]
    fn changing_loaded_model_replaces_visual_menu() {
        let model_a = loaded(vec![item(1, Some("Arquivo"), true)]);
        let model_b = loaded(vec![item(9, Some("Preferências"), true)]);
        let view_a = context_view(&output(600), &workspace(), Some(&model_a), None, &[]);
        let view_b = context_view(&output(600), &workspace(), Some(&model_b), None, &[]);

        assert_eq!(view_a.menu[0].id, MenuItemId(1));
        assert_eq!(view_b.menu[0].id, MenuItemId(9));
        assert_eq!(view_b.menu[0].label, "Preferências");
    }

    #[test]
    fn renders_only_focused_workspace() {
        let workspaces = vec![
            WorkspaceState {
                name: "1".into(),
                output: Some("HDMI-1".into()),
                focused: false,
            },
            WorkspaceState {
                name: "2".into(),
                output: Some("HDMI-1".into()),
                focused: false,
            },
            WorkspaceState {
                name: "3".into(),
                output: Some("HDMI-1".into()),
                focused: true,
            },
            WorkspaceState {
                name: "4".into(),
                output: Some("HDMI-1".into()),
                focused: false,
            },
        ];
        let view = context_view(&output(600), &workspaces, None, None, &[]);
        assert_eq!(view.workspaces.len(), 1);
        assert_eq!(view.workspaces[0].width, 24);
    }

    #[test]
    fn right_flow_places_workspace_before_menu_at_left_edge() {
        let model = loaded(vec![
            item(1, Some("Arquivo"), true),
            item(2, Some("Editar"), true),
        ]);
        let view = context_view(&output(600), &workspace(), Some(&model), None, &[]);
        let workspace = view.workspaces[0];
        assert_eq!(workspace.x + workspace.width as i16, view.menu[0].rect.x);
        assert_eq!(
            view.menu.last().unwrap().rect.x + view.menu.last().unwrap().rect.width as i16,
            176
        );
        assert_eq!(view.future.x, 176);
        assert_eq!(view.future.x + view.future.width as i16, 592);
    }

    #[test]
    fn menu_is_clipped_without_losing_focused_workspace() {
        let model = loaded(
            (0..30)
                .map(|id| item(id, Some("Long menu item"), true))
                .collect(),
        );
        let view = context_view(&output(120), &workspace(), Some(&model), None, &[]);
        assert_eq!(view.workspaces.len(), 1);
        assert!(view.workspaces[0].width > 0);
        assert!(view.menu.iter().all(|item| item.rect.x >= 0
            && item.rect.width > 0
            && item.rect.x + item.rect.width as i16 <= 120));
    }

    #[test]
    fn changing_menu_keeps_workspace_geometry_on_same_workspace() {
        let short = loaded(vec![item(1, Some("Arquivo"), true)]);
        let long = loaded(vec![
            item(1, Some("Arquivo"), true),
            item(2, Some("Editar"), true),
        ]);
        let before = context_view(&output(640), &workspace(), Some(&short), None, &[]);
        let after = context_view(&output(640), &workspace(), Some(&long), None, &[]);
        assert_eq!(before.workspaces[0].width, after.workspaces[0].width);
        assert_eq!(
            after.workspaces[0].x + after.workspaces[0].width as i16,
            after.menu[0].rect.x
        );
    }

    #[test]
    fn formats_clock_with_zero_padding() {
        let clock = ClockState {
            hour: 8,
            minute: 3,
            day: 1,
            month: 9,
        };
        assert_eq!(format_clock(&clock), "08:03 01/09");
    }

    #[test]
    fn datetime_is_last_and_stays_at_output_edge() {
        let clock = ClockState {
            hour: 18,
            minute: 42,
            day: 31,
            month: 8,
        };
        let view = context_view(&output(640), &workspace(), None, Some(&clock), &[]);
        let datetime = view.datetime.unwrap();
        assert_eq!(datetime.text, "18:42 31/08");
        assert_eq!(datetime.rect.x + datetime.rect.width as i16, 632);
        assert_eq!(view.workspaces[0].x + view.workspaces[0].width as i16, 32);
        assert_eq!(view.future.x, 32);
        assert_eq!(
            view.future.x + view.future.width as i16,
            datetime.rect.x - 8
        );
    }

    #[test]
    fn focused_app_name_is_after_workspace_and_clipped() {
        let clock = ClockState {
            hour: 18,
            minute: 42,
            day: 31,
            month: 8,
        };
        let view = context_view_with_app_name(
            &output(240),
            &workspace(),
            None,
            Some(&clock),
            &[],
            Some("A very long focused window title"),
            &BAR_STYLE,
        );
        let app = view.app_name.unwrap();
        assert!(app.rect.x >= view.workspaces[0].x + view.workspaces[0].width as i16);
        assert!(app.text.ends_with('…'));
    }

    #[test]
    fn empty_focused_app_name_is_not_rendered() {
        let view = context_view_with_app_name(
            &output(640),
            &workspace(),
            None,
            None,
            &[],
            Some(""),
            &BAR_STYLE,
        );
        assert!(view.app_name.is_none());
    }

    #[test]
    fn capitalize_app_name_preserves_the_rest() {
        assert_eq!(capitalize_app_name("alacritty"), "Alacritty");
        assert_eq!(capitalize_app_name("zen browser"), "Zen browser");
        assert_eq!(capitalize_app_name("LibreOffice"), "LibreOffice");
        assert_eq!(capitalize_app_name(""), "");
    }

    #[test]
    fn datetime_priority_preserves_workspace_when_menu_is_too_large() {
        let clock = ClockState {
            hour: 18,
            minute: 42,
            day: 31,
            month: 8,
        };
        let model = loaded(
            (0..30)
                .map(|id| item(id, Some("Long menu item"), true))
                .collect(),
        );
        let view = context_view(&output(160), &workspace(), Some(&model), Some(&clock), &[]);
        let datetime = view.datetime.unwrap();
        assert_eq!(view.workspaces.len(), 1);
        assert!(view.menu.iter().all(|item| {
            item.rect.x >= 0 && item.rect.x + item.rect.width as i16 <= datetime.rect.x - 8
        }));
        assert_eq!(datetime.rect.x + datetime.rect.width as i16, 152);
    }

    #[test]
    fn datetime_formats_date_boundaries() {
        assert_eq!(
            format_clock(&ClockState {
                hour: 23,
                minute: 59,
                day: 31,
                month: 12,
            }),
            "23:59 31/12"
        );
        assert_eq!(
            format_clock(&ClockState {
                hour: 0,
                minute: 0,
                day: 1,
                month: 1,
            }),
            "00:00 01/01"
        );
    }

    #[test]
    fn datetime_can_render_before_first_workspace_snapshot() {
        let clock = ClockState {
            hour: 8,
            minute: 3,
            day: 1,
            month: 9,
        };
        let view = context_view(&output(640), &[], None, Some(&clock), &[]);
        assert!(view.workspaces.is_empty());
        assert_eq!(view.datetime.unwrap().text, "08:03 01/09");
    }

    #[test]
    fn active_tray_item_occupies_future_area_before_datetime() {
        let clock = ClockState {
            hour: 18,
            minute: 42,
            day: 31,
            month: 8,
        };
        let view = context_view(
            &output(640),
            &workspace(),
            None,
            Some(&clock),
            &[tray_item(StatusNotifierStatus::Active)],
        );
        let tray = view.tray[0].rect;
        let datetime = view.datetime.unwrap().rect;
        assert_eq!(tray.x + tray.width as i16, datetime.x - 8);
        assert!(tray.x >= view.future.x);
        assert_eq!(datetime.x + datetime.width as i16, 632);
    }

    #[test]
    fn no_tray_has_no_phantom_status_gap() {
        let network = crate::core::NetworkState {
            available: true,
            connectivity: NetworkConnectivity::Connected,
            link_kind: NetworkLinkKind::Wifi,
            signal_percent: Some(80),
            ..Default::default()
        };
        let view = context_view_with_app_name_and_audio(
            &output(640),
            &workspace(),
            None,
            Some(&ClockState {
                hour: 12,
                minute: 34,
                day: 1,
                month: 9,
            }),
            &[],
            None,
            None,
            Some(&network),
            &BAR_STYLE,
        );
        let network_rect = view.network.unwrap().rect;
        assert_eq!(view.future.x + view.future.width as i16, network_rect.x);
    }

    #[test]
    fn passive_tray_item_is_not_rendered() {
        let view = context_view(
            &output(640),
            &workspace(),
            None,
            None,
            &[tray_item(StatusNotifierStatus::Passive)],
        );
        assert!(view.tray.is_empty());
    }

    #[test]
    fn multiple_tray_items_keep_order_and_compact_after_removal() {
        let mut a = tray_item(StatusNotifierStatus::Active);
        let mut b = tray_item(StatusNotifierStatus::Active);
        let mut c = tray_item(StatusNotifierStatus::Active);
        a.endpoint.object_path = "/a".into();
        b.endpoint.object_path = "/b".into();
        c.endpoint.object_path = "/c".into();
        let view = context_view(
            &output(640),
            &workspace(),
            None,
            None,
            &[a.clone(), b, c.clone()],
        );
        assert_eq!(view.tray.len(), 3);
        assert_eq!(view.tray[0].endpoint.object_path, "/a");
        assert_eq!(view.tray[2].endpoint.object_path, "/c");
        let compact = context_view(&output(640), &workspace(), None, None, &[a, c]);
        assert_eq!(compact.tray.len(), 2);
        assert_eq!(compact.tray[0].endpoint.object_path, "/a");
        assert_eq!(compact.tray[1].endpoint.object_path, "/c");
    }

    #[test]
    fn plugin_zone_is_between_flex_and_tray_and_is_zero_width_when_empty() {
        let empty = context_view(&output(640), &workspace(), None, None, &[]);
        assert!(empty.plugins.is_empty());

        let audio = crate::core::AudioState {
            available: true,
            ..Default::default()
        };
        let network = crate::core::NetworkState {
            available: true,
            connectivity: NetworkConnectivity::Connected,
            link_kind: NetworkLinkKind::Wifi,
            signal_percent: Some(72),
            ..Default::default()
        };
        let bluetooth = crate::core::BluetoothState {
            available: true,
            powered: true,
            ..Default::default()
        };
        let mut baseline_tray = tray_item(StatusNotifierStatus::Active);
        baseline_tray.endpoint.object_path = "/baseline".into();
        let baseline = context_view_with_app_name_and_audio_and_bluetooth(
            &output(640),
            &workspace(),
            None,
            Some(&ClockState {
                hour: 12,
                minute: 34,
                day: 1,
                month: 9,
            }),
            &[baseline_tray.clone()],
            Some("app"),
            Some(&audio),
            Some(&network),
            Some(&bluetooth),
            &BAR_STYLE,
        );
        let empty_plugin_path = context_view_with_app_name_and_audio_and_bluetooth_and_plugins(
            &output(640),
            &workspace(),
            None,
            Some(&ClockState {
                hour: 12,
                minute: 34,
                day: 1,
                month: 9,
            }),
            &[baseline_tray.clone()],
            Some("app"),
            Some(&audio),
            Some(&network),
            Some(&bluetooth),
            &[],
            &BAR_STYLE,
        );
        assert_eq!(baseline.future, empty_plugin_path.future);
        assert_eq!(baseline.tray, empty_plugin_path.tray);
        assert_eq!(baseline.network, empty_plugin_path.network);
        assert_eq!(baseline.bluetooth, empty_plugin_path.bluetooth);
        assert_eq!(baseline.audio, empty_plugin_path.audio);
        assert_eq!(baseline.datetime, empty_plugin_path.datetime);

        let plugin = crate::core::PluginSummary {
            id: crate::core::PluginId("test:plugin:default".into()),
            display_name: "Test".into(),
            text: "Test 72%".into(),
            status: crate::core::PluginStatus::Ready,
        };
        let mut tray = tray_item(StatusNotifierStatus::Active);
        tray.endpoint.object_path = "/tray".into();
        let view = context_view_with_app_name_and_audio_and_bluetooth_and_plugins(
            &output(640),
            &workspace(),
            None,
            None,
            &[tray],
            None,
            None,
            None,
            None,
            &[plugin],
            &BAR_STYLE,
        );
        assert_eq!(view.plugins.len(), 1);
        assert_eq!(view.tray.len(), 1);
        assert!(view.plugins[0].rect.x + view.plugins[0].rect.width as i16 <= view.tray[0].rect.x);
        assert_eq!(
            view.future.x + view.future.width as i16,
            view.plugins[0].rect.x
        );
    }
}
