use postbox_core::{ChatKey, MediaKind, Message};
use postbox_platform::external_image_extension_allowed;
use std::path::{Path, PathBuf};

/// Default click-close debounce matching Windows `GetDoubleClickTime()` (`500 ms`,
/// `image_viewer.zig:278-306`).
pub const LIGHTBOX_CLICK_DEBOUNCE_MS: u64 = 500;
pub const LIGHTBOX_HORIZONTAL_GUTTER_PX: i32 = 112;
pub const LIGHTBOX_VERTICAL_GUTTER_PX: i32 = 64;

/// Exact port of `image_viewer.viewerTargetSize` (`image_viewer.zig:29-41`):
/// fits image inside `(max_w, max_h)`, allowing upscaling up to `<= 2.0x`.
pub fn viewer_target_size(src_w: u32, src_h: u32, max_w: i32, max_h: i32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (1, 1);
    }
    if max_w <= 0 || max_h <= 0 {
        return (src_w, src_h);
    }
    let mut scale = (max_w as f32 / src_w as f32).min(max_h as f32 / src_h as f32);
    if scale > 2.0 {
        scale = 2.0;
    }
    if scale <= 0.0 {
        scale = 1.0;
    }
    let w = ((src_w as f32 * scale) as u32).max(1);
    let h = ((src_h as f32 * scale) as u32).max(1);
    (w, h)
}

/// Computes available image viewport box inside full-window lightbox (`(vp_w - 112) × (vp_h - 64)`).
pub fn lightbox_viewport_bounds(vp_w: i32, vp_h: i32) -> (i32, i32) {
    (
        (vp_w - LIGHTBOX_HORIZONTAL_GUTTER_PX).max(1),
        (vp_h - LIGHTBOX_VERTICAL_GUTTER_PX).max(1),
    )
}

/// Exact gallery inclusion predicate (`main.zig:8631-8633`):
/// includes `Image`, `Sticker`, and non-video `Gif` (excludes `video/*` GIFs, videos, docs, audio).
pub fn is_viewer_image(msg: &Message) -> bool {
    if msg.deleted {
        return false;
    }
    let is_video_mime = msg
        .mime_type
        .as_deref()
        .is_some_and(|m| m.to_ascii_lowercase().starts_with("video/"));
    match msg.media_kind {
        Some(MediaKind::Image | MediaKind::Sticker) => true,
        Some(MediaKind::Gif) => !is_video_mime,
        _ => {
            !is_video_mime
                && msg
                    .mime_type
                    .as_deref()
                    .is_some_and(|m| m.eq_ignore_ascii_case("image/gif"))
        }
    }
}

/// Strict external preview extension allowlist (`attachments.zig:3-12`).
pub fn external_preview_allowed(path: &Path) -> bool {
    external_image_extension_allowed(path)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GalleryItem {
    pub msg_id: String,
    pub chat: ChatKey,
    pub filename: Option<String>,
    pub local_path: Option<PathBuf>,
    pub is_animated_gif: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightboxState {
    pub chat: ChatKey,
    pub items: Vec<GalleryItem>,
    pub index: usize,
    pub opened_at_ms: u64,
    pub decode_failed: bool,
}

impl LightboxState {
    /// Builds a lightbox gallery from all viewer-eligible images in `messages` (chronological chat
    /// order), selecting `target_msg_id`.
    pub fn open_for_message(
        chat: &ChatKey,
        messages: &[Message],
        target_msg_id: &str,
        media_paths: &std::collections::HashMap<String, PathBuf>,
        now_ms: u64,
    ) -> Option<Self> {
        let mut items = Vec::new();
        let mut target_index = None;
        for msg in messages {
            if !is_viewer_image(msg) {
                continue;
            }
            if msg.msg_id == target_msg_id {
                target_index = Some(items.len());
            }
            let is_animated_gif = msg.media_kind == Some(MediaKind::Gif)
                || msg
                    .mime_type
                    .as_deref()
                    .is_some_and(|m| m.eq_ignore_ascii_case("image/gif"));
            items.push(GalleryItem {
                msg_id: msg.msg_id.clone(),
                chat: chat.clone(),
                filename: msg.filename.clone(),
                local_path: media_paths.get(&msg.msg_id).cloned(),
                is_animated_gif,
            });
        }
        let index = target_index?;
        Some(Self {
            chat: chat.clone(),
            items,
            index,
            opened_at_ms: now_ms,
            decode_failed: false,
        })
    }

    pub fn current(&self) -> Option<&GalleryItem> {
        self.items.get(self.index)
    }

    pub fn has_prev(&self) -> bool {
        self.index > 0
    }

    pub fn has_next(&self) -> bool {
        self.index + 1 < self.items.len()
    }

    /// No-wrap previous navigation (`Left` / `<`). Returns `true` if index moved.
    pub fn step_prev(&mut self) -> bool {
        if self.has_prev() {
            self.index -= 1;
            self.decode_failed = false;
            true
        } else {
            false
        }
    }

    /// No-wrap next navigation (`Right` / `>`). Returns `true` if index moved.
    pub fn step_next(&mut self) -> bool {
        if self.has_next() {
            self.index += 1;
            self.decode_failed = false;
            true
        } else {
            false
        }
    }

    /// Exact counter label `"{position} / {total}"` (`image_viewer.zig:263-275`).
    pub fn position_label(&self) -> String {
        if self.items.is_empty() {
            "0 / 0".to_owned()
        } else {
            format!("{} / {}", self.index + 1, self.items.len())
        }
    }

    /// Returns whether a backdrop click at `now_ms` should close the lightbox (debounced so the
    /// release/second-click of an opening gesture does not immediately close it).
    pub fn should_close_on_click(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.opened_at_ms) >= LIGHTBOX_CLICK_DEBOUNCE_MS
    }

    /// Fallback status label when decoded bitmap is unavailable (`image_viewer.zig:259-262`).
    pub fn fallback_status_label(&self) -> &'static str {
        if self.decode_failed {
            "Could not load image"
        } else if self.current().is_none() {
            "Image no longer in chat"
        } else {
            "Loading..."
        }
    }

    /// Updates local path when an on-demand download completes for an item in this gallery.
    pub fn update_local_path(&mut self, msg_id: &str, path: PathBuf) -> bool {
        if let Some(item) = self.items.iter_mut().find(|i| i.msg_id == msg_id) {
            item.local_path = Some(path);
            if self.current().is_some_and(|cur| cur.msg_id == msg_id) {
                self.decode_failed = false;
            }
            true
        } else {
            false
        }
    }

    /// Marks the current item's download/decode failed and returns `Some(path)` if safe external
    /// preview fallback is allowed by the 12-extension allowlist.
    pub fn mark_failed_and_check_external(&mut self, msg_id: &str) -> Option<PathBuf> {
        if self.current().is_some_and(|c| c.msg_id == msg_id) {
            self.decode_failed = true;
            if let Some(path) = self.current().and_then(|c| c.local_path.as_ref())
                && external_preview_allowed(path)
            {
                return Some(path.clone());
            }
        }
        None
    }

    /// Computes rendered size of the current image inside `(vp_w, vp_h)`.
    pub fn scale_for_viewport(&self, img_w: u32, img_h: u32, vp_w: i32, vp_h: i32) -> (u32, u32) {
        let (max_w, max_h) = lightbox_viewport_bounds(vp_w, vp_h);
        viewer_target_size(img_w, img_h, max_w, max_h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::{Provider, ReadStatus};
    use std::collections::HashMap;

    #[test]
    fn lightbox_gallery_navigation_scale_debounce_and_allowlist() {
        let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");
        let mk = |id: &str, kind: Option<MediaKind>, mime: Option<&str>| Message {
            chat: chat.clone(),
            msg_id: id.into(),
            sender_id: "u1".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2026-11-26T10:00:00Z".into(),
            body: String::new(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: kind,
            mime_type: mime.map(str::to_owned),
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: ReadStatus::None,
            deleted: false,
        };
        let msgs = vec![
            mk("m1", Some(MediaKind::Image), Some("image/png")),
            mk("m2", Some(MediaKind::Video), Some("video/mp4")), // excluded
            mk("m3", Some(MediaKind::Gif), Some("video/mp4")),   // video GIF excluded
            mk("m4", Some(MediaKind::Gif), Some("image/gif")),   // included!
            mk("m5", Some(MediaKind::Sticker), Some("image/webp")), // included!
        ];
        let paths = HashMap::new();
        let mut lb = LightboxState::open_for_message(&chat, &msgs, "m4", &paths, 1000).unwrap();
        assert_eq!(lb.items.len(), 3);
        assert_eq!(lb.position_label(), "2 / 3");
        assert!(lb.has_prev() && lb.has_next());

        // No wrap-around on step_prev/step_next
        assert!(lb.step_prev());
        assert_eq!(lb.position_label(), "1 / 3");
        assert!(!lb.step_prev());
        assert!(lb.step_next());
        assert!(lb.step_next());
        assert_eq!(lb.position_label(), "3 / 3");
        assert!(!lb.step_next());

        // <= 2.0x upscale cap inside (vp_w - 112) x (vp_h - 64)
        assert_eq!(lb.scale_for_viewport(100, 100, 1000, 800), (200, 200));
        assert_eq!(lb.scale_for_viewport(2000, 1000, 1112, 864), (1000, 500));

        // Debounce
        assert!(!lb.should_close_on_click(1200));
        assert!(lb.should_close_on_click(1500));

        // External preview allowlist
        assert!(external_preview_allowed(Path::new("cache/pic.webp")));
        assert!(!external_preview_allowed(Path::new("cache/pic.png.exe")));
        assert!(!external_preview_allowed(Path::new("cache/pic.png:ads")));
    }
}
