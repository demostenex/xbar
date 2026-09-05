use crate::core::{ChildrenDisplay, MenuItem, MenuItemId, MenuItemType};
use crate::core::{OutputState, WorkspaceState};
use crate::ui::style::{TextMeasurer, BAR_STYLE};
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorkspaceRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuRect {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

pub const AUDIO_POPUP_BORDER: u16 = 1;
const AUDIO_DEVICE_ROW_HEIGHT: u16 = 24;

/// Audio device rows use root coordinates, including the popup's one-pixel border.
/// Both drawing and pointer lookup consume these same rows.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioDeviceRow {
    pub name: String,
    pub display_name: String,
    pub rect: MenuRect,
    baseline_offset: i16,
}

impl AudioDeviceRow {
    pub fn label_position(&self, popup: MenuRect) -> (i32, i32) {
        (
            i32::from(self.rect.x) - i32::from(popup.x) - i32::from(AUDIO_POPUP_BORDER) + 8,
            i32::from(self.rect.y) - i32::from(popup.y) - i32::from(AUDIO_POPUP_BORDER)
                + i32::from(self.baseline_offset),
        )
    }

    pub fn contains(&self, root_x: i16, root_y: i16) -> bool {
        let x = i32::from(root_x) - i32::from(self.rect.x);
        let y = i32::from(root_y) - i32::from(self.rect.y);
        x >= 0 && x < i32::from(self.rect.width) && y >= 0 && y < i32::from(self.rect.height)
    }
}

pub fn audio_device_rows<M: TextMeasurer>(
    popup: MenuRect,
    devices: &[crate::core::AudioDevice],
    first_baseline: i16,
    measurer: &M,
) -> Vec<AudioDeviceRow> {
    let baseline_offset = measurer.baseline(AUDIO_DEVICE_ROW_HEIGHT);
    devices
        .iter()
        .take(8)
        .enumerate()
        .map(|(index, device)| AudioDeviceRow {
            name: device.name.clone(),
            display_name: device.display_name.clone(),
            rect: MenuRect {
                x: popup.x + AUDIO_POPUP_BORDER as i16 + 14,
                y: popup.y
                    + AUDIO_POPUP_BORDER as i16
                    + first_baseline
                    + index as i16 * AUDIO_DEVICE_ROW_HEIGHT as i16
                    - baseline_offset,
                width: popup.width.saturating_sub(28),
                height: AUDIO_DEVICE_ROW_HEIGHT,
            },
            baseline_offset,
        })
        .collect()
}

pub const LEFT_PADDING: i32 = 8;
pub const RIGHT_PADDING: i32 = 8;

#[derive(Clone, Debug, PartialEq)]
pub struct PopupItemRect {
    pub id: MenuItemId,
    pub rect: MenuRect,
    pub label: String,
    pub enabled: bool,
    pub separator: bool,
    pub has_submenu: bool,
    pub shortcut: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PopupLayout {
    pub parent_id: MenuItemId,
    pub rect: MenuRect,
    pub items: Vec<PopupItemRect>,
}

fn text_width<M: TextMeasurer>(measurer: &M, text: &str) -> u16 {
    measurer.measure_width(text)
}

fn shortcut_text(item: &MenuItem) -> Option<String> {
    item.shortcut
        .as_ref()
        .and_then(|shortcut| shortcut.keys.first())
        .map(|keys| {
            keys.iter()
                .map(|key| key.trim_matches('<'))
                .collect::<Vec<_>>()
                .join("+")
        })
}

fn visible_children(parent: &MenuItem) -> impl Iterator<Item = &MenuItem> {
    parent.children.iter().filter(|item| item.visible)
}

#[allow(dead_code)]
pub fn popup_layout(
    output: &OutputState,
    parent: &MenuItem,
    anchor: MenuRect,
    submenu: bool,
) -> PopupLayout {
    popup_layout_with_measurer(output, parent, anchor, submenu, &BAR_STYLE)
}

pub fn popup_layout_with_measurer<M: TextMeasurer>(
    output: &OutputState,
    parent: &MenuItem,
    anchor: MenuRect,
    submenu: bool,
    measurer: &M,
) -> PopupLayout {
    let children: Vec<_> = visible_children(parent).collect();
    let mut width = 120_u16;
    for item in &children {
        let label = item
            .label
            .as_deref()
            .map(crate::ui::view::present_label)
            .unwrap_or_default();
        let shortcut = shortcut_text(item);
        let indicator = if item.children_display == Some(ChildrenDisplay::Submenu) {
            16
        } else {
            0
        };
        width = width.max(
            24_u16
                .saturating_add(text_width(measurer, &label))
                .saturating_add(
                    shortcut
                        .as_deref()
                        .map(|text| text_width(measurer, text))
                        .unwrap_or(0),
                )
                .saturating_add(indicator),
        );
    }
    width = width.min(output.width.max(1));
    let item_height = 26_i32;
    let separator_height = 10_i32;
    let content_height: i32 = children
        .iter()
        .map(|item| {
            if item.item_type == MenuItemType::Separator {
                separator_height
            } else {
                item_height
            }
        })
        .sum();
    let height = (content_height + 8).min(output.height.max(1) as i32).max(1) as u16;
    let ox = output.x as i32;
    let oy = output.y as i32;
    let right = ox + output.width as i32;
    let x = if submenu {
        let right_candidate = anchor.x as i32 + anchor.width as i32;
        if right_candidate + width as i32 <= right {
            right_candidate
        } else {
            (anchor.x as i32 - width as i32).max(ox)
        }
    } else {
        (anchor.x as i32).clamp(ox, right - width as i32)
    };
    let desired_y = if submenu {
        anchor.y as i32
    } else {
        anchor.y as i32 + anchor.height as i32
    };
    let y = desired_y.clamp(oy, oy + output.height as i32 - height as i32);
    let rect = MenuRect {
        x: x as i16,
        y: y as i16,
        width,
        height,
    };
    let mut cursor = y + 4;
    let items = children
        .into_iter()
        .map(|item| {
            let separator = item.item_type == MenuItemType::Separator;
            let h = if separator {
                separator_height
            } else {
                item_height
            };
            let item_rect = MenuRect {
                x: rect.x,
                y: cursor as i16,
                width,
                height: h as u16,
            };
            cursor += h;
            PopupItemRect {
                id: item.id,
                rect: item_rect,
                label: item
                    .label
                    .as_deref()
                    .map(crate::ui::view::present_label)
                    .unwrap_or_default(),
                enabled: item.enabled,
                separator,
                has_submenu: item.children_display == Some(ChildrenDisplay::Submenu)
                    && !item.children.is_empty(),
                shortcut: shortcut_text(item),
            }
        })
        .collect();
    PopupLayout {
        parent_id: parent.id,
        rect,
        items,
    }
}

pub fn find_item(root: &MenuItem, id: MenuItemId) -> Option<&MenuItem> {
    if root.id == id {
        return Some(root);
    }
    root.children.iter().find_map(|child| find_item(child, id))
}

#[allow(dead_code)]
pub fn allocate_context(
    output: &OutputState,
    workspaces: &[WorkspaceState],
    menu: &[(MenuItemId, String, bool)],
    datetime: Option<&str>,
) -> (
    Vec<WorkspaceRect>,
    Vec<MenuRect>,
    Option<MenuRect>,
    MenuRect,
) {
    allocate_context_with_measurer(output, workspaces, menu, datetime, &BAR_STYLE)
}

pub fn allocate_context_with_measurer<M: TextMeasurer>(
    output: &OutputState,
    workspaces: &[WorkspaceState],
    menu: &[(MenuItemId, String, bool)],
    datetime: Option<&str>,
    measurer: &M,
) -> (
    Vec<WorkspaceRect>,
    Vec<MenuRect>,
    Option<MenuRect>,
    MenuRect,
) {
    allocate_context_with_reserved_right(output, workspaces, menu, datetime, 0, measurer)
}

pub fn allocate_context_with_reserved_right<M: TextMeasurer>(
    output: &OutputState,
    workspaces: &[WorkspaceState],
    menu: &[(MenuItemId, String, bool)],
    datetime: Option<&str>,
    reserved_right: i32,
    measurer: &M,
) -> (
    Vec<WorkspaceRect>,
    Vec<MenuRect>,
    Option<MenuRect>,
    MenuRect,
) {
    let output_left = output.x as i32;
    let output_right = output_left + output.width as i32;
    let workspace_width = workspaces
        .first()
        .map(|workspace| {
            (text_width(measurer, &workspace.name) as i32
                + (BAR_STYLE.horizontal_padding as i32 * 2))
                .clamp(24, output.width as i32) as u16
        })
        .unwrap_or(0);
    let datetime_width = datetime
        .map(|text| {
            (text_width(measurer, text) as i32 + (BAR_STYLE.horizontal_padding as i32 * 2))
                .min(output.width as i32)
        })
        .unwrap_or(0);
    let datetime_x = output_right - RIGHT_PADDING - datetime_width;
    let content_right = datetime_x - if datetime.is_some() { RIGHT_PADDING } else { 0 };
    let content_left = output_left + LEFT_PADDING;
    let available_menu =
        (content_right - content_left - workspace_width as i32 - reserved_right.max(0)).max(0);
    let mut menu_width = 0_i32;
    let mut widths = Vec::new();
    for (_, label, _) in menu {
        if menu_width >= available_menu {
            break;
        }
        let natural = (text_width(measurer, label) as i32
            + (BAR_STYLE.horizontal_padding as i32 * 2)
            + BAR_STYLE.item_spacing as i32)
            .max(20);
        let width = natural.min(available_menu - menu_width);
        widths.push(width as u16);
        menu_width += width;
    }
    let workspace_x = content_left;
    let workspace_rect = (workspace_width > 0).then_some(WorkspaceRect {
        x: workspace_x as i16,
        y: output.y,
        width: workspace_width,
        height: 26,
    });
    let mut x = workspace_x + workspace_width as i32;
    let menu_rects = widths
        .into_iter()
        .map(|width| {
            let rect = MenuRect {
                x: x as i16,
                y: output.y,
                width,
                height: 26,
            };
            x += width as i32;
            rect
        })
        .collect();
    let datetime_rect = datetime.map(|_| MenuRect {
        x: datetime_x as i16,
        y: output.y,
        width: datetime_width as u16,
        height: 26,
    });
    let future_left = x;
    let future_right = content_right;
    let future_rect = MenuRect {
        x: future_left as i16,
        y: output.y,
        width: (future_right - future_left).max(0) as u16,
        height: 26,
    };
    (
        workspace_rect.into_iter().collect(),
        menu_rects,
        datetime_rect,
        future_rect,
    )
}

pub fn allocate_tray(future: MenuRect, count: usize) -> Vec<MenuRect> {
    const ITEM_WIDTH: i32 = 20;
    let mut right = future.x as i32 + future.width as i32;
    let left = future.x as i32;
    let mut result = Vec::new();
    for _ in 0..count {
        if right - ITEM_WIDTH < left {
            break;
        }
        right -= ITEM_WIDTH;
        result.push(MenuRect {
            x: right as i16,
            y: future.y,
            width: ITEM_WIDTH as u16,
            height: future.height,
        });
    }
    result.reverse();
    result
}

pub fn allocate_plugins(
    left: i16,
    right: i16,
    labels: &[String],
    measurer: &impl TextMeasurer,
) -> Vec<MenuRect> {
    let mut cursor = right as i32;
    let mut rects = Vec::with_capacity(labels.len());
    for label in labels.iter().rev() {
        let width = (measurer.measure_width(label) as i32 + 12).max(20);
        let x = cursor - width;
        if x < left as i32 {
            break;
        }
        rects.push(MenuRect {
            x: x as i16,
            y: 0,
            width: width as u16,
            height: 26,
        });
        cursor = x - crate::ui::style::STATUS_ITEM_GAP as i32;
    }
    rects.reverse();
    rects
}
#[cfg(test)]
pub fn allocate(output: &OutputState, workspaces: &[WorkspaceState]) -> Vec<WorkspaceRect> {
    if workspaces.is_empty() {
        return Vec::new();
    }
    let width = (output.width / workspaces.len() as u16).max(1);
    workspaces
        .iter()
        .enumerate()
        .map(|(i, _)| WorkspaceRect {
            x: output.x.saturating_add((i as u16 * width) as i16),
            y: output.y,
            width: if i + 1 == workspaces.len() {
                output.width.saturating_sub(width.saturating_mul(i as u16))
            } else {
                width
            },
            height: 26,
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;

    struct AudioMeasurer(crate::ui::style::FontMetrics);

    impl TextMeasurer for AudioMeasurer {
        fn measure_width(&self, text: &str) -> u16 {
            text.chars().count() as u16 * 10
        }

        fn metrics(&self) -> crate::ui::style::FontMetrics {
            self.0
        }
    }

    fn audio_fixture() -> (MenuRect, Vec<crate::core::AudioDevice>, AudioMeasurer) {
        (
            MenuRect {
                x: 1580,
                y: 26,
                width: 340,
                height: 404,
            },
            vec![
                crate::core::AudioDevice {
                    name: "sink.z".into(),
                    display_name: "Headphones".into(),
                },
                crate::core::AudioDevice {
                    name: "sink.a".into(),
                    display_name: "Speaker".into(),
                },
            ],
            AudioMeasurer(crate::ui::style::FontMetrics {
                ascent: 16,
                descent: 5,
            }),
        )
    }

    #[test]
    fn audio_two_rendered_output_labels_hit_their_own_rows() {
        let (popup, devices, measurer) = audio_fixture();
        let rows = audio_device_rows(popup, &devices, 254, &measurer);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.name, devices[index].name);
            assert_eq!(row.display_name, devices[index].display_name);
            assert_eq!(row.label_position(popup), (22, 254 + index as i32 * 24));
            assert_eq!(
                row.rect,
                MenuRect {
                    x: 1595,
                    y: 264 + index as i16 * 24,
                    width: 312,
                    height: 24,
                }
            );
            let (label_x, baseline) = row.label_position(popup);
            let root_x = popup.x + AUDIO_POPUP_BORDER as i16 + label_x as i16;
            let root_baseline = popup.y + AUDIO_POPUP_BORDER as i16 + baseline as i16;
            // Every vertical pixel in the rendered font box selects that label's ID.
            for y in root_baseline - measurer.metrics().ascent
                ..root_baseline + measurer.metrics().descent
            {
                let hits = rows
                    .iter()
                    .filter(|r| r.contains(root_x, y))
                    .collect::<Vec<_>>();
                assert_eq!(hits.len(), 1, "rendered row {index}, root y={y}");
                assert_eq!(hits[0].name, devices[index].name);
            }
        }
    }

    #[test]
    fn audio_output_boundaries_have_no_dead_gap_or_double_hit() {
        let (popup, devices, measurer) = audio_fixture();
        let rows = audio_device_rows(popup, &devices, 254, &measurer);
        // Includes the physical trace's 265..279, 280..303 and 304..327 ranges.
        for y in 263..=327 {
            let hits = rows
                .iter()
                .filter(|r| r.contains(1610, y))
                .collect::<Vec<_>>();
            match y {
                264..=287 => assert_eq!(hits, vec![&rows[0]], "y={y}"),
                288..=311 => assert_eq!(hits, vec![&rows[1]], "y={y}"),
                _ => assert!(hits.is_empty(), "y={y}"),
            }
        }
        for row in &rows {
            assert!(row.contains(row.rect.x, row.rect.y));
            assert!(row.contains(row.rect.x + 311, row.rect.y + 23));
            assert!(!row.contains(row.rect.x - 1, row.rect.y));
            assert!(!row.contains(row.rect.x + 312, row.rect.y));
            assert!(!row.contains(row.rect.x, row.rect.y + 24));
        }
    }

    #[test]
    fn audio_rows_preserve_inventory_order_and_visible_limit() {
        let (popup, mut devices, measurer) = audio_fixture();
        devices.reverse();
        devices.extend((0..8).map(|i| crate::core::AudioDevice {
            name: format!("sink.{i}"),
            display_name: format!("Device {i}"),
        }));
        let rows = audio_device_rows(popup, &devices, 254, &measurer);
        assert_eq!(rows.len(), 8);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.name, devices[index].name);
            assert_eq!(row.display_name, devices[index].display_name);
            assert_eq!(row.label_position(popup).1, 254 + index as i32 * 24);
        }
    }

    #[test]
    fn audio_row_draw_and_hit_share_origin_border_and_font_metrics() {
        let (mut popup, devices, _) = audio_fixture();
        for (x, y) in [(0, 0), (-800, -100), (1580, 26)] {
            popup.x = x;
            popup.y = y;
            for (ascent, descent) in [(16, 5), (12, 4), (18, 6)] {
                let measurer = AudioMeasurer(crate::ui::style::FontMetrics { ascent, descent });
                let rows = audio_device_rows(popup, &devices, 254, &measurer);
                for (index, row) in rows.iter().enumerate() {
                    let (local_x, local_baseline) = row.label_position(popup);
                    assert_eq!((local_x, local_baseline), (22, 254 + index as i32 * 24));
                    let root_baseline = popup.y + 1 + local_baseline as i16;
                    assert_eq!(row.rect.y + measurer.baseline(24), root_baseline);
                    for ink_y in root_baseline - ascent..root_baseline + descent {
                        assert!(row.contains(popup.x + 1 + local_x as i16, ink_y));
                    }
                }
                assert_eq!(rows[0].rect.y + 24, rows[1].rect.y);
            }
        }
    }

    #[test]
    fn audio_input_rows_keep_draw_baselines_and_hit_their_own_labels() {
        let (popup, devices, measurer) = audio_fixture();
        let input_header_baseline = 254 + 2 * 24;
        let rows = audio_device_rows(popup, &devices, input_header_baseline + 22, &measurer);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row.label_position(popup), (22, 324 + index as i32 * 24));
            assert_eq!(row.rect.y, 334 + index as i16 * 24);
            let label_y = popup.y + 1 + 324 + index as i16 * 24 - 8;
            assert_eq!(
                rows.iter()
                    .find(|r| r.contains(1610, label_y))
                    .unwrap()
                    .name,
                devices[index].name
            );
        }
        assert!(!rows[0].contains(1610, popup.y + 1 + input_header_baseline));
    }

    #[test]
    fn audio_empty_inventory_has_no_draw_or_hit_rows() {
        let (popup, _, measurer) = audio_fixture();
        assert!(audio_device_rows(popup, &[], 254, &measurer).is_empty());
    }

    fn output() -> OutputState {
        OutputState {
            id: crate::core::OutputId(1),
            name: "HDMI-1".into(),
            x: 100,
            y: 20,
            width: 900,
            height: 600,
        }
    }
    fn ws(n: usize) -> Vec<WorkspaceState> {
        (0..n)
            .map(|i| WorkspaceState {
                name: i.to_string(),
                output: None,
                focused: false,
            })
            .collect()
    }
    #[test]
    fn allocates_inside_output() {
        let r = allocate(&output(), &ws(3));
        assert_eq!(r[0].x, 100);
        assert_eq!(
            r.last().unwrap().x + r.last().unwrap().width as i16,
            100 + 900
        );
        assert!(r.iter().all(|x| x.y == 20 && x.height == 26));
    }
    #[test]
    fn no_overflow_for_empty() {
        assert!(allocate(&output(), &[]).is_empty());
    }

    #[test]
    fn tray_slots_keep_registration_order_and_grow_from_right() {
        let future = MenuRect {
            x: 100,
            y: 20,
            width: 100,
            height: 26,
        };
        let slots = allocate_tray(future, 3);
        assert_eq!(
            slots.iter().map(|slot| slot.x).collect::<Vec<_>>(),
            vec![140, 160, 180]
        );
        assert_eq!(slots[0].x + slots[0].width as i16, slots[1].x);
        assert_eq!(slots[2].x + slots[2].width as i16, 200);
    }

    #[test]
    fn tray_overflow_preserves_items_nearest_date_time() {
        let future = MenuRect {
            x: 100,
            y: 20,
            width: 40,
            height: 26,
        };
        let slots = allocate_tray(future, 3);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].x, 100);
        assert_eq!(slots[1].x, 120);
    }

    #[test]
    fn right_flow_respects_offset_output_edge() {
        let output = OutputState {
            x: 1920,
            width: 2560,
            ..output()
        };
        let workspaces = vec![WorkspaceState {
            name: "3".into(),
            output: Some("HDMI-1".into()),
            focused: true,
        }];
        let (_, menu, _, _) = allocate_context(
            &output,
            &workspaces,
            &[(MenuItemId(1), "Arquivo".into(), true)],
            None,
        );
        let last = menu.last().unwrap();
        assert_eq!(last.x, output.x + 32);
        assert_eq!(last.x + last.width as i16, output.x + 108);
    }

    fn popup_parent() -> MenuItem {
        MenuItem {
            id: MenuItemId(1),
            label: Some("Arquivo".into()),
            enabled: true,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: Some(ChildrenDisplay::Submenu),
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![
                MenuItem {
                    id: MenuItemId(2),
                    label: Some("Novo".into()),
                    enabled: true,
                    visible: true,
                    item_type: MenuItemType::Standard,
                    children_display: None,
                    shortcut: None,
                    icon_name: None,
                    action: None,
                    children: vec![],
                },
                MenuItem {
                    id: MenuItemId(3),
                    label: None,
                    enabled: true,
                    visible: true,
                    item_type: MenuItemType::Separator,
                    children_display: None,
                    shortcut: None,
                    icon_name: None,
                    action: None,
                    children: vec![],
                },
                MenuItem {
                    id: MenuItemId(4),
                    label: Some("Oculto".into()),
                    enabled: true,
                    visible: false,
                    item_type: MenuItemType::Standard,
                    children_display: None,
                    shortcut: None,
                    icon_name: None,
                    action: None,
                    children: vec![],
                },
            ],
        }
    }

    #[test]
    fn popup_is_below_anchor_and_hides_hidden_items() {
        let p = popup_layout(
            &output(),
            &popup_parent(),
            MenuRect {
                x: 120,
                y: 20,
                width: 80,
                height: 26,
            },
            false,
        );
        assert_eq!(p.rect.y, 46);
        assert_eq!(
            p.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![MenuItemId(2), MenuItemId(3)]
        );
        assert!(p.items[1].separator && p.items[1].rect.height > 0);
    }

    #[test]
    fn submenu_flips_left_and_clamps_vertical_on_offset_output() {
        let mut o = output();
        o.x = 1920;
        o.y = 100;
        o.width = 300;
        o.height = 120;
        let p = popup_layout(
            &o,
            &popup_parent(),
            MenuRect {
                x: 2150,
                y: 190,
                width: 40,
                height: 26,
            },
            true,
        );
        assert!(p.rect.x >= o.x && p.rect.x + p.rect.width as i16 <= o.x + o.width as i16);
        assert!(p.rect.y >= o.y && p.rect.y + p.rect.height as i16 <= o.y + o.height as i16);
    }

    #[test]
    fn huge_popup_is_clipped_to_output_without_invalid_geometry() {
        let mut o = output();
        o.width = 40;
        o.height = 20;
        let p = popup_layout(
            &o,
            &popup_parent(),
            MenuRect {
                x: 0,
                y: 0,
                width: 10,
                height: 10,
            },
            false,
        );
        assert!(p.rect.width > 0 && p.rect.height > 0);
        assert!(p.rect.width <= o.width && p.rect.height <= o.height);
    }
}
