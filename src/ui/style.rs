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

/// Temporary physical-review material for popup shells. The dock deliberately
/// remains on `GLASS_MATERIAL` so material judgment changes one surface class
/// at a time while popup blur is still unresolved.
pub const POPUP_REVIEW_MATERIAL: GlassMaterial = GlassMaterial {
    background: Rgba::new(0x20, 0x24, 0x2b, 0x98),
    foreground: GLASS_MATERIAL.foreground,
};

/// Shared shell geometry and colors for every interactive glass popup.  Domain
/// layouts may retain specialized controls, but their outer surface and normal
/// menu rows resolve through this one compact contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupStyle {
    pub material: GlassMaterial,
    pub border: u32,
    pub card_background: Rgba,
    pub card_border: u32,
    pub hover_background: Rgba,
    pub muted_foreground: u32,
    pub border_width: u16,
    pub outer_padding: u16,
    pub row_height: u16,
    pub row_horizontal_padding: u16,
    pub section_gap: u16,
    pub separator_height: u16,
    pub card_padding: u16,
    pub card_gap: u16,
    pub card_row_gap: u16,
    pub card_radius: u16,
}

pub const POPUP_STYLE: PopupStyle = PopupStyle {
    material: POPUP_REVIEW_MATERIAL,
    // The shell is deliberately quieter than the content cards.
    border: 0x2b3340,
    card_background: Rgba::new(0x2a, 0x30, 0x3a, 0x94),
    card_border: 0x394353,
    hover_background: Rgba::new(0x5a, 0x68, 0x7d, 0x72),
    muted_foreground: 0x7b8492,
    border_width: 1,
    outer_padding: 12,
    row_height: 30,
    row_horizontal_padding: 12,
    section_gap: 8,
    separator_height: 10,
    card_padding: 10,
    card_gap: 10,
    card_row_gap: 4,
    card_radius: 7,
};

/// Toast-only foreground and edge tokens. Keep notification toasts visually
/// legible without changing the shared Notification Center/popup language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToastStyle {
    pub title_foreground: u32,
    pub body_foreground: u32,
    pub card_border: u32,
}

pub const TOAST_STYLE: ToastStyle = ToastStyle {
    title_foreground: GLASS_MATERIAL.foreground,
    body_foreground: 0xdce3ea,
    card_border: 0x465365,
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
    fn bar_and_popups_keep_one_tint_with_an_explicit_review_alpha() {
        assert_eq!(
            BAR_STYLE.material.background.rgb(),
            POPUP_STYLE.material.background.rgb()
        );
        assert_eq!(BAR_STYLE.material.background.alpha, 0xb8);
        assert_eq!(POPUP_STYLE.material.background.alpha, 0x98);
        assert_eq!(BAR_STYLE.material.foreground, GLASS_MATERIAL.foreground);
    }

    #[test]
    fn popup_shell_keeps_the_shared_glass_contract_explicit() {
        assert_eq!(POPUP_STYLE.material, POPUP_REVIEW_MATERIAL);
        assert_eq!(POPUP_STYLE.material.background.rgb(), DOCK_BACKGROUND.rgb());
        assert_eq!(POPUP_STYLE.material.background.alpha, 0x98);
        assert_eq!(POPUP_STYLE.border_width, 1);
        assert_eq!(POPUP_STYLE.outer_padding, 12);
        assert_eq!(POPUP_STYLE.row_height, 30);
        assert_eq!(POPUP_STYLE.row_horizontal_padding, 12);
        assert_eq!(POPUP_STYLE.section_gap, 8);
        assert_eq!(POPUP_STYLE.card_padding, 10);
        assert_eq!(POPUP_STYLE.card_gap, 10);
        assert_eq!(POPUP_STYLE.card_radius, 7);
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
