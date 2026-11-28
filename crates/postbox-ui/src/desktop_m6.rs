//! Milestone 6 GPUI desktop UI: cached video thumbnails in the timeline plus the inline video
//! player (thumbnail + play badge + duration chip, `F11` fullscreen, `Esc` exits fullscreen
//! first, `Space` play/pause, scrubber, mute, `elapsed / total` clock, hardware badge).
//!
//! Frames are pulled from the open `VideoPlaybackSession` on a `16 ms` timer that only exists
//! while a video is playing and the window is active, uploaded through the existing
//! `<= 16 MiB` GPU texture LRU (exactly one player frame is resident at a time), and audio is
//! driven through `AudioBackend::open_render` (WASAPI on Windows) with the render-stream clock
//! driving presentation. Occluded/minimized pauses playback; closing or switching chats stops
//! the render stream and releases the frame buffer.

use crate::desktop::{PostboxShell, RADIUS_BUBBLE_PX, RADIUS_CHIP_PX, RADIUS_MODAL_PX};
use crate::m6::{VIDEO_PLAYER_TICK_MS, VideoThumbReady};
use crate::theme::ShadcnPalette;
use crate::{FONT_MONO, scaled_px};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::{
    AnyElement, Context, InteractiveElement as _, IntoElement, MouseButton, ObjectFit,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
    div, img, px, rgb,
};
use postbox_core::Message;
use postbox_media::decode::{DecodedFrame, DecodedImage};
use postbox_media::video::VideoThumbnailCache;
use postbox_media::{PREVIEW_MAX_H, PREVIEW_MAX_W};
use postbox_platform::{AudioRenderStream, VideoFrame, VideoPlaybackSession};
use std::path::PathBuf;
use std::time::Duration;

/// Texture key prefix for video bubble thumbnails (transient: purged on chat switch/minimize).
pub(crate) const VIDEO_THUMB_KEY_PREFIX: &str = "videothumb:";
/// Texture key prefix for the single live player frame.
pub(crate) const VIDEO_PLAYER_KEY_PREFIX: &str = "videoplayer:";
/// Scrubber segments: clicking a segment seeks to that fraction of the clip.
pub(crate) const VIDEO_SCRUB_SEGMENTS: usize = 20;
/// Most PCM chunks handed to the render stream in one presentation tick (0.5 s at 20 ms each).
pub(crate) const MAX_AUDIO_CHUNKS_PER_TICK: usize = 25;

pub(crate) fn video_thumb_tex_key(chat_id: &str, msg_id: &str, path: &std::path::Path) -> String {
    format!(
        "{VIDEO_THUMB_KEY_PREFIX}{chat_id}:{msg_id}:{}",
        path.display()
    )
}

pub(crate) fn video_player_tex_key(msg_id: &str, pts_ms: u64) -> String {
    format!("{VIDEO_PLAYER_KEY_PREFIX}{msg_id}:{pts_ms}")
}

/// Audio playback clock derived from frames actually handed to the render stream.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AudioClock {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames_written: u64,
    /// Position the current burst started from (a seek re-bases the clock instead of
    /// pretending the clip restarted at zero).
    pub base_ms: u64,
}

impl AudioClock {
    fn position_ms(&self) -> u64 {
        if self.sample_rate == 0 {
            return self.base_ms;
        }
        self.base_ms
            .saturating_add(self.frames_written.saturating_mul(1_000) / u64::from(self.sample_rate))
    }

    fn add_chunk(&mut self, sample_rate: u32, channels: u16, frames: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.frames_written = self.frames_written.saturating_add(frames as u64);
    }

    fn rebase(&mut self, position_ms: u64) {
        self.base_ms = position_ms;
        self.frames_written = 0;
    }
}

/// One open decode session plus its WASAPI render stream and clock.
pub(crate) struct VideoRuntime {
    pub session: Box<dyn VideoPlaybackSession>,
    pub audio: Option<Box<dyn AudioRenderStream>>,
    pub clock: AudioClock,
    pub pending_frame: Option<VideoFrame>,
    pub resident_key: Option<String>,
    pub msg_id: String,
}

impl VideoRuntime {
    fn release(&mut self) {
        if let Some(mut stream) = self.audio.take() {
            let _ = stream.pause();
            let _ = stream.stop();
        }
        self.session.close();
        self.pending_frame = None;
        self.resident_key = None;
    }
}

/// Converts a decoded `VideoFrame` into the BGRA `DecodedImage` the GPU texture LRU uploads.
pub(crate) fn video_frame_to_decoded(frame: &VideoFrame) -> Option<DecodedImage> {
    let mut bgra = frame.rgba.clone();
    postbox_media::decode::rgba_to_bgra_in_place(&mut bgra);
    if bgra.len() != (frame.width as usize) * (frame.height as usize) * 4 {
        return None;
    }
    Some(DecodedImage {
        src_width: frame.width,
        src_height: frame.height,
        width: frame.width,
        height: frame.height,
        frames: vec![DecodedFrame {
            width: frame.width,
            height: frame.height,
            bgra,
            delay_ms: 0,
        }],
        is_animated_gif: false,
    })
}

impl PostboxShell {
    // ---------------------------------------------------------------------
    // Open / close
    // ---------------------------------------------------------------------

    /// Double-click on a video bubble (or a command) opens the inline player.
    pub(crate) fn open_video_player_for(
        &mut self,
        msg_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let now = self.now_ms();
        let Some(request) = self.vm.request_video_player(msg_id, now) else {
            return false;
        };
        let Some(backend) = self.video_backend.clone() else {
            return false;
        };
        let Ok(session) = backend.open_session(&request.path) else {
            return false;
        };
        let metadata = session.metadata();
        if !self.vm.attach_video_session(&request, metadata, now) {
            return false;
        }
        self.release_video_runtime(window, cx);
        self.video_runtime = Some(VideoRuntime {
            session,
            audio: None,
            clock: AudioClock::default(),
            pending_frame: None,
            resident_key: None,
            msg_id: request.msg_id.clone(),
        });
        self.pump_video_player(now, window, cx);
        self.sync_video_player_timer(window, cx);
        self.sync_after_vm_change(window, cx);
        true
    }

    /// Releases the decoder, the render stream and the resident player frame.
    pub(crate) fn release_video_runtime(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut runtime) = self.video_runtime.take() else {
            return;
        };
        if let Some(key) = runtime.resident_key.take() {
            self.gpu_textures.remove_key(&key, window, cx);
        }
        runtime.release();
        self.video_timer_task = None;
    }

    pub(crate) fn close_video_player_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let closed = self.vm.close_video_player();
        self.release_video_runtime(window, cx);
        cx.notify();
        closed
    }

    /// `Esc`: exit fullscreen first, close only on the second press.
    pub(crate) fn handle_video_escape(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.vm.video_escape() {
            crate::m6::VideoEscapeOutcome::Ignored => false,
            crate::m6::VideoEscapeOutcome::ExitedFullscreen => {
                cx.notify();
                true
            }
            crate::m6::VideoEscapeOutcome::Closed => {
                self.release_video_runtime(window, cx);
                cx.notify();
                true
            }
        }
    }

    pub(crate) fn toggle_video_fullscreen_ui(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.vm.toggle_video_fullscreen();
        if changed {
            cx.notify();
        }
        changed
    }

    pub(crate) fn toggle_video_play_pause_ui(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.vm.video_player().is_none() {
            return false;
        }
        let now = self.now_ms();
        let toggled = self.vm.toggle_video_play_pause(now);
        let playing = toggled && self.vm.video_player().is_some_and(|p| p.playing);
        if let Some(runtime) = self.video_runtime.as_mut() {
            match runtime.audio.as_mut() {
                Some(stream) if playing => {
                    let _ = stream.resume();
                }
                Some(stream) => {
                    let _ = stream.pause();
                }
                None => {}
            }
        }
        self.sync_video_player_timer(window, cx);
        cx.notify();
        true
    }

    pub(crate) fn toggle_video_mute_ui(&mut self, cx: &mut Context<Self>) -> bool {
        let changed = self.vm.toggle_video_mute();
        if changed {
            let muted = self.vm.video_player().is_some_and(|p| p.muted);
            let position = self.vm.video_player().map(|p| p.position_ms).unwrap_or(0);
            if let Some(runtime) = self.video_runtime.as_mut() {
                if muted {
                    if let Some(stream) = runtime.audio.as_mut() {
                        let _ = stream.pause();
                    }
                } else {
                    // Unmuting resumes audio from the current picture position instead of
                    // letting the stale audio clock drag the video backwards.
                    let _ = runtime.session.seek_ms(position);
                    runtime.clock.rebase(position);
                    runtime.pending_frame = None;
                    if let Some(stream) = runtime.audio.as_mut() {
                        let _ = stream.reset();
                        let _ = stream.resume();
                    }
                }
            }
            cx.notify();
        }
        changed
    }

    pub(crate) fn seek_video_fraction_ui(
        &mut self,
        fraction: f32,
        now: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.vm.seek_video_fraction(fraction);
        if let Some(mut runtime) = self.video_runtime.take() {
            let _ = runtime.session.seek_ms(target);
            runtime.clock.rebase(target);
            runtime.pending_frame = None;
            if let Some(stream) = runtime.audio.as_mut() {
                let _ = stream.reset();
            }
            self.video_runtime = Some(runtime);
        }
        self.pump_video_player(now, window, cx);
        cx.notify();
    }

    /// Window lost focus / minimized: pause (explicit resume afterwards).
    pub(crate) fn pause_video_when_occluded(&mut self, cx: &mut Context<Self>) {
        if !self.vm.video_player_is_open() {
            return;
        }
        let now = self.now_ms();
        if self.vm.set_video_occluded(true, now) {
            if let Some(runtime) = self.video_runtime.as_mut()
                && let Some(stream) = runtime.audio.as_mut()
            {
                let _ = stream.pause();
            }
            self.video_timer_task = None;
            cx.notify();
        }
    }

    // ---------------------------------------------------------------------
    // Pump
    // ---------------------------------------------------------------------

    /// One presentation tick: advance the clock, push PCM to the render stream, upload the
    /// frame that belongs at the current position.
    pub(crate) fn pump_video_player(
        &mut self,
        now_ms: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vm.video_player().is_none() {
            return;
        }
        self.vm.tick_video_player(now_ms);
        let Some(player) = self.vm.video_player().cloned() else {
            return;
        };
        let Some(mut runtime) = self.video_runtime.take() else {
            return;
        };

        // Audio: keep the WASAPI stream fed up to ~200 ms ahead of the presentation clock.
        // Bounded per tick so one slow tick can never drain the whole track.
        if player.clock_follows_audio() {
            let position = player.presentation_ms();
            let mut fed = 0;
            while fed < MAX_AUDIO_CHUNKS_PER_TICK
                && runtime.clock.position_ms() < position.saturating_add(200)
            {
                let Ok(Some(chunk)) = runtime.session.next_audio_chunk() else {
                    break;
                };
                if runtime.audio.is_none()
                    && let Some(audio) = self.audio_backend.clone()
                {
                    runtime.audio = audio.open_render(chunk.sample_rate, chunk.channels).ok();
                }
                if let Some(stream) = runtime.audio.as_mut() {
                    let _ = stream.write_frames(&chunk.pcm_f32);
                }
                runtime
                    .clock
                    .add_chunk(chunk.sample_rate, chunk.channels, chunk.frames());
                fed += 1;
            }
            self.vm.set_video_audio_clock(runtime.clock.position_ms());
        }

        // Video: decode forward to the frame that belongs at (or just before) the position.
        if self.vm.video_player_wants_frame() {
            let position = self
                .vm
                .video_player()
                .map(|p| p.presentation_ms())
                .unwrap_or(0);
            let mut presented = None;
            while let Ok(Some(frame)) = runtime.session.next_video_frame() {
                let pts = frame.pts_ms;
                runtime.pending_frame = Some(frame);
                presented = runtime.pending_frame.clone();
                if pts >= position {
                    break;
                }
            }
            if let Some(frame) = presented {
                let key = video_player_tex_key(&runtime.msg_id, frame.pts_ms);
                if let Some(decoded) = video_frame_to_decoded(&frame) {
                    let bytes = decoded.byte_size();
                    let old_key = runtime.resident_key.replace(key.clone());
                    self.gpu_textures
                        .ensure_decoded_inserted(&key, &decoded, window, cx);
                    if let Some(old) = old_key
                        && old != key
                    {
                        self.gpu_textures.remove_key(&old, window, cx);
                    }
                    self.vm.present_video_frame(frame.pts_ms, bytes);
                }
            }
        }

        self.video_runtime = Some(runtime);
        self.sync_video_player_timer(window, cx);
    }

    /// Arms the `16 ms` presentation timer ONLY while a video is playing in an active window.
    pub(crate) fn sync_video_player_timer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let should_run = window.is_window_active()
            && self
                .vm
                .video_player()
                .is_some_and(|p| p.playing && !p.occluded);
        if !should_run {
            self.video_timer_task = None;
            return;
        }
        if self.video_timer_task.is_some() {
            return;
        }
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(VIDEO_PLAYER_TICK_MS))
                    .await;
                let keep_going = this
                    .update_in(cx, |shell, window, cx| {
                        if !window.is_window_active() {
                            shell.pause_video_when_occluded(cx);
                            shell.video_timer_task = None;
                            return false;
                        }
                        if !shell
                            .vm
                            .video_player()
                            .is_some_and(|p| p.playing && !p.occluded)
                        {
                            shell.video_timer_task = None;
                            return false;
                        }
                        let now =
                            u64::try_from(crate::viewmodel::now_epoch_millis_public()).unwrap_or(0);
                        shell.pump_video_player(now, window, cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        });
        self.video_timer_task = Some(task);
    }

    // ---------------------------------------------------------------------
    // Thumbnails
    // ---------------------------------------------------------------------

    /// Resolves cached thumbnails synchronously and queues extraction for the rest.
    pub(crate) fn pump_video_thumbnails(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cache) = self.video_thumb_cache.clone() else {
            return;
        };
        // Cache hits first (cheap: one indexed row + a file stat), then off-thread extraction.
        let ready: Vec<(ChatKey, String, VideoThumbReady)> = self
            .vm
            .video_thumbnails_to_extract()
            .into_iter()
            .filter_map(|(chat, msg_id, _path, sha)| {
                let cached = cache.lookup(self.vm.store(), &sha)?;
                Some((chat, msg_id, VideoThumbReady::from_cached(&cached)))
            })
            .collect();
        for (chat, msg_id, thumb) in ready {
            self.vm.set_video_thumbnail_ready(&chat, &msg_id, thumb);
        }

        let Some(backend) = self.video_backend.clone() else {
            return;
        };
        for (chat, msg_id, path, sha) in self.vm.video_thumbnails_to_extract() {
            if !self.vm.set_video_thumbnail_extracting(&chat, &msg_id) {
                continue;
            }
            let prepare_backend = backend.clone();
            let video_path = path.clone();
            let bg = cx.background_executor().spawn(async move {
                postbox_media::video::prepare_video_thumbnail(
                    prepare_backend.as_ref(),
                    &video_path,
                    PREVIEW_MAX_W,
                    PREVIEW_MAX_H,
                )
            });
            let cache = cache.clone();
            cx.spawn_in(
                window,
                async move |this: gpui_kit::WeakEntity<PostboxShell>,
                            cx: &mut gpui_kit::AsyncWindowContext| {
                    let prepared = bg.await;
                    let _ = this.update_in(cx, |shell, window, cx| {
                        shell.finish_video_thumbnail(&cache, &chat, &msg_id, &sha, prepared);
                        cx.notify();
                        let _ = window;
                    });
                },
            )
            .detach();
        }
    }

    fn finish_video_thumbnail(
        &mut self,
        cache: &VideoThumbnailCache,
        chat: &ChatKey,
        msg_id: &str,
        sha: &str,
        prepared: Result<
            Option<postbox_media::video::PreparedVideoThumb>,
            postbox_media::MediaError,
        >,
    ) {
        let prepared = match prepared {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                self.vm
                    .set_video_thumbnail_failed(chat, msg_id, "no_frame_decoded");
                return;
            }
            Err(err) => {
                self.vm
                    .set_video_thumbnail_failed(chat, msg_id, err.to_string());
                return;
            }
        };
        match cache.store_prepared(self.vm.store(), sha, &prepared) {
            Ok(cached) => self.vm.set_video_thumbnail_ready(
                chat,
                msg_id,
                VideoThumbReady::from_cached(&cached),
            ),
            Err(err) => self
                .vm
                .set_video_thumbnail_failed(chat, msg_id, err.to_string()),
        }
    }

    /// Feeds cached video thumbnails into the existing bounded off-UI-thread decode queue, so
    /// they live in the same `<= 16 MiB` GPU texture LRU as inline images.
    pub(crate) fn queue_video_thumb_textures(&mut self) {
        let ready: Vec<(String, String, PathBuf)> = self
            .vm
            .messages()
            .iter()
            .filter(|m| self.vm.is_video_message_id(&m.msg_id))
            .filter_map(|m| match self.vm.video_thumb_state(&m.chat, &m.msg_id) {
                Some(crate::m6::VideoThumbState::Ready(thumb)) => {
                    Some((m.chat.id.clone(), m.msg_id.clone(), thumb.path.clone()))
                }
                _ => None,
            })
            .collect();
        for (chat_id, msg_id, path) in ready {
            let key = video_thumb_tex_key(&chat_id, &msg_id, &path);
            self.gpu_textures.enqueue(crate::desktop_m3::DecodeJob {
                key,
                path,
                kind: crate::desktop_m3::DecodeKind::Standard {
                    max_dims: (PREVIEW_MAX_W, PREVIEW_MAX_H),
                    allow_upscale_2x: false,
                },
            });
        }
    }

    // ---------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------

    /// Video bubble: cached thumbnail + play badge + duration chip, or a clean fallback card
    /// while the thumbnail is still being extracted.
    pub(crate) fn render_video_bubble(
        &self,
        msg: &Message,
        p: &ShadcnPalette,
        ix: usize,
        id_prefix: &str,
        shell: &gpui_kit::Entity<Self>,
    ) -> Option<AnyElement> {
        let scale = self.vm.font_scale().percent();
        let chip_px = scaled_px(11.0, scale);
        let state = self.vm.video_thumb_state(&msg.chat, &msg.msg_id).cloned();

        let frame_el: AnyElement = match &state {
            Some(crate::m6::VideoThumbState::Ready(thumb)) => {
                let key = video_thumb_tex_key(&msg.chat.id, &msg.msg_id, &thumb.path);
                match self.gpu_textures.peek(&key).and_then(|e| e.frame_at(0)) {
                    Some(frame) => {
                        let entry = self.gpu_textures.peek(&key);
                        let (w, h) = entry
                            .map(|e| (e.width as f32, e.height as f32))
                            .unwrap_or((PREVIEW_MAX_W as f32, PREVIEW_MAX_H as f32));
                        div()
                            .relative()
                            .rounded(px(RADIUS_BUBBLE_PX))
                            .overflow_hidden()
                            .child(
                                img(frame)
                                    .w(px(w))
                                    .h(px(h))
                                    .object_fit(ObjectFit::Contain)
                                    .rounded(px(RADIUS_BUBBLE_PX)),
                            )
                            .child(self.render_video_play_badge(thumb, chip_px))
                            .into_any_element()
                    }
                    None => self.render_video_fallback_card(p, chip_px, "Loading thumbnail"),
                }
            }
            Some(crate::m6::VideoThumbState::Extracting) => {
                self.render_video_fallback_card(p, chip_px, "Extracting thumbnail")
            }
            Some(crate::m6::VideoThumbState::Failed { .. }) | None => {
                return None;
            }
        };

        let click_id = msg.msg_id.clone();
        let shell_click = shell.clone();
        let open_id = msg.msg_id.clone();
        let shell_open = shell.clone();
        Some(
            div()
                .id(gpui_kit::SharedString::from(format!(
                    "{id_prefix}-video-{ix}"
                )))
                .test_support()
                .cursor_pointer()
                .rounded(px(RADIUS_BUBBLE_PX))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_down(
                    MouseButton::Left,
                    move |event: &gpui_kit::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        if event.click_count >= 2 {
                            shell_open.update(cx, |this, cx| {
                                this.open_video_player_for(&open_id, window, cx);
                            });
                        } else if event.click_count == 1 {
                            let id = click_id.clone();
                            shell_click.update(cx, |this, cx| {
                                if let Some(idx) =
                                    this.vm.messages.iter().position(|m| m.msg_id == id)
                                {
                                    this.vm.select_message_index(idx);
                                    this.sync_after_vm_change(window, cx);
                                }
                            });
                        }
                    },
                )
                .child(frame_el)
                .into_any_element(),
        )
    }

    fn render_video_play_badge(&self, thumb: &VideoThumbReady, chip_px: f32) -> AnyElement {
        div()
            .absolute()
            .left(px(8.0))
            .bottom(px(8.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .id("video-play-badge")
                    .test_support()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.55))
                    .border_1()
                    .border_color(rgb(0xffffff))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .text_size(px(chip_px + 2.0))
                            .text_color(rgb(0xffffff))
                            .child("▶"),
                    ),
            )
            .child(
                div()
                    .id("video-duration-chip")
                    .test_support()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.65))
                    .font_family(FONT_MONO)
                    .text_size(px(chip_px))
                    .text_color(rgb(0xffffff))
                    .child(thumb.duration_label()),
            )
            .into_any_element()
    }

    fn render_video_fallback_card(
        &self,
        p: &ShadcnPalette,
        chip_px: f32,
        label: &'static str,
    ) -> AnyElement {
        div()
            .id("video-thumb-fallback")
            .test_support()
            .w(px(260.0))
            .h(px(146.0))
            .rounded(px(RADIUS_BUBBLE_PX))
            .bg(rgb(p.secondary))
            .border_1()
            .border_color(rgb(p.border))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(chip_px))
            .text_color(rgb(p.muted_foreground))
            .child(label)
            .into_any_element()
    }

    /// The inline player: frame + controls, or a fullscreen overlay when `F11` is active.
    pub(crate) fn render_video_player_overlay(
        &self,
        p: &ShadcnPalette,
        focus: &gpui_kit::FocusHandle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let player = self.vm.video_player()?.clone();
        let scale = self.vm.font_scale().percent();
        let body_px = scaled_px(13.0, scale);
        let label_px = scaled_px(11.0, scale);

        let frame_key = video_player_tex_key(&player.msg_id, player.frame_pts_ms);
        let frame = self
            .gpu_textures
            .peek(&frame_key)
            .and_then(|e| e.frame_at(0));

        let (max_w, max_h) = if player.fullscreen {
            (1600.0, 900.0)
        } else {
            (760.0, 420.0)
        };
        let (frame_w, frame_h) = fit_display(player.width, player.height, max_w, max_h);

        let video_area: AnyElement = match frame {
            Some(frame) => div()
                .w(px(frame_w))
                .h(px(frame_h))
                .bg(rgb(0x000000))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(frame)
                        .w(px(frame_w))
                        .h(px(frame_h))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            None => div()
                .id("video-player-frame-placeholder")
                .test_support()
                .w(px(frame_w))
                .h(px(frame_h))
                .bg(rgb(0x000000))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(body_px))
                .text_color(rgb(p.muted_foreground))
                .child("Buffering")
                .into_any_element(),
        };

        // Scrubber: 20 clickable segments spanning the clip.
        let mut scrubber = div()
            .id("video-scrubber")
            .test_support()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(1.0))
            .h(px(14.0))
            .flex_1()
            .cursor_pointer();
        let progress = player.progress_fraction();
        for segment in 0..VIDEO_SCRUB_SEGMENTS {
            let filled = (segment as f32 + 0.5) / VIDEO_SCRUB_SEGMENTS as f32 <= progress;
            let color = if filled { p.primary } else { p.muted };
            let seg = div()
                .id(gpui_kit::SharedString::from(format!(
                    "video-scrub-seg-{segment}"
                )))
                .test_support()
                .flex_1()
                .h(px(if filled { 6.0 } else { 3.0 }))
                .rounded_full()
                .bg(rgb(color))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let now = this.now_ms();
                    let fraction = (segment as f32 + 0.5) / VIDEO_SCRUB_SEGMENTS as f32;
                    this.seek_video_fraction_ui(fraction, now, window, cx);
                }));
            scrubber = scrubber.child(seg);
        }

        let play_label = if player.playing { "❚❚" } else { "▶" };
        let mute_label = if player.muted { "Unmute" } else { "Mute" };
        let fullscreen_label = if player.fullscreen { "Exit F11" } else { "F11" };

        let controls = div()
            .w_full()
            .px_3()
            .py_2()
            .bg(rgb(p.card))
            .border_t_1()
            .border_color(rgb(p.border))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .id("video-play-pause")
                    .test_support()
                    .w(px(28.0))
                    .h(px(28.0))
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.primary))
                    .text_color(rgb(p.primary_foreground))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_video_play_pause_ui(window, cx);
                    }))
                    .child(play_label),
            )
            .child(
                div()
                    .id("video-clock")
                    .test_support()
                    .font_family(FONT_MONO)
                    .text_size(px(label_px))
                    .text_color(rgb(p.foreground))
                    .child(player.clock_label()),
            )
            .child(scrubber)
            .child(
                div()
                    .id("video-hw-badge")
                    .test_support()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.secondary))
                    .text_size(px(label_px))
                    .text_color(rgb(p.muted_foreground))
                    .child(player.decode_badge()),
            )
            .child(
                div()
                    .id("video-mute")
                    .test_support()
                    .px_2()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.muted))
                    .text_size(px(label_px))
                    .text_color(rgb(p.foreground))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_video_mute_ui(cx);
                    }))
                    .child(mute_label),
            )
            .child(
                div()
                    .id("video-fullscreen")
                    .test_support()
                    .px_2()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.muted))
                    .text_size(px(label_px))
                    .text_color(rgb(p.foreground))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_video_fullscreen_ui(cx);
                    }))
                    .child(fullscreen_label),
            )
            .child(
                div()
                    .id("video-close")
                    .test_support()
                    .px_2()
                    .py_0p5()
                    .rounded(px(RADIUS_CHIP_PX))
                    .bg(rgb(p.muted))
                    .text_size(px(label_px))
                    .text_color(rgb(p.foreground))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_video_player_ui(window, cx);
                    }))
                    .child("Esc"),
            );

        let body = div()
            .flex()
            .flex_col()
            .rounded(px(RADIUS_MODAL_PX))
            .overflow_hidden()
            .bg(rgb(p.background))
            .border_1()
            .border_color(rgb(p.border))
            .child(video_area)
            .child(controls);

        let wrapper = if player.fullscreen {
            div()
                .id("video-player-overlay")
                .test_support()
                .absolute()
                .inset_0()
                .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.92))
                .track_focus(focus)
                .key_context("PostboxVideoPlayer")
                .flex()
                .items_center()
                .justify_center()
                .on_click(cx.listener(|this, _, window, cx| {
                    // Click outside the player exits fullscreen first (Esc semantics).
                    this.handle_video_escape(window, cx);
                }))
                .child(body)
        } else {
            div()
                .id("video-player-overlay")
                .test_support()
                .absolute()
                .right(px(24.0))
                .bottom(px(96.0))
                .track_focus(focus)
                .key_context("PostboxVideoPlayer")
                .rounded(px(RADIUS_MODAL_PX))
                .shadow_lg()
                .child(body)
        };
        Some(wrapper.into_any_element())
    }
}

/// Fits a decoded frame into the player's display box without upscaling past the panel.
pub(crate) fn fit_display(width: u32, height: u32, max_w: f32, max_h: f32) -> (f32, f32) {
    if width == 0 || height == 0 {
        return (max_w.min(320.0), max_h.min(180.0));
    }
    let scale_w = max_w / width as f32;
    let scale_h = max_h / height as f32;
    let scale = scale_w.min(scale_h);
    (
        (width as f32 * scale).round(),
        (height as f32 * scale).round(),
    )
}

/// Local alias so this module does not need the fully-qualified chat key type everywhere.
use postbox_core::ChatKey;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_fit_keeps_aspect_and_handles_zero_sizes() {
        assert_eq!(fit_display(640, 360, 760.0, 420.0), (747.0, 420.0));
        assert_eq!(fit_display(360, 640, 760.0, 420.0), (236.0, 420.0));
        assert_eq!(fit_display(0, 0, 760.0, 420.0), (320.0, 180.0));
    }

    #[test]
    fn decoded_conversion_swaps_red_and_blue() {
        let frame = VideoFrame::new(1, 1, 0, vec![10, 20, 30, 255]).unwrap();
        let decoded = video_frame_to_decoded(&frame).unwrap();
        assert_eq!(decoded.frames[0].bgra, vec![30, 20, 10, 255]);
        assert_eq!(decoded.byte_size(), 4);
        assert!(
            video_frame_to_decoded(&VideoFrame {
                width: 2,
                height: 2,
                pts_ms: 0,
                rgba: vec![0; 4],
            })
            .is_none()
        );
    }

    #[test]
    fn audio_clock_tracks_written_frames() {
        let mut clock = AudioClock::default();
        assert_eq!(clock.position_ms(), 0);
        clock.add_chunk(48_000, 2, 960);
        assert_eq!(clock.position_ms(), 20);
        clock.add_chunk(48_000, 2, 960);
        assert_eq!(clock.position_ms(), 40);
        // A stream with no sample rate stays at its base position.
        let empty = AudioClock::default();
        assert_eq!(empty.position_ms(), 0);
        // Seeking re-bases the clock instead of restarting the clip at zero.
        let mut seeked = AudioClock::default();
        seeked.rebase(58_500);
        assert_eq!(seeked.position_ms(), 58_500);
        seeked.add_chunk(48_000, 2, 960);
        assert_eq!(seeked.position_ms(), 58_520);
    }

    #[test]
    fn texture_keys_are_prefix_scoped() {
        let key = video_thumb_tex_key("chat", "m1", std::path::Path::new("/tmp/t.png"));
        assert!(key.starts_with(VIDEO_THUMB_KEY_PREFIX));
        assert!(key.contains("chat:m1"));
        let play = video_player_tex_key("m1", 1_000);
        assert!(player_key_is_fresh(&play));
        assert_eq!(play, "videoplayer:m1:1000");
    }

    fn player_key_is_fresh(key: &str) -> bool {
        key.starts_with(VIDEO_PLAYER_KEY_PREFIX)
    }

    use postbox_media::video::VideoThumbMeta;
    use postbox_platform::{AudioBackend, VideoBackend};
    use std::sync::Arc;

    #[test]
    fn thumb_ready_helpers_are_exposed_to_the_bubble() {
        let thumb = VideoThumbReady {
            path: PathBuf::from("/tmp/t.png"),
            width: 420,
            height: 236,
            duration_ms: 65_000,
            hardware_accelerated: true,
        };
        assert_eq!(thumb.duration_label(), "1:05");
        let _ = VideoThumbMeta {
            width: 420,
            height: 236,
            duration_ms: 65_000,
            hardware_accelerated: true,
        };
        let _ = std::mem::size_of::<Arc<dyn VideoBackend>>();
        let _ = std::mem::size_of::<Arc<dyn AudioBackend>>();
    }
}

#[cfg(all(test, feature = "desktop-test"))]
mod desktop_tests {
    use super::*;
    use crate::viewmodel::PostboxLaunchContext;
    use gpui_kit::base::Root;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowHandle};
    use postbox_core::{
        Account, Chat, ChatKey, FontScale, MediaKind, Message, Provider, ReadStatus, ThemeChoice,
    };
    use postbox_keymap::Keymap;
    use postbox_media::VideoThumbnailCache;
    use postbox_media::drain::media_cache_key;
    use postbox_platform::{MockAudioBackend, MockVideoBackend, MockVideoConfig};
    use postbox_store::{MediaCacheEntry, Store};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const VIDEO_SHA: &str = "aabbccdd11223344";

    fn unique_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(1);
        let dir = std::env::temp_dir().join(format!(
            "postbox-m6-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn video_message(chat: &ChatKey) -> Message {
        Message {
            chat: chat.clone(),
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
            file_size: Some(4_096),
            reactions: vec![],
            read_status: ReadStatus::Read,
            deleted: false,
        }
    }

    struct Harness {
        handle: WindowHandle<Root>,
        shell: gpui_kit::Entity<PostboxShell>,
        chat: ChatKey,
        mock: MockVideoBackend,
        audio: Arc<MockAudioBackend>,
        data_dir: PathBuf,
        video_path: PathBuf,
    }

    fn open_harness(cx: &mut TestAppContext) -> Harness {
        open_harness_with(
            cx,
            MockVideoConfig {
                width: 1280,
                height: 720,
                duration_ms: 60_000,
                ..MockVideoConfig::default()
            },
        )
    }

    fn open_harness_with(cx: &mut TestAppContext, video_config: MockVideoConfig) -> Harness {
        let data_dir = unique_dir("shell");
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
        store.upsert_message(&video_message(&chat.key)).unwrap();
        // A second chat proves that switching chats stops the player and drops its frame.
        store
            .upsert_account(&Account {
                provider: Provider::Slack,
                account_id: "T1".into(),
                label: "Acme Slack".into(),
                state: "connected".into(),
                sort_order: 2,
                last_event_at: None,
            })
            .unwrap();
        store
            .upsert_chat(
                &Chat {
                    key: ChatKey::new(Provider::Slack, "T1", "slack:T1:C-bob"),
                    title: "Bob".into(),
                    is_group: false,
                    archived_local: false,
                    archived_remote: false,
                    pinned: false,
                    unread_count: 0,
                    last_msg_ts: "2025-01-15T11:00:00Z".into(),
                    last_msg_preview: "hi".into(),
                    avatar_hash: None,
                },
                90,
            )
            .unwrap();

        // The video file itself is content-addressed in the Hub media cache, exactly as a real
        // auto-download leaves it (path + sha256 row).
        let video_path = data_dir.join("hub-media").join("clip.mp4");
        std::fs::create_dir_all(video_path.parent().unwrap()).unwrap();
        std::fs::write(&video_path, b"fake-mp4-bytes").unwrap();
        store
            .upsert_media_cache(&MediaCacheEntry {
                cache_key: media_cache_key(&chat.key, "vid1"),
                sha256: VIDEO_SHA.into(),
                rel_path: "clip.mp4".into(),
                mime_type: "video/mp4".into(),
                byte_size: 14,
                last_accessed_at: postbox_media::age::now_unix_secs(),
            })
            .unwrap();

        let mock = MockVideoBackend::with_config(video_config);
        let audio = Arc::new(MockAudioBackend::new());
        let launch_ctx = PostboxLaunchContext {
            store,
            keymap: Keymap::postbox_defaults(),
            version: "0.1.0".into(),
            started_at: std::time::Instant::now(),
            hub_status: crate::sync_bridge::HubConnectionStatus::NotConfigured {
                missing_url: true,
                missing_token: true,
                configured_url: None,
                detail: None,
            },
            sync_bridge: None,
            data_dir: data_dir.clone(),
            giphy_key_available: false,
            deepgram_key_available: false,
            llm_format_key_available: false,
            memory_platform: None,
            video_backend: Some(Arc::new(mock.clone())),
            audio_backend: Some(audio.clone()),
            video_thumb_cache: Some(VideoThumbnailCache::new(data_dir.join("hub-media"))),
            startup_link: None,
            last_exit_record: None,
            os_integration: false,
            release_source: None,
            restore_window: None,
        };

        let keymap = launch_ctx.keymap.clone();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::desktop::register_bundled_fonts(cx);
            crate::desktop::bind_postbox_keys(cx, &keymap);
            crate::desktop::apply_postbox_theme(cx, ThemeChoice::ZincDark, FontScale::default());
        });
        let (handle, shell) = cx.update(|cx| {
            let bounds = gpui_kit::Bounds {
                origin: gpui_kit::Point::default(),
                size: gpui_kit::size(px(1200.0), px(800.0)),
            };
            let (window, shell) = gpui_kit::open_window(
                gpui_kit::WindowOptions {
                    window_bounds: Some(gpui_kit::WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                cx,
                move |window, cx| cx.new(|cx| PostboxShell::new(launch_ctx, window, cx)),
            )
            .expect("open Postbox test window");
            (window.downcast::<Root>().expect("Base Root"), shell)
        });

        // Register the downloaded video path exactly like `SyncEvent::MediaDownloaded` does.
        let chat_key = chat.key.clone();
        let video_for_vm = video_path.clone();
        cx.update_window(handle.into(), |_, window, cx| {
            shell.update(cx, |this, cx| {
                this.vm
                    .m3
                    .media_paths
                    .insert((chat_key.clone(), "vid1".into()), video_for_vm);
                this.sync_after_vm_change(window, cx);
            });
            window.activate_window();
        })
        .unwrap();
        cx.run_until_parked();

        Harness {
            handle,
            shell,
            chat: chat.key.clone(),
            mock,
            audio,
            data_dir,
            video_path,
        }
    }

    fn video_bubble_id_in(window: &mut gpui_kit::Window) -> Option<String> {
        (0..8).find_map(|ix| {
            let id = format!("msg-video-{ix}");
            window.try_find(id.clone()).map(|_| id)
        })
    }

    fn video_bubble_id(cx: &mut TestAppContext, h: &Harness) -> Option<String> {
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            (0..8).find_map(|ix| {
                let id = format!("msg-video-{ix}");
                window.try_find(id.clone()).map(|_| id)
            })
        })
        .unwrap()
    }

    #[gpui_kit::test]
    fn video_thumbnail_is_extracted_once_cached_and_painted_with_badge(cx: &mut TestAppContext) {
        let h = open_harness(cx);
        // The first refresh extracts and caches the thumbnail.
        cx.update(|cx| {
            let state = h
                .shell
                .read(cx)
                .vm
                .video_thumb_state(&h.chat, "vid1")
                .cloned();
            assert!(
                matches!(state, Some(crate::m6::VideoThumbState::Ready(_))),
                "thumbnail must be ready after extraction, got {state:?}"
            );
        });
        assert_eq!(h.mock.extract_count(), 1, "one extraction, never more");
        let thumb_path = cx.update(|cx| {
            match h
                .shell
                .read(cx)
                .vm
                .video_thumb_state(&h.chat, "vid1")
                .cloned()
            {
                Some(crate::m6::VideoThumbState::Ready(thumb)) => Some(thumb.path),
                _ => None,
            }
        });
        let thumb_path = thumb_path.expect("ready thumbnail");
        assert!(thumb_path.is_file(), "cached PNG must exist on disk");
        assert!(
            thumb_path.starts_with(h.data_dir.join("hub-media")),
            "thumbnail lives in the content-addressed media cache"
        );

        // Reopening the chat / re-rendering never re-extracts (disk cache hit).
        let video_path = h.video_path.clone();
        let chat_key = h.chat.clone();
        cx.update_window(h.handle.into(), |_, window, cx| {
            h.shell.update(cx, |this, cx| {
                this.vm
                    .m3
                    .media_paths
                    .insert((chat_key, "vid1".into()), video_path);
                this.sync_after_vm_change(window, cx);
                this.refresh_visible_gpu_textures(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(
            h.mock.extract_count(),
            1,
            "cache hit must skip re-extraction"
        );

        // And the bubble paints the thumbnail with its play badge + duration chip.
        let id = video_bubble_id(cx, &h);
        assert!(id.is_some(), "video bubble must render");
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("video-play-badge").is_some());
            assert!(window.try_find("video-duration-chip").is_some());
        })
        .unwrap();

        let _ = std::fs::remove_dir_all(&h.data_dir);
    }

    #[gpui_kit::test]
    fn double_click_opens_player_and_f11_escape_space_mute_scrub_all_work(cx: &mut TestAppContext) {
        let h = open_harness(cx);
        let id = video_bubble_id(cx, &h).expect("video bubble");
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.double_click(id, cx);
        })
        .unwrap();
        cx.run_until_parked();

        cx.update(|cx| {
            let player = h.shell.read(cx).vm.video_player().cloned();
            assert!(player.is_some(), "double-click must open the inline player");
            let player = player.unwrap();
            assert!(player.playing);
            assert_eq!(player.duration_ms, 60_000);
            assert!(
                player.hardware_accelerated,
                "mock reports Arc hardware decode"
            );
            assert!(player.frame_bytes > 0, "the first frame is presented");
        });
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("video-player-overlay").is_some());
            assert!(window.try_find("video-clock").is_some());
            assert!(window.try_find("video-hw-badge").is_some());
            assert!(window.try_find("video-scrub-seg-0").is_some());
        })
        .unwrap();

        // The `16 ms` presentation timer is armed only while playing.
        cx.update(|cx| assert!(h.shell.read(cx).video_timer_task.is_some()));

        // F11 toggles fullscreen; Esc exits fullscreen first and only closes on the second press.
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.press("f11", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(h.shell.read(cx).vm.video_player().unwrap().fullscreen);
        });
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let player = h.shell.read(cx).vm.video_player().cloned();
            assert!(player.is_some(), "Esc must exit fullscreen before closing");
            assert!(!player.unwrap().fullscreen);
        });
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(h.shell.read(cx).vm.video_player().is_none());
            assert!(h.shell.read(cx).video_runtime.is_none(), "decoder released");
            assert!(h.shell.read(cx).video_timer_task.is_none());
        });

        let _ = std::fs::remove_dir_all(&h.data_dir);
    }

    #[gpui_kit::test]
    fn space_pause_mute_and_scrubber_drive_the_shared_state(cx: &mut TestAppContext) {
        let h = open_harness(cx);
        cx.update_window(h.handle.into(), |_, window, cx| {
            h.shell.update(cx, |this, cx| {
                this.open_video_player_for("vid1", window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();

        // Space pauses, then resumes.
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.press("space", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(!h.shell.read(cx).vm.video_player().unwrap().playing);
            assert!(h.shell.read(cx).video_timer_task.is_none());
        });
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.press("space", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(h.shell.read(cx).vm.video_player().unwrap().playing));

        // The scrubber seeks to a fraction of the clip (last segment ~= 97.5%).
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("video-scrub-seg-19", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let player = h.shell.read(cx).vm.video_player().unwrap();
            assert!(
                player.position_ms >= 55_000,
                "scrubber seek landed at {} ms",
                player.position_ms
            );
        });

        // Mute flips the badge/clock source and pauses the WASAPI render stream.
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click("video-mute", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(h.shell.read(cx).vm.video_player().unwrap().muted);
            let snapshots = h.audio.render_snapshots();
            assert!(!snapshots.is_empty(), "audio render stream was opened");
            assert!(
                snapshots.iter().any(|s| s.paused),
                "muting pauses the stream"
            );
        });

        let _ = std::fs::remove_dir_all(&h.data_dir);
    }

    #[gpui_kit::test]
    fn audio_clock_drives_presentation_and_occlusion_pauses_playback(cx: &mut TestAppContext) {
        let h = open_harness(cx);
        cx.update_window(h.handle.into(), |_, window, cx| {
            h.shell.update(cx, |this, cx| {
                this.open_video_player_for("vid1", window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();

        // Pump a few ticks: PCM reaches the render stream and the audio clock drives the clock.
        for step in 1..=4u64 {
            cx.update_window(h.handle.into(), |_, window, cx| {
                h.shell
                    .update(cx, |this, cx| this.pump_video_player(step * 20, window, cx));
            })
            .unwrap();
            cx.run_until_parked();
        }
        cx.update(|cx| {
            let player = h.shell.read(cx).vm.video_player().unwrap();
            assert!(player.audio_clock_ms.is_some(), "audio clock is reported");
            assert!(
                player.position_ms > 0,
                "presentation follows the audio clock, got {}",
                player.position_ms
            );
            assert!(!h.audio.render_snapshots().is_empty());
        });

        // Minimize/occlude: playback pauses, the timer stops, audio stream pauses.
        let mut visual = gpui_kit::VisualTestContext::from_window(h.handle.into(), cx);
        visual.deactivate_window();
        cx.run_until_parked();
        cx.update(|cx| {
            let player = h.shell.read(cx).vm.video_player().cloned().unwrap();
            assert!(!player.playing, "occluded playback pauses");
            assert!(player.occluded);
            assert!(h.shell.read(cx).video_timer_task.is_none());
            assert!(h.audio.render_snapshots().iter().any(|s| s.paused));
        });

        let _ = std::fs::remove_dir_all(&h.data_dir);
    }

    #[gpui_kit::test]
    fn switching_chats_stops_the_player_and_releases_its_frame(cx: &mut TestAppContext) {
        let h = open_harness(cx);
        cx.update_window(h.handle.into(), |_, window, cx| {
            h.shell.update(cx, |this, cx| {
                this.open_video_player_for("vid1", window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let shell = h.shell.read(cx);
            assert!(shell.vm.video_player_is_open());
            assert!(shell.video_runtime.is_some());
            let player_keys = shell
                .gpu_textures
                .resident_keys()
                .into_iter()
                .filter(|k| k.starts_with(VIDEO_PLAYER_KEY_PREFIX))
                .count();
            assert_eq!(player_keys, 1, "exactly one player frame is resident");
        });

        // Selecting the other (Bob) chat must stop the render stream and drop the player.
        cx.update_window(h.handle.into(), |_, window, cx| {
            h.shell.update(cx, |this, cx| {
                let bob = this
                    .vm
                    .chats()
                    .iter()
                    .find(|c| c.key.provider == Provider::Slack)
                    .map(|c| c.key.clone());
                if let Some(bob) = bob {
                    this.vm.select_chat_key(&bob);
                }
                this.sync_after_vm_change(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let shell = h.shell.read(cx);
            assert!(
                shell.vm.video_player().is_none(),
                "player closed on chat switch"
            );
            assert!(
                shell.video_runtime.is_none(),
                "decoder + render stream released"
            );
            assert!(
                shell
                    .gpu_textures
                    .resident_keys()
                    .iter()
                    .all(|k| !k.starts_with(VIDEO_PLAYER_KEY_PREFIX)),
                "no player frame survives the chat switch"
            );
            assert!(h.audio.render_snapshots().iter().any(|s| s.stopped));
        });

        let _ = std::fs::remove_dir_all(&h.data_dir);
    }

    #[gpui_kit::test]
    fn failing_decoder_marks_the_thumbnail_failed_and_never_opens_a_player(
        cx: &mut TestAppContext,
    ) {
        let h = open_harness_with(
            cx,
            MockVideoConfig {
                fail_extract: true,
                ..MockVideoConfig::default()
            },
        );
        cx.update(|cx| {
            let shell = h.shell.read(cx);
            assert!(
                matches!(
                    shell.vm.video_thumb_state(&h.chat, "vid1"),
                    Some(crate::m6::VideoThumbState::Failed { .. })
                ),
                "a failing decoder must not leave the bubble stuck on Extracting"
            );
            assert_eq!(h.mock.extract_count(), 1);
            // Opening the player for a video that cannot decode does nothing at all.
            assert!(shell.vm.video_player().is_none());
        });
        cx.update_window(h.handle.into(), |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("video-play-badge").is_none(),
                "no play badge without a thumbnail"
            );
            // The bubble falls back to the ordinary attachment pill, which still lets the user
            // retry the download by clicking it.
            assert!(window.try_find("msg-attach-1").is_some());
            assert!(video_bubble_id_in(window).is_none());
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                h.shell.read(cx).vm.video_player().is_none(),
                "failed video never opens a player"
            );
        });
        let _ = std::fs::remove_dir_all(&h.data_dir);
    }
}
