use crate::ui::style::Rgba;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SurfaceKind {
    Default,
    Argb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SurfaceVisual {
    pub(crate) kind: SurfaceKind,
    pub(crate) visual: u32,
    pub(crate) depth: u8,
    pub(crate) colormap: u32,
    pub(crate) alpha_mask: u16,
    pub(crate) pixel_format: Option<DirectPixelFormat>,
    pub(crate) owned_colormap: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirectPixelFormat {
    pub(crate) red_shift: u16,
    pub(crate) red_mask: u16,
    pub(crate) green_shift: u16,
    pub(crate) green_mask: u16,
    pub(crate) blue_shift: u16,
    pub(crate) blue_mask: u16,
    pub(crate) alpha_shift: u16,
    pub(crate) alpha_mask: u16,
}

impl DirectPixelFormat {
    fn pack_channel(value: u8, mask: u16, shift: u16) -> u32 {
        (((value as u32 * mask as u32 + 127) / 255) << shift) as u32
    }

    pub(crate) fn pack(self, color: Rgba) -> u32 {
        Self::pack_channel(color.red, self.red_mask, self.red_shift)
            | Self::pack_channel(color.green, self.green_mask, self.green_shift)
            | Self::pack_channel(color.blue, self.blue_mask, self.blue_shift)
            | Self::pack_channel(color.alpha, self.alpha_mask, self.alpha_shift)
    }
}

impl SurfaceVisual {
    pub(crate) const fn default(visual: u32, depth: u8, colormap: u32) -> Self {
        Self {
            kind: SurfaceKind::Default,
            visual,
            depth,
            colormap,
            alpha_mask: 0,
            pixel_format: None,
            owned_colormap: None,
        }
    }

    pub(crate) const fn argb(visual: u32, colormap: u32, pixel_format: DirectPixelFormat) -> Self {
        Self {
            kind: SurfaceKind::Argb,
            visual,
            depth: 32,
            colormap,
            alpha_mask: pixel_format.alpha_mask,
            pixel_format: Some(pixel_format),
            owned_colormap: Some(colormap),
        }
    }

    pub(crate) fn background_pixel(self, color: Rgba) -> u32 {
        match self.kind {
            SurfaceKind::Default => color.rgb(),
            SurfaceKind::Argb => self
                .pixel_format
                .expect("ARGB surface always has a direct pixel format")
                .pack(color),
        }
    }

    pub(crate) fn opaque_pixel(self, rgb: u32) -> u32 {
        self.background_pixel(Rgba::opaque_rgb(rgb))
    }

    pub(crate) const fn window_opacity(self, fallback: f32) -> Option<f32> {
        match self.kind {
            SurfaceKind::Argb => None,
            SurfaceKind::Default => Some(fallback),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VisualCandidate {
    pub(crate) visual: u32,
    pub(crate) depth: u8,
    pub(crate) true_color: bool,
    pub(crate) pixel_format: DirectPixelFormat,
}

pub(crate) fn select_argb_visual(candidates: &[VisualCandidate]) -> Option<VisualCandidate> {
    candidates.iter().copied().find(|candidate| {
        candidate.depth == 32 && candidate.true_color && candidate.pixel_format.alpha_mask != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARGB_8888: DirectPixelFormat = DirectPixelFormat {
        red_shift: 16,
        red_mask: 0xff,
        green_shift: 8,
        green_mask: 0xff,
        blue_shift: 0,
        blue_mask: 0xff,
        alpha_shift: 24,
        alpha_mask: 0xff,
    };

    #[test]
    fn argb_selection_requires_true_color_depth_and_alpha() {
        let candidates = [
            VisualCandidate {
                visual: 1,
                depth: 32,
                true_color: true,
                pixel_format: DirectPixelFormat {
                    alpha_mask: 0,
                    ..ARGB_8888
                },
            },
            VisualCandidate {
                visual: 2,
                depth: 24,
                true_color: true,
                pixel_format: ARGB_8888,
            },
            VisualCandidate {
                visual: 3,
                depth: 32,
                true_color: false,
                pixel_format: ARGB_8888,
            },
            VisualCandidate {
                visual: 4,
                depth: 32,
                true_color: true,
                pixel_format: ARGB_8888,
            },
        ];

        assert_eq!(select_argb_visual(&candidates), Some(candidates[3]));
    }

    #[test]
    fn missing_argb_visual_has_no_selection() {
        assert_eq!(
            select_argb_visual(&[VisualCandidate {
                visual: 1,
                depth: 32,
                true_color: true,
                pixel_format: DirectPixelFormat {
                    alpha_mask: 0,
                    ..ARGB_8888
                },
            }]),
            None
        );
    }

    #[test]
    fn argb_surface_packs_fractional_background_with_native_channel_masks() {
        let surface = SurfaceVisual::argb(0x22, 0x33, ARGB_8888);
        assert_eq!(surface.visual, 0x22);
        assert_eq!(surface.depth, 32);
        assert_eq!(surface.colormap, 0x33);
        assert_eq!(surface.alpha_mask, 0xff);
        assert_eq!(surface.owned_colormap, Some(0x33));
        assert_eq!(
            surface.background_pixel(Rgba::new(0x20, 0x24, 0x2b, 0xb8)),
            0xb820_242b
        );
        assert_eq!(surface.opaque_pixel(0x20_242b), 0xff20_242b);
        assert_eq!(surface.window_opacity(0.90), None);
    }

    #[test]
    fn background_restoration_paths_share_one_canonical_native_pixel() {
        let surface = SurfaceVisual::argb(0x22, 0x33, ARGB_8888);
        let initial_map = surface.background_pixel(Rgba::new(0x20, 0x24, 0x2b, 0xb8));
        let full_redraw = surface.background_pixel(Rgba::new(0x20, 0x24, 0x2b, 0xb8));
        let regional_redraw = surface.background_pixel(Rgba::new(0x20, 0x24, 0x2b, 0xb8));

        assert_eq!(initial_map, full_redraw);
        assert_eq!(full_redraw, regional_redraw);
    }

    #[test]
    fn native_packing_follows_the_selected_channel_shifts() {
        let bgr_format = DirectPixelFormat {
            red_shift: 0,
            red_mask: 0xff,
            green_shift: 8,
            green_mask: 0xff,
            blue_shift: 16,
            blue_mask: 0xff,
            alpha_shift: 24,
            alpha_mask: 0xff,
        };

        assert_eq!(
            bgr_format.pack(Rgba::new(0x20, 0x24, 0x2b, 0xb8)),
            0xb82b_2420
        );
    }

    #[test]
    fn default_surface_leaves_existing_rgb_pixels_unchanged() {
        let surface = SurfaceVisual::default(0x21, 24, 0x31);
        assert_eq!(
            surface.background_pixel(Rgba::new(0x20, 0x24, 0x2b, 0xb8)),
            0x20_242b
        );
        assert_eq!(surface.opaque_pixel(0x20_242b), 0x20_242b);
        assert_eq!(surface.owned_colormap, None);
        assert_eq!(surface.window_opacity(0.90), Some(0.90));
    }
}
