//! The compiled-in visual contract shared by layout and X11 rendering.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontMetrics {
    pub ascent: i16,
    pub descent: i16,
}

impl FontMetrics {
    pub fn centered_baseline(self, height: u16) -> i16 {
        ((height as i16 - self.descent + self.ascent) / 2).max(1)
    }
}

pub trait TextMeasurer {
    fn measure_width(&self, text: &str) -> u16;
    fn metrics(&self) -> FontMetrics;

    fn measure_status_icon_width(&self, text: &str) -> u16 {
        self.measure_width(text)
    }

    fn baseline(&self, height: u16) -> i16 {
        self.metrics().centered_baseline(height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypographyRole {
    BarText,
    PopupText,
    StatusIcon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FontSpec {
    pub family: &'static str,
    pub style: &'static str,
    pub size: u16,
    /// Used only by pure layout/tests before Xft is initialized. Live X11
    /// layout always consumes metrics from the resolved Xft font.
    pub fallback_metrics: FontMetrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Typography {
    pub bar_text: FontSpec,
    pub popup_text: FontSpec,
    pub status_icon: FontSpec,
}

impl Typography {
    pub const fn role(self, role: TypographyRole) -> FontSpec {
        match role {
            TypographyRole::BarText => self.bar_text,
            TypographyRole::PopupText => self.popup_text,
            TypographyRole::StatusIcon => self.status_icon,
        }
    }
}

pub const TYPOGRAPHY: Typography = Typography {
    bar_text: FontSpec {
        family: "MesloLGS Nerd Font Mono",
        style: "Regular",
        size: 10,
        fallback_metrics: FontMetrics {
            ascent: 12,
            descent: 4,
        },
    },
    popup_text: FontSpec {
        family: "MesloLGS Nerd Font Mono",
        style: "Regular",
        size: 10,
        fallback_metrics: FontMetrics {
            ascent: 12,
            descent: 4,
        },
    },
    status_icon: FontSpec {
        family: "MesloLGS Nerd Font Mono",
        style: "Regular",
        size: 13,
        fallback_metrics: FontMetrics {
            ascent: 17,
            descent: 5,
        },
    },
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarStyle {
    pub material: GlassMaterial,
    pub workspace_background: u32,
    pub workspace_foreground: u32,
    pub menu_hover_background: u32,
    pub menu_hover_foreground: u32,
    pub menu_disabled_foreground: u32,
    /// Legacy whole-window opacity used only by the default depth-24 fallback.
    /// ARGB dock surfaces use the per-pixel alpha in `background` instead.
    pub fallback_window_opacity: f32,
    pub horizontal_padding: u16,
    pub item_spacing: u16,
}

pub const BAR_STYLE: BarStyle = BarStyle {
    material: GLASS_MATERIAL,
    workspace_background: 0x3a4352,
    workspace_foreground: 0xffffff,
    menu_hover_background: 0x4b5568,
    menu_hover_foreground: 0xffffff,
    menu_disabled_foreground: 0x7b8492,
    fallback_window_opacity: 0.90,
    horizontal_padding: 8,
    item_spacing: 4,
};

/// Fixed dark material for the ARGB capability rollout. Later material work may
/// replace this token, but every dock-background restoration uses this value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Rgba {
    pub const fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    pub const fn opaque_rgb(rgb: u32) -> Self {
        Self::new(
            ((rgb >> 16) & 0xff) as u8,
            ((rgb >> 8) & 0xff) as u8,
            (rgb & 0xff) as u8,
            u8::MAX,
        )
    }

    pub const fn rgb(self) -> u32 {
        ((self.red as u32) << 16) | ((self.green as u32) << 8) | self.blue as u32
    }
}

pub const DOCK_BACKGROUND: Rgba = Rgba::new(0x20, 0x24, 0x2b, 0xb8);

/// Shared visual material for normal xbar glass surfaces. The alpha is
/// provisional while blur and future material tuning remain pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlassMaterial {
    pub background: Rgba,
    pub foreground: u32,
}

pub const GLASS_MATERIAL: GlassMaterial = GlassMaterial {
    background: DOCK_BACKGROUND,
    foreground: 0xe6eaf0,
};

pub const STATUS_ITEM_GAP: i16 = 6;

impl TextMeasurer for BarStyle {
    fn measure_width(&self, text: &str) -> u16 {
        (text.chars().count() as u16).saturating_mul(8)
    }

    fn metrics(&self) -> FontMetrics {
        TYPOGRAPHY.bar_text.fallback_metrics
    }
}

pub fn opacity_cardinal(opacity: f32) -> u32 {
    (opacity.clamp(0.0, 1.0) * u32::MAX as f32).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typography_centralizes_text_roles() {
        assert_eq!(TYPOGRAPHY.bar_text.family, TYPOGRAPHY.popup_text.family);
        assert_eq!(TYPOGRAPHY.bar_text.style, TYPOGRAPHY.popup_text.style);
        assert_eq!(TYPOGRAPHY.bar_text.style, "Regular");
        assert_eq!(TYPOGRAPHY.bar_text.size, 10);
        assert_eq!(TYPOGRAPHY.popup_text.size, 10);
        assert_eq!(TYPOGRAPHY.status_icon.size, 13);
        assert_eq!(
            TYPOGRAPHY.role(TypographyRole::StatusIcon),
            TYPOGRAPHY.status_icon
        );
        assert_eq!(
            TYPOGRAPHY.bar_text.fallback_metrics,
            TYPOGRAPHY.popup_text.fallback_metrics
        );
        assert_eq!(BAR_STYLE.horizontal_padding, 8);
        assert_eq!(BAR_STYLE.item_spacing, 4);
    }

    #[test]
    fn opacity_uses_ewmh_cardinal_range() {
        assert_eq!(opacity_cardinal(1.0), u32::MAX);
        assert_eq!(opacity_cardinal(0.0), 0);
        assert_eq!(
            opacity_cardinal(BAR_STYLE.fallback_window_opacity),
            3_865_470_464
        );
    }

    #[test]
    fn dock_background_is_the_canonical_fractional_dark_material() {
        assert_eq!(DOCK_BACKGROUND, Rgba::new(0x20, 0x24, 0x2b, 0xb8));
        assert_eq!(GLASS_MATERIAL.background, DOCK_BACKGROUND);
        assert_eq!(BAR_STYLE.material, GLASS_MATERIAL);
        assert_eq!(BAR_STYLE.material.background.rgb(), 0x20_242b);
    }

    #[test]
    fn bar_and_popups_resolve_the_same_glass_material() {
        assert_eq!(BAR_STYLE.material.background, GLASS_MATERIAL.background);
        assert_eq!(BAR_STYLE.material.foreground, GLASS_MATERIAL.foreground);
    }

    #[test]
    fn baseline_is_shared_by_bar_text() {
        assert_eq!(BAR_STYLE.baseline(26), 17);
    }

    #[test]
    fn centered_baseline_uses_actual_metric_shape() {
        let bar_metrics = FontMetrics {
            ascent: 12,
            descent: 4,
        };
        let bar_baseline = bar_metrics.centered_baseline(26);
        assert_eq!(bar_baseline, 17);
        assert_eq!(
            FontMetrics {
                ascent: 16,
                descent: 5
            }
            .centered_baseline(26),
            18
        );
        assert_ne!(
            bar_baseline,
            FontMetrics {
                ascent: 16,
                descent: 5
            }
            .centered_baseline(26),
        );
        assert_eq!(
            bar_baseline - bar_metrics.ascent,
            5,
            "baseline is not the row top"
        );
    }
}
