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
    pub(crate) owned_colormap: Option<u32>,
}

impl SurfaceVisual {
    pub(crate) const fn default(visual: u32, depth: u8, colormap: u32) -> Self {
        Self {
            kind: SurfaceKind::Default,
            visual,
            depth,
            colormap,
            alpha_mask: 0,
            owned_colormap: None,
        }
    }

    pub(crate) const fn argb(visual: u32, colormap: u32, alpha_mask: u16) -> Self {
        Self {
            kind: SurfaceKind::Argb,
            visual,
            depth: 32,
            colormap,
            alpha_mask,
            owned_colormap: Some(colormap),
        }
    }

    pub(crate) const fn opaque_pixel(self, rgb: u32) -> u32 {
        match self.kind {
            SurfaceKind::Default => rgb,
            SurfaceKind::Argb => 0xff00_0000 | (rgb & 0x00ff_ffff),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VisualCandidate {
    pub(crate) visual: u32,
    pub(crate) depth: u8,
    pub(crate) true_color: bool,
    pub(crate) alpha_mask: u16,
}

pub(crate) fn select_argb_visual(candidates: &[VisualCandidate]) -> Option<VisualCandidate> {
    candidates.iter().copied().find(|candidate| {
        candidate.depth == 32 && candidate.true_color && candidate.alpha_mask != 0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argb_selection_requires_true_color_depth_and_alpha() {
        let candidates = [
            VisualCandidate {
                visual: 1,
                depth: 32,
                true_color: true,
                alpha_mask: 0,
            },
            VisualCandidate {
                visual: 2,
                depth: 24,
                true_color: true,
                alpha_mask: 0xff,
            },
            VisualCandidate {
                visual: 3,
                depth: 32,
                true_color: false,
                alpha_mask: 0xff,
            },
            VisualCandidate {
                visual: 4,
                depth: 32,
                true_color: true,
                alpha_mask: 0xff,
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
                alpha_mask: 0,
            }]),
            None
        );
    }

    #[test]
    fn argb_capability_phase_keeps_pixels_opaque() {
        let surface = SurfaceVisual::argb(0x22, 0x33, 0xff);
        assert_eq!(surface.visual, 0x22);
        assert_eq!(surface.depth, 32);
        assert_eq!(surface.colormap, 0x33);
        assert_eq!(surface.alpha_mask, 0xff);
        assert_eq!(surface.owned_colormap, Some(0x33));
        assert_eq!(surface.opaque_pixel(0x20_242b), 0xff20_242b);
    }

    #[test]
    fn default_surface_leaves_existing_rgb_pixels_unchanged() {
        let surface = SurfaceVisual::default(0x21, 24, 0x31);
        assert_eq!(surface.opaque_pixel(0x20_242b), 0x20_242b);
        assert_eq!(surface.owned_colormap, None);
    }
}
