use postbox_core::ThemeChoice;

/// Foreground blending weight (`180 / 255`) for archived chat rows, matching Zig `theme.zig:113-124`.
pub const ARCHIVED_ROW_FOREGROUND_WEIGHT: u32 = 180;

/// Semantic shadcn/ui color roles (24-bit RGB hex) with WCAG 2.1 AA (`>= 4.5`) contrast,
/// crisp 1px borders, and focus rings across ZincDark, Amoled, and Light themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadcnPalette {
    pub background: u32,
    pub foreground: u32,
    pub sidebar: u32,
    pub card: u32,
    pub popover: u32,
    pub primary: u32,
    pub primary_foreground: u32,
    pub secondary: u32,
    pub muted: u32,
    pub muted_foreground: u32,
    pub accent: u32,
    pub border: u32,
    pub ring: u32,
    pub destructive: u32,
    pub bubble_outgoing: u32,
    pub bubble_outgoing_foreground: u32,
    pub bubble_outgoing_muted: u32,
    pub bubble_incoming: u32,
    pub selection_fill: u32,
    pub selection_foreground: u32,
    pub draft_foreground: u32,
}

impl ShadcnPalette {
    pub fn for_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::ZincDark => Self {
                background: 0x09090B,
                foreground: 0xFAFAFA,
                sidebar: 0x0C0C0F,
                card: 0x111114,
                popover: 0x111114,
                primary: 0xFAFAFA,
                primary_foreground: 0x18181B,
                secondary: 0x18181B,
                muted: 0x18181B,
                muted_foreground: 0xA1A1AA,
                accent: 0x27272A,
                border: 0x27272A,
                ring: 0xD4D4D8,
                destructive: 0xF87171,
                bubble_outgoing: 0x27272A,
                bubble_outgoing_foreground: 0xFAFAFA,
                bubble_outgoing_muted: 0xA1A1AA,
                bubble_incoming: 0x141417,
                selection_fill: 0xFAFAFA,
                selection_foreground: 0x18181B,
                draft_foreground: 0xFAFAFA,
            },
            ThemeChoice::Amoled => Self {
                background: 0x000000,
                foreground: 0xFAFAFA,
                sidebar: 0x050506,
                card: 0x0A0A0C,
                popover: 0x0A0A0C,
                primary: 0xFAFAFA,
                primary_foreground: 0x000000,
                secondary: 0x121215,
                muted: 0x121215,
                muted_foreground: 0xA1A1AA,
                accent: 0x1F1F23,
                border: 0x222226,
                ring: 0xD4D4D8,
                destructive: 0xF87171,
                bubble_outgoing: 0x1F1F24,
                bubble_outgoing_foreground: 0xFAFAFA,
                bubble_outgoing_muted: 0xA1A1AA,
                bubble_incoming: 0x0D0D10,
                selection_fill: 0xFAFAFA,
                selection_foreground: 0x000000,
                draft_foreground: 0xFAFAFA,
            },
            ThemeChoice::Light => Self {
                background: 0xFFFFFF,
                foreground: 0x09090B,
                sidebar: 0xFAFAFA,
                card: 0xFFFFFF,
                popover: 0xFFFFFF,
                primary: 0x18181B,
                primary_foreground: 0xFAFAFA,
                secondary: 0xF4F4F5,
                muted: 0xF4F4F5,
                muted_foreground: 0x52525B,
                accent: 0xF4F4F5,
                border: 0xE4E4E7,
                ring: 0x18181B,
                destructive: 0xB91C1C,
                bubble_outgoing: 0x18181B,
                bubble_outgoing_foreground: 0xFAFAFA,
                bubble_outgoing_muted: 0xD4D4D8,
                bubble_incoming: 0xF4F4F5,
                selection_fill: 0x18181B,
                selection_foreground: 0xFAFAFA,
                draft_foreground: 0x18181B,
            },
        }
    }

    /// Returns the WCAG-verified draft preview color on `bg`: prefers `primary` when it meets
    /// `>= 4.5` contrast, otherwise falls back to `foreground`.
    pub fn draft_color(&self, bg: u32) -> u32 {
        if contrast_ratio(self.primary, bg) >= 4.5 {
            self.primary
        } else {
            self.foreground
        }
    }
}

fn linear_channel(channel: u8) -> f64 {
    let v = f64::from(channel) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// Computes WCAG 2.1 relative luminance for a 24-bit `0xRRGGBB` color.
pub fn relative_luminance(rgb: u32) -> f64 {
    let r = ((rgb >> 16) & 0xff) as u8;
    let g = ((rgb >> 8) & 0xff) as u8;
    let b = (rgb & 0xff) as u8;
    0.2126 * linear_channel(r) + 0.7152 * linear_channel(g) + 0.0722 * linear_channel(b)
}

/// Computes WCAG 2.1 contrast ratio between two `0xRRGGBB` colors (range `1.0..=21.0`).
pub fn contrast_ratio(a: u32, b: u32) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// Blends `fg` toward `bg` with integer weight `180 / 255` per RGB channel,
/// matching Zig `theme.zig:113-124` (`archivedRowColor`).
pub const fn archived_row_color(fg: u32, bg: u32) -> u32 {
    let w = ARCHIVED_ROW_FOREGROUND_WEIGHT;
    let inv = 255 - w;
    let r = (((fg >> 16) & 0xff) * w + ((bg >> 16) & 0xff) * inv) / 255;
    let g = (((fg >> 8) & 0xff) * w + ((bg >> 8) & 0xff) * inv) / 255;
    let b = ((fg & 0xff) * w + (bg & 0xff) * inv) / 255;
    (r << 16) | (g << 8) | b
}

/// Returns `archived_row_color(fg, bg)` when its contrast on `bg` remains `>= 4.5`,
/// otherwise falls back to `fg` so muted previews never drop below WCAG AA.
pub fn archived_text_color(fg: u32, bg: u32) -> u32 {
    let blended = archived_row_color(fg, bg);
    if contrast_ratio(blended, bg) >= 4.5 {
        blended
    } else {
        fg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wcag_contrast_reference_values() {
        assert!((contrast_ratio(0x000000, 0xFFFFFF) - 21.0).abs() < 1e-6);
        assert!((contrast_ratio(0x252B28, 0x252B28) - 1.0).abs() < 1e-6);
        assert!((contrast_ratio(0x777777, 0xFFFFFF) - 4.478089).abs() < 1e-5);
        assert!(
            (contrast_ratio(0x123456, 0xABCDEF) - contrast_ratio(0xABCDEF, 0x123456)).abs() < 1e-12
        );
    }

    #[test]
    fn all_three_themes_meet_wcag_aa_contrast() {
        for choice in [
            ThemeChoice::ZincDark,
            ThemeChoice::Amoled,
            ThemeChoice::Light,
        ] {
            let p = ShadcnPalette::for_choice(choice);
            let surfaces = [
                ("background", p.background),
                ("sidebar", p.sidebar),
                ("card", p.card),
                ("popover", p.popover),
                ("secondary", p.secondary),
                ("muted", p.muted),
                ("accent", p.accent),
                ("bubble_incoming", p.bubble_incoming),
            ];
            let texts = [
                ("foreground", p.foreground),
                ("muted_foreground", p.muted_foreground),
                ("primary", p.primary),
                ("destructive", p.destructive),
                ("draft_foreground", p.draft_foreground),
            ];
            for (sname, surface) in surfaces {
                for (tname, text) in texts {
                    let ratio = contrast_ratio(text, surface);
                    assert!(
                        ratio >= 4.5,
                        "{choice:?} {tname} on {sname} failed WCAG AA: {ratio:.2}"
                    );
                }
            }
            assert!(contrast_ratio(p.primary_foreground, p.primary) >= 4.5);
            assert!(contrast_ratio(p.selection_foreground, p.selection_fill) >= 4.5);
            assert!(contrast_ratio(p.bubble_outgoing_foreground, p.bubble_outgoing) >= 4.5);
            assert!(contrast_ratio(p.bubble_outgoing_muted, p.bubble_outgoing) >= 4.5);
            assert!(contrast_ratio(archived_row_color(p.foreground, p.sidebar), p.sidebar) >= 4.5);
            assert!(contrast_ratio(archived_row_color(p.foreground, p.accent), p.accent) >= 4.5);
            assert!(
                contrast_ratio(
                    archived_text_color(p.muted_foreground, p.sidebar),
                    p.sidebar
                ) >= 4.5
            );
            assert!(contrast_ratio(p.draft_color(p.sidebar), p.sidebar) >= 4.5);
            assert!(contrast_ratio(p.draft_color(p.accent), p.accent) >= 4.5);
        }
    }

    #[test]
    fn archived_row_color_matches_180_255_blend() {
        let zinc = ShadcnPalette::for_choice(ThemeChoice::ZincDark);
        let amoled = ShadcnPalette::for_choice(ThemeChoice::Amoled);
        let light = ShadcnPalette::for_choice(ThemeChoice::Light);
        assert_eq!(archived_row_color(zinc.foreground, zinc.sidebar), 0xB4B4B4);
        assert_eq!(
            archived_row_color(amoled.foreground, amoled.sidebar),
            0xB1B1B2
        );
        assert_eq!(
            archived_row_color(light.foreground, light.sidebar),
            0x4F4F51
        );
    }
}
