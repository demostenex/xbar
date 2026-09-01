//! The compiled-in visual contract shared by layout and X11 rendering.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontMetrics {
    pub ascent: i16,
    pub descent: i16,
}

pub trait TextMeasurer {
    fn measure_width(&self, text: &str) -> u16;
    fn metrics(&self) -> FontMetrics;

    fn measure_status_icon_width(&self, text: &str) -> u16 {
        self.measure_width(text)
    }

    fn baseline(&self, height: u16) -> i16 {
        let metrics = self.metrics();
        ((height as i16 - metrics.descent + metrics.ascent) / 2).max(1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarStyle {
    pub font_family: &'static str,
    pub font_size: u16,
    pub metrics: FontMetrics,
    pub background: u32,
    pub foreground: u32,
    pub workspace_background: u32,
    pub workspace_foreground: u32,
    pub menu_hover_background: u32,
    pub menu_hover_foreground: u32,
    pub menu_disabled_foreground: u32,
    pub popup_background: u32,
    pub popup_foreground: u32,
    pub opacity: f32,
    pub horizontal_padding: u16,
    pub item_spacing: u16,
}

pub const BAR_STYLE: BarStyle = BarStyle {
    font_family: "MesloLGS Nerd Font Mono",
    font_size: 9,
    metrics: FontMetrics {
        ascent: 10,
        descent: 3,
    },
    background: 0x20242b,
    foreground: 0xe6eaf0,
    workspace_background: 0x3a4352,
    workspace_foreground: 0xffffff,
    menu_hover_background: 0x4b5568,
    menu_hover_foreground: 0xffffff,
    menu_disabled_foreground: 0x7b8492,
    popup_background: 0x20242b,
    popup_foreground: 0xe6eaf0,
    opacity: 0.90,
    horizontal_padding: 8,
    item_spacing: 4,
};

pub const STATUS_ITEM_GAP: i16 = 6;

impl TextMeasurer for BarStyle {
    fn measure_width(&self, text: &str) -> u16 {
        (text.chars().count() as u16).saturating_mul(8)
    }

    fn metrics(&self) -> FontMetrics {
        self.metrics
    }
}

pub fn opacity_cardinal(opacity: f32) -> u32 {
    (opacity.clamp(0.0, 1.0) * u32::MAX as f32).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_centralized() {
        assert_eq!(BAR_STYLE.font_family, "MesloLGS Nerd Font Mono");
        assert_eq!(BAR_STYLE.font_size, 9);
        assert_eq!(BAR_STYLE.horizontal_padding, 8);
        assert_eq!(BAR_STYLE.item_spacing, 4);
    }

    #[test]
    fn opacity_uses_ewmh_cardinal_range() {
        assert_eq!(opacity_cardinal(1.0), u32::MAX);
        assert_eq!(opacity_cardinal(0.0), 0);
        assert_eq!(opacity_cardinal(BAR_STYLE.opacity), 3_865_470_464);
    }

    #[test]
    fn baseline_is_shared_by_bar_text() {
        assert_eq!(BAR_STYLE.baseline(26), 16);
    }
}
