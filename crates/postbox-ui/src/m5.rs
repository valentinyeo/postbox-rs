//! Milestone 5 headless state & controller for Feature #32:
//! Deepgram Nova-3 live streaming dictation, anchored interim-span replacement,
//! readback tamper guard (`live_detached`), origin `ChatKey` draft isolation,
//! `CloseStream` final single replacement, `Ctrl+Enter` finish+auto-send, `Esc` cancel+restore,
//! and 8-bar `micPill` waveform history.

use crate::sync_bridge::SyncCommand;
use crate::viewmodel::{
    FocusTarget, PostboxViewModel, now_epoch_millis_public, now_epoch_secs_public,
};
use postbox_core::dictation::{
    DictationLanguage, MIC_PILL_BAR_COUNT, MIC_PILL_TICK_MS, PrepareDictationError,
    dictation_padding, prepare_dictation,
};
use postbox_core::drafts::MAX_DRAFT_TEXT_BYTES;
use postbox_core::{ChatKey, Draft, ViewTab};
use std::ops::Range;
use std::path::PathBuf;

pub const DICTATION_METER_BARS: usize = MIC_PILL_BAR_COUNT;
pub const DICTATION_METER_TICK_MS: u64 = MIC_PILL_TICK_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictationPhase {
    #[default]
    Idle,
    Starting,
    Listening,
    Draining,
}

impl DictationPhase {
    pub const fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationContextSnapshot {
    pub origin_chat: ChatKey,
    pub in_thread: bool,
    pub thread_root_msg_id: Option<String>,
    pub active_view: Option<ViewTab>,
    pub show_archived_only: bool,
    pub show_archived_inline: bool,
    pub reply_to_msg_id: Option<String>,
    pub staged_attachment_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDictationUiState {
    pub generation: u64,
    pub origin_chat: ChatKey,
    pub in_thread: bool,
    pub context: DictationContextSnapshot,
    /// Original composer text captured at `Ctrl+D` start (used for `Esc` cancel+restore & interim baseline).
    pub pre_text: String,
    /// Original UTF-8 byte selection range captured at `Ctrl+D` start.
    pub pre_selection: Range<usize>,
    /// Current UTF-8 byte range in the composer occupied by the live dictated span (including padding).
    pub live_anchor: Range<usize>,
    /// Exact full composer text expected on readback after our latest programmatic update.
    /// `None` before the first non-empty interim is written (readback compared against `pre_text`).
    pub live_snapshot: Option<String>,
    /// Last applied monotonic live transcript revision from `postbox-audio`.
    pub live_revision: u64,
    /// Set true if user manually edited the composer or draft overflowed -> never clobber user text.
    pub live_detached: bool,
    /// Armed by `Ctrl+Enter`; disarmed immediately on any context/chat/draft/selection drift.
    pub auto_send_armed: bool,
    /// Snapshot captured when `Ctrl+Enter` armed auto-send.
    pub armed_text: Option<String>,
    pub armed_selection: Option<Range<usize>>,
    /// Wall-clock start (epoch ms) and rendered elapsed ms for `m:ss` clock.
    pub started_at_ms: u64,
    pub elapsed_ms: u64,
    /// 8-bar scrolling peak/RMS waveform history (`0..=255`), shifted left every 120 ms.
    pub meter_bars: [u8; DICTATION_METER_BARS],
    /// Latest instantaneous audio level (`0..=255`) from `SyncEvent::DictationLevel`.
    pub latest_level: u8,
    pub phase: DictationPhase,
}

#[derive(Debug, Default)]
pub struct M5State {
    pub active: Option<LiveDictationUiState>,
    pub generation_seq: u64,
    pub composer_selection: Range<usize>,
    pub composer_selection_set: bool,
    pub thread_selection: Range<usize>,
    pub thread_selection_set: bool,
    pub pending_composer_selection: Option<Range<usize>>,
    pub pending_thread_selection: Option<Range<usize>>,
}

fn clamp_selection_utf8(text: &str, range: Range<usize>) -> Range<usize> {
    let mut start = range.start.min(text.len());
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = range.end.max(start).min(text.len());
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    start..end
}

pub use postbox_core::dictation::dictation_padding as ui_dictation_padding;

impl PostboxViewModel {
    pub fn is_dictation_active(&self) -> bool {
        self.m5.active.as_ref().is_some_and(|s| s.phase.is_active())
    }

    pub fn live_dictation(&self) -> Option<&LiveDictationUiState> {
        self.m5.active.as_ref()
    }

    pub fn set_dictation_language(&mut self, lang: DictationLanguage) {
        self.dictation_language = lang;
        let _ = self.store.save_dictation_language(lang);
        self.emit_sync_command(SyncCommand::SaveDictationLanguage(lang));
        self.status_notice = Some(lang.status_text().to_owned());
        if self.palette.open {
            self.rebuild_palette_entries();
        }
    }

    pub fn set_composer_selection(&mut self, range: Range<usize>) {
        self.m5.composer_selection = clamp_selection_utf8(&self.composer_text, range);
        self.m5.composer_selection_set = true;
    }

    pub fn set_thread_selection(&mut self, range: Range<usize>) {
        let clamped = self
            .thread_panel
            .as_ref()
            .map(|p| clamp_selection_utf8(&p.composer_text, range.clone()))
            .unwrap_or(0..0);
        self.m5.thread_selection = clamped;
        self.m5.thread_selection_set = true;
    }

    pub fn composer_selection(&self) -> Range<usize> {
        if self.m5.composer_selection_set {
            clamp_selection_utf8(&self.composer_text, self.m5.composer_selection.clone())
        } else {
            let len = self.composer_text.len();
            len..len
        }
    }

    pub fn thread_selection(&self) -> Range<usize> {
        let text = self
            .thread_panel
            .as_ref()
            .map(|p| p.composer_text.as_str())
            .unwrap_or("");
        if self.m5.thread_selection_set {
            clamp_selection_utf8(text, self.m5.thread_selection.clone())
        } else {
            let len = text.len();
            len..len
        }
    }

    pub fn take_pending_composer_selection(&mut self) -> Option<Range<usize>> {
        self.m5.pending_composer_selection.take()
    }

    pub fn take_pending_thread_selection(&mut self) -> Option<Range<usize>> {
        self.m5.pending_thread_selection.take()
    }

    fn capture_dictation_context(
        &self,
        origin_chat: ChatKey,
        in_thread: bool,
    ) -> DictationContextSnapshot {
        DictationContextSnapshot {
            origin_chat,
            in_thread,
            thread_root_msg_id: self.thread_panel.as_ref().map(|p| p.root_msg_id.clone()),
            active_view: self.active_view().cloned(),
            show_archived_only: self.show_archived_only(),
            show_archived_inline: self.show_archived_inline(),
            reply_to_msg_id: self.reply_to_msg_id.clone(),
            staged_attachment_paths: self
                .m3
                .staged_attachments
                .items()
                .iter()
                .map(|i| i.path.clone())
                .collect(),
        }
    }

    fn same_dictation_context(&self, expected: &DictationContextSnapshot) -> bool {
        let Some(selected) = self.selected_chat() else {
            return false;
        };
        if selected.key != expected.origin_chat {
            return false;
        }
        if expected.in_thread {
            let Some(panel) = self.thread_panel.as_ref() else {
                return false;
            };
            if panel.chat != expected.origin_chat
                || Some(panel.root_msg_id.as_str()) != expected.thread_root_msg_id.as_deref()
            {
                return false;
            }
        } else if self.thread_panel.as_ref().map(|p| p.root_msg_id.as_str())
            != expected.thread_root_msg_id.as_deref()
        {
            return false;
        }
        if self.active_view() != expected.active_view.as_ref()
            || self.show_archived_only() != expected.show_archived_only
            || self.show_archived_inline() != expected.show_archived_inline
            || self.reply_to_msg_id != expected.reply_to_msg_id
        {
            return false;
        }
        let current_paths: Vec<&PathBuf> = self
            .m3
            .staged_attachments
            .items()
            .iter()
            .map(|i| &i.path)
            .collect();
        if current_paths.len() != expected.staged_attachment_paths.len() {
            return false;
        }
        current_paths
            .iter()
            .zip(expected.staged_attachment_paths.iter())
            .all(|(a, b)| *a == b)
    }

    fn save_origin_chat_draft(&mut self, origin_chat: &ChatKey, text: &str) {
        let existing_staged = self
            .drafts
            .get(origin_chat)
            .and_then(|d| d.staged_image_path.clone());
        let existing_reply = if self.selected_chat().is_some_and(|c| &c.key == origin_chat) {
            self.reply_to_msg_id.clone()
        } else {
            self.drafts
                .get(origin_chat)
                .and_then(|d| d.reply_to_id.clone())
        };
        if text.is_empty() && existing_staged.is_none() {
            self.drafts.remove(origin_chat);
            let _ = self.store.delete_draft(origin_chat);
        } else {
            let draft = Draft {
                chat: origin_chat.clone(),
                text_body: text.to_owned(),
                reply_to_id: existing_reply,
                staged_image_path: existing_staged,
                updated_at: now_epoch_secs_public(),
            };
            if self.store.save_draft(&draft).is_ok() {
                self.drafts.insert(origin_chat.clone(), draft);
            }
        }
    }

    /// `Ctrl+D`: toggles live Deepgram Nova-3 streaming dictation.
    pub fn toggle_dictation(&mut self, window_active: bool) -> bool {
        if !window_active {
            return false;
        }
        if let Some(active) = self.m5.active.as_ref() {
            return match active.phase {
                DictationPhase::Starting | DictationPhase::Listening => self.stop_dictation(),
                DictationPhase::Draining => {
                    self.status_notice = Some("Transcription is still running".to_owned());
                    true
                }
                DictationPhase::Idle => false,
            };
        }
        if self.m4.recording.is_active() {
            self.status_notice = Some("Finish or cancel voice recording first".to_owned());
            return false;
        }
        let Some(chat) = self.selected_chat().cloned() else {
            self.status_notice = Some("Select a chat first".to_owned());
            return false;
        };
        if !self.m4.deepgram_key_available {
            self.status_notice = Some("Set DEEPGRAM_API_KEY to enable dictation".to_owned());
            return false;
        }

        // Stop active voice playback so mic never captures speaker audio.
        self.m4.auto_advance_enabled = false;
        self.m4.pending_auto_advance = None;
        if self.m4.playback.is_some() {
            self.stop_voice_playback();
        }

        let in_thread = self.focus == FocusTarget::ThreadComposer && self.thread_panel.is_some();
        if !in_thread {
            self.focus = FocusTarget::Composer;
        }

        let (pre_text, pre_selection) = if in_thread {
            let text = self
                .thread_panel
                .as_ref()
                .map(|p| p.composer_text.clone())
                .unwrap_or_default();
            let sel = self.thread_selection();
            (text, sel)
        } else {
            let text = self.composer_text.clone();
            let sel = self.composer_selection();
            (text, sel)
        };

        let context = self.capture_dictation_context(chat.key.clone(), in_thread);
        self.m5.generation_seq = self.m5.generation_seq.wrapping_add(1).max(1);
        let generation = self.m5.generation_seq;
        let now_ms = now_epoch_millis_public().max(0) as u64;

        self.m5.active = Some(LiveDictationUiState {
            generation,
            origin_chat: chat.key.clone(),
            in_thread,
            context,
            pre_text,
            pre_selection: pre_selection.clone(),
            live_anchor: pre_selection,
            live_snapshot: None,
            live_revision: 0,
            live_detached: false,
            auto_send_armed: false,
            armed_text: None,
            armed_selection: None,
            started_at_ms: now_ms,
            elapsed_ms: 0,
            meter_bars: [0u8; DICTATION_METER_BARS],
            latest_level: 0,
            phase: DictationPhase::Starting,
        });

        self.emit_sync_command(SyncCommand::DictationStart {
            generation,
            origin_chat: chat.key,
            language: self.dictation_language,
            preferred_mic: self.m4.selected_microphone.clone(),
        });
        self.status_notice = Some("Listening... press Ctrl+D to stop".to_owned());
        true
    }

    pub fn stop_dictation(&mut self) -> bool {
        let Some(state) = self.m5.active.as_mut() else {
            return false;
        };
        if matches!(
            state.phase,
            DictationPhase::Starting | DictationPhase::Listening
        ) {
            state.phase = DictationPhase::Draining;
            state.latest_level = 0;
            let generation = state.generation;
            self.emit_sync_command(SyncCommand::DictationStop { generation });
            self.status_notice = Some("Transcribing...".to_owned());
            return true;
        }
        false
    }

    /// `Ctrl+Enter` while dictating: arms auto-send if context/chat/draft are intact,
    /// then stops recording and drains `CloseStream`.
    pub fn finish_dictation_and_auto_send(&mut self) -> bool {
        self.observe_dictation_context();
        let Some(state) = self.m5.active.as_ref() else {
            return false;
        };
        if state.auto_send_armed {
            return true;
        }

        let in_thread = state.in_thread;
        let live_detached = state.live_detached;
        let same_ctx = self.same_dictation_context(&state.context);
        let expected = state
            .live_snapshot
            .clone()
            .unwrap_or_else(|| state.pre_text.clone());
        let current_text = if in_thread {
            self.thread_panel
                .as_ref()
                .map(|p| p.composer_text.clone())
                .unwrap_or_default()
        } else {
            self.composer_text.clone()
        };
        let current_sel = if in_thread {
            self.thread_selection()
        } else {
            self.composer_selection()
        };

        let mut should_emit_stop = false;
        let mut generation = 0u64;
        let mut detached_now = false;

        if let Some(state) = self.m5.active.as_mut() {
            generation = state.generation;
            if !live_detached && same_ctx {
                if current_text != expected {
                    state.live_detached = true;
                    state.auto_send_armed = false;
                    state.armed_text = None;
                    state.armed_selection = None;
                    detached_now = true;
                } else {
                    state.auto_send_armed = true;
                    state.armed_text = Some(current_text);
                    state.armed_selection = Some(current_sel);
                }
            }
            if state.phase != DictationPhase::Draining {
                state.phase = DictationPhase::Draining;
                state.latest_level = 0;
                should_emit_stop = true;
            }
        }

        if should_emit_stop {
            self.emit_sync_command(SyncCommand::DictationStop { generation });
        }
        self.status_notice = Some(if detached_now {
            "Dictation stopped; draft edited".to_owned()
        } else {
            "Transcribing...".to_owned()
        });
        true
    }

    /// `Esc` while dictating (after all modal overlays are dismissed): cancels the session
    /// and restores the exact pre-dictation draft + UTF-8 selection on `origin_chat`.
    pub fn cancel_dictation(&mut self) -> bool {
        let Some(state) = self.m5.active.take() else {
            return false;
        };
        self.emit_sync_command(SyncCommand::DictationCancel {
            generation: state.generation,
        });

        if !state.live_detached {
            if state.in_thread {
                if let Some(panel) = self.thread_panel.as_mut()
                    && panel.chat == state.origin_chat
                    && Some(panel.root_msg_id.as_str())
                        == state.context.thread_root_msg_id.as_deref()
                {
                    panel.composer_text = state.pre_text.clone();
                    self.m5.thread_selection = state.pre_selection.clone();
                    self.m5.thread_selection_set = true;
                    self.m5.pending_thread_selection = Some(state.pre_selection);
                }
            } else {
                self.save_origin_chat_draft(&state.origin_chat, &state.pre_text);
                if self
                    .selected_chat()
                    .is_some_and(|c| c.key == state.origin_chat)
                {
                    self.composer_text = state.pre_text.clone();
                    self.m5.composer_selection = state.pre_selection.clone();
                    self.m5.composer_selection_set = true;
                    self.m5.pending_composer_selection = Some(state.pre_selection);
                }
            }
        }

        self.status_notice = Some("Dictation cancelled".to_owned());
        true
    }

    /// Advances the 8-bar scrolling `micPill` waveform and `m:ss` elapsed timer (called on 120 ms tick
    /// ONLY while dictation is active and the window is active -> `0.0%` idle CPU/GPU).
    pub fn tick_dictation_meter(&mut self, now_ms: u64) -> bool {
        let Some(state) = self.m5.active.as_mut() else {
            return false;
        };
        state.elapsed_ms = now_ms.saturating_sub(state.started_at_ms);
        state.meter_bars.copy_within(1..DICTATION_METER_BARS, 0);
        state.meter_bars[DICTATION_METER_BARS - 1] = match state.phase {
            DictationPhase::Starting | DictationPhase::Listening => state.latest_level,
            DictationPhase::Draining | DictationPhase::Idle => 0,
        };
        true
    }

    /// Called whenever the user edits composer or thread composer text; compares against
    /// `live_snapshot` (or `pre_text`) and detaches cleanly without clobbering user text.
    pub(crate) fn on_composer_user_edit(&mut self, in_thread: bool, new_text: &str) {
        let mut emit_stop_gen = None;
        if let Some(state) = self.m5.active.as_mut()
            && state.in_thread == in_thread
            && !state.live_detached
        {
            let expected = state
                .live_snapshot
                .as_deref()
                .unwrap_or(state.pre_text.as_str());
            if new_text != expected {
                state.live_detached = true;
                state.auto_send_armed = false;
                state.armed_text = None;
                state.armed_selection = None;
                if matches!(
                    state.phase,
                    DictationPhase::Starting | DictationPhase::Listening
                ) {
                    state.phase = DictationPhase::Draining;
                    state.latest_level = 0;
                    emit_stop_gen = Some(state.generation);
                }
                self.status_notice = Some("Dictation stopped; draft edited".to_owned());
            }
        }
        if let Some(generation) = emit_stop_gen {
            self.emit_sync_command(SyncCommand::DictationStop { generation });
        }
    }

    /// Observes chat/view/reply/thread/attachment context transitions and disarms auto-send
    /// immediately on any drift.
    pub(crate) fn observe_dictation_context(&mut self) {
        let Some(ctx) = self.m5.active.as_ref().map(|s| s.context.clone()) else {
            return;
        };
        if !self.same_dictation_context(&ctx)
            && let Some(state) = self.m5.active.as_mut()
        {
            state.auto_send_armed = false;
            state.armed_text = None;
            state.armed_selection = None;
        }
    }

    pub(crate) fn handle_dictation_connected(&mut self, generation: u64) {
        if let Some(state) = self.m5.active.as_mut()
            && state.generation == generation
            && state.phase == DictationPhase::Starting
        {
            state.phase = DictationPhase::Listening;
        }
    }

    pub(crate) fn handle_dictation_level(&mut self, generation: u64, level: u8) {
        if let Some(state) = self.m5.active.as_mut()
            && state.generation == generation
            && matches!(
                state.phase,
                DictationPhase::Starting | DictationPhase::Listening
            )
        {
            state.latest_level = level;
        }
    }

    pub(crate) fn handle_dictation_draining(&mut self, generation: u64) {
        if let Some(state) = self.m5.active.as_mut()
            && state.generation == generation
            && state.phase != DictationPhase::Draining
        {
            state.phase = DictationPhase::Draining;
            state.latest_level = 0;
            self.status_notice = Some("Transcribing...".to_owned());
        }
    }

    pub(crate) fn handle_dictation_interim(
        &mut self,
        generation: u64,
        revision: u64,
        text: String,
    ) {
        let (in_thread, origin_chat, pre_text, pre_selection, expected, context) = {
            let Some(state) = self.m5.active.as_ref() else {
                return;
            };
            if state.generation != generation
                || revision <= state.live_revision
                || state.phase != DictationPhase::Listening
                || state.live_detached
                || state.auto_send_armed
            {
                return;
            }
            (
                state.in_thread,
                state.origin_chat.clone(),
                state.pre_text.clone(),
                state.pre_selection.clone(),
                state
                    .live_snapshot
                    .clone()
                    .unwrap_or_else(|| state.pre_text.clone()),
                state.context.clone(),
            )
        };

        // Never leak interim text into a switched chat or changed context!
        if !self.same_dictation_context(&context) {
            return;
        }

        // Readback tamper check against expected live_snapshot / pre_text.
        let current = if in_thread {
            self.thread_panel
                .as_ref()
                .map(|p| p.composer_text.clone())
                .unwrap_or_default()
        } else {
            self.composer_text.clone()
        };
        if current != expected {
            if let Some(state) = self.m5.active.as_mut() {
                state.live_detached = true;
                state.auto_send_armed = false;
                state.armed_text = None;
                state.armed_selection = None;
                state.phase = DictationPhase::Draining;
                state.latest_level = 0;
            }
            self.emit_sync_command(SyncCommand::DictationStop { generation });
            self.status_notice = Some("Dictation stopped; draft edited".to_owned());
            return;
        }

        // Shrinking / empty interim restores pre_text + pre_selection cleanly.
        if text.chars().all(char::is_whitespace) {
            if in_thread {
                if let Some(panel) = self.thread_panel.as_mut() {
                    panel.composer_text = pre_text;
                }
                self.m5.thread_selection = pre_selection.clone();
                self.m5.thread_selection_set = true;
                self.m5.pending_thread_selection = Some(pre_selection.clone());
            } else {
                self.composer_text = pre_text.clone();
                self.save_origin_chat_draft(&origin_chat, &pre_text);
                self.m5.composer_selection = pre_selection.clone();
                self.m5.composer_selection_set = true;
                self.m5.pending_composer_selection = Some(pre_selection.clone());
            }
            if let Some(state) = self.m5.active.as_mut() {
                state.live_snapshot = None;
                state.live_anchor = pre_selection;
                state.live_revision = revision;
            }
            return;
        }

        match prepare_dictation(&pre_text, pre_selection.start, pre_selection.end, &text) {
            Ok(prepared) => {
                let caret = prepared.new_end;
                let new_anchor = pre_selection.start..caret;
                if in_thread {
                    if let Some(panel) = self.thread_panel.as_mut() {
                        panel.composer_text = prepared.whole.clone();
                    }
                    self.m5.thread_selection = caret..caret;
                    self.m5.thread_selection_set = true;
                    self.m5.pending_thread_selection = Some(caret..caret);
                } else {
                    self.composer_text = prepared.whole.clone();
                    self.save_origin_chat_draft(&origin_chat, &prepared.whole);
                    self.m5.composer_selection = caret..caret;
                    self.m5.composer_selection_set = true;
                    self.m5.pending_composer_selection = Some(caret..caret);
                }
                if let Some(state) = self.m5.active.as_mut() {
                    state.live_snapshot = Some(prepared.whole);
                    state.live_anchor = new_anchor;
                    state.live_revision = revision;
                }
            }
            Err(PrepareDictationError::DraftTooLong) => {
                if let Some(state) = self.m5.active.as_mut() {
                    state.live_detached = true;
                    state.auto_send_armed = false;
                    state.armed_text = None;
                    state.armed_selection = None;
                    state.phase = DictationPhase::Draining;
                    state.latest_level = 0;
                }
                self.emit_sync_command(SyncCommand::DictationStop { generation });
                self.status_notice = Some("Dictation needs a shorter draft".to_owned());
            }
            Err(_) => {}
        }
    }

    pub(crate) fn handle_dictation_final(&mut self, generation: u64, text: String) {
        if !self
            .m5
            .active
            .as_ref()
            .is_some_and(|s| s.generation == generation)
        {
            return;
        }
        let state = self.m5.active.take().unwrap();

        if state.live_detached {
            self.status_notice = Some("Dictation stopped; draft edited".to_owned());
            return;
        }

        let same_chat = self
            .selected_chat()
            .is_some_and(|c| c.key == state.origin_chat);
        let same_ctx = self.same_dictation_context(&state.context);

        let base = if state.in_thread && same_ctx {
            self.thread_panel
                .as_ref()
                .map(|p| p.composer_text.clone())
                .unwrap_or_default()
        } else if !state.in_thread && same_chat {
            self.composer_text.clone()
        } else {
            self.drafts
                .get(&state.origin_chat)
                .map(|d| d.text_body.clone())
                .unwrap_or_default()
        };

        let expected = state
            .live_snapshot
            .as_deref()
            .unwrap_or(state.pre_text.as_str());
        if base != expected {
            self.status_notice = Some("Dictation stopped; draft edited".to_owned());
            return;
        }

        // Replace ONLY [live_anchor.start .. live_anchor.end] ONCE so interim text is never duplicated!
        let anchor = clamp_selection_utf8(&base, state.live_anchor.clone());
        let prepared = match prepare_dictation(&base, anchor.start, anchor.end, &text) {
            Ok(p) => p,
            Err(PrepareDictationError::DraftTooLong) => {
                self.status_notice = Some("Dictation needs a shorter draft".to_owned());
                return;
            }
            Err(PrepareDictationError::EmptyTranscript) => {
                if !state.in_thread {
                    self.save_origin_chat_draft(&state.origin_chat, &state.pre_text);
                    if same_chat {
                        self.composer_text = state.pre_text;
                        self.m5.composer_selection = state.pre_selection.clone();
                        self.m5.composer_selection_set = true;
                        self.m5.pending_composer_selection = Some(state.pre_selection);
                    }
                } else if same_ctx && let Some(panel) = self.thread_panel.as_mut() {
                    panel.composer_text = state.pre_text;
                    self.m5.thread_selection = state.pre_selection.clone();
                    self.m5.thread_selection_set = true;
                    self.m5.pending_thread_selection = Some(state.pre_selection);
                }
                self.status_notice = Some("No speech detected".to_owned());
                return;
            }
            Err(_) => {
                self.status_notice = Some("Dictation could not be inserted".to_owned());
                return;
            }
        };

        let caret = prepared.new_end;
        if state.in_thread {
            if same_ctx && let Some(panel) = self.thread_panel.as_mut() {
                panel.composer_text = prepared.whole.clone();
                self.m5.thread_selection = caret..caret;
                self.m5.thread_selection_set = true;
                self.m5.pending_thread_selection = Some(caret..caret);
            }
        } else {
            self.save_origin_chat_draft(&state.origin_chat, &prepared.whole);
            if same_chat {
                self.composer_text = prepared.whole.clone();
                self.m5.composer_selection = caret..caret;
                self.m5.composer_selection_set = true;
                self.m5.pending_composer_selection = Some(caret..caret);
            }
        }

        // Ctrl+Enter auto-send fires ONLY when context & draft stayed completely intact.
        let can_auto_send = state.auto_send_armed
            && same_ctx
            && state.armed_text.as_deref() == Some(base.as_str())
            && prepared.whole.len() <= MAX_DRAFT_TEXT_BYTES;

        if can_auto_send {
            let sent = if state.in_thread {
                self.submit_thread_reply()
            } else {
                self.submit_composer_message()
            };
            if !sent {
                self.status_notice = Some("Dictation saved; send manually".to_owned());
            }
        } else {
            self.status_notice = Some(if !same_ctx || state.auto_send_armed {
                "Dictation saved; send manually".to_owned()
            } else {
                "Dictation inserted".to_owned()
            });
        }
    }

    /// Matches Zig `main.zig:12249-12259`: releases dictation operation without clobbering
    /// composer text and surfaces the failure status notice.
    pub(crate) fn handle_dictation_failed(&mut self, generation: u64, message: String) {
        if self
            .m5
            .active
            .as_ref()
            .is_some_and(|s| s.generation == generation)
        {
            self.m5.active = None;
            self.status_notice = Some(message);
        }
    }
}

pub fn compute_dictation_padding(text: &str, start: usize, end: usize) -> (bool, bool) {
    let p = dictation_padding(text, start, end);
    (p.before, p.after)
}
