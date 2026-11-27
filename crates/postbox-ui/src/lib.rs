pub mod geometry;
pub mod m3;
pub mod m4;
pub mod palette;
pub mod sync_bridge;
pub mod theme;
pub mod timeline;
pub mod viewmodel;

#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "desktop")]
mod desktop_m3;
#[cfg(feature = "desktop")]
mod desktop_m4;

pub use geometry::RowGeometry;
pub use m3::{M3State, MentionPopupState, ShortcodePopupState};
pub use m4::M4State;
pub use palette::{
    PaletteCommand, PaletteEntry, PaletteEntryKind, PaletteGroupKind, PaletteMode, PaletteSection,
};
pub use sync_bridge::{
    HubConnectionStatus, MediaDownloadTarget, SyncBridge, SyncCommand, SyncEvent, SyncWorkerHandle,
    create_sync_bridge,
};
pub use theme::{
    ARCHIVED_ROW_FOREGROUND_WEIGHT, ShadcnPalette, archived_row_color, archived_text_color,
    contrast_ratio, relative_luminance,
};
pub use timeline::{
    OutboxBadge, ReadTickDisplay, TimelineItem, TimelineOptions, build_timeline_items,
    build_timeline_items_with_options, extract_links_and_media, format_time_hm,
};
pub use viewmodel::{
    FocusTarget, PostboxLaunchContext, PostboxViewModel, SidebarChatRow, ThreadPanelState,
};

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
