//! GPUI rendering & Intel Arc `<= 16 MB` VRAM texture LRU for Milestone 3 features (#18–#26).

use crate::desktop::{
    PostboxShell, RADIUS_BUBBLE_PX, RADIUS_CHIP_PX, RADIUS_MODAL_PX, RADIUS_ROW_PX,
};
use crate::geometry::{TYPE_LABEL_PX, TYPE_TIME_PX};
use crate::theme::ShadcnPalette;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement, MouseButton,
    ObjectFit, ParentElement as _, RenderImage, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Window, div, hsla, img, px, rgb,
};
use postbox_core::{ChatKey, Message, Provider};
use postbox_media::decode::{
    BitmapLru, DecodedImage, decode_circle_avatar_file, decode_file_sync,
    decode_workspace_icon_file, recommended_decode_workers,
};
use postbox_media::emoji::{EMOJI_GRID_COLUMNS, EmojiCatalog};
use postbox_media::giphy::{GIPHY_GRID_COLUMNS, GiphyStatus};
use postbox_media::unfurl::{
    UNFURL_THUMB_MAX_WIDTH_PX, UNFURL_VIDEO_CARD_HEIGHT_PX, UnfurlCard, UnfurlProvider,
    is_page_fetch_allowed,
};
use postbox_media::{PREVIEW_MAX_H, PREVIEW_MAX_W, VRAM_TEXTURE_BUDGET_BYTES};
use smallvec::smallvec;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Texture key prefixes. M5.5 purges by prefix on chat switch and on minimize/idle, so these
/// must stay in sync with the `*_tex_key` builders below.
pub(crate) const INLINE_MEDIA_KEY_PREFIX: &str = "inline:";
pub(crate) const LIGHTBOX_KEY_PREFIX: &str = "lightbox:";
pub(crate) const GIPHY_KEY_PREFIX: &str = "giphy:";
pub(crate) const AVATAR_KEY_PREFIX: &str = "avatar:";
pub(crate) const WORKSPACE_ICON_KEY_PREFIX: &str = "wsicon:";
pub(crate) const SLACK_EMOJI_KEY_PREFIX: &str = "slackemoji:";
pub(crate) const UNFURL_KEY_PREFIX: &str = "unfurl:";
pub(crate) const STAGED_KEY_PREFIX: &str = "staged:";
pub(crate) const VIDEO_THUMB_KEY_PREFIX: &str = "videothumb:";
pub(crate) const VIDEO_PLAYER_KEY_PREFIX: &str = "videoplayer:";

pub(crate) fn inline_media_tex_key(chat_id: &str, msg_id: &str, path: &Path) -> String {
    format!(
        "{INLINE_MEDIA_KEY_PREFIX}{chat_id}:{msg_id}:{}",
        path.display()
    )
}

pub(crate) fn lightbox_tex_key(msg_id: &str, path: &Path) -> String {
    format!("{LIGHTBOX_KEY_PREFIX}{msg_id}:{}", path.display())
}

pub(crate) fn giphy_tex_key(idx: usize, path: &Path) -> String {
    format!("{GIPHY_KEY_PREFIX}{idx}:{}", path.display())
}

pub(crate) fn avatar_tex_key(chat: &ChatKey, path: &Path) -> String {
    format!(
        "{AVATAR_KEY_PREFIX}{:?}:{}:{}:{}",
        chat.provider,
        chat.account,
        chat.id,
        path.display()
    )
}

pub(crate) fn workspace_icon_tex_key(team: &str, path: &Path) -> String {
    format!("{WORKSPACE_ICON_KEY_PREFIX}{team}:{}", path.display())
}

pub(crate) fn slack_emoji_tex_key(team: &str, name: &str, path: &Path) -> String {
    format!("{SLACK_EMOJI_KEY_PREFIX}{team}:{name}:{}", path.display())
}

pub(crate) fn unfurl_tex_key(provider: UnfurlProvider, id: &str, path: &Path) -> String {
    format!(
        "{UNFURL_KEY_PREFIX}{}:{id}:{}",
        provider.as_str(),
        path.display()
    )
}

/// Keys that survive an idle/minimize trim: small, reused across chats, and cheap to keep.
pub(crate) fn is_persistent_texture_key(key: &str) -> bool {
    key.starts_with(AVATAR_KEY_PREFIX)
        || key.starts_with(WORKSPACE_ICON_KEY_PREFIX)
        || key.starts_with(SLACK_EMOJI_KEY_PREFIX)
        || key.starts_with(STAGED_KEY_PREFIX)
}

/// Keys whose lifetime is bound to one chat's timeline and must go when that chat is left.
pub(crate) fn is_transient_chat_texture_key(key: &str, chat_id: &str) -> bool {
    key.starts_with(&format!("{INLINE_MEDIA_KEY_PREFIX}{chat_id}:"))
        || key.starts_with(&format!("{VIDEO_THUMB_KEY_PREFIX}{chat_id}:"))
        || key.starts_with(VIDEO_PLAYER_KEY_PREFIX)
        || key.starts_with(UNFURL_KEY_PREFIX)
}

#[derive(Clone, Debug)]
pub(crate) enum DecodeKind {
    Standard {
        max_dims: (u32, u32),
        allow_upscale_2x: bool,
    },
    CircleAvatar {
        target_size: usize,
    },
    WorkspaceIcon {
        target_size: usize,
        radius: f32,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct DecodeJob {
    pub key: String,
    pub path: PathBuf,
    pub kind: DecodeKind,
}

fn execute_decode_job(job: DecodeJob) -> Option<DecodedImage> {
    match job.kind {
        DecodeKind::Standard {
            max_dims,
            allow_upscale_2x,
        } => decode_file_sync(&job.path, max_dims.0, max_dims.1, allow_upscale_2x).ok(),
        DecodeKind::CircleAvatar { target_size } => {
            decode_circle_avatar_file(&job.path, target_size).ok()
        }
        DecodeKind::WorkspaceIcon {
            target_size,
            radius,
        } => decode_workspace_icon_file(&job.path, target_size, radius).ok(),
    }
}

#[derive(Clone)]
pub(crate) struct GpuTextureEntry {
    pub frames: Vec<Arc<RenderImage>>,
    pub width: u32,
    pub height: u32,
    pub is_animated: bool,
}

impl GpuTextureEntry {
    pub fn frame_at(&self, tick: usize) -> Option<Arc<RenderImage>> {
        if self.frames.is_empty() {
            None
        } else {
            Some(self.frames[tick % self.frames.len()].clone())
        }
    }
}

/// Intel Arc GPU texture atlas LRU strictly capped at `VRAM_TEXTURE_BUDGET_BYTES` (16 MiB, M5.5 RAM diet) with
/// bounded off-UI-thread decode scheduling and deduplication.
/// Drops every evicted `Arc<RenderImage>` via `cx.drop_image(frame, Some(window))`.
pub(crate) struct GpuTextureLru {
    lru: BitmapLru<String, GpuTextureEntry>,
    pending_queue: VecDeque<DecodeJob>,
    queued_or_in_flight: HashSet<String>,
    failed_keys: HashSet<String>,
    in_flight: usize,
    max_concurrent: usize,
}

impl Default for GpuTextureLru {
    fn default() -> Self {
        Self {
            lru: BitmapLru::new(VRAM_TEXTURE_BUDGET_BYTES),
            pending_queue: VecDeque::new(),
            queued_or_in_flight: HashSet::new(),
            failed_keys: HashSet::new(),
            in_flight: 0,
            max_concurrent: recommended_decode_workers(),
        }
    }
}

impl GpuTextureLru {
    pub fn total_vram_bytes(&self) -> usize {
        self.lru.total_bytes()
    }

    /// Number of resident textures (test observability).
    #[cfg(test)]
    pub fn resident_count(&self) -> usize {
        self.lru.len()
    }

    /// Resident texture keys, least-recently-used first (test observability).
    #[cfg(test)]
    pub fn resident_keys(&self) -> Vec<String> {
        self.lru.keys()
    }

    pub fn peek(&self, key: &str) -> Option<&GpuTextureEntry> {
        self.lru.peek(key)
    }

    #[cfg(test)]
    pub fn in_flight_count(&self) -> usize {
        self.in_flight
    }

    pub fn touch_or_needs_decode(&mut self, key: &str) -> bool {
        if self.lru.get(&key.to_owned()).is_some() {
            return false;
        }
        !self.queued_or_in_flight.contains(key) && !self.failed_keys.contains(key)
    }

    pub fn enqueue(&mut self, job: DecodeJob) -> bool {
        if !self.touch_or_needs_decode(&job.key) {
            return false;
        }
        self.queued_or_in_flight.insert(job.key.clone());
        self.pending_queue.push_back(job);
        true
    }

    pub fn pop_next_job(&mut self) -> Option<DecodeJob> {
        if self.in_flight >= self.max_concurrent {
            return None;
        }
        let job = self.pending_queue.pop_front()?;
        self.in_flight += 1;
        Some(job)
    }

    pub fn complete_job(
        &mut self,
        key: &str,
        decoded: Option<DecodedImage>,
        window: &mut Window,
        cx: &mut Context<PostboxShell>,
    ) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.queued_or_in_flight.remove(key);
        let Some(img) = decoded else {
            if self.failed_keys.len() >= 512 {
                self.failed_keys.clear();
            }
            self.failed_keys.insert(key.to_owned());
            return false;
        };
        let byte_size = img.byte_size();
        let Some(entry) = decoded_to_gpu_entry(img) else {
            if self.failed_keys.len() >= 512 {
                self.failed_keys.clear();
            }
            self.failed_keys.insert(key.to_owned());
            return false;
        };
        let evicted = self.lru.insert(key.to_owned(), entry, byte_size);
        for (_k, old_entry) in evicted {
            for frame in old_entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
        true
    }

    pub fn ensure_decoded_inserted(
        &mut self,
        key: &str,
        decoded: &DecodedImage,
        window: &mut Window,
        cx: &mut Context<PostboxShell>,
    ) -> Option<GpuTextureEntry> {
        if let Some(existing) = self.lru.get(&key.to_owned()) {
            return Some(existing.clone());
        }
        let byte_size = decoded.byte_size();
        let entry = decoded_to_gpu_entry(decoded.clone())?;
        let evicted = self.lru.insert(key.to_owned(), entry.clone(), byte_size);
        for (_k, old_entry) in evicted {
            for frame in old_entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
        Some(entry)
    }

    /// Drops one texture by key (the video player keeps exactly one frame resident).
    pub fn remove_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<PostboxShell>,
    ) -> bool {
        self.pending_queue.retain(|job| job.key != key);
        self.queued_or_in_flight.remove(key);
        let mut dropped = false;
        for entry in self.lru.remove_where(|k| k == key) {
            dropped = true;
            for frame in entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
        dropped
    }

    pub fn clear_all(&mut self, window: &mut Window, cx: &mut Context<PostboxShell>) {
        self.pending_queue.clear();
        self.queued_or_in_flight.clear();
        self.failed_keys.clear();
        self.in_flight = 0;
        for (_k, old_entry) in self.lru.clear() {
            for frame in old_entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
    }

    /// Drops decoded images and animated GIF frames that belong to a chat the user just left,
    /// keeping avatars, workspace icons, Slack custom emoji, and the newly selected chat.
    /// Returns how many entries were released.
    pub fn evict_chat_media(
        &mut self,
        previous_chat_id: &str,
        window: &mut Window,
        cx: &mut Context<PostboxShell>,
    ) -> usize {
        let doomed: Vec<String> = self
            .lru
            .keys()
            .into_iter()
            .filter(|key| {
                !is_persistent_texture_key(key)
                    && is_transient_chat_texture_key(key, previous_chat_id)
            })
            .collect();
        // Queued decodes for the chat we just left must not resurrect its textures.
        self.pending_queue
            .retain(|job| !doomed.iter().any(|key| key == &job.key));
        self.queued_or_in_flight
            .retain(|key| !doomed.iter().any(|doomed_key| doomed_key == key));
        let mut dropped = 0usize;
        for entry in self
            .lru
            .remove_where(|key| doomed.iter().any(|doomed_key| doomed_key == key))
        {
            dropped += 1;
            for frame in entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
        dropped
    }

    /// Purges every transient texture (inline media, animated GIF frames, lightbox, Giphy
    /// previews, unfurl thumbnails) when the window is minimized or idle. Avatars, workspace
    /// icons, and Slack custom emoji stay because they are small and reused across chats;
    /// everything else is re-decoded on demand from the on-disk media cache.
    pub fn purge_transient_textures(
        &mut self,
        window: &mut Window,
        cx: &mut Context<PostboxShell>,
    ) -> usize {
        self.pending_queue.clear();
        self.queued_or_in_flight.clear();
        self.in_flight = 0;
        let mut dropped = 0usize;
        for entry in self.lru.remove_where(|key| !is_persistent_texture_key(key)) {
            dropped += 1;
            for frame in entry.frames {
                cx.drop_image(frame, Some(window));
            }
        }
        dropped
    }
}

/// Converts a BGRA `DecodedImage` into GPUI `RenderImage` frames (R/B already swapped to BGRA).
pub(crate) fn decoded_to_gpu_entry(decoded: DecodedImage) -> Option<GpuTextureEntry> {
    let mut frames = Vec::with_capacity(decoded.frames.len());
    for frame in decoded.frames {
        let img_buf = image::RgbaImage::from_raw(frame.width, frame.height, frame.bgra)?;
        let gpui_frame = image::Frame::new(img_buf);
        frames.push(Arc::new(RenderImage::new(smallvec![gpui_frame])));
    }
    if frames.is_empty() {
        return None;
    }
    Some(GpuTextureEntry {
        width: decoded.width,
        height: decoded.height,
        is_animated: decoded.is_animated_gif && frames.len() > 1,
        frames,
    })
}

impl PostboxShell {
    /// Ensures the pause-when-occluded `120 ms` GIF tick task is running ONLY when the window is
    /// active AND an animated GIF is currently visible (`0.0%` idle CPU/GPU otherwise!).
    pub(crate) fn sync_gif_animation_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_visible_anim = window.is_window_active() && self.has_visible_animated_gif();
        if !has_visible_anim {
            self.gif_timer_task = None;
            return;
        }
        if self.gif_timer_task.is_some() {
            return;
        }
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(
                        postbox_media::GIF_FRAME_INTERVAL_MS,
                    ))
                    .await;
                let keep_going = this
                    .update_in(cx, |shell, window, cx| {
                        if !window.is_window_active() || !shell.has_visible_animated_gif() {
                            shell.gif_timer_task = None;
                            return false;
                        }
                        shell.gif_tick = shell.gif_tick.wrapping_add(1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
        self.gif_timer_task = Some(task);
    }

    fn has_visible_animated_gif(&self) -> bool {
        if let Some(lb) = self.vm.lightbox()
            && lb.current().is_some_and(|c| c.is_animated_gif)
        {
            return true;
        }
        for msg in self.vm.messages() {
            if let Some(path) = self.vm.media_path_for(&msg.chat, &msg.msg_id) {
                let key = inline_media_tex_key(&msg.chat.id, &msg.msg_id, path);
                if self.gpu_textures.peek(&key).is_some_and(|e| e.is_animated) {
                    return true;
                }
            }
        }
        let gp = self.vm.giphy_picker();
        if gp.is_open() && gp.status == GiphyStatus::Ready {
            let (start, visible) = gp.visible_slice();
            for offset in 0..visible.len() {
                let idx = start + offset;
                if let Some(path) = self.vm.giphy_preview_path(idx) {
                    let key = giphy_tex_key(idx, path);
                    if self.gpu_textures.peek(&key).is_some_and(|e| e.is_animated) {
                        return true;
                    }
                }
            }
        }
        if let Some(chat) = self.vm.selected_chat()
            && chat.key.provider == Provider::Slack
        {
            let team = chat.key.account.as_str();
            for ((t, name), path) in &self.vm.m3().slack_emoji_paths {
                if t == team {
                    let key = slack_emoji_tex_key(t, name, path);
                    if self.gpu_textures.peek(&key).is_some_and(|e| e.is_animated) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn pump_texture_decodes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Some(job) = self.gpu_textures.pop_next_job() {
            let key = job.key.clone();
            let bg_task = cx
                .background_executor()
                .spawn(async move { execute_decode_job(job) });
            cx.spawn_in(
                window,
                async move |this: gpui_kit::WeakEntity<PostboxShell>,
                            cx: &mut gpui_kit::AsyncWindowContext| {
                    let decoded = bg_task.await;
                    let _ = this.update_in(cx, |shell, window, cx| {
                        let inserted = shell.gpu_textures.complete_job(&key, decoded, window, cx);
                        if inserted {
                            shell.sync_gif_animation_timer(window, cx);
                            cx.notify();
                        }
                        shell.pump_texture_decodes(window, cx);
                    });
                },
            )
            .detach();
        }
    }

    /// Schedules bounded off-UI-thread decodes for all visible textures (inline media, lightbox,
    /// Giphy tiles, sidebar avatars, view-rail Slack workspace icons, Slack custom emoji, and
    /// unfurl thumbnails) into `GpuTextureLru` (`<= 16 MB` VRAM) and updates the GIF timer.
    pub(crate) fn refresh_visible_gpu_textures(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 0. Only the active chat's media stays resident: drop the chat we just left first.
        self.evict_previous_chat_media(window, cx);
        // 0b. Milestone 6: cached video thumbnails join the same bounded decode queue.
        self.queue_video_thumb_textures();
        self.pump_video_thumbnails(window, cx);

        // 1. Inline message images/GIFs/stickers
        let msgs: Vec<(String, String, PathBuf)> = self
            .vm
            .messages()
            .iter()
            .filter(|m| postbox_media::lightbox::is_viewer_image(m))
            .filter_map(|m| {
                let p = self.vm.media_path_for(&m.chat, &m.msg_id)?;
                Some((m.chat.id.clone(), m.msg_id.clone(), p.clone()))
            })
            .collect();
        for (chat_id, msg_id, path) in msgs {
            let key = inline_media_tex_key(&chat_id, &msg_id, &path);
            self.gpu_textures.enqueue(DecodeJob {
                key,
                path,
                kind: DecodeKind::Standard {
                    max_dims: (PREVIEW_MAX_W, PREVIEW_MAX_H),
                    allow_upscale_2x: false,
                },
            });
        }

        // 2. Lightbox full-resolution image if open
        if let Some(lb) = self.vm.lightbox().cloned()
            && let Some(cur) = lb.current()
            && let Some(path) = &cur.local_path
        {
            let key = lightbox_tex_key(&cur.msg_id, path);
            self.gpu_textures.enqueue(DecodeJob {
                key,
                path: path.clone(),
                kind: DecodeKind::Standard {
                    max_dims: (1120, 740),
                    allow_upscale_2x: true,
                },
            });
        }

        // 3. Staged attachment 44x44 preview thumbnails (already decoded in memory during staging)
        let staged: Vec<(String, DecodedImage)> = self
            .vm
            .staged_attachments()
            .items()
            .iter()
            .filter_map(|item| {
                let thumb = item.thumb.clone()?;
                Some((format!("{STAGED_KEY_PREFIX}{}", item.path.display()), thumb))
            })
            .collect();
        for (key, thumb) in staged {
            let _ = self
                .gpu_textures
                .ensure_decoded_inserted(&key, &thumb, window, cx);
        }

        // 4. Giphy preview tiles
        if self.vm.giphy_picker().is_open() {
            let giphy_paths: Vec<(usize, PathBuf)> = self
                .vm
                .m3()
                .giphy_preview_paths
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for (idx, path) in giphy_paths {
                let key = giphy_tex_key(idx, &path);
                self.gpu_textures.enqueue(DecodeJob {
                    key,
                    path,
                    kind: DecodeKind::Standard {
                        max_dims: (124, 96),
                        allow_upscale_2x: false,
                    },
                });
            }
        }

        // 5. Sidebar chat avatars (circle masked)
        let avatar_jobs: Vec<(String, PathBuf)> = self
            .sidebar_rows_cache
            .iter()
            .filter_map(|row| {
                let path = self.vm.avatar_path_for(&row.chat.key)?;
                Some((avatar_tex_key(&row.chat.key, path), path.clone()))
            })
            .collect();
        for (key, path) in avatar_jobs {
            self.gpu_textures.enqueue(DecodeJob {
                key,
                path,
                kind: DecodeKind::CircleAvatar {
                    target_size: postbox_media::avatar::AVATAR_BASE_SIZE,
                },
            });
        }

        // 6. View-rail Slack workspace icons (square area + rounded-rect SDF mask)
        let ws_jobs: Vec<(String, PathBuf)> = self
            .vm
            .accounts()
            .iter()
            .filter(|acc| acc.provider == Provider::Slack)
            .filter_map(|acc| {
                let path = self.vm.workspace_icon_path_for(&acc.account_id)?;
                Some((workspace_icon_tex_key(&acc.account_id, path), path.clone()))
            })
            .collect();
        for (key, path) in ws_jobs {
            self.gpu_textures.enqueue(DecodeJob {
                key,
                path,
                kind: DecodeKind::WorkspaceIcon {
                    target_size: 34,
                    radius: 10.0,
                },
            });
        }

        // 7. Open Slack chat custom emoji
        if let Some(chat) = self.vm.selected_chat()
            && chat.key.provider == Provider::Slack
        {
            let team = chat.key.account.clone();
            let emoji_jobs: Vec<(String, PathBuf)> = self
                .vm
                .m3()
                .slack_emoji_paths
                .iter()
                .filter(|((t, _), _)| t == &team)
                .map(|((t, name), path)| (slack_emoji_tex_key(t, name, path), path.clone()))
                .collect();
            for (key, path) in emoji_jobs {
                self.gpu_textures.enqueue(DecodeJob {
                    key,
                    path,
                    kind: DecodeKind::Standard {
                        max_dims: (48, 48),
                        allow_upscale_2x: false,
                    },
                });
            }
        }

        // 8. Visible link unfurl card thumbnails
        let unfurl_jobs: Vec<(String, PathBuf)> = self
            .vm
            .messages()
            .iter()
            .filter_map(|msg| {
                let (card, _) = self.vm.unfurl_card_for_message(msg)?;
                let path = card.thumb_path.as_ref()?;
                Some((unfurl_tex_key(card.provider, &card.id, path), path.clone()))
            })
            .collect();
        for (key, path) in unfurl_jobs {
            self.gpu_textures.enqueue(DecodeJob {
                key,
                path,
                kind: DecodeKind::Standard {
                    max_dims: (UNFURL_THUMB_MAX_WIDTH_PX, UNFURL_VIDEO_CARD_HEIGHT_PX),
                    allow_upscale_2x: false,
                },
            });
        }

        self.pump_texture_decodes(window, cx);
        self.sync_gif_animation_timer(window, cx);
    }

    pub(crate) fn render_message_body_element(
        &self,
        msg: &Message,
        body_text: String,
        body_px: f32,
        bubble_fg: u32,
        ix: usize,
        id_prefix: &str,
    ) -> AnyElement {
        if msg.chat.provider != Provider::Slack || !body_text.contains(':') {
            return div()
                .text_size(px(body_px))
                .text_color(rgb(bubble_fg))
                .child(body_text)
                .into_any_element();
        }

        let team = msg.chat.account.as_str();
        let emoji_px = (body_px * 1.35).round().clamp(16.0, 28.0);
        let mut parts: Vec<AnyElement> = Vec::new();
        let mut cursor = 0usize;
        let mut matched_any = false;
        let mut emoji_ix = 0usize;

        let mut scan = 0usize;
        while let Some(tok) = postbox_media::avatar::next_slack_emoji_token(&body_text, scan) {
            scan = tok.end;
            if let Some(path) = self.vm.slack_emoji_path_for(team, tok.name)
                && let Some(entry) = self
                    .gpu_textures
                    .peek(&slack_emoji_tex_key(team, tok.name, path))
                && let Some(frame) = entry.frame_at(self.gif_tick)
            {
                if tok.start > cursor {
                    parts.push(
                        div()
                            .child(body_text[cursor..tok.start].to_owned())
                            .into_any_element(),
                    );
                }
                parts.push(
                    div()
                        .id(SharedString::from(format!(
                            "{id_prefix}-slack-emoji-{ix}-{emoji_ix}"
                        )))
                        .test_support()
                        .w(px(emoji_px))
                        .h(px(emoji_px))
                        .flex_shrink_0()
                        .child(
                            img(frame)
                                .w(px(emoji_px))
                                .h(px(emoji_px))
                                .object_fit(ObjectFit::Contain),
                        )
                        .into_any_element(),
                );
                emoji_ix += 1;
                matched_any = true;
                cursor = tok.end;
            }
        }

        if !matched_any {
            return div()
                .text_size(px(body_px))
                .text_color(rgb(bubble_fg))
                .child(body_text)
                .into_any_element();
        }
        if cursor < body_text.len() {
            parts.push(
                div()
                    .child(body_text[cursor..].to_owned())
                    .into_any_element(),
            );
        }
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_1()
            .text_size(px(body_px))
            .text_color(rgb(bubble_fg))
            .children(parts)
            .into_any_element()
    }

    pub(crate) fn render_inline_media_element(
        &self,
        msg: &Message,
        p: &ShadcnPalette,
        ix: usize,
        id_prefix: &str,
        shell: &Entity<Self>,
    ) -> Option<AnyElement> {
        let kind = msg.media_kind?;
        if kind.is_voice_or_audio() {
            return Some(self.render_voice_note_pill(msg, p, ix, id_prefix, shell));
        }
        // Milestone 6: videos render their cached thumbnail + play badge + duration chip.
        if self.vm.is_video_message_id(&msg.msg_id)
            && let Some(el) = self.render_video_bubble(msg, p, ix, id_prefix, shell)
        {
            return Some(el);
        }
        let label = self.vm.attachment_display_label(msg);
        let msg_id = msg.msg_id.clone();

        if let Some(path) = self.vm.media_path_for(&msg.chat, &msg.msg_id) {
            let key = inline_media_tex_key(&msg.chat.id, &msg.msg_id, path);
            if let Some(entry) = self.gpu_textures.peek(&key)
                && let Some(frame) = entry.frame_at(self.gif_tick)
            {
                let shell_click = shell.clone();
                let open_id = msg_id.clone();
                return Some(
                    div()
                        .id(SharedString::from(format!("{id_prefix}-media-{ix}")))
                        .test_support()
                        .cursor_pointer()
                        .rounded(px(RADIUS_BUBBLE_PX))
                        .overflow_hidden()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            shell_click.update(cx, |this, cx| {
                                let now = this.now_ms();
                                this.vm.open_lightbox_for_message(&open_id, now);
                                this.sync_after_vm_change(window, cx);
                            });
                        })
                        .child(
                            img(frame)
                                .w(px(entry.width as f32))
                                .h(px(entry.height as f32))
                                .object_fit(ObjectFit::Contain)
                                .rounded(px(RADIUS_BUBBLE_PX)),
                        )
                        .into_any_element(),
                );
            }
        }

        // Attachment / loading / oversized / retry / gone pill
        let shell_dl = shell.clone();
        let is_image_like = matches!(
            kind,
            postbox_core::MediaKind::Image
                | postbox_core::MediaKind::Gif
                | postbox_core::MediaKind::Sticker
        );
        let click_id = msg_id;
        Some(
            div()
                .id(SharedString::from(format!("{id_prefix}-attach-{ix}")))
                .test_support()
                .px_2p5()
                .py_1p5()
                .rounded(px(RADIUS_BUBBLE_PX))
                .bg(rgb(p.secondary))
                .border_1()
                .border_color(rgb(p.border))
                .text_size(px(TYPE_LABEL_PX))
                .text_color(rgb(p.foreground))
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    shell_dl.update(cx, |this, cx| {
                        let now = this.now_ms();
                        if is_image_like
                            && this
                                .vm
                                .selected_chat()
                                .and_then(|c| this.vm.media_path_for(&c.key, &click_id))
                                .is_some()
                        {
                            this.vm.open_lightbox_for_message(&click_id, now);
                        } else {
                            this.vm.request_explicit_media_download(&click_id);
                        }
                        this.sync_after_vm_change(window, cx);
                    });
                })
                .child(label)
                .into_any_element(),
        )
    }

    pub(crate) fn render_reaction_chips(
        &self,
        msg: &Message,
        p: &ShadcnPalette,
        ix: usize,
        id_prefix: &str,
        shell: &Entity<Self>,
    ) -> Option<AnyElement> {
        if msg.reactions.is_empty() {
            return None;
        }
        let pending = self.vm.is_reaction_pending(&msg.chat, &msg.msg_id);
        let mut row = div()
            .mt(px(-8.0))
            .mx_2p5()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1();
        for (seg_ix, r) in msg.reactions.iter().enumerate() {
            let label = if r.count > 1 {
                format!("{} {}", r.emoji, r.count)
            } else {
                r.emoji.clone()
            };
            let bg = if r.from_me { p.accent } else { p.card };
            let fg = if pending {
                p.muted_foreground
            } else {
                p.foreground
            };
            let shell_chip = shell.clone();
            let msg_id = msg.msg_id.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!(
                        "{id_prefix}-react-{ix}-{seg_ix}"
                    )))
                    .test_support()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(bg))
                    .border_1()
                    .border_color(rgb(p.border))
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(fg))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        shell_chip.update(cx, |this, cx| {
                            this.vm.click_reaction_chip(&msg_id, seg_ix);
                            this.sync_after_vm_change(window, cx);
                        });
                    })
                    .child(label),
            );
        }
        Some(row.into_any_element())
    }

    pub(crate) fn render_unfurl_card(
        &self,
        card: &UnfurlCard,
        p: &ShadcnPalette,
        ix: usize,
        id_prefix: &str,
        shell: &Entity<Self>,
    ) -> AnyElement {
        let (card_w, card_h, thumb_w) = card.card_geometry(320);
        let open_url = card.canonical.clone();
        let shell_click = shell.clone();
        let mut container = div()
            .id(SharedString::from(format!("{id_prefix}-unfurl-{ix}")))
            .test_support()
            .mt_1()
            .w(px(card_w as f32))
            .min_h(px(card_h as f32))
            .rounded(px(RADIUS_BUBBLE_PX))
            .bg(rgb(p.card))
            .border_1()
            .border_color(rgb(p.border))
            .overflow_hidden()
            .flex()
            .flex_row()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                if is_page_fetch_allowed(&open_url) {
                    cx.open_url(&open_url);
                    shell_click.update(cx, |this, cx| {
                        this.vm.set_status_notice(format!("Opened {open_url}"));
                        this.sync_after_vm_change(window, cx);
                    });
                }
            });

        if thumb_w > 0 {
            let decoded_thumb = card.thumb_path.as_ref().and_then(|path| {
                let key = unfurl_tex_key(card.provider, &card.id, path);
                self.gpu_textures.peek(&key).and_then(|e| e.frame_at(0))
            });
            if let Some(frame) = decoded_thumb {
                let thumb_box = div()
                    .id(SharedString::from(format!("{id_prefix}-unfurl-thumb-{ix}")))
                    .test_support()
                    .relative()
                    .w(px(thumb_w as f32))
                    .h(px(card_h as f32))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(
                        img(frame)
                            .w(px(thumb_w as f32))
                            .h(px(card_h as f32))
                            .object_fit(ObjectFit::Cover),
                    )
                    .when(card.provider != UnfurlProvider::Page, |el| {
                        el.child(
                            div()
                                .absolute()
                                .inset_0()
                                .bg(hsla(0.0, 0.0, 0.0, 0.25))
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .w(px(28.0))
                                        .h(px(28.0))
                                        .rounded(px(RADIUS_CHIP_PX))
                                        .bg(hsla(0.0, 0.0, 0.0, 0.65))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_size(px(12.0))
                                        .text_color(rgb(0xFFFFFF))
                                        .child("▶"),
                                ),
                        )
                    });
                container = container.child(thumb_box);
            } else {
                let accent = card.provider.placeholder_color_hex().unwrap_or(p.secondary);
                container = container.child(
                    div()
                        .w(px(thumb_w as f32))
                        .h(px(card_h as f32))
                        .flex_shrink_0()
                        .bg(rgb(accent))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(TYPE_LABEL_PX))
                        .text_color(rgb(p.foreground))
                        .child(if card.provider == UnfurlProvider::Page {
                            card.site.clone()
                        } else {
                            "▶".to_owned()
                        }),
                );
            }
        }

        container = container.child(
            div()
                .flex_1()
                .min_w_0()
                .p_2()
                .flex()
                .flex_col()
                .justify_center()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(TYPE_TIME_PX))
                        .text_color(rgb(p.muted_foreground))
                        .truncate()
                        .child(card.site.clone()),
                )
                .child(
                    div()
                        .text_size(px(TYPE_LABEL_PX))
                        .text_color(rgb(p.foreground))
                        .truncate()
                        .child(card.display_title().to_owned()),
                )
                .when(!card.description.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .truncate()
                            .child(card.description.clone()),
                    )
                }),
        );

        container.into_any_element()
    }

    pub(crate) fn render_staged_attachments_strip(
        &self,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let items = self.vm.staged_attachments().items();
        if items.is_empty() {
            return None;
        }
        let mut strip = div()
            .id("staged-attachments-strip")
            .test_support()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .py_1();

        for (idx, item) in items.iter().enumerate() {
            let key = format!("{STAGED_KEY_PREFIX}{}", item.path.display());
            let mut chip = div()
                .id(SharedString::from(format!("staged-chip-{idx}")))
                .test_support()
                .relative()
                .w(px(52.0))
                .h(px(52.0))
                .rounded(px(RADIUS_BUBBLE_PX))
                .bg(rgb(p.secondary))
                .border_1()
                .border_color(rgb(p.border))
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden();

            if let Some(entry) = self.gpu_textures.peek(&key)
                && let Some(frame) = entry.frame_at(0)
            {
                chip = chip.child(
                    img(frame)
                        .w(px(44.0))
                        .h(px(44.0))
                        .object_fit(ObjectFit::Contain)
                        .rounded(px(RADIUS_BUBBLE_PX)),
                );
            } else {
                chip = chip.child(
                    div()
                        .px_1()
                        .text_size(px(TYPE_TIME_PX))
                        .text_color(rgb(p.foreground))
                        .truncate()
                        .child(item.filename.clone()),
                );
            }

            chip = chip.child(
                div()
                    .id(SharedString::from(format!("staged-remove-{idx}")))
                    .test_support()
                    .absolute()
                    .top(px(2.0))
                    .right(px(2.0))
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.destructive))
                    .text_size(px(10.0))
                    .text_color(rgb(p.primary_foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.vm.remove_staged_attachment(idx);
                        this.sync_after_vm_change(window, cx);
                    }))
                    .child("×"),
            );

            strip = strip.child(chip);
        }

        Some(strip.into_any_element())
    }

    pub(crate) fn render_composer_popups(
        &self,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(mp) = self.vm.mention_popup() {
            let mut list = div()
                .id("mention-popup")
                .test_support()
                .w(px(300.0))
                .p_1()
                .rounded(px(RADIUS_MODAL_PX))
                .bg(rgb(p.popover))
                .border_1()
                .border_color(rgb(p.border))
                .flex()
                .flex_col()
                .gap_0p5();
            for (idx, m) in mp.matches.iter().enumerate() {
                let selected = idx == mp.selected;
                let fg = if m.in_chat() {
                    p.foreground
                } else {
                    p.muted_foreground
                };
                list = list.child(
                    div()
                        .id(SharedString::from(format!("mention-row-{idx}")))
                        .test_support()
                        .px_2p5()
                        .py_1()
                        .rounded(px(RADIUS_ROW_PX))
                        .bg(rgb(if selected { p.accent } else { p.popover }))
                        .text_size(px(TYPE_LABEL_PX))
                        .text_color(rgb(fg))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(pop) = &mut this.vm.m3.mention_popup {
                                pop.selected = idx;
                            }
                            this.vm.accept_composer_popup();
                            this.sync_after_vm_change(window, cx);
                        }))
                        .child(m.display_label().to_owned()),
                );
            }
            return Some(list.into_any_element());
        }

        if let Some(sp) = self.vm.shortcode_popup() {
            let mut list = div()
                .id("shortcode-popup")
                .test_support()
                .w(px(300.0))
                .p_1()
                .rounded(px(RADIUS_MODAL_PX))
                .bg(rgb(p.popover))
                .border_1()
                .border_color(rgb(p.border))
                .flex()
                .flex_col()
                .gap_0p5();
            for (idx, m) in sp.matches.iter().enumerate() {
                let selected = idx == sp.selected;
                list = list.child(
                    div()
                        .id(SharedString::from(format!("shortcode-row-{idx}")))
                        .test_support()
                        .px_2p5()
                        .py_1()
                        .rounded(px(RADIUS_ROW_PX))
                        .bg(rgb(if selected { p.accent } else { p.popover }))
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(TYPE_LABEL_PX))
                        .text_color(rgb(p.foreground))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if let Some(pop) = &mut this.vm.m3.shortcode_popup {
                                pop.selected = idx;
                            }
                            this.vm.accept_composer_popup();
                            this.sync_after_vm_change(window, cx);
                        }))
                        .child(m.emoji.to_owned())
                        .child(format!(":{}:", m.code)),
                );
            }
            return Some(list.into_any_element());
        }

        None
    }

    pub(crate) fn render_lightbox_overlay(
        &self,
        p: &ShadcnPalette,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let lb = self.vm.lightbox()?;
        let cur = lb.current();
        let counter = lb.position_label();
        let has_prev = lb.has_prev();
        let has_next = lb.has_next();

        let center_content = if let Some(item) = cur
            && let Some(path) = &item.local_path
            && let Some(entry) = self
                .gpu_textures
                .peek(&lightbox_tex_key(&item.msg_id, path))
            && let Some(frame) = entry.frame_at(self.gif_tick)
        {
            img(frame)
                .max_w(px(entry.width as f32))
                .max_h(px(entry.height as f32))
                .object_fit(ObjectFit::Contain)
                .rounded(px(RADIUS_BUBBLE_PX))
                .into_any_element()
        } else {
            div()
                .px_4()
                .py_3()
                .rounded(px(RADIUS_BUBBLE_PX))
                .bg(rgb(p.card))
                .border_1()
                .border_color(rgb(p.border))
                .text_size(px(TYPE_LABEL_PX))
                .text_color(rgb(p.foreground))
                .child(lb.fallback_status_label())
                .into_any_element()
        };

        Some(
            div()
                .id("lightbox-overlay")
                .test_support()
                .track_focus(focus)
                .key_context("PostboxLightbox")
                .absolute()
                .inset_0()
                .bg(hsla(0.0, 0.0, 0.0, 0.85))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, window, cx| {
                    let now = this.now_ms();
                    if this.vm.close_lightbox_on_click(now) {
                        this.sync_after_vm_change(window, cx);
                    }
                }))
                .child(
                    div()
                        .w_full()
                        .flex_1()
                        .px_4()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .id("lightbox-prev")
                                .test_support()
                                .w(px(40.0))
                                .h(px(48.0))
                                .rounded(px(RADIUS_CHIP_PX))
                                .bg(rgb(p.card))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(if has_prev {
                                    p.foreground
                                } else {
                                    p.muted_foreground
                                }))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.vm.lightbox_prev();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child("<"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(center_content),
                        )
                        .child(
                            div()
                                .id("lightbox-next")
                                .test_support()
                                .w(px(40.0))
                                .h(px(48.0))
                                .rounded(px(RADIUS_CHIP_PX))
                                .bg(rgb(p.card))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(if has_next {
                                    p.foreground
                                } else {
                                    p.muted_foreground
                                }))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.vm.lightbox_next();
                                    this.sync_after_vm_change(window, cx);
                                }))
                                .child(">"),
                        ),
                )
                .child(
                    div()
                        .id("lightbox-counter")
                        .test_support()
                        .pb_3()
                        .text_size(px(TYPE_LABEL_PX))
                        .text_color(rgb(p.foreground))
                        .child(counter),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_emoji_picker_overlay(
        &self,
        p: &ShadcnPalette,
        focus: &FocusHandle,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let picker = self.vm.emoji_picker()?;
        let (start, visible) = picker.visible_slice();
        let catalog = EmojiCatalog::global();

        let mut grid = div().flex().flex_col().gap_1();
        if visible.is_empty() {
            grid = grid.child(
                div()
                    .p_4()
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .child("No matching emoji"),
            );
        } else {
            for (row_idx, chunk) in visible.chunks(EMOJI_GRID_COLUMNS).enumerate() {
                let mut row_el = div().flex().flex_row().gap_1();
                for (col_idx, &cat_idx) in chunk.iter().enumerate() {
                    let match_idx = start + row_idx * EMOJI_GRID_COLUMNS + col_idx;
                    let selected = match_idx == picker.selected;
                    let emoji = catalog.get(cat_idx).map(|e| e.emoji).unwrap_or("?");
                    row_el = row_el.child(
                        div()
                            .id(SharedString::from(format!("emoji-cell-{match_idx}")))
                            .test_support()
                            .w(px(40.0))
                            .h(px(40.0))
                            .rounded(px(RADIUS_ROW_PX))
                            .bg(rgb(if selected { p.accent } else { p.popover }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(22.0))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.vm.activate_emoji_picker_index(match_idx);
                                this.sync_after_vm_change(window, cx);
                            }))
                            .child(emoji),
                    );
                }
                grid = grid.child(row_el);
            }
        }

        Some(
            div()
                .id("emoji-picker-overlay")
                .test_support()
                .track_focus(focus)
                .key_context("PostboxEmojiPicker")
                .absolute()
                .inset_0()
                .bg(hsla(0.0, 0.0, 0.0, 0.35))
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.vm.close_emoji_picker();
                    this.sync_after_vm_change(window, cx);
                }))
                .child(
                    div()
                        .id("emoji-picker-card")
                        .test_support()
                        .w(px(376.0))
                        .p_3()
                        .rounded(px(RADIUS_MODAL_PX))
                        .bg(rgb(p.popover))
                        .border_1()
                        .border_color(rgb(p.border))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(Input::new(input))
                        .child(grid),
                )
                .into_any_element(),
        )
    }

    pub(crate) fn render_giphy_picker_overlay(
        &self,
        p: &ShadcnPalette,
        focus: &FocusHandle,
        input: &Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let gp = self.vm.giphy_picker();
        if !gp.is_open() {
            return None;
        }

        let mut grid = div().flex().flex_col().gap_2();
        if gp.status != GiphyStatus::Ready || gp.items.is_empty() {
            grid = grid.child(
                div()
                    .p_6()
                    .text_size(px(TYPE_LABEL_PX))
                    .text_color(rgb(p.muted_foreground))
                    .child(gp.empty_state_text()),
            );
        } else {
            let (start, visible) = gp.visible_slice();
            for (row_idx, chunk) in visible.chunks(GIPHY_GRID_COLUMNS).enumerate() {
                let mut row_el = div().flex().flex_row().gap_2();
                for (col_idx, item) in chunk.iter().enumerate() {
                    let tile_idx = start + row_idx * GIPHY_GRID_COLUMNS + col_idx;
                    let selected = tile_idx == gp.selected;
                    let preview_el = if let Some(path) = self.vm.giphy_preview_path(tile_idx)
                        && let Some(entry) = self.gpu_textures.peek(&giphy_tex_key(tile_idx, path))
                        && let Some(frame) = entry.frame_at(self.gif_tick)
                    {
                        img(frame)
                            .w(px(124.0))
                            .h(px(96.0))
                            .object_fit(ObjectFit::Contain)
                            .rounded(px(RADIUS_BUBBLE_PX))
                            .into_any_element()
                    } else {
                        div()
                            .text_size(px(TYPE_TIME_PX))
                            .text_color(rgb(p.muted_foreground))
                            .truncate()
                            .child(if item.title.is_empty() {
                                item.id.clone()
                            } else {
                                item.title.clone()
                            })
                            .into_any_element()
                    };

                    row_el = row_el.child(
                        div()
                            .id(SharedString::from(format!("giphy-tile-{tile_idx}")))
                            .test_support()
                            .w(px(132.0))
                            .h(px(104.0))
                            .p_1()
                            .rounded(px(RADIUS_BUBBLE_PX))
                            .bg(rgb(p.secondary))
                            .border_1()
                            .border_color(rgb(if selected { p.ring } else { p.border }))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.vm.request_giphy_tile_download(tile_idx);
                                this.sync_after_vm_change(window, cx);
                            }))
                            .child(preview_el),
                    );
                }
                grid = grid.child(row_el);
            }
        }

        Some(
            div()
                .id("giphy-picker-overlay")
                .test_support()
                .track_focus(focus)
                .key_context("PostboxGiphyPicker")
                .absolute()
                .inset_0()
                .bg(hsla(0.0, 0.0, 0.0, 0.4))
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.vm.close_giphy_picker();
                    this.sync_after_vm_change(window, cx);
                }))
                .child(
                    div()
                        .id("giphy-picker-card")
                        .test_support()
                        .w(px(584.0))
                        .p_3()
                        .rounded(px(RADIUS_MODAL_PX))
                        .bg(rgb(p.popover))
                        .border_1()
                        .border_color(rgb(p.border))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(Input::new(input))
                        .child(grid),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod ram_diet_tests {
    use super::*;

    #[test]
    fn transient_and_persistent_texture_keys_partition_by_prefix() {
        let chat = ChatKey::new(
            postbox_core::Provider::Whatsapp,
            "main",
            "alice@s.whatsapp.net",
        );
        let p = Path::new("/tmp/media.png");

        // Persistent: avatars, workspace icons, Slack custom emoji, staged thumbnails.
        for key in [
            avatar_tex_key(&chat, p),
            workspace_icon_tex_key("T1", p),
            slack_emoji_tex_key("T1", "shipit", p),
            format!("{STAGED_KEY_PREFIX}{}", p.display()),
        ] {
            assert!(is_persistent_texture_key(&key), "{key} must survive a trim");
            assert!(
                !is_transient_chat_texture_key(&key, &chat.id),
                "{key} must not be chat-evicted"
            );
        }

        // Transient: inline media for the chat being left, and unfurl cards.
        let inline = inline_media_tex_key(&chat.id, "msg1", p);
        assert!(is_transient_chat_texture_key(&inline, &chat.id));
        assert!(!is_persistent_texture_key(&inline));
        let unfurl = unfurl_tex_key(UnfurlProvider::Page, "abc", p);
        assert!(is_transient_chat_texture_key(&unfurl, &chat.id));

        // Inline media of a different chat must not be evicted by this chat's switch.
        assert!(!is_transient_chat_texture_key(&inline, "someone-else"));

        // Lightbox and Giphy tiles are transient but not chat-scoped.
        assert!(!is_persistent_texture_key(&lightbox_tex_key("msg1", p)));
        assert!(!is_persistent_texture_key(&giphy_tex_key(3, p)));
    }

    #[test]
    fn lru_remove_where_returns_only_matching_values_and_keeps_budget_honest() {
        let mut lru: postbox_media::decode::BitmapLru<String, u32> =
            postbox_media::decode::BitmapLru::new(1024);
        lru.insert("keep-a".into(), 1, 100);
        lru.insert("drop-1".into(), 2, 100);
        lru.insert("keep-b".into(), 3, 100);
        lru.insert("drop-2".into(), 4, 100);
        assert_eq!(lru.total_bytes(), 400);

        let mut removed = lru.remove_where(|k| k.starts_with("drop-"));
        removed.sort_unstable();
        assert_eq!(removed, vec![2, 4]);
        assert_eq!(lru.total_bytes(), 200);
        assert_eq!(lru.len(), 2);
        assert!(lru.contains(&"keep-a".to_owned()));
        assert!(lru.contains(&"keep-b".to_owned()));

        // Removing what is not there is a no-op, and the LRU keeps evicting to budget after.
        assert!(lru.remove_where(|k| k == "nope").is_empty());
        lru.insert("big".into(), 5, 900);
        assert!(lru.total_bytes() <= lru.budget_bytes());
    }
}
