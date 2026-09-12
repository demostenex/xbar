use crate::core::{ChildrenDisplay, MenuItem, MenuItemId, MenuItemType};
use crate::core::{OutputState, WorkspaceState};
use crate::ui::style::{TextMeasurer, BAR_STYLE, POPUP_STYLE};
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

impl MenuRect {
    pub fn contains(self, x: i16, y: i16) -> bool {
        let x = i32::from(x);
        let y = i32::from(y);
        let left = i32::from(self.x);
        let top = i32::from(self.y);
        x >= left
            && x < left + i32::from(self.width)
            && y >= top
            && y < top + i32::from(self.height)
    }
}

/// The reusable content area inside a popup card. Card bounds are expressed in
/// root coordinates, so every specialized popup can derive its content axis
/// without inventing another local offset.
pub fn popup_card_content_rect(card: MenuRect) -> MenuRect {
    let inset = POPUP_STYLE.card_padding;
    MenuRect {
        x: card.x + inset as i16,
        y: card.y + inset as i16,
        width: card.width.saturating_sub(inset.saturating_mul(2)),
        height: card.height.saturating_sub(inset.saturating_mul(2)),
    }
}

pub const AUDIO_POPUP_BORDER: u16 = POPUP_STYLE.border_width;
const AUDIO_DEVICE_ROW_HEIGHT: u16 = 24;
const NETWORK_STATUS_CARD_HEIGHT: u16 = 72;
const NETWORK_SECTION_HEADER_HEIGHT: u16 = 24;
const NETWORK_ROW_HEIGHT: u16 = 28;

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkInterfaceCardLayout {
    pub card: MenuRect,
    pub header: MenuRect,
    pub rows: Vec<MenuRect>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NetworkPopupLayout {
    pub status_card: MenuRect,
    pub available_section: MenuRect,
    pub interfaces: Vec<NetworkInterfaceCardLayout>,
}

pub fn network_popup_content_height(interface_row_counts: &[usize]) -> u16 {
    let shell = POPUP_STYLE.outer_padding;
    let interface_heights: u32 = interface_row_counts
        .iter()
        .map(|count| u32::from(network_interface_card_height(*count)))
        .fold(0_u32, u32::saturating_add);
    let interface_gaps = u32::from(POPUP_STYLE.card_gap)
        .saturating_mul(interface_row_counts.len().saturating_sub(1) as u32);
    u32::from(shell)
        .saturating_mul(2)
        .saturating_add(u32::from(NETWORK_STATUS_CARD_HEIGHT))
        .saturating_add(u32::from(POPUP_STYLE.card_gap))
        .saturating_add(u32::from(NETWORK_SECTION_HEADER_HEIGHT))
        .saturating_add(u32::from(POPUP_STYLE.card_gap))
        .saturating_add(interface_heights)
        .saturating_add(interface_gaps)
        .min(u32::from(u16::MAX)) as u16
}

pub fn network_popup_layout(rect: MenuRect, interface_row_counts: &[usize]) -> NetworkPopupLayout {
    let shell = POPUP_STYLE.outer_padding as i16;
    let card_padding = POPUP_STYLE.card_padding as i16;
    let card_width = rect
        .width
        .saturating_sub(POPUP_STYLE.outer_padding.saturating_mul(2));
    let status_card = MenuRect {
        x: rect.x + shell,
        y: rect.y + shell,
        width: card_width,
        height: NETWORK_STATUS_CARD_HEIGHT,
    };
    let available_section = MenuRect {
        x: status_card.x,
        y: status_card.y + status_card.height as i16 + POPUP_STYLE.card_gap as i16,
        width: card_width,
        height: NETWORK_SECTION_HEADER_HEIGHT,
    };
    let mut cursor_y =
        available_section.y + available_section.height as i16 + POPUP_STYLE.card_gap as i16;
    let popup_bottom = rect.y + rect.height as i16;
    let mut interfaces = Vec::new();
    for count in interface_row_counts {
        if cursor_y >= popup_bottom {
            break;
        }
        let card_height =
            network_interface_card_height(*count).min((popup_bottom - cursor_y).max(0) as u16);
        let card = MenuRect {
            x: status_card.x,
            y: cursor_y,
            width: card_width,
            height: card_height,
        };
        let header = MenuRect {
            x: card.x + card_padding,
            y: card.y + card_padding,
            width: card
                .width
                .saturating_sub(POPUP_STYLE.card_padding.saturating_mul(2)),
            height: NETWORK_SECTION_HEADER_HEIGHT,
        };
        let row_area = card
            .height
            .saturating_sub(POPUP_STYLE.card_padding.saturating_mul(2))
            .saturating_sub(header.height)
            .saturating_sub(POPUP_STYLE.card_row_gap);
        let visible_count = usize::from(row_area / (NETWORK_ROW_HEIGHT + POPUP_STYLE.card_row_gap));
        let mut row_y = header.y + header.height as i16 + POPUP_STYLE.card_row_gap as i16;
        let rows = (0..(*count).min(visible_count))
            .map(|_| {
                let row = MenuRect {
                    x: header.x,
                    y: row_y,
                    width: header.width,
                    height: NETWORK_ROW_HEIGHT,
                };
                row_y += NETWORK_ROW_HEIGHT as i16 + POPUP_STYLE.card_row_gap as i16;
                row
            })
            .collect();
        cursor_y += card.height as i16 + POPUP_STYLE.card_gap as i16;
        interfaces.push(NetworkInterfaceCardLayout { card, header, rows });
    }
    NetworkPopupLayout {
        status_card,
        available_section,
        interfaces,
    }
}

fn network_interface_card_height(row_count: usize) -> u16 {
    let row_count = u32::try_from(row_count).unwrap_or(u32::MAX);
    let rows = u32::from(NETWORK_ROW_HEIGHT + POPUP_STYLE.card_row_gap)
        .saturating_mul(row_count)
        .saturating_sub(if row_count == 0 {
            0
        } else {
            u32::from(POPUP_STYLE.card_row_gap)
        });
    let height = u32::from(POPUP_STYLE.card_padding)
        .saturating_mul(2)
        .saturating_add(u32::from(NETWORK_SECTION_HEADER_HEIGHT))
        .saturating_add(if row_count == 0 {
            0
        } else {
            u32::from(POPUP_STYLE.card_row_gap)
        })
        .saturating_add(rows);
    height.min(u32::from(u16::MAX)) as u16
}

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
    let popup_bottom = popup.y + popup.height as i16;
    devices
        .iter()
        .take(8)
        .enumerate()
        .filter_map(|(index, device)| {
            let rect = MenuRect {
                x: popup.x + POPUP_STYLE.outer_padding as i16 + POPUP_STYLE.card_padding as i16,
                y: popup.y
                    + AUDIO_POPUP_BORDER as i16
                    + first_baseline
                    + index as i16 * AUDIO_DEVICE_ROW_HEIGHT as i16
                    - baseline_offset,
                width: popup.width.saturating_sub(
                    POPUP_STYLE
                        .outer_padding
                        .saturating_add(POPUP_STYLE.card_padding)
                        .saturating_mul(2),
                ),
                height: AUDIO_DEVICE_ROW_HEIGHT,
            };
            (rect.y >= popup.y && rect.y + rect.height as i16 <= popup_bottom).then_some(
                AudioDeviceRow {
                    name: device.name.clone(),
                    display_name: device.display_name.clone(),
                    rect,
                    baseline_offset,
                },
            )
        })
        .collect()
}

pub fn bluetooth_device_row(popup: MenuRect, index: usize) -> MenuRect {
    let card = MenuRect {
        x: popup.x + POPUP_STYLE.outer_padding as i16,
        y: popup.y + POPUP_STYLE.outer_padding as i16,
        width: popup.width.saturating_sub(POPUP_STYLE.outer_padding * 2),
        height: popup.height.saturating_sub(POPUP_STYLE.outer_padding * 2),
    };
    let content = popup_card_content_rect(card);
    MenuRect {
        x: content.x,
        y: content.y + 40 + index as i16 * 30,
        width: content.width,
        height: 28,
    }
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

impl PopupLayout {
    pub fn content_rect(&self) -> MenuRect {
        let padding = POPUP_STYLE.outer_padding;
        MenuRect {
            x: self.rect.x + padding as i16,
            y: self.rect.y + padding as i16,
            width: self.rect.width.saturating_sub(padding.saturating_mul(2)),
            height: self.rect.height.saturating_sub(padding.saturating_mul(2)),
        }
    }

    /// The popup renderer and event dispatcher consume the same item
    /// rectangles. Coordinates here are relative to the popup client window.
    pub fn item_at_local(&self, x: i16, y: i16) -> Option<&PopupItemRect> {
        self.items.iter().find(|item| {
            MenuRect {
                x: item.rect.x - self.rect.x,
                y: item.rect.y - self.rect.y,
                width: item.rect.width,
                height: item.rect.height,
            }
            .contains(x, y)
        })
    }
}

fn text_width<M: TextMeasurer>(measurer: &M, text: &str) -> u16 {
    measurer.measure_width(text)
}

pub fn truncate_text_to_width<M: TextMeasurer>(
    text: &str,
    width: u16,
    measurer: &M,
) -> Option<String> {
    if width == 0 {
        return None;
    }
    if text_width(measurer, text) <= width {
        return Some(text.to_owned());
    }
    let ellipsis = "…";
    if text_width(measurer, ellipsis) > width {
        return None;
    }
    let mut result = String::new();
    for ch in text.chars() {
        let candidate = format!("{result}{ch}{ellipsis}");
        if text_width(measurer, &candidate) > width {
            break;
        }
        result.push(ch);
    }
    Some(format!("{result}{ellipsis}"))
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
    let mut width = 120_u16.saturating_add(POPUP_STYLE.outer_padding.saturating_mul(2));
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
            POPUP_STYLE
                .outer_padding
                .saturating_mul(2)
                .saturating_add(POPUP_STYLE.row_horizontal_padding.saturating_mul(2))
                .saturating_add(8)
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
    let item_height = i32::from(POPUP_STYLE.row_height);
    let separator_height = i32::from(POPUP_STYLE.separator_height);
    let content_height: i32 = children
        .iter()
        .map(|item| {
            if item.item_type == MenuItemType::Separator {
                separator_height
            } else {
                item_height
            }
        })
        .fold(0_i32, i32::saturating_add);
    let height = (content_height + i32::from(POPUP_STYLE.outer_padding.saturating_mul(2)))
        .min(output.height.max(1) as i32)
        .max(1) as u16;
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
    let content = MenuRect {
        x: rect.x + POPUP_STYLE.outer_padding as i16,
        y: rect.y + POPUP_STYLE.outer_padding as i16,
        width: rect
            .width
            .saturating_sub(POPUP_STYLE.outer_padding.saturating_mul(2)),
        height: rect
            .height
            .saturating_sub(POPUP_STYLE.outer_padding.saturating_mul(2)),
    };
    let mut cursor = i32::from(content.y);
    let mut items = Vec::new();
    for item in children {
        let separator = item.item_type == MenuItemType::Separator;
        let h = if separator {
            separator_height
        } else {
            item_height
        };
        if cursor + h > i32::from(content.y) + i32::from(content.height) {
            break;
        }
        let item_rect = MenuRect {
            x: content.x,
            y: cursor as i16,
            width: content.width,
            height: h as u16,
        };
        cursor += h;
        let indicator = if item.children_display == Some(ChildrenDisplay::Submenu) {
            16
        } else {
            0
        };
        let shortcut_limit = content
            .width
            .saturating_sub(POPUP_STYLE.row_horizontal_padding.saturating_mul(2))
            .saturating_sub(indicator)
            .saturating_sub(4);
        let shortcut = shortcut_text(item)
            .and_then(|shortcut| truncate_text_to_width(&shortcut, shortcut_limit, measurer));
        let shortcut_width = shortcut
            .as_deref()
            .map(|text| text_width(measurer, text))
            .unwrap_or(0);
        let label_width = content
            .width
            .saturating_sub(POPUP_STYLE.row_horizontal_padding.saturating_mul(2))
            .saturating_sub(indicator)
            .saturating_sub(shortcut_width)
            .saturating_sub(4);
        let label = item
            .label
            .as_deref()
            .map(crate::ui::view::present_label)
            .unwrap_or_default();
        items.push(PopupItemRect {
            id: item.id,
            rect: item_rect,
            label: truncate_text_to_width(&label, label_width, measurer).unwrap_or_default(),
            enabled: item.enabled,
            separator,
            has_submenu: item.children_display == Some(ChildrenDisplay::Submenu)
                && !item.children.is_empty(),
            shortcut,
        });
    }
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
    allocate_context_with_reserved_right_and_tail(
        output,
        workspaces,
        menu,
        datetime,
        reserved_right,
        0,
        measurer,
    )
}

pub fn allocate_context_with_reserved_right_and_tail<M: TextMeasurer>(
    output: &OutputState,
    workspaces: &[WorkspaceState],
    menu: &[(MenuItemId, String, bool)],
    datetime: Option<&str>,
    reserved_right: i32,
    tail_width: i32,
    measurer: &M,
) -> (
    Vec<WorkspaceRect>,
    Vec<MenuRect>,
    Option<MenuRect>,
    MenuRect,
) {
    let output_left = output.x as i32;
    let output_right = output_left + output.width as i32;
    let workspace_natural_width = workspaces
        .first()
        .map(|workspace| {
            (text_width(measurer, &workspace.name) as i32
                + (BAR_STYLE.horizontal_padding as i32 * 2))
                .clamp(24.min(output.width as i32), output.width as i32)
        })
        .unwrap_or(0);
    let datetime_width = datetime
        .map(|text| {
            (text_width(measurer, text) as i32 + (BAR_STYLE.horizontal_padding as i32 * 2))
                .min(output.width as i32)
        })
        .unwrap_or(0);
    let datetime_x = (output_right - RIGHT_PADDING - datetime_width - tail_width).max(output_left);
    let content_right = datetime_x - if datetime.is_some() { RIGHT_PADDING } else { 0 };
    let content_left = output_left + LEFT_PADDING;
    let workspace_available = content_right
        .saturating_sub(content_left)
        .saturating_sub(reserved_right.max(0))
        .max(0);
    let workspace_width = workspace_natural_width.min(workspace_available) as u16;
    let available_menu = content_right
        .saturating_sub(content_left)
        .saturating_sub(workspace_width as i32)
        .saturating_sub(reserved_right.max(0))
        .max(0);
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
        let width = natural.min(available_menu.saturating_sub(menu_width));
        if width == 0 {
            break;
        }
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
    let future_right = content_right;
    let future_left = x.min(future_right).max(output_left);
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
    const ITEM_GAP: i32 = 4;
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
        right -= ITEM_GAP;
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
        let available = cursor.saturating_sub(left as i32);
        if available <= 0 {
            break;
        }
        let width = width.min(available);
        let x = cursor - width;
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
                height: 500,
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
            assert_eq!(row.label_position(popup), (29, 254 + index as i32 * 24));
            assert_eq!(
                row.rect,
                MenuRect {
                    x: 1602,
                    y: 264 + index as i16 * 24,
                    width: 296,
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
            assert!(row.contains(row.rect.x + row.rect.width as i16 - 1, row.rect.y + 23));
            assert!(!row.contains(row.rect.x - 1, row.rect.y));
            assert!(!row.contains(row.rect.x + row.rect.width as i16, row.rect.y));
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
                    assert_eq!((local_x, local_baseline), (29, 254 + index as i32 * 24));
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
            assert_eq!(row.label_position(popup), (29, 324 + index as i32 * 24));
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
            vec![132, 156, 180]
        );
        assert_eq!(slots[0].x + slots[0].width as i16 + 4, slots[1].x);
        assert_eq!(slots[1].x + slots[1].width as i16 + 4, slots[2].x);
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
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].x, 120);
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

    struct WidthMeasurer(u16);

    impl TextMeasurer for WidthMeasurer {
        fn measure_width(&self, text: &str) -> u16 {
            text.chars().count() as u16 * self.0
        }

        fn metrics(&self) -> crate::ui::style::FontMetrics {
            crate::ui::style::FontMetrics {
                ascent: 16,
                descent: 5,
            }
        }
    }

    #[test]
    fn popup_layout_tracks_changed_resolved_text_widths() {
        let mut parent = popup_parent();
        parent.children[0].label = Some("A deliberately wider popup label".into());
        let anchor = MenuRect {
            x: 120,
            y: 20,
            width: 80,
            height: 26,
        };
        let narrow =
            popup_layout_with_measurer(&output(), &parent, anchor, false, &WidthMeasurer(4));
        let wide =
            popup_layout_with_measurer(&output(), &parent, anchor, false, &WidthMeasurer(12));
        assert!(wide.rect.width > narrow.rect.width);
        assert!(wide.rect.width <= output().width);
    }

    #[test]
    fn truncation_is_deterministic_and_never_exceeds_allocation() {
        let measurer = WidthMeasurer(8);
        assert_eq!(
            truncate_text_to_width("abcdef", 48, &measurer),
            Some("abcdef".into())
        );
        let clipped = truncate_text_to_width("abcdefgh", 32, &measurer).expect("ellipsis fits");
        assert!(measurer.measure_width(&clipped) <= 32);
        assert_eq!(truncate_text_to_width("abcdef", 0, &measurer), None);
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
    fn revision_eight_submenus_project_all_children_into_popups() {
        let child = |id: i32, label: &str| MenuItem {
            id: MenuItemId(id),
            label: Some(label.into()),
            enabled: true,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: None,
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![],
        };
        let tools = MenuItem {
            id: MenuItemId(8),
            label: Some("Tools".into()),
            enabled: true,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: Some(ChildrenDisplay::Submenu),
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![child(16, "Settings"), child(17, "Reload")],
        };
        let view = MenuItem {
            id: MenuItemId(9),
            label: Some("View".into()),
            enabled: true,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: Some(ChildrenDisplay::Submenu),
            shortcut: None,
            icon_name: None,
            action: None,
            children: vec![
                child(18, "show/hide Menu"),
                child(19, "Zoom +"),
                child(20, "Zoom -"),
            ],
        };
        let tools_popup = popup_layout(
            &output(),
            &tools,
            MenuRect {
                x: 120,
                y: 20,
                width: 80,
                height: 26,
            },
            true,
        );
        let view_popup = popup_layout(
            &output(),
            &view,
            MenuRect {
                x: 120,
                y: 20,
                width: 80,
                height: 26,
            },
            true,
        );
        assert_eq!(tools_popup.items.len(), 2);
        assert_eq!(view_popup.items.len(), 3);
    }

    #[test]
    fn menu_draw_and_hit_share_the_popup_item_rect() {
        let popup = popup_layout(
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
        let item = &popup.items[0];
        let local_x = item.rect.x - popup.rect.x + 1;
        let local_y = item.rect.y - popup.rect.y + 1;
        assert_eq!(
            popup.item_at_local(local_x, local_y).map(|item| item.id),
            Some(item.id)
        );
        assert!(popup
            .item_at_local(item.rect.x - popup.rect.x + item.rect.width as i16, local_y)
            .is_none());
    }

    #[test]
    fn popup_shell_keeps_card_content_symmetric_and_rows_inside_it() {
        let popup = popup_layout(
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
        let content = popup.content_rect();
        assert_eq!(content.x - popup.rect.x, POPUP_STYLE.outer_padding as i16);
        assert_eq!(
            popup.rect.x + popup.rect.width as i16 - (content.x + content.width as i16),
            POPUP_STYLE.outer_padding as i16
        );
        assert_eq!(popup.items[0].rect.height, POPUP_STYLE.row_height);
        assert!(popup.items.iter().all(|item| {
            item.rect.x >= content.x
                && item.rect.x + item.rect.width as i16 <= content.x + content.width as i16
        }));
    }

    #[test]
    fn popup_vertical_overflow_omits_rows_and_hit_targets() {
        let mut small = output();
        small.height = 60;
        let popup = popup_layout(
            &small,
            &popup_parent(),
            MenuRect {
                x: 20,
                y: 20,
                width: 80,
                height: 26,
            },
            false,
        );
        assert!(popup.items.iter().all(|item| {
            item.rect.y >= popup.rect.y
                && item.rect.y + item.rect.height as i16 <= popup.rect.y + popup.rect.height as i16
        }));
        assert!(popup.item_at_local(20, 59).is_none());
    }

    #[test]
    fn narrow_context_widths_do_not_wrap_or_escape_output() {
        let mut narrow = output();
        narrow.width = 1;
        let workspaces = vec![WorkspaceState {
            name: "1".into(),
            output: Some("HDMI-1".into()),
            focused: true,
        }];
        let (_, menus, datetime, future) = allocate_context_with_reserved_right(
            &narrow,
            &workspaces,
            &[(MenuItemId(1), "A very long menu label".into(), true)],
            Some("A very long date"),
            i32::MAX,
            &WidthMeasurer(8),
        );
        assert!(menus.iter().all(|rect| rect.width == 0));
        assert!(datetime.is_none_or(|rect| rect.width <= narrow.width));
        assert!(future.width <= narrow.width);
    }

    #[test]
    fn plugins_clip_to_remaining_viewport_instead_of_wrapping() {
        let labels = vec!["a very long plugin".into(), "another long plugin".into()];
        let rects = allocate_plugins(100, 130, &labels, &WidthMeasurer(8));
        assert!(!rects.is_empty());
        assert!(rects
            .iter()
            .all(|rect| { rect.x >= 100 && rect.x + rect.width as i16 <= 130 && rect.width > 0 }));
    }

    #[test]
    fn network_style_rows_use_the_same_rect_for_draw_and_hit_bounds() {
        let row = MenuRect {
            x: 1570,
            y: 108,
            width: 330,
            height: 22,
        };
        assert!(row.contains(1570, 108));
        assert!(row.contains(1899, 129));
        assert!(!row.contains(1900, 129));
        assert!(!row.contains(1570, 130));
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

    #[test]
    fn network_section_has_a_positive_gap_before_the_first_interface_card() {
        let rect = MenuRect {
            x: 1540,
            y: 26,
            width: 380,
            height: network_popup_content_height(&[2, 1]),
        };
        let network = network_popup_layout(rect, &[2, 1]);
        let section_bottom = network.available_section.y + network.available_section.height as i16;
        assert!(section_bottom + POPUP_STYLE.card_gap as i16 <= network.interfaces[0].card.y);
    }

    #[test]
    fn network_interfaces_have_independent_non_overlapping_cards_and_rows() {
        let rect = MenuRect {
            x: 1540,
            y: 26,
            width: 380,
            height: network_popup_content_height(&[2, 1]),
        };
        let network = network_popup_layout(rect, &[2, 1]);
        let wlan0 = &network.interfaces[0];
        let wlan1 = &network.interfaces[1];
        assert!(wlan0.card.y + wlan0.card.height as i16 <= wlan1.card.y);
        for card in &network.interfaces {
            for row in &card.rows {
                assert!(row.x >= card.card.x + POPUP_STYLE.card_padding as i16);
                assert!(
                    row.x + row.width as i16
                        <= card.card.x + card.card.width as i16 - POPUP_STYLE.card_padding as i16
                );
                assert!(row.y >= card.card.y);
                assert!(row.y + row.height as i16 <= card.card.y + card.card.height as i16);
            }
        }
    }

    #[test]
    fn audio_rows_respect_popup_outer_and_card_padding() {
        let popup = MenuRect {
            x: 100,
            y: 200,
            width: 340,
            height: 400,
        };
        let rows = audio_device_rows(
            popup,
            &[crate::core::AudioDevice {
                name: "sink".to_owned(),
                display_name: "Sink".to_owned(),
            }],
            254,
            &AudioMeasurer(crate::ui::style::FontMetrics {
                ascent: 16,
                descent: 5,
            }),
        );
        let row = &rows[0].rect;
        let inset = (POPUP_STYLE.outer_padding + POPUP_STYLE.card_padding) as i16;
        assert!(row.x >= popup.x + inset);
        assert!(row.x + row.width as i16 <= popup.x + popup.width as i16 - inset);
        let card = MenuRect {
            x: popup.x + POPUP_STYLE.outer_padding as i16,
            y: popup.y + POPUP_STYLE.outer_padding as i16,
            width: popup.width.saturating_sub(POPUP_STYLE.outer_padding * 2),
            height: popup.height.saturating_sub(POPUP_STYLE.outer_padding * 2),
        };
        assert_eq!(row.x, popup_card_content_rect(card).x);
    }

    #[test]
    fn bluetooth_rows_respect_popup_outer_and_card_padding() {
        let popup = MenuRect {
            x: 100,
            y: 200,
            width: 330,
            height: 180,
        };
        let row = bluetooth_device_row(popup, 0);
        let inset = (POPUP_STYLE.outer_padding + POPUP_STYLE.card_padding) as i16;
        assert!(row.x >= popup.x + inset);
        assert!(row.x + row.width as i16 <= popup.x + popup.width as i16 - inset);
        assert!(row.y >= popup.y + POPUP_STYLE.outer_padding as i16);
        assert!(row.y + row.height as i16 <= popup.y + popup.height as i16);
    }

    #[test]
    fn specialized_cards_use_canonical_content_top_and_left_axes() {
        let popup = MenuRect {
            x: 100,
            y: 200,
            width: 340,
            height: 500,
        };
        let card = MenuRect {
            x: popup.x + POPUP_STYLE.outer_padding as i16,
            y: popup.y + POPUP_STYLE.outer_padding as i16,
            width: popup.width.saturating_sub(POPUP_STYLE.outer_padding * 2),
            height: 100,
        };
        let content = popup_card_content_rect(card);
        assert_eq!(content.x - popup.x, 22);
        assert_eq!(content.y - popup.y, 22);

        let bluetooth_row = bluetooth_device_row(popup, 0);
        assert_eq!(bluetooth_row.x, content.x);
        assert_eq!(bluetooth_row.y, content.y + 40);

        let audio_rows = audio_device_rows(
            popup,
            &[crate::core::AudioDevice {
                name: "sink".into(),
                display_name: "Sink".into(),
            }],
            254,
            &AudioMeasurer(crate::ui::style::FontMetrics {
                ascent: 16,
                descent: 5,
            }),
        );
        assert_eq!(audio_rows[0].rect.x, content.x);
        let (label_x, _) = audio_rows[0].label_position(popup);
        assert_eq!(
            popup.x + AUDIO_POPUP_BORDER as i16 + label_x as i16,
            content.x + 8
        );
    }

    #[test]
    fn bluetooth_rows_remain_inside_their_card_after_top_inset() {
        let popup = MenuRect {
            x: 100,
            y: 200,
            width: 330,
            height: 180,
        };
        let card = MenuRect {
            x: popup.x + POPUP_STYLE.outer_padding as i16,
            y: popup.y + POPUP_STYLE.outer_padding as i16,
            width: popup.width.saturating_sub(POPUP_STYLE.outer_padding * 2),
            height: popup.height.saturating_sub(POPUP_STYLE.outer_padding * 2),
        };
        let row = bluetooth_device_row(popup, 0);
        assert!(row.y >= card.y + POPUP_STYLE.card_padding as i16);
        assert!(row.y + row.height as i16 <= card.y + card.height as i16);
        assert!(
            row.x + row.width as i16
                <= card.x + card.width as i16 - POPUP_STYLE.card_padding as i16
        );
    }
}
