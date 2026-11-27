//! Milestone 6 headless state & controller for Features #33/#34:
//! disk-cached video thumbnails plus the inline video player (double-click to open, `Space`
//! play/pause, scrubber seek, mute, `F11` fullscreen, `Esc` exits fullscreen before closing),
//! A/V sync against the WASAPI playback clock, pause-when-occluded, and chat-switch cleanup.
//!
//! Everything here is pure state so it is fully testable headless; `desktop_m6.rs` only paints
//! it and pumps frames/audio from the open `VideoPlaybackSession`.

use crate::viewmodel::{PostboxViewModel, now_epoch_millis_public};
use postbox_core::{ChatKey, Message};
use postbox_media::drain::media_cache_key;
use postbox_media::{CachedVideoThumbnail, format_duration_chip};
use postbox_platform::VideoMetadata;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Frame presentation tick while the player is playing (`16 ms`, one 60 Hz frame). The timer is
/// armed only while playing, never at idle (`0.0%` idle CPU/GPU).
pub const VIDEO_PLAYER_TICK_MS: u64 = 16;
/// How far the audio clock may lag the picture before the report is treated as stale.
pub const AUDIO_CLOCK_LAG_TOLERANCE_MS: u64 = 250;
/// Hard cap on one decoded frame held by the player (`PREVIEW_MAX_W` x `PREVIEW_MAX_H` x 4 is
/// ~420 KiB; anything far beyond that is a decode bug, not a real frame).
pub const MAX_VIDEO_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Thumbnail state for one video message bubble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoThumbState {
    /// Extraction/caching is running off the UI thread (bubble shows the fallback card).
    Extracting,
    /// Cached PNG on disk, ready for the GPU texture pipeline.
    Ready(VideoThumbReady),
    /// Extraction failed; the bubble keeps its attachment pill.
    Failed { message: String },
}

/// Decoded thumbnail plus the metadata the bubble chip needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoThumbReady {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub hardware_accelerated: bool,
}

impl VideoThumbReady {
    /// Duration chip text (`1:04`).
    pub fn duration_label(&self) -> String {
        format_duration_chip(self.duration_ms)
    }

    pub fn from_cached(cached: &CachedVideoThumbnail) -> Self {
        Self {
            path: cached.path.clone(),
            width: cached.meta.width,
            height: cached.meta.height,
            duration_ms: cached.meta.duration_ms,
            hardware_accelerated: cached.meta.hardware_accelerated,
        }
    }
}

/// What the shell needs to open a `VideoPlaybackSession` for a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPlayRequest {
    pub chat: ChatKey,
    pub msg_id: String,
    pub path: PathBuf,
}

/// Result of `Esc` inside the player: fullscreen is always exited before the player closes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEscapeOutcome {
    /// No player open.
    Ignored,
    /// Was fullscreen, now inline; the player is still open.
    ExitedFullscreen,
    /// Was inline, now closed.
    Closed,
}

/// Live inline-player state (one player at a time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoPlayerUiState {
    pub chat: ChatKey,
    pub msg_id: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub duration_ms: u64,
    pub has_audio: bool,
    pub hardware_accelerated: bool,
    pub playing: bool,
    pub muted: bool,
    pub fullscreen: bool,
    /// Paused because the window was minimized/occluded.
    pub occluded: bool,
    /// Presentation clock the scrubber and frame selection follow (`ms`).
    pub position_ms: u64,
    /// Monotonic wall-clock clock, advanced by `tick` while playing.
    pub wall_clock_ms: u64,
    /// Audio playback clock from the WASAPI render stream (`None` before the first chunk).
    pub audio_clock_ms: Option<u64>,
    /// PTS of the frame currently on screen.
    pub frame_pts_ms: u64,
    /// Bytes held by the current frame (0 after close/eviction).
    pub frame_bytes: usize,
    pub frames_presented: u64,
    pub started_at_ms: u64,
    last_tick_ms: u64,
    open_seq: u64,
}

impl VideoPlayerUiState {
    /// True when audio should drive the clock (unmuted clip with a real audio stream).
    pub fn clock_follows_audio(&self) -> bool {
        self.has_audio && !self.muted
    }

    /// Presentation clock: the WASAPI clock when audio is audible, wall clock otherwise
    /// (`mfplay` semantics: silent/muted videos advance on the system clock).
    pub fn presentation_ms(&self) -> u64 {
        av_sync_position_ms(
            self.audio_clock_ms,
            self.wall_clock_ms,
            self.clock_follows_audio(),
        )
        .min(self.duration_ms)
    }

    /// `elapsed / total` clock text for the controls row.
    pub fn clock_label(&self) -> String {
        format!(
            "{} / {}",
            format_duration_chip(self.presentation_ms()),
            format_duration_chip(self.duration_ms)
        )
    }

    /// Scrubber fill fraction in `0.0..=1.0`.
    pub fn progress_fraction(&self) -> f32 {
        if self.duration_ms == 0 {
            return 0.0;
        }
        (self.presentation_ms() as f64 / self.duration_ms as f64).clamp(0.0, 1.0) as f32
    }

    /// Hardware-decode badge text shown next to the clock.
    pub fn decode_badge(&self) -> &'static str {
        if self.hardware_accelerated {
            "HW · Arc"
        } else {
            "SW"
        }
    }

    pub fn is_at_end(&self) -> bool {
        self.duration_ms > 0 && self.presentation_ms() >= self.duration_ms
    }
}

/// Chooses the presentation clock: audio when audible and known, wall clock otherwise.
pub fn av_sync_position_ms(
    audio_clock_ms: Option<u64>,
    wall_clock_ms: u64,
    clock_follows_audio: bool,
) -> u64 {
    match (clock_follows_audio, audio_clock_ms) {
        (true, Some(audio)) => audio,
        _ => wall_clock_ms,
    }
}

/// All Milestone 6 state: thumbnail pipeline bookkeeping plus the single live player.
#[derive(Debug, Default)]
pub struct M6State {
    pub thumbnails: HashMap<(ChatKey, String), VideoThumbState>,
    /// Video content SHA-256 per message, so the thumbnail cache is keyed by bytes, not by id.
    pub video_hashes: HashMap<(ChatKey, String), String>,
    /// Messages whose extraction task is already running (dedup).
    pub extraction_in_flight: HashSet<(ChatKey, String)>,
    pub player: Option<VideoPlayerUiState>,
    pub open_seq: u64,
    /// Set by a command (`Ctrl+K > Play video inline`); the shell answers it on its next sync.
    pub pending_open_msg_id: Option<String>,
}

impl M6State {
    pub fn reset_for_tests(&mut self) {
        *self = Self::default();
    }
}

fn is_video_message(message: &Message) -> bool {
    if let Some(kind) = message.media_kind
        && kind == postbox_core::MediaKind::Video
    {
        return true;
    }
    message
        .mime_type
        .as_deref()
        .is_some_and(postbox_media::is_video_mime)
}

impl PostboxViewModel {
    // ---------------------------------------------------------------------
    // Thumbnails
    // ---------------------------------------------------------------------

    pub fn m6(&self) -> &M6State {
        &self.m6
    }

    pub fn video_thumb_state(&self, chat: &ChatKey, msg_id: &str) -> Option<&VideoThumbState> {
        self.m6.thumbnails.get(&(chat.clone(), msg_id.to_owned()))
    }

    /// `true` for `media_kind == video` or any `video/*` MIME message.
    pub fn is_video_message_id(&self, msg_id: &str) -> bool {
        self.messages
            .iter()
            .find(|m| m.msg_id == msg_id)
            .is_some_and(is_video_message)
    }

    /// Video content SHA-256 for a message: cached in `M6State` after a download event, else
    /// read from the `media_cache` row the downloader wrote.
    pub fn video_content_sha256(&mut self, chat: &ChatKey, msg_id: &str) -> Option<String> {
        let pair = (chat.clone(), msg_id.to_owned());
        if let Some(sha) = self.m6.video_hashes.get(&pair) {
            return Some(sha.clone());
        }
        let entry = self
            .store
            .get_media_cache(&media_cache_key(chat, msg_id))
            .ok()
            .flatten()?;
        if entry.sha256.is_empty() {
            return None;
        }
        self.m6.video_hashes.insert(pair, entry.sha256.clone());
        Some(entry.sha256)
    }

    pub fn record_video_sha256(&mut self, chat: &ChatKey, msg_id: &str, sha256: &str) {
        if sha256.is_empty() {
            return;
        }
        self.m6
            .video_hashes
            .insert((chat.clone(), msg_id.to_owned()), sha256.to_owned());
    }

    /// Marks a thumbnail ready from the disk cache result.
    pub fn set_video_thumbnail_ready(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        ready: VideoThumbReady,
    ) {
        let pair = (chat.clone(), msg_id.to_owned());
        self.m6.extraction_in_flight.remove(&pair);
        self.m6
            .thumbnails
            .insert(pair, VideoThumbState::Ready(ready));
    }

    pub fn set_video_thumbnail_extracting(&mut self, chat: &ChatKey, msg_id: &str) -> bool {
        let pair = (chat.clone(), msg_id.to_owned());
        if self.m6.extraction_in_flight.contains(&pair) {
            return false;
        }
        self.m6.extraction_in_flight.insert(pair.clone());
        self.m6.thumbnails.insert(pair, VideoThumbState::Extracting);
        true
    }

    pub fn set_video_thumbnail_failed(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        message: impl Into<String>,
    ) {
        let pair = (chat.clone(), msg_id.to_owned());
        self.m6.extraction_in_flight.remove(&pair);
        self.m6.thumbnails.insert(
            pair,
            VideoThumbState::Failed {
                message: message.into(),
            },
        );
    }

    /// Video messages in the active chat that still need a thumbnail extraction task.
    pub fn video_thumbnails_to_extract(&mut self) -> Vec<(ChatKey, String, PathBuf, String)> {
        let mut out = Vec::new();
        for msg in self.messages.clone() {
            if !is_video_message(&msg) {
                continue;
            }
            let pair = (msg.chat.clone(), msg.msg_id.clone());
            if self.m6.extraction_in_flight.contains(&pair)
                || self.m6.thumbnails.contains_key(&pair)
            {
                continue;
            }
            let Some(path) = self.media_path_for(&msg.chat, &msg.msg_id).cloned() else {
                continue;
            };
            let Some(sha) = self.video_content_sha256(&msg.chat, &msg.msg_id) else {
                continue;
            };
            out.push((msg.chat.clone(), msg.msg_id.clone(), path, sha));
        }
        out
    }

    // ---------------------------------------------------------------------
    // Player open / close
    // ---------------------------------------------------------------------

    pub fn video_player(&self) -> Option<&VideoPlayerUiState> {
        self.m6.player.as_ref()
    }

    pub fn video_player_is_open(&self) -> bool {
        self.m6.player.is_some()
    }

    /// Resolves a video message into a play request. The shell answers it by opening a
    /// `VideoPlaybackSession` and calling `attach_video_session`.
    pub fn request_video_player(&mut self, msg_id: &str, now_ms: u64) -> Option<VideoPlayRequest> {
        let msg = self.messages.iter().find(|m| m.msg_id == msg_id)?.clone();
        if !is_video_message(&msg) {
            return None;
        }
        let path = self.media_path_for(&msg.chat, &msg.msg_id).cloned()?;
        self.dismiss_all_modals();
        let _ = now_ms;
        Some(VideoPlayRequest {
            chat: msg.chat,
            msg_id: msg.msg_id,
            path,
        })
    }

    /// Command path: `Ctrl+K > Play video inline` for the selected message. The shell picks the
    /// request up through `take_pending_video_open` on its next sync pass.
    pub fn request_video_player_open_selected(&mut self) -> bool {
        let Some(msg) = self.selected_message() else {
            self.status_notice = Some("Select a video message first".into());
            return false;
        };
        if !is_video_message(msg) {
            self.status_notice = Some("Selected message is not a video".into());
            return false;
        }
        self.m6.pending_open_msg_id = Some(msg.msg_id.clone());
        true
    }

    pub fn take_pending_video_open(&mut self) -> Option<String> {
        self.m6.pending_open_msg_id.take()
    }

    /// Same request but for the currently selected message (palette command / keyboard path).
    pub fn request_video_player_for_selected(&mut self, now_ms: u64) -> Option<VideoPlayRequest> {
        let msg_id = self.selected_message()?.msg_id.clone();
        self.request_video_player(&msg_id, now_ms)
    }

    /// Installs the player once the shell has a session with real metadata.
    pub fn attach_video_session(
        &mut self,
        request: &VideoPlayRequest,
        metadata: VideoMetadata,
        now_ms: u64,
    ) -> bool {
        if !metadata.is_decodable() {
            return false;
        }
        self.m6.open_seq = self.m6.open_seq.saturating_add(1);
        let state = VideoPlayerUiState {
            chat: request.chat.clone(),
            msg_id: request.msg_id.clone(),
            path: request.path.clone(),
            width: metadata.width,
            height: metadata.height,
            duration_ms: metadata.duration_ms,
            has_audio: metadata.has_audio,
            hardware_accelerated: metadata.hardware_accelerated,
            playing: true,
            muted: false,
            fullscreen: false,
            occluded: false,
            position_ms: 0,
            wall_clock_ms: 0,
            audio_clock_ms: None,
            frame_pts_ms: 0,
            frame_bytes: 0,
            frames_presented: 0,
            started_at_ms: now_ms,
            last_tick_ms: now_ms,
            open_seq: self.m6.open_seq,
        };
        self.m6.player = Some(state);
        self.set_focus_raw(crate::viewmodel::FocusTarget::VideoPlayer);
        true
    }

    /// Closes the player and releases the frame buffer (0 bytes left resident).
    pub fn close_video_player(&mut self) -> bool {
        let Some(mut player) = self.m6.player.take() else {
            return false;
        };
        player.playing = false;
        player.frame_bytes = 0;
        player.audio_clock_ms = None;
        true
    }

    /// `Esc` inside the player: exit fullscreen first, close only on the second press.
    pub fn video_escape(&mut self) -> VideoEscapeOutcome {
        let Some(player) = self.m6.player.as_mut() else {
            return VideoEscapeOutcome::Ignored;
        };
        if player.fullscreen {
            player.fullscreen = false;
            return VideoEscapeOutcome::ExitedFullscreen;
        }
        self.close_video_player();
        VideoEscapeOutcome::Closed
    }

    /// Chat switch / minimize cleanup: stop the render stream and drop frame buffers.
    pub fn cleanup_video_player_for_chat(&mut self, chat: Option<&ChatKey>) -> bool {
        let Some(player) = self.m6.player.as_ref() else {
            return false;
        };
        let leaving = match chat {
            Some(active) => &player.chat != active,
            None => true,
        };
        if leaving {
            self.close_video_player();
            return true;
        }
        false
    }

    // ---------------------------------------------------------------------
    // Player controls
    // ---------------------------------------------------------------------

    pub fn toggle_video_play_pause(&mut self, now_ms: u64) -> bool {
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        if player.playing && !player.is_at_end() {
            player.playing = false;
            return true;
        }
        if player.is_at_end() {
            player.position_ms = 0;
            player.wall_clock_ms = 0;
            player.audio_clock_ms = None;
        }
        player.playing = true;
        player.occluded = false;
        player.last_tick_ms = now_ms;
        true
    }

    pub fn toggle_video_fullscreen(&mut self) -> bool {
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        player.fullscreen = !player.fullscreen;
        true
    }

    pub fn toggle_video_mute(&mut self) -> bool {
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        player.muted = !player.muted;
        if player.muted {
            // Muted video advances on the wall clock; keep it aligned with the audio position
            // we had so the picture does not jump.
            player.wall_clock_ms = player.position_ms;
        } else {
            player.wall_clock_ms = player.position_ms;
            player.audio_clock_ms = Some(player.position_ms);
        }
        true
    }

    /// Seeks to an absolute position (`ms`), clamped into the clip.
    pub fn seek_video_to_ms(&mut self, position_ms: u64) -> u64 {
        let Some(player) = self.m6.player.as_mut() else {
            return 0;
        };
        let target = if player.duration_ms == 0 {
            position_ms
        } else {
            position_ms.min(player.duration_ms)
        };
        player.position_ms = target;
        player.wall_clock_ms = target;
        player.audio_clock_ms = None;
        player.frame_pts_ms = target;
        target
    }

    /// Scrubber click/drag: `fraction` in `0.0..=1.0`.
    pub fn seek_video_fraction(&mut self, fraction: f32) -> u64 {
        let Some(player) = self.m6.player.as_ref() else {
            return 0;
        };
        let fraction = f64::from(fraction).clamp(0.0, 1.0);
        let target = (fraction * player.duration_ms as f64).round() as u64;
        self.seek_video_to_ms(target)
    }

    /// Window minimized/occluded: pause. Resuming is the user's explicit choice.
    pub fn set_video_occluded(&mut self, occluded: bool, now_ms: u64) -> bool {
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        if occluded {
            let was_playing = player.playing;
            player.playing = false;
            player.occluded = true;
            return was_playing;
        }
        player.occluded = false;
        player.last_tick_ms = now_ms;
        false
    }

    /// Advances the presentation clock while playing. Returns `true` when the clock moved.
    pub fn tick_video_player(&mut self, now_ms: u64) -> bool {
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        let delta = now_ms.saturating_sub(player.last_tick_ms);
        player.last_tick_ms = now_ms;
        if !player.playing || player.occluded {
            return false;
        }
        if delta > 0 {
            player.wall_clock_ms = player.wall_clock_ms.saturating_add(delta);
        }
        let next = player.presentation_ms();
        let advanced = next != player.position_ms;
        player.position_ms = next;
        if player.is_at_end() {
            player.playing = false;
        }
        advanced
    }

    /// Audio playback position reported by the render stream (`WASAPI` padding math).
    ///
    /// A report more than `AUDIO_CLOCK_LAG_TOLERANCE_MS` behind the picture is ignored: after a
    /// mute/unmute or a device reset the stream clock can be stale, and the video must never
    /// jump backwards.
    pub fn set_video_audio_clock(&mut self, audio_ms: u64) {
        if let Some(player) = self.m6.player.as_mut() {
            player.audio_clock_ms = Some(audio_ms);
            if player.clock_follows_audio()
                && audio_ms.saturating_add(AUDIO_CLOCK_LAG_TOLERANCE_MS) >= player.position_ms
            {
                player.position_ms = audio_ms.min(player.duration_ms);
                player.wall_clock_ms = player.position_ms;
            }
        }
    }

    /// Records a decoded frame; `bytes` must stay inside `MAX_VIDEO_FRAME_BYTES`.
    pub fn present_video_frame(&mut self, pts_ms: u64, bytes: usize) -> bool {
        if bytes > MAX_VIDEO_FRAME_BYTES {
            return false;
        }
        let Some(player) = self.m6.player.as_mut() else {
            return false;
        };
        player.frame_pts_ms = pts_ms;
        player.frame_bytes = bytes;
        player.frames_presented = player.frames_presented.saturating_add(1);
        true
    }

    /// Whether the shell should pull another frame now (playing, not occluded, not at end).
    pub fn video_player_wants_frame(&self) -> bool {
        self.m6
            .player
            .as_ref()
            .is_some_and(|p| p.playing && !p.occluded && !p.is_at_end() && p.duration_ms > 0)
    }

    /// Current frame PTS, used as the texture key so a new frame replaces the old one.
    pub fn video_player_frame_pts(&self) -> Option<u64> {
        self.m6.player.as_ref().map(|p| p.frame_pts_ms)
    }

    pub fn video_player_clock_label(&self) -> Option<String> {
        self.m6.player.as_ref().map(VideoPlayerUiState::clock_label)
    }

    /// Current time for the player (`epoch ms`, matching the other M6 entry points).
    pub fn video_now_ms() -> u64 {
        u64::try_from(now_epoch_millis_public()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::{Account, Chat, MediaKind, Provider};
    use postbox_keymap::Keymap;
    use postbox_platform::{MockVideoBackend, MockVideoConfig, VideoBackend};
    use postbox_store::Store;

    fn seeded_vm() -> (PostboxViewModel, ChatKey) {
        let mut store = Store::open_in_memory().unwrap();
        store
            .upsert_account(&Account {
                provider: Provider::Whatsapp,
                account_id: "main".into(),
                label: "WhatsApp".into(),
                state: "connected".into(),
                sort_order: 1,
                last_event_at: None,
            })
            .unwrap();
        let chat = Chat {
            key: ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net"),
            title: "Alice".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2025-01-15T12:00:00Z".into(),
            last_msg_preview: "clip".into(),
            avatar_hash: None,
        };
        store.upsert_chat(&chat, 100).unwrap();
        let video = Message {
            chat: chat.key.clone(),
            msg_id: "vid1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2025-01-15T12:00:00Z".into(),
            body: String::new(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: Some(MediaKind::Video),
            mime_type: Some("video/mp4".into()),
            filename: Some("clip.mp4".into()),
            file_size: Some(1_024),
            reactions: vec![],
            read_status: postbox_core::ReadStatus::Read,
            deleted: false,
        };
        store.upsert_message(&video).unwrap();
        let text = Message {
            chat: chat.key.clone(),
            msg_id: "txt1".into(),
            sender_id: "alice".into(),
            sender_name: "Alice".into(),
            from_me: false,
            ts: "2025-01-15T12:01:00Z".into(),
            body: "hello".into(),
            reply_to_id: None,
            reply_broadcast: false,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: vec![],
            read_status: postbox_core::ReadStatus::Read,
            deleted: false,
        };
        store.upsert_message(&text).unwrap();

        let mut vm = PostboxViewModel::new(store, Keymap::postbox_defaults(), "test");
        vm.select_chat_key(&chat.key);
        (vm, chat.key)
    }

    fn meta(duration_ms: u64) -> VideoMetadata {
        VideoMetadata {
            width: 640,
            height: 360,
            duration_ms,
            has_audio: true,
            hardware_accelerated: true,
        }
    }

    fn seed_media_path(vm: &mut PostboxViewModel, chat: &ChatKey, path: PathBuf) {
        vm.m3
            .media_paths
            .insert((chat.clone(), "vid1".to_owned()), path);
    }

    fn open_player(vm: &mut PostboxViewModel, duration_ms: u64) -> bool {
        let req = vm.request_video_player("vid1", 0).unwrap();
        vm.attach_video_session(&req, meta(duration_ms), 0)
    }

    #[test]
    fn double_click_open_requires_a_cached_file_and_a_video_message() {
        let (mut vm, chat) = seeded_vm();
        assert!(vm.is_video_message_id("vid1"));
        assert!(!vm.is_video_message_id("txt1"));
        // No local media yet: the bubble shows the attachment pill, not a player.
        assert!(vm.request_video_player("vid1", 0).is_none());
        assert!(vm.request_video_player("txt1", 0).is_none());

        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        let req = vm.request_video_player("vid1", 0).unwrap();
        assert_eq!(req.msg_id, "vid1");
        assert_eq!(req.path, PathBuf::from("/tmp/clip.mp4"));
        assert!(vm.attach_video_session(&req, meta(4_000), 0));
        assert!(vm.video_player().unwrap().playing);
        assert_eq!(vm.video_player().unwrap().clock_label(), "0:00 / 0:04");
        assert!(vm.video_player_wants_frame());
    }

    #[test]
    fn palette_command_queues_an_open_only_for_a_video_message() {
        let (mut vm, chat) = seeded_vm();
        // Select the text message: the command refuses it with a notice.
        let text_idx = vm.messages.iter().position(|m| m.msg_id == "txt1").unwrap();
        vm.select_message_index(text_idx);
        assert!(!vm.request_video_player_open_selected());
        assert!(vm.take_pending_video_open().is_none());

        let video_idx = vm.messages.iter().position(|m| m.msg_id == "vid1").unwrap();
        vm.select_message_index(video_idx);
        assert!(vm.request_video_player_open_selected());
        assert_eq!(vm.take_pending_video_open().as_deref(), Some("vid1"));
        // Consuming twice yields nothing (the shell opens it exactly once).
        assert!(vm.take_pending_video_open().is_none());
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
    }

    #[test]
    fn undecodable_metadata_never_opens_a_player() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        let req = vm.request_video_player("vid1", 0).unwrap();
        let bad = VideoMetadata {
            width: 0,
            height: 0,
            ..meta(1_000)
        };
        assert!(!vm.attach_video_session(&req, bad, 0));
        assert!(vm.video_player().is_none());
    }

    #[test]
    fn escape_exits_fullscreen_first_then_closes() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 4_000));
        assert!(!vm.video_player().unwrap().fullscreen);

        assert!(vm.toggle_video_fullscreen());
        assert!(vm.video_player().unwrap().fullscreen);
        assert_eq!(vm.video_escape(), VideoEscapeOutcome::ExitedFullscreen);
        assert!(vm.video_player().is_some(), "player stays open inline");
        assert!(!vm.video_player().unwrap().fullscreen);

        assert_eq!(vm.video_escape(), VideoEscapeOutcome::Closed);
        assert!(vm.video_player().is_none());
        assert_eq!(vm.video_escape(), VideoEscapeOutcome::Ignored);
    }

    #[test]
    fn play_pause_seek_and_mute_controls() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 10_000));

        // Playing by default; pause stops the clock.
        assert!(vm.video_player().unwrap().playing);
        assert!(vm.toggle_video_play_pause(100));
        assert!(!vm.video_player().unwrap().playing);
        assert!(!vm.tick_video_player(200));
        assert_eq!(vm.video_player().unwrap().position_ms, 0);

        assert!(vm.toggle_video_play_pause(300));
        assert!(vm.tick_video_player(800));
        assert_eq!(vm.video_player().unwrap().position_ms, 500);

        // Scrub to half the clip and to the very end.
        assert_eq!(vm.seek_video_fraction(0.5), 5_000);
        assert_eq!(vm.video_player().unwrap().position_ms, 5_000);
        assert_eq!(vm.seek_video_to_ms(99_999), 10_000);
        assert!(vm.video_player().unwrap().is_at_end());
        assert!(!vm.video_player_wants_frame());

        // Play from the end restarts at zero.
        assert!(vm.toggle_video_play_pause(1_000));
        assert_eq!(vm.video_player().unwrap().position_ms, 0);

        // Mute toggles the clock source and keeps the picture where it was.
        assert!(!vm.video_player().unwrap().muted);
        assert!(vm.toggle_video_mute());
        assert!(vm.video_player().unwrap().muted);
        assert!(!vm.video_player().unwrap().clock_follows_audio());
        assert!(vm.toggle_video_mute());
        assert!(vm.video_player().unwrap().clock_follows_audio());
    }

    #[test]
    fn presentation_clock_follows_audio_when_audible_and_wall_clock_when_muted() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 60_000));

        // Wall clock advanced to 900 ms by ticks.
        vm.tick_video_player(900);
        assert_eq!(vm.video_player().unwrap().position_ms, 900);

        // WASAPI reports 1_200 ms of audio played: the picture realigns on the audio clock.
        vm.set_video_audio_clock(1_200);
        assert_eq!(vm.video_player().unwrap().position_ms, 1_200);
        assert_eq!(
            av_sync_position_ms(Some(1_200), 900, true),
            1_200,
            "audible video follows the audio clock"
        );
        assert_eq!(
            av_sync_position_ms(Some(1_200), 900, false),
            900,
            "silent/muted video falls back to the monotonic wall clock"
        );

        // Muting drops back to the wall clock, which was kept aligned at 1_200 ms.
        assert!(vm.toggle_video_mute());
        vm.set_video_audio_clock(9_999);
        assert_eq!(vm.video_player().unwrap().position_ms, 1_200);
        // The wall clock was realigned at 1_200 ms when muting, so the next tick only adds
        // the time since the last tick (900 -> 1_000).
        vm.tick_video_player(1_000);
        assert_eq!(vm.video_player().unwrap().position_ms, 1_300);
    }

    #[test]
    fn a_stale_audio_clock_never_drags_the_video_backwards() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 60_000));
        vm.set_video_audio_clock(20_000);
        assert_eq!(vm.video_player().unwrap().position_ms, 20_000);

        // A device reset reports a clock far behind the picture: ignore it.
        vm.set_video_audio_clock(1_000);
        assert_eq!(
            vm.video_player().unwrap().position_ms,
            20_000,
            "stale audio clocks must not rewind the video"
        );
        // A small lag inside the tolerance still realigns (normal drift correction).
        vm.set_video_audio_clock(19_900);
        assert_eq!(vm.video_player().unwrap().position_ms, 19_900);
    }

    #[test]
    fn silent_clips_ignore_the_audio_clock_entirely() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        let req = vm.request_video_player("vid1", 0).unwrap();
        assert!(vm.attach_video_session(
            &req,
            VideoMetadata {
                has_audio: false,
                ..meta(5_000)
            },
            0
        ));
        vm.tick_video_player(400);
        vm.set_video_audio_clock(4_000);
        assert_eq!(vm.video_player().unwrap().position_ms, 400);
    }

    #[test]
    fn occlusion_pauses_playback_and_does_not_auto_resume() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 10_000));
        assert!(vm.set_video_occluded(true, 100));
        assert!(!vm.video_player().unwrap().playing);
        assert!(vm.video_player().unwrap().occluded);
        assert!(!vm.tick_video_player(5_000));
        assert_eq!(vm.video_player().unwrap().position_ms, 0);

        // Un-occluding keeps it paused (explicit resume).
        assert!(!vm.set_video_occluded(false, 5_100));
        assert!(!vm.video_player().unwrap().playing);
        assert!(vm.toggle_video_play_pause(5_200));
        assert!(vm.tick_video_player(5_400));
        assert_eq!(vm.video_player().unwrap().position_ms, 200);
    }

    #[test]
    fn chat_switch_releases_the_player_and_its_frame_buffer() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 10_000));
        assert!(vm.present_video_frame(0, 640 * 360 * 4));
        assert_eq!(vm.video_player().unwrap().frame_bytes, 640 * 360 * 4);

        // Staying in the same chat keeps it open.
        assert!(!vm.cleanup_video_player_for_chat(Some(&chat)));
        assert!(vm.video_player().is_some());

        let other = ChatKey::new(Provider::Slack, "T1", "C-other");
        assert!(vm.cleanup_video_player_for_chat(Some(&other)));
        assert!(vm.video_player().is_none());
        // Closing it again is a no-op.
        assert!(!vm.cleanup_video_player_for_chat(None));
    }

    #[test]
    fn frames_are_presented_inside_the_byte_cap_only() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 10_000));
        assert!(vm.present_video_frame(0, 1_024));
        assert_eq!(vm.video_player().unwrap().frames_presented, 1);
        assert_eq!(vm.video_player_frame_pts(), Some(0));
        assert!(!vm.present_video_frame(40, MAX_VIDEO_FRAME_BYTES + 1));
        assert_eq!(vm.video_player().unwrap().frame_bytes, 1_024);
    }

    #[test]
    fn thumbnails_track_extracting_ready_and_failed_states() {
        let (mut vm, chat) = seeded_vm();
        assert!(vm.set_video_thumbnail_extracting(&chat, "vid1"));
        assert!(!vm.set_video_thumbnail_extracting(&chat, "vid1"));
        assert_eq!(
            vm.video_thumb_state(&chat, "vid1"),
            Some(&VideoThumbState::Extracting)
        );

        let mock = MockVideoBackend::with_config(MockVideoConfig {
            duration_ms: 65_000,
            ..MockVideoConfig::default()
        });
        let frame = mock
            .extract_thumbnail(std::path::Path::new("clip.mp4"), 420, 250)
            .unwrap()
            .unwrap();
        vm.set_video_thumbnail_ready(
            &chat,
            "vid1",
            VideoThumbReady {
                path: PathBuf::from("/tmp/thumb.png"),
                width: frame.width,
                height: frame.height,
                duration_ms: 65_000,
                hardware_accelerated: true,
            },
        );
        let ready = match vm.video_thumb_state(&chat, "vid1").unwrap() {
            VideoThumbState::Ready(r) => r.clone(),
            other => panic!("expected ready, got {other:?}"),
        };
        assert_eq!(ready.duration_label(), "1:05");
        assert_eq!((ready.width, ready.height), (420, 236));

        vm.set_video_thumbnail_failed(&chat, "vid1", "decode_failed");
        assert!(matches!(
            vm.video_thumb_state(&chat, "vid1"),
            Some(VideoThumbState::Failed { .. })
        ));
    }

    #[test]
    fn thumbnail_extraction_queue_needs_a_local_file_and_a_sha() {
        let (mut vm, chat) = seeded_vm();
        assert!(vm.video_thumbnails_to_extract().is_empty());

        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        // No media_cache row yet -> no content hash -> nothing to queue.
        assert!(vm.video_thumbnails_to_extract().is_empty());

        vm.record_video_sha256(&chat, "vid1", "0011223344556677");
        let queued = vm.video_thumbnails_to_extract();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1, "vid1");
        assert_eq!(queued[0].3, "0011223344556677");

        // Second call after the state is set does not re-queue.
        vm.set_video_thumbnail_extracting(&chat, "vid1");
        assert!(vm.video_thumbnails_to_extract().is_empty());
    }

    #[test]
    fn player_helpers_expose_badge_and_progress() {
        let (mut vm, chat) = seeded_vm();
        seed_media_path(&mut vm, &chat, PathBuf::from("/tmp/clip.mp4"));
        assert!(open_player(&mut vm, 4_000));
        vm.seek_video_fraction(0.25);
        let player = vm.video_player().unwrap();
        assert!((player.progress_fraction() - 0.25).abs() < 1e-6);
        assert_eq!(player.decode_badge(), "HW · Arc");
        assert_eq!(player.clock_label(), "0:01 / 0:04");
        assert_eq!(vm.video_player_clock_label().unwrap(), "0:01 / 0:04");
    }
}
