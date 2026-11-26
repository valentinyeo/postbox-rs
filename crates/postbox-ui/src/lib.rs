pub mod geometry;
pub mod palette;
pub mod theme;
pub mod timeline;
pub mod viewmodel;

#[cfg(feature = "desktop")]
pub mod desktop;

pub use geometry::RowGeometry;
pub use palette::{
    PaletteCommand, PaletteEntry, PaletteEntryKind, PaletteGroupKind, PaletteMode, PaletteSection,
};
pub use theme::{
    ARCHIVED_ROW_FOREGROUND_WEIGHT, ShadcnPalette, archived_row_color, archived_text_color,
    contrast_ratio, relative_luminance,
};
pub use timeline::{TimelineItem, build_timeline_items, extract_links_and_media, format_time_hm};
pub use viewmodel::{FocusTarget, PostboxLaunchContext, PostboxViewModel, SidebarChatRow};

pub const FONT_SANS: &str = "IBM Plex Sans";
pub const FONT_MONO: &str = "IBM Plex Mono";
pub const BASE_FONT_PX: f32 = 15.0;

pub fn scaled_px(base_px: f32, scale_percent: u16) -> f32 {
    (base_px * f32::from(scale_percent)) / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::{FontScale, ThemeChoice};

    #[test]
    fn font_scaling_matches_percent_contract() {
        assert!((scaled_px(BASE_FONT_PX, FontScale::DEFAULT) - 15.0).abs() < f32::EPSILON);
        assert!((scaled_px(BASE_FONT_PX, FontScale::clamped(120).percent()) - 18.0).abs() < 1e-4);
    }

    #[test]
    fn themes_expose_distinct_shadcn_backgrounds() {
        let zinc = theme::ShadcnPalette::for_choice(ThemeChoice::ZincDark);
        let amoled = theme::ShadcnPalette::for_choice(ThemeChoice::Amoled);
        let light = theme::ShadcnPalette::for_choice(ThemeChoice::Light);
        assert_eq!(amoled.background, 0x000000);
        assert_ne!(zinc.background, amoled.background);
        assert_ne!(zinc.background, light.background);
    }
}
