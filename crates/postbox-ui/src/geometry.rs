use crate::scaled_px;
use postbox_core::FontScale;

pub const TYPE_TITLE_PX: f32 = 24.0;
pub const TYPE_CHAT_NAME_PX: f32 = 15.0;
pub const TYPE_BODY_PX: f32 = 15.0;
pub const TYPE_ROW_NAME_PX: f32 = 14.0;
pub const TYPE_PREVIEW_PX: f32 = 13.0;
pub const TYPE_TIME_PX: f32 = 12.0;
pub const TYPE_LABEL_PX: f32 = 11.0;

pub const SIDEBAR_WIDTH_PX: f32 = 300.0;
pub const VIEW_RAIL_WIDTH_PX: f32 = 52.0;
pub const CHAT_COLUMN_MAX_WIDTH_PX: f32 = 920.0;
pub const BASE_CHAT_ROW_HEIGHT_PX: f32 = 56.0;
pub const ROW_PADDING_PX: f32 = 4.0;
pub const ROW_MIN_BAND_PX: f32 = 24.0;

/// Line-height ratio for IBM Plex Sans glyphs + ascender/descender clearance.
pub const LINE_HEIGHT_FACTOR: f32 = 1.4;

/// Dynamic sidebar chat row geometry that scales smoothly from `60%` to `160%`
/// so chat names, timestamps, and previews never clip at larger font sizes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RowGeometry {
    pub padding: f32,
    pub name_top: f32,
    pub name_font_px: f32,
    pub name_band: f32,
    pub preview_top: f32,
    pub preview_font_px: f32,
    pub preview_band: f32,
    pub preview_bottom: f32,
    pub time_font_px: f32,
    pub height: f32,
}

impl RowGeometry {
    /// Logical-pixel port of Zig `font_config.zig:61-68` (`rowGeometry`).
    pub fn from_line_heights(name_height: f32, preview_height: f32, time_height: f32) -> Self {
        let padding = ROW_PADDING_PX;
        let name_band = ROW_MIN_BAND_PX.max(name_height.max(time_height));
        let preview_band = ROW_MIN_BAND_PX.max(preview_height);
        let preview_top = padding + name_band;
        let preview_bottom = preview_top + preview_band;
        Self {
            padding,
            name_top: padding,
            name_font_px: TYPE_ROW_NAME_PX,
            name_band,
            preview_top,
            preview_font_px: TYPE_PREVIEW_PX,
            preview_band,
            preview_bottom,
            time_font_px: TYPE_TIME_PX,
            height: preview_bottom + padding,
        }
    }

    /// Computes dynamic sidebar chat row geometry for any `FontScale` (`60..=160%`).
    pub fn for_font_scale(scale: FontScale) -> Self {
        let pct = scale.percent();
        let name_font_px = scaled_px(TYPE_ROW_NAME_PX, pct);
        let preview_font_px = scaled_px(TYPE_PREVIEW_PX, pct);
        let time_font_px = scaled_px(TYPE_TIME_PX, pct);
        let name_line_h = (name_font_px * LINE_HEIGHT_FACTOR).ceil();
        let preview_line_h = (preview_font_px * LINE_HEIGHT_FACTOR).ceil();
        let time_line_h = (time_font_px * LINE_HEIGHT_FACTOR).ceil();
        let mut geom = Self::from_line_heights(name_line_h, preview_line_h, time_line_h);
        geom.name_font_px = name_font_px;
        geom.preview_font_px = preview_font_px;
        geom.time_font_px = time_font_px;
        geom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_geometry_never_clips_across_60_to_160_percent() {
        let default_geom = RowGeometry::for_font_scale(FontScale::default());
        assert!((default_geom.height - BASE_CHAT_ROW_HEIGHT_PX).abs() < f32::EPSILON);

        let mut prev_height = 0.0f32;
        for pct in FontScale::MIN..=FontScale::MAX {
            let geom = RowGeometry::for_font_scale(FontScale::clamped(pct));
            let needed_name = (geom.name_font_px * LINE_HEIGHT_FACTOR).ceil();
            let needed_preview = (geom.preview_font_px * LINE_HEIGHT_FACTOR).ceil();
            let needed_time = (geom.time_font_px * LINE_HEIGHT_FACTOR).ceil();

            assert!(geom.preview_top - geom.name_top >= needed_name);
            assert!(geom.preview_top - geom.name_top >= needed_time);
            assert!(geom.preview_bottom - geom.preview_top >= needed_preview);
            assert!((geom.height - (geom.preview_bottom + geom.padding)).abs() < f32::EPSILON);
            assert!(geom.height >= prev_height);
            prev_height = geom.height;
        }

        assert!(
            RowGeometry::for_font_scale(FontScale::clamped(FontScale::MAX)).height
                > default_geom.height
        );
    }
}
