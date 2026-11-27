//! Milestone 4 view-model state and controller methods for `PostboxViewModel`:
//! - #27 Ogg Opus playback, WSOLA `1x`/`1.5x`/`2x`, played set (2048), resume positions (64, 500 ms end clear), auto-advance, `Ctrl+P`
//! - #28 Voice note recording (`Ctrl+Shift+M`), min/max bounds, durable outbox enqueue (`ptt: true`, `voice.ogg`), queue-full retention, `voice_sent` FIFO echo matching
//! - #29 Deepgram Nova-3 REST cached transcription (immutable SQLite + `.raw.txt`/`.txt` sidecars), 1 retry per chat open, `Ctrl+T` toggle, `Copy transcript` / `Copy all transcripts` (raw oldest-first `\r\n\r\n`, legacy-formatted guard), optional ADHD formatting

use crate::sync_bridge::SyncCommand;
use crate::viewmodel::{
    FocusTarget, PostboxViewModel, format_utc_rfc3339_public, new_client_id_public,
    now_epoch_millis_public, now_epoch_secs_public,
};
use postbox_audio::{
    FormatRequests, MarkedVoiceExportInput, WAVEFORM_BARS, export_marked_raw_transcripts,
    load_cached_transcript, parse_ogg_opus, write_transcript_sidecars,
};
use postbox_core::archive::{ArchiveStatus, PendingArchiveIntent, unix_seconds};
use postbox_core::dictation::DictationLanguage;
use postbox_core::outbox::{OutboxRecord, sha256_bytes};
use postbox_core::threads::channel_message_visible;
use postbox_core::voice::{
    AudioDeviceInfo, PlaybackSpeed, TranscriptError, TranscriptRecord, TranscriptStatus,
    VOICE_POS_END_CLEAR_MS, VoicePlaybackState, VoicePlaybackStatus, VoiceRecordState,
    format_voice_clock,
};
use postbox_core::{ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_media::drain::{MediaItemStatus, media_cache_key};
use postbox_store::{StoreError, gone_media_pair_key};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Consolidated Milestone 4 audio, recording, and transcription UI state owned by `PostboxViewModel`.
#[derive(Debug, Default)]
pub struct M4State {
    pub playback: Option<VoicePlaybackState>,
    pub playback_speed: PlaybackSpeed,
    pub played_keys: HashSet<String>,
    pub resume_positions: HashMap<String, (u64, u64)>,
    pub pending_auto_advance: Option<(ChatKey, String)>,
    pub auto_advance_enabled: bool,
    pub media_sha256: HashMap<(ChatKey, String), String>,
    pub waveforms: HashMap<(ChatKey, String), Vec<u8>>,
    pub transcripts: HashMap<(ChatKey, String), TranscriptStatus>,
    pub transcript_expanded: HashSet<(ChatKey, String)>,
    pub transcript_attempted_this_open: HashSet<(ChatKey, String)>,
    pub format_requests: FormatRequests,
    pub recording: VoiceRecordState,
    pub active_record_generation: u64,
    pub recording_finishing: bool,
    pub recording_reply_to: Option<String>,
    pub voice_sent_fifo: postbox_audio::VoiceSentFifo,
    pub microphones: Vec<AudioDeviceInfo>,
    pub selected_microphone: Option<String>,
    pub deepgram_key_available: bool,
    pub llm_format_key_available: bool,
    pub pending_clipboard_text: Option<String>,
    pub marked_anchor_idx: Option<usize>,
    pub voice_file_seq: u64,
}

fn hex_encode_32(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

fn default_waveform() -> Vec<u8> {
    (0..WAVEFORM_BARS)
        .map(|i| {
            let phase = (i as f32 / WAVEFORM_BARS as f32) * std::f32::consts::TAU * 2.0;
            (32.0 + phase.sin().abs() * 52.0) as u8
        })
        .collect()
}

impl PostboxViewModel {
    pub fn with_deepgram_key_available(mut self, available: bool) -> Self {
        self.m4.deepgram_key_available = available;
        self
    }

    pub fn with_llm_format_key_available(mut self, available: bool) -> Self {
        self.m4.llm_format_key_available = available;
        self
    }

    pub fn set_deepgram_key_available(&mut self, available: bool) {
        self.m4.deepgram_key_available = available;
    }

    pub fn set_llm_format_key_available(&mut self, available: bool) {
        self.m4.llm_format_key_available = available;
    }

    pub fn deepgram_key_available(&self) -> bool {
        self.m4.deepgram_key_available
    }

    pub fn llm_format_key_available(&self) -> bool {
        self.m4.llm_format_key_available
    }

    pub fn take_pending_clipboard_text(&mut self) -> Option<String> {
        self.m4.pending_clipboard_text.take()
    }

    pub fn voice_playback(&self) -> Option<&VoicePlaybackState> {
        self.m4.playback.as_ref().filter(|state| state.is_active())
    }

    pub fn is_voice_playing(&self) -> bool {
        self.m4
            .playback
            .as_ref()
            .is_some_and(VoicePlaybackState::is_playing)
    }

    pub fn playback_speed(&self) -> PlaybackSpeed {
        self.m4.playback_speed
    }

    pub fn voice_record_state(&self) -> &VoiceRecordState {
        &self.m4.recording
    }

    pub fn is_voice_recording(&self) -> bool {
        self.m4.recording.is_recording()
    }

    pub fn is_voice_retained(&self) -> bool {
        self.m4.recording.is_retained()
    }

    pub fn active_record_generation(&self) -> u64 {
        self.m4.active_record_generation
    }

    pub fn microphones(&self) -> &[AudioDeviceInfo] {
        &self.m4.microphones
    }

    pub fn selected_microphone(&self) -> Option<&str> {
        self.m4.selected_microphone.as_deref()
    }

    pub fn set_microphones(&mut self, devices: Vec<AudioDeviceInfo>) {
        self.m4.microphones = devices;
        if self.palette.open {
            self.rebuild_palette_entries();
        }
    }

    pub fn select_microphone(&mut self, name: Option<String>) {
        let normalized = name.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        self.m4.selected_microphone = normalized.clone();
        let _ = self.store.save_microphone_preference(normalized.as_deref());
        self.status_notice = Some(if normalized.is_none() {
            "Microphone: System default".to_owned()
        } else {
            "Microphone changed".to_owned()
        });
        if self.palette.open {
            self.rebuild_palette_entries();
        }
    }

    pub fn set_playback_speed(&mut self, speed: PlaybackSpeed) {
        self.m4.playback_speed = speed;
        self.playback_speed_pct = speed.percent();
        let _ = self.store.save_playback_speed(speed);
        if let Some(pb) = self.m4.playback.as_mut() {
            pb.speed = speed;
            self.emit_sync_command(SyncCommand::VoiceSetSpeed(speed));
        }
        self.status_notice = Some(format!("Playback speed {}", speed.label()));
        if self.palette.open {
            self.rebuild_palette_entries();
        }
    }

    pub fn cycle_playback_speed(&mut self) {
        let next = self.m4.playback_speed.cycle_next();
        self.set_playback_speed(next);
    }

    pub fn is_voice_played(&self, chat: &ChatKey, msg_id: &str) -> bool {
        let key = gone_media_pair_key(chat, msg_id);
        self.m4.played_keys.contains(&key)
    }

    pub fn voice_resume_position(&self, chat: &ChatKey, msg_id: &str) -> Option<(u64, u64)> {
        let key = gone_media_pair_key(chat, msg_id);
        self.m4.resume_positions.get(&key).copied()
    }

    pub fn voice_waveform(&self, chat: &ChatKey, msg_id: &str) -> Option<&[u8]> {
        self.m4
            .waveforms
            .get(&(chat.clone(), msg_id.to_owned()))
            .map(Vec::as_slice)
    }

    pub fn transcript_status(&self, chat: &ChatKey, msg_id: &str) -> &TranscriptStatus {
        static NONE: TranscriptStatus = TranscriptStatus::None;
        self.m4
            .transcripts
            .get(&(chat.clone(), msg_id.to_owned()))
            .unwrap_or(&NONE)
    }

    pub fn is_transcript_expanded(&self, chat: &ChatKey, msg_id: &str) -> bool {
        self.m4
            .transcript_expanded
            .contains(&(chat.clone(), msg_id.to_owned()))
    }

    /// Returns how many currently marked visible messages in the selected chat are voice/audio.
    pub fn marked_voice_messages_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| {
                self.marked_messages.contains(&m.msg_id)
                    && m.media_kind.is_some_and(MediaKind::is_voice_or_audio)
            })
            .count()
    }

    /// Marks visible messages in `[anchor ..= clicked]` (`Shift+click`; additive when `additive == true`).
    pub fn range_mark_messages_to(&mut self, target_idx: usize, additive: bool) {
        if target_idx >= self.messages.len() {
            return;
        }
        let fold_and_provider = self
            .selected_chat()
            .map(|c| (self.chat_folds_replies(c), c.key.provider));
        let anchor = self
            .m4
            .marked_anchor_idx
            .or(self.selected_message_idx)
            .unwrap_or(target_idx)
            .min(self.messages.len().saturating_sub(1));
        if !additive {
            self.marked_messages.clear();
        }
        let (lo, hi) = if anchor <= target_idx {
            (anchor, target_idx)
        } else {
            (target_idx, anchor)
        };
        for msg in &self.messages[lo..=hi] {
            if let Some((fold, provider)) = fold_and_provider
                && !channel_message_visible(msg, &self.messages, fold, provider)
            {
                continue;
            }
            self.marked_messages.insert(msg.msg_id.clone());
        }
        self.selected_message_idx = Some(target_idx);
        self.rebuild_timeline();
    }

    // -------------------------------------------------------------------------
    // Feature #27 — Voice Playback, Played Set, Resume Positions & Auto-Advance
    // -------------------------------------------------------------------------

    fn persist_current_voice_position(&mut self) {
        let Some(pb) = self.m4.playback.as_ref() else {
            return;
        };
        let pos_ms = pb.pos_ms;
        let dur_ms = pb.dur_ms;
        let pair_key = gone_media_pair_key(&pb.chat, &pb.msg_id);
        let now = now_epoch_secs_public();
        let _ = self
            .store
            .save_voice_position(&pb.chat, &pb.msg_id, pos_ms, dur_ms, now);
        if pos_ms == 0 || (dur_ms > 0 && dur_ms.saturating_sub(pos_ms) <= VOICE_POS_END_CLEAR_MS) {
            self.m4.resume_positions.remove(&pair_key);
        } else {
            self.m4.resume_positions.insert(pair_key, (pos_ms, dur_ms));
        }
    }

    fn mark_voice_played_internal(&mut self, chat: &ChatKey, msg_id: &str) {
        let now = now_epoch_secs_public();
        let _ = self.store.mark_voice_played(chat, msg_id, now);
        self.m4
            .played_keys
            .insert(gone_media_pair_key(chat, msg_id));
    }

    pub fn start_voice_playback(&mut self, chat: &ChatKey, msg_id: &str) -> bool {
        let pair = (chat.clone(), msg_id.to_owned());
        let Some(path) = self.m3.media_paths.get(&pair).cloned() else {
            return false;
        };

        // Save position of previously active voice note before switching.
        self.persist_current_voice_position();

        let saved_pos = self
            .voice_resume_position(chat, msg_id)
            .map(|(p, _)| p)
            .unwrap_or(0);
        let waveform = self
            .m4
            .waveforms
            .get(&pair)
            .cloned()
            .unwrap_or_else(default_waveform);
        let cached_dur_ms = self
            .voice_resume_position(chat, msg_id)
            .map(|(_, d)| d)
            .filter(|&d| d > 0)
            .or_else(|| {
                // Lightweight O(pages) container-only scan (zero Opus frame decoding) on UI thread;
                // the background audio worker supplies the decoded 64-bar waveform via progress event.
                std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| parse_ogg_opus(&bytes).ok())
                    .map(|parsed| parsed.duration_ms)
            })
            .unwrap_or(0);

        self.mark_voice_played_internal(chat, msg_id);
        self.m4.auto_advance_enabled = true;
        self.m4.pending_auto_advance = None;
        self.m4.playback = Some(VoicePlaybackState {
            chat: chat.clone(),
            msg_id: msg_id.to_owned(),
            status: VoicePlaybackStatus::Playing,
            pos_ms: saved_pos,
            dur_ms: cached_dur_ms,
            speed: self.m4.playback_speed,
            waveform,
        });
        self.emit_sync_command(SyncCommand::VoicePlay {
            chat: chat.clone(),
            msg_id: msg_id.to_owned(),
            path,
            speed: self.m4.playback_speed,
            start_ms: saved_pos,
        });
        self.status_notice = Some("Playing voice message...".to_owned());
        true
    }

    pub fn toggle_voice_playback(&mut self) -> bool {
        let Some(pb) = self.m4.playback.as_mut() else {
            return false;
        };
        match pb.status {
            VoicePlaybackStatus::Playing => {
                pb.status = VoicePlaybackStatus::Paused;
                self.persist_current_voice_position();
                self.emit_sync_command(SyncCommand::VoicePause);
                self.status_notice = Some("Voice message paused".to_owned());
                true
            }
            VoicePlaybackStatus::Paused => {
                pb.status = VoicePlaybackStatus::Playing;
                self.emit_sync_command(SyncCommand::VoiceResume);
                self.status_notice = Some("Voice message playing".to_owned());
                true
            }
            VoicePlaybackStatus::Empty => false,
        }
    }

    pub fn stop_voice_playback(&mut self) {
        self.m4.auto_advance_enabled = false;
        self.m4.pending_auto_advance = None;
        if self.m4.playback.is_none() {
            return;
        }
        self.persist_current_voice_position();
        self.m4.playback = None;
        self.emit_sync_command(SyncCommand::VoiceStop);
        self.status_notice = Some("Voice message stopped".to_owned());
    }

    pub fn seek_voice_playback(&mut self, pos_ms: u64) {
        let Some(pb) = self.m4.playback.as_mut() else {
            return;
        };
        let was_paused = pb.status == VoicePlaybackStatus::Paused;
        let clamped = if pb.dur_ms > 0 {
            pos_ms.min(pb.dur_ms.saturating_sub(1))
        } else {
            pos_ms
        };
        pb.pos_ms = clamped;
        pb.status = VoicePlaybackStatus::Playing;
        self.emit_sync_command(SyncCommand::VoiceSeek { pos_ms: clamped });
        if was_paused {
            self.emit_sync_command(SyncCommand::VoiceResume);
        }
    }

    pub fn click_voice_bubble_play(&mut self, chat: &ChatKey, msg_id: &str) {
        if let Some(pb) = self.m4.playback.as_ref()
            && pb.chat == *chat
            && pb.msg_id == msg_id
            && pb.is_active()
        {
            self.toggle_voice_playback();
            return;
        }
        if self.start_voice_playback(chat, msg_id) {
            return;
        }
        if self.request_explicit_media_download(msg_id) {
            self.m4.pending_auto_advance = Some((chat.clone(), msg_id.to_owned()));
        }
    }

    pub fn click_voice_bubble_scrub(&mut self, chat: &ChatKey, msg_id: &str, fraction: f32) {
        let frac = fraction.clamp(0.0, 1.0);
        let is_active = self
            .m4
            .playback
            .as_ref()
            .is_some_and(|pb| pb.chat == *chat && pb.msg_id == msg_id && pb.is_active());
        if !is_active && !self.start_voice_playback(chat, msg_id) {
            return;
        }
        if let Some(dur_ms) = self.m4.playback.as_ref().map(|pb| pb.dur_ms)
            && dur_ms > 0
        {
            let target = ((dur_ms as f64) * f64::from(frac)).round() as u64;
            self.seek_voice_playback(target);
        }
    }

    /// `Ctrl+P`: plays/pauses the selected voice message (or downloads & auto-plays if uncached).
    pub fn play_or_pause_selected_voice(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            self.status_notice = Some("Select a voice message with Alt+J/K first".to_owned());
            return false;
        };
        if !msg.media_kind.is_some_and(MediaKind::is_voice_or_audio) {
            self.status_notice = Some("Select a voice message with Alt+J/K first".to_owned());
            return false;
        }
        self.click_voice_bubble_play(&msg.chat, &msg.msg_id);
        true
    }

    pub fn jump_to_playing_chat(&mut self) -> bool {
        let Some(chat) = self.m4.playback.as_ref().map(|pb| pb.chat.clone()) else {
            return false;
        };
        if self.select_chat_key(&chat) {
            self.status_notice = Some("Back to the chat that is playing".to_owned());
            true
        } else {
            false
        }
    }

    pub(crate) fn handle_voice_playback_progress(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        pos_ms: u64,
        dur_ms: u64,
        waveform: Vec<u8>,
        playing: bool,
    ) {
        let pair = (chat.clone(), msg_id.clone());
        if !waveform.is_empty() {
            self.m4.waveforms.insert(pair, waveform.clone());
        }
        if let Some(pb) = self.m4.playback.as_mut()
            && pb.chat == chat
            && pb.msg_id == msg_id
        {
            pb.pos_ms = pos_ms;
            if dur_ms > 0 {
                pb.dur_ms = dur_ms;
            }
            if !waveform.is_empty() {
                pb.waveform = waveform;
            }
            pb.status = if playing {
                VoicePlaybackStatus::Playing
            } else {
                VoicePlaybackStatus::Paused
            };
        }
    }

    pub(crate) fn handle_voice_playback_ended(&mut self, chat: ChatKey, msg_id: String) {
        let pair_key = gone_media_pair_key(&chat, &msg_id);
        let _ = self.store.clear_voice_position(&chat, &msg_id);
        self.m4.resume_positions.remove(&pair_key);
        self.mark_voice_played_internal(&chat, &msg_id);

        let is_current = self
            .m4
            .playback
            .as_ref()
            .is_some_and(|pb| pb.chat == chat && pb.msg_id == msg_id);
        if !is_current {
            return;
        }
        let should_advance = self.m4.auto_advance_enabled;
        self.m4.playback = None;

        if should_advance {
            self.advance_to_next_voice_message(&chat, &msg_id);
        }
    }

    fn advance_to_next_voice_message(&mut self, chat: &ChatKey, after_msg_id: &str) -> bool {
        if self.selected_chat().is_none_or(|c| &c.key != chat) {
            return false;
        }
        let Some(start_idx) = self.messages.iter().position(|m| m.msg_id == after_msg_id) else {
            return false;
        };
        let next_voice = self.messages[start_idx + 1..]
            .iter()
            .find(|m| {
                !m.msg_id.is_empty() && m.media_kind.is_some_and(MediaKind::is_voice_or_audio)
            })
            .cloned();
        let Some(next) = next_voice else {
            return false;
        };

        if self.start_voice_playback(&next.chat, &next.msg_id) {
            return true;
        }
        // Not downloaded yet -> trigger explicit download and auto-play on MediaDownloaded.
        let pair = (next.chat.clone(), next.msg_id.clone());
        let already_downloading = matches!(
            self.m3.media_statuses.get(&pair),
            Some(MediaItemStatus::Downloading)
        );
        if already_downloading || self.request_explicit_media_download(&next.msg_id) {
            self.m4.pending_auto_advance = Some((next.chat, next.msg_id));
            return true;
        }
        false
    }

    pub(crate) fn handle_voice_playback_failed(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        message: String,
    ) {
        if self
            .m4
            .playback
            .as_ref()
            .is_some_and(|pb| pb.chat == chat && pb.msg_id == msg_id)
        {
            self.m4.playback = None;
        }
        self.m4.pending_auto_advance = None;
        self.status_notice = Some(message);
    }

    // -------------------------------------------------------------------------
    // Feature #28 — Voice Recording (`Ctrl+Shift+M`), Outbox & Echo Matching
    // -------------------------------------------------------------------------

    pub fn toggle_voice_recording(&mut self, window_active: bool) -> bool {
        if !window_active || self.focus == FocusTarget::ThreadComposer {
            return false;
        }
        if self.m4.recording.is_retained() {
            return self.retry_retained_voice_note();
        }
        if self.m4.recording.is_recording() {
            return self.finish_voice_recording();
        }
        let Some(chat) = self.selected_chat().cloned() else {
            self.status_notice = Some("Select a chat first".to_owned());
            return false;
        };
        // Stop active playback and cancel any pending auto-advance before capturing microphone.
        self.m4.auto_advance_enabled = false;
        self.m4.pending_auto_advance = None;
        if self.m4.playback.is_some() {
            self.stop_voice_playback();
        }
        let now_ms = now_epoch_millis_public().max(0) as u64;
        self.m4.active_record_generation = self.m4.active_record_generation.wrapping_add(1);
        let generation = self.m4.active_record_generation;
        self.m4.recording_finishing = false;
        self.m4.recording_reply_to = self.reply_to_msg_id.clone();
        self.m4.recording = VoiceRecordState::Recording {
            origin_chat: chat.key.clone(),
            started_at_ms: now_ms,
            elapsed_ms: 0,
            live_bars: Vec::new(),
        };
        self.emit_sync_command(SyncCommand::VoiceRecordStart {
            generation,
            origin_chat: chat.key,
            preferred_mic: self.m4.selected_microphone.clone(),
        });
        self.status_notice = Some("Recording... Enter sends, Esc cancels".to_owned());
        true
    }

    pub fn finish_voice_recording(&mut self) -> bool {
        if self.m4.recording.is_retained() {
            return self.retry_retained_voice_note();
        }
        if !self.m4.recording.is_recording() || self.m4.recording_finishing {
            return false;
        }
        self.m4.recording_finishing = true;
        self.emit_sync_command(SyncCommand::VoiceRecordFinish);
        self.status_notice = Some("Preparing voice note...".to_owned());
        true
    }

    pub fn cancel_voice_recording(&mut self, notice: &str) -> bool {
        if !self.m4.recording.is_active() {
            return false;
        }
        if let VoiceRecordState::Retained { staged_path, .. } = &self.m4.recording {
            let _ = std::fs::remove_file(staged_path);
        }
        self.m4.active_record_generation = self.m4.active_record_generation.wrapping_add(1);
        self.m4.recording = VoiceRecordState::Idle;
        self.m4.recording_finishing = false;
        self.m4.recording_reply_to = None;
        self.emit_sync_command(SyncCommand::VoiceRecordCancel);
        self.status_notice = Some(notice.to_owned());
        true
    }

    pub(crate) fn cancel_voice_recording_on_chat_switch(&mut self) {
        if self.m4.recording.is_recording() && !self.m4.recording_finishing {
            self.cancel_voice_recording("Voice note cancelled: you switched chats");
        }
    }

    pub(crate) fn handle_voice_record_tick(
        &mut self,
        generation: u64,
        elapsed_ms: u64,
        bars: Vec<u8>,
    ) {
        if generation != self.m4.active_record_generation {
            return;
        }
        if let VoiceRecordState::Recording {
            elapsed_ms: em,
            live_bars,
            ..
        } = &mut self.m4.recording
        {
            *em = elapsed_ms;
            *live_bars = bars;
        }
    }

    pub(crate) fn handle_voice_record_finished(
        &mut self,
        generation: u64,
        origin_chat: ChatKey,
        ogg_bytes: Vec<u8>,
        duration_secs: u32,
        waveform: Vec<u8>,
    ) {
        if generation != self.m4.active_record_generation
            || !matches!(
                &self.m4.recording,
                VoiceRecordState::Recording { origin_chat: active, .. } if *active == origin_chat
            )
        {
            return;
        }
        self.m4.active_record_generation = self.m4.active_record_generation.wrapping_add(1);
        self.m4.recording_finishing = false;
        let voice_dir = self.m3.data_dir.join("voice");
        if std::fs::create_dir_all(&voice_dir).is_err() {
            self.m4.recording = VoiceRecordState::Idle;
            self.status_notice = Some("Could not save voice note".to_owned());
            return;
        }
        self.m4.voice_file_seq = self.m4.voice_file_seq.saturating_add(1);
        let now_ms = now_epoch_millis_public().max(0) as u64;
        let filename = format!("voice-{now_ms}-{}.ogg", self.m4.voice_file_seq);
        let staged_path = voice_dir.join(&filename);
        let tmp_path = voice_dir.join(format!("{filename}.tmp"));
        if std::fs::write(&tmp_path, &ogg_bytes).is_err()
            || std::fs::rename(&tmp_path, &staged_path).is_err()
        {
            let _ = std::fs::remove_file(&tmp_path);
            self.m4.recording = VoiceRecordState::Idle;
            self.status_notice = Some("Could not save voice note".to_owned());
            return;
        }

        let sha = sha256_bytes(&ogg_bytes);
        let file_bytes = ogg_bytes.len() as u64;
        let secs = duration_secs.max(1);
        self.enqueue_staged_voice_note(origin_chat, staged_path, sha, file_bytes, secs, waveform);
    }

    pub fn retry_retained_voice_note(&mut self) -> bool {
        let VoiceRecordState::Retained {
            origin_chat,
            staged_path,
            sha256,
            file_bytes,
            duration_secs,
            waveform,
        } = self.m4.recording.clone()
        else {
            return false;
        };
        self.enqueue_staged_voice_note(
            origin_chat,
            staged_path,
            sha256,
            file_bytes,
            duration_secs,
            waveform,
        )
    }

    fn enqueue_staged_voice_note(
        &mut self,
        origin_chat: ChatKey,
        staged_path: PathBuf,
        sha: [u8; 32],
        file_bytes: u64,
        duration_secs: u32,
        waveform: Vec<u8>,
    ) -> bool {
        let now_ms = now_epoch_millis_public();
        let now_secs = now_epoch_secs_public();
        let client_id = new_client_id_public(&mut self.local_send_seq);
        let reply_to = self.m4.recording_reply_to.clone();
        let record = OutboxRecord::new_voice(
            client_id.clone(),
            origin_chat.clone(),
            reply_to.clone(),
            staged_path.to_string_lossy().into_owned(),
            sha,
            file_bytes,
            duration_secs,
            now_ms,
        );

        match self.store.enqueue_outbox(&record) {
            Ok(()) => {
                let sha_hex = hex_encode_32(&sha);
                let mut wf_arr = [0u8; WAVEFORM_BARS];
                for (dst, src) in wf_arr.iter_mut().zip(waveform.iter().copied()) {
                    *dst = src;
                }
                let _ = self.m4.voice_sent_fifo.push(
                    origin_chat.clone(),
                    staged_path.clone(),
                    sha_hex.clone(),
                    now_secs,
                    duration_secs,
                    wf_arr,
                );
                let ts = format_utc_rfc3339_public(now_secs);
                let label = format!(
                    "Voice message {}",
                    format_voice_clock(u64::from(duration_secs) * 1000)
                );
                let msg = Message {
                    chat: origin_chat.clone(),
                    msg_id: client_id.clone(),
                    sender_id: "me".to_owned(),
                    sender_name: "You".to_owned(),
                    from_me: true,
                    ts: ts.clone(),
                    body: label.clone(),
                    reply_to_id: reply_to,
                    reply_broadcast: false,
                    media_kind: Some(MediaKind::Voice),
                    mime_type: Some("audio/ogg".to_owned()),
                    filename: Some("voice.ogg".to_owned()),
                    file_size: Some(file_bytes),
                    reactions: Vec::new(),
                    read_status: ReadStatus::Sending,
                    deleted: false,
                };
                let _ = self.store.upsert_message(&msg);

                let pair = (origin_chat.clone(), client_id);
                self.m3
                    .media_paths
                    .insert(pair.clone(), staged_path.clone());
                self.m3
                    .media_statuses
                    .insert(pair.clone(), MediaItemStatus::Ready(staged_path));
                self.m4.media_sha256.insert(pair.clone(), sha_hex);
                self.m4.waveforms.insert(pair, wf_arr.to_vec());

                if let Ok(Some(mut chat)) = self.store.get_chat(&origin_chat) {
                    // Auto-unarchive on send for `.archived` (NOT `.kept_archived`, per main.zig:5925-5931).
                    if self.chat_archive_status(&chat) == ArchiveStatus::Archived {
                        chat.archived_local = false;
                        chat.archived_remote = false;
                        if chat.key.provider == Provider::Whatsapp {
                            self.archive_table.return_to_inbox(&chat.key);
                            if let Some(entry) = self.archive_table.get(&chat.key) {
                                let _ = self.store.upsert_archive_entry(entry);
                            }
                            let _ = self.store.enqueue_pending_archive(
                                &chat.key,
                                PendingArchiveIntent::UnarchivePending,
                                now_ms,
                            );
                            self.emit_sync_command(SyncCommand::WakeArchive);
                        } else {
                            self.archive_table.remove(&chat.key);
                            let _ = self.store.delete_archive_entry(&chat.key);
                        }
                    }
                    chat.last_msg_ts = ts;
                    chat.last_msg_preview = label;
                    let _ = self.store.upsert_chat(&chat, now_secs);
                }

                if self.selected_chat().is_some_and(|c| c.key == origin_chat) {
                    self.reply_to_msg_id = None;
                }
                self.m4.recording = VoiceRecordState::Idle;
                self.m4.recording_finishing = false;
                self.m4.recording_reply_to = None;
                self.scroll_to_latest_pending = true;
                self.emit_sync_command(SyncCommand::WakeOutbox);
                self.refresh_chats();
                self.status_notice = Some("Voice note saved".to_owned());
                true
            }
            Err(StoreError::OutboxFull) => {
                self.m4.recording = VoiceRecordState::Retained {
                    origin_chat,
                    staged_path,
                    sha256: sha,
                    file_bytes,
                    duration_secs,
                    waveform,
                };
                self.m4.recording_finishing = false;
                self.status_notice = Some("Voice note retained; Enter retries".to_owned());
                false
            }
            Err(_) => {
                self.m4.recording = VoiceRecordState::Retained {
                    origin_chat,
                    staged_path,
                    sha256: sha,
                    file_bytes,
                    duration_secs,
                    waveform,
                };
                self.m4.recording_finishing = false;
                self.status_notice = Some("Voice note retained; Enter retries".to_owned());
                false
            }
        }
    }

    pub(crate) fn handle_voice_record_failed(&mut self, generation: u64, message: String) {
        if generation != self.m4.active_record_generation || !self.m4.recording.is_recording() {
            return;
        }
        self.m4.active_record_generation = self.m4.active_record_generation.wrapping_add(1);
        self.m4.recording = VoiceRecordState::Idle;
        self.m4.recording_finishing = false;
        self.m4.recording_reply_to = None;
        self.status_notice = Some(message);
    }

    /// Echo-matches own sent voice rows from Hub against `voice_sent_fifo` so newly sent own
    /// voice notes immediately reuse their local staged Ogg file & waveform without re-downloading.
    pub(crate) fn apply_voice_sent_echoes(&mut self) {
        if self.m4.voice_sent_fifo.is_empty() {
            return;
        }
        for msg in &self.messages {
            if !msg.from_me
                || msg.read_status == ReadStatus::Sending
                || self
                    .outbox_for_chat
                    .iter()
                    .any(|r| r.client_id == msg.msg_id)
                || !msg.media_kind.is_some_and(MediaKind::is_voice_or_audio)
            {
                continue;
            }
            let pair = (msg.chat.clone(), msg.msg_id.clone());
            if self.m3.media_paths.contains_key(&pair) {
                continue;
            }
            let row_unix = unix_seconds(&msg.ts).unwrap_or_else(now_epoch_secs_public);
            if let Some((local_path, sha256, waveform)) =
                self.m4
                    .voice_sent_fifo
                    .match_echo_row(&msg.chat, &msg.msg_id, true, true, row_unix)
                && local_path.is_file()
            {
                self.m3.media_paths.insert(pair.clone(), local_path.clone());
                self.m3
                    .media_statuses
                    .insert(pair.clone(), MediaItemStatus::Ready(local_path));
                self.m4.media_sha256.insert(pair.clone(), sha256);
                self.m4.waveforms.insert(pair, waveform.to_vec());
            }
        }
    }

    // -------------------------------------------------------------------------
    // Feature #29 — Cached Transcription (`Ctrl+T`), Copy Raw & Marked Export
    // -------------------------------------------------------------------------

    /// Resets per-chat-open attempt tracking and clears `.failed` statuses in the current chat
    /// so failed notes get **1 retry on next chat open** (never touches `Ready` or `Transcribing`).
    pub(crate) fn begin_transcription_open(&mut self) {
        self.m4.transcript_attempted_this_open.clear();
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            return;
        };
        for (pair, status) in &mut self.m4.transcripts {
            if pair.0 == chat_key && matches!(status, TranscriptStatus::Failed(_)) {
                *status = TranscriptStatus::None;
            }
        }
    }

    /// Scans the currently open chat newest-first for voice/audio messages with local files:
    /// 1. Resolves cached transcripts from SQLite (`media_sha256`) or `.raw.txt`/`.txt` sidecars
    ///    WITHOUT ever re-requesting Deepgram.
    /// 2. For uncached voice messages, if `deepgram_key_available` and not yet attempted this open,
    ///    marks `Transcribing` and emits `SyncCommand::TranscribeAudio`.
    pub(crate) fn schedule_chat_transcriptions(&mut self) {
        let Some(chat) = self.selected_chat().cloned() else {
            return;
        };
        let contact_name = chat.title.clone();
        let mut to_emit = Vec::new();

        for idx in (0..self.messages.len()).rev() {
            let (msg_id, media_kind, mime_type, from_me) = {
                let msg = &self.messages[idx];
                (
                    msg.msg_id.clone(),
                    msg.media_kind,
                    msg.mime_type.clone(),
                    msg.from_me,
                )
            };
            if msg_id.is_empty() || !media_kind.is_some_and(MediaKind::is_voice_or_audio) {
                continue;
            }
            let pair = (chat.key.clone(), msg_id.clone());
            if matches!(
                self.m4.transcripts.get(&pair),
                Some(TranscriptStatus::Ready(_) | TranscriptStatus::Transcribing)
            ) {
                continue;
            }

            let local_path = self.m3.media_paths.get(&pair).cloned();
            let sha256 = self.resolve_media_sha256(&chat.key, &msg_id, local_path.as_deref());

            // Immutable cache check: SQLite `transcripts` + `.raw.txt`/`.txt` sidecars.
            if let Some((raw_text, formatted_text, language)) =
                load_cached_transcript(&self.store, sha256.as_deref(), local_path.as_deref())
            {
                let record = TranscriptRecord {
                    media_sha256: sha256.clone().unwrap_or_default(),
                    language,
                    raw_text: raw_text.clone(),
                    formatted_text: formatted_text.clone(),
                    updated_at: now_epoch_secs_public(),
                };
                self.m4
                    .transcripts
                    .insert(pair.clone(), TranscriptStatus::Ready(record));
                // Optional ADHD formatting (only if user configured Anthropic/OpenRouter key).
                if self.m4.llm_format_key_available
                    && formatted_text.is_none()
                    && !raw_text.is_empty()
                    && let Some(sha) = sha256
                    && self.m4.format_requests.claim(&msg_id, &raw_text)
                {
                    to_emit.push(SyncCommand::FormatTranscript {
                        chat: chat.key.clone(),
                        msg_id,
                        sha256: sha,
                        raw_text,
                        from_me,
                        contact_name: contact_name.clone(),
                        local_path,
                    });
                }
                continue;
            }

            // Not cached -> schedule Deepgram transcription once per chat open if key available.
            let Some(path) = local_path else {
                continue;
            };
            if !self.m4.deepgram_key_available {
                continue;
            }
            if !self.m4.transcript_attempted_this_open.insert(pair.clone()) {
                continue;
            }
            let sha = sha256.unwrap_or_else(|| compute_file_sha256_hex(&path).unwrap_or_default());
            if !sha.is_empty() {
                self.m4.media_sha256.insert(pair.clone(), sha.clone());
            }
            self.m4
                .transcripts
                .insert(pair, TranscriptStatus::Transcribing);
            to_emit.push(SyncCommand::TranscribeAudio {
                chat: chat.key.clone(),
                msg_id,
                path,
                sha256: sha,
                mime_type: mime_type
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "audio/ogg".to_owned()),
                from_me,
                contact_name: contact_name.clone(),
                language: DictationLanguage::Automatic,
            });
        }

        for cmd in to_emit {
            self.emit_sync_command(cmd);
        }
    }

    fn resolve_media_sha256(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        local_path: Option<&Path>,
    ) -> Option<String> {
        let pair = (chat.clone(), msg_id.to_owned());
        if let Some(sha) = self.m4.media_sha256.get(&pair)
            && !sha.is_empty()
        {
            return Some(sha.clone());
        }
        let cache_key = media_cache_key(chat, msg_id);
        if let Ok(Some(entry)) = self.store.get_media_cache(&cache_key)
            && !entry.sha256.is_empty()
        {
            self.m4.media_sha256.insert(pair, entry.sha256.clone());
            return Some(entry.sha256);
        }
        if let Some(path) = local_path
            && let Some(sha) = compute_file_sha256_hex(path)
        {
            self.m4.media_sha256.insert(pair, sha.clone());
            return Some(sha);
        }
        None
    }

    pub(crate) fn on_voice_media_downloaded(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        sha256: String,
    ) {
        let pair = (chat.clone(), msg_id.to_owned());
        if !sha256.is_empty() {
            self.m4.media_sha256.insert(pair.clone(), sha256);
        }
        let is_selected_chat = self.selected_chat().is_some_and(|c| &c.key == chat);
        let is_voice = is_selected_chat
            && self
                .messages
                .iter()
                .find(|m| &m.chat == chat && m.msg_id == msg_id)
                .and_then(|m| m.media_kind)
                .is_some_and(MediaKind::is_voice_or_audio);
        if !is_voice {
            if self.m4.pending_auto_advance.as_ref() == Some(&pair) {
                self.m4.pending_auto_advance = None;
            }
            return;
        }
        if self.m4.pending_auto_advance.as_ref() == Some(&pair) {
            self.m4.pending_auto_advance = None;
            self.start_voice_playback(chat, msg_id);
        }
        self.schedule_chat_transcriptions();
    }

    pub(crate) fn handle_transcript_ready(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        sha256: String,
        raw_text: String,
        formatted_text: Option<String>,
        language: String,
    ) {
        let now = now_epoch_secs_public();
        if !sha256.is_empty() && !raw_text.trim().is_empty() {
            let _ = self
                .store
                .cache_raw_transcript(&sha256, &language, &raw_text, now);
            if let Some(ref fmt) = formatted_text {
                let _ = self.store.save_formatted_transcript(&sha256, fmt, now);
            }
        }
        let pair = (chat.clone(), msg_id.clone());
        let local_path = self.m3.media_paths.get(&pair).cloned();
        if let Some(ref path) = local_path
            && !raw_text.trim().is_empty()
        {
            let _ = write_transcript_sidecars(path, &raw_text);
        }
        let record = TranscriptRecord {
            media_sha256: sha256.clone(),
            language,
            raw_text: raw_text.clone(),
            formatted_text: formatted_text.clone(),
            updated_at: now,
        };
        self.m4
            .transcripts
            .insert(pair, TranscriptStatus::Ready(record));

        if self.m4.llm_format_key_available
            && formatted_text.is_none()
            && !sha256.is_empty()
            && !raw_text.is_empty()
            && self.m4.format_requests.claim(&msg_id, &raw_text)
        {
            let from_me = self
                .messages
                .iter()
                .find(|m| m.chat == chat && m.msg_id == msg_id)
                .is_some_and(|m| m.from_me);
            let contact_name = self
                .chats
                .iter()
                .find(|c| c.key == chat)
                .map(|c| c.title.clone())
                .unwrap_or_default();
            self.emit_sync_command(SyncCommand::FormatTranscript {
                chat,
                msg_id,
                sha256,
                raw_text,
                from_me,
                contact_name,
                local_path,
            });
        }
    }

    pub(crate) fn handle_transcript_failed(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        error: TranscriptError,
    ) {
        let pair = (chat, msg_id);
        self.m4
            .transcripts
            .insert(pair, TranscriptStatus::Failed(error));
        self.status_notice = Some("Transcription failed for a voice message".to_owned());
    }

    pub(crate) fn handle_transcript_formatted(
        &mut self,
        chat: ChatKey,
        msg_id: String,
        sha256: String,
        formatted_text: String,
    ) {
        let now = now_epoch_secs_public();
        if !sha256.is_empty() {
            let _ = self
                .store
                .save_formatted_transcript(&sha256, &formatted_text, now);
        }
        let pair = (chat, msg_id);
        if let Some(TranscriptStatus::Ready(rec)) = self.m4.transcripts.get_mut(&pair) {
            rec.formatted_text = Some(formatted_text);
            rec.updated_at = now;
        }
    }

    /// `Ctrl+T`: expands/collapses the selected voice message's transcript.
    pub fn toggle_selected_transcript(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            self.status_notice = Some("Select a voice message with Alt+J/K first".to_owned());
            return false;
        };
        let pair = (msg.chat.clone(), msg.msg_id.clone());
        let has_transcript = self
            .m4
            .transcripts
            .get(&pair)
            .and_then(TranscriptStatus::display_text)
            .is_some_and(|t| !t.trim().is_empty());
        if !has_transcript {
            self.status_notice = Some("Select a voice message with Alt+J/K first".to_owned());
            return false;
        }
        self.toggle_transcript_for_message(&msg.chat, &msg.msg_id);
        true
    }

    pub fn toggle_transcript_for_message(&mut self, chat: &ChatKey, msg_id: &str) {
        let pair = (chat.clone(), msg_id.to_owned());
        if !self.m4.transcript_expanded.remove(&pair) {
            self.m4.transcript_expanded.insert(pair);
        }
    }

    pub fn copy_selected_message_text(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            self.status_notice = Some("No message selected".to_owned());
            return false;
        };
        if msg.body.is_empty() {
            self.status_notice = Some("Message has no text to copy".to_owned());
            return false;
        }
        self.m4.pending_clipboard_text = Some(msg.body);
        self.status_notice = Some("Message copied".to_owned());
        true
    }

    /// `Copy transcript`: copies the selected voice message's display transcript to clipboard.
    pub fn copy_selected_transcript(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            self.status_notice = Some("Select a voice message with Alt+J/K first".to_owned());
            return false;
        };
        let pair = (msg.chat.clone(), msg.msg_id.clone());
        let Some(text) = self
            .m4
            .transcripts
            .get(&pair)
            .and_then(TranscriptStatus::display_text)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
        else {
            self.status_notice = Some("No transcript available for selected message".to_owned());
            return false;
        };
        self.m4.pending_clipboard_text = Some(text);
        self.status_notice = Some("Transcript copied".to_owned());
        true
    }

    /// `Copy all transcripts`: exports verified raw transcripts for marked voice messages
    /// oldest-first joined by `"\r\n\r\n"`, refusing legacy formatted summaries as raw and
    /// reporting missing notes cleanly.
    pub fn copy_all_marked_transcripts(&mut self) -> bool {
        let mut raw_cache: Vec<(usize, String, Option<String>, Option<PathBuf>)> = Vec::new();
        for (idx, msg) in self.messages.iter().enumerate() {
            if !self.marked_messages.contains(&msg.msg_id)
                || !msg.media_kind.is_some_and(MediaKind::is_voice_or_audio)
            {
                continue;
            }
            let pair = (msg.chat.clone(), msg.msg_id.clone());
            let local_path = self.m3.media_paths.get(&pair).cloned();
            let raw_from_store = match self.m4.transcripts.get(&pair) {
                Some(TranscriptStatus::Ready(rec)) if !rec.raw_text.trim().is_empty() => {
                    Some(rec.raw_text.clone())
                }
                _ => self
                    .m4
                    .media_sha256
                    .get(&pair)
                    .and_then(|sha| self.store.get_transcript_record(sha).ok().flatten())
                    .map(|rec| rec.raw_text)
                    .filter(|s| !s.trim().is_empty()),
            };
            let label = crate::timeline::format_time_hm(&msg.ts);
            let display_label = if label.is_empty() {
                msg.msg_id.clone()
            } else {
                label
            };
            raw_cache.push((idx, display_label, raw_from_store, local_path));
        }

        if raw_cache.is_empty() {
            self.status_notice = Some("No marked voice messages".to_owned());
            return false;
        }

        let inputs: Vec<MarkedVoiceExportInput<'_>> = raw_cache
            .iter()
            .map(|(idx, label, raw_store, path)| MarkedVoiceExportInput {
                ts: &self.messages[*idx].ts,
                original_index: *idx,
                display_label: label.as_str(),
                raw_from_store: raw_store.as_deref(),
                local_path: path.as_deref(),
            })
            .collect();

        let export = export_marked_raw_transcripts(&inputs);
        if let Some(clip) = export.clipboard_text {
            self.m4.pending_clipboard_text = Some(clip);
        }
        self.status_notice = Some(export.notice.clone());
        self.title_notice = Some(export.notice);
        export.copied_count > 0
    }
}

fn compute_file_sha256_hex(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(hex_encode_32(&sha256_bytes(&bytes)))
}
