use crate::geometry::RowGeometry;
use crate::m3::M3State;
use crate::m4::M4State;
use crate::m5::M5State;
use crate::m6::M6State;
use crate::palette::{
    PaletteCatalogContext, PaletteCommand, PaletteEntry, PaletteEntryKind, PaletteFilterInput,
    PaletteMode, PaletteSection, PaletteState, build_palette_items, filter_palette,
};
use crate::sync_bridge::{HubConnectionStatus, SyncBridge, SyncCommand, SyncEvent};
use crate::timeline::{
    TimelineItem, TimelineOptions, build_timeline_items_with_options, extract_links_and_media,
};
use postbox_core::archive::{
    ArchiveStatus, ArchiveTable, PendingArchiveIntent, archive_baseline, unix_seconds,
};
use postbox_core::deep_link::DeepLinkTarget;
use postbox_core::dictation::DictationLanguage;
use postbox_core::drafts::validate_draft_text;
use postbox_core::outbox::{OutboxRecord, generate_client_id};
use postbox_core::read::{PendingReadQueue, ReadState, should_auto_mark_read};
use postbox_core::threads::{can_broadcast, in_thread, local_thread_root, replies_fold};
use postbox_core::views::{merge_order, save_order};
use postbox_core::{
    Account, Chat, ChatKey, Draft, FontScale, Message, Provider, ReadStatus, ThemeChoice,
    UiSettings, ViewTab,
};
use postbox_keymap::{
    Action, CtrlBadgeState, InboxChord, Keymap, LatestChord, LatestChordOutcome,
    ctrl_a_selects_chats,
};
use postbox_media::drain::MediaItemStatus;
use postbox_store::{MessageCursor, Store, StoreError};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_CHATS_QUERY: usize = 500;
const MAX_MESSAGES_QUERY: usize = 200;
const MAX_FTS_HITS: usize = 20;
const PAGE_STEP_CHATS: usize = 10;
const PAGE_STEP_MESSAGES: usize = 10;
const OLDER_PAGE_LIMIT: usize = 80;
const MAX_HISTORY_MESSAGES: usize = 1000;

/// Launch bundle passed from `postbox-app` to either desktop GPUI or headless mode.
pub struct PostboxLaunchContext {
    pub store: Store,
    pub keymap: Keymap,
    pub version: String,
    pub started_at: Instant,
    pub hub_status: HubConnectionStatus,
    pub sync_bridge: Option<SyncBridge>,
    pub data_dir: PathBuf,
    pub giphy_key_available: bool,
    pub deepgram_key_available: bool,
    pub llm_format_key_available: bool,
    /// M5.5 RAM diet: working-set trim + `GetProcessMemoryInfo` telemetry. `None` in tests that
    /// do not exercise the OS hook; the shell falls back to a no-op trim.
    pub memory_platform: Option<std::sync::Arc<dyn postbox_platform::MemoryPressure>>,
    /// Milestone 6: Windows Media Foundation + Intel Arc DXVA (mock on Linux/tests).
    pub video_backend: Option<std::sync::Arc<dyn postbox_platform::VideoBackend>>,
    /// Milestone 6: WASAPI render stream that clocks inline video playback.
    pub audio_backend: Option<std::sync::Arc<dyn postbox_platform::AudioBackend>>,
    /// Milestone 6: disk cache for video thumbnails (keyed by video content SHA-256).
    pub video_thumb_cache: Option<postbox_media::VideoThumbnailCache>,
    /// Milestone 7: `postbox://` URL this launch should open.
    pub startup_link: Option<String>,
    /// Milestone 7: the previous run's exit record, so a crash can be reported.
    pub last_exit_record: Option<postbox_core::crash::ExitRecord>,
    /// Milestone 7: `false` in tests and in CI, so no registry, mutex or pipe is touched.
    pub os_integration: bool,
    /// Milestone 7: manifest/archive source for "Check for updates". `None` keeps the shell
    /// offline; the app injects the GitHub Releases source.
    pub release_source: Option<std::sync::Arc<dyn postbox_platform::ReleaseSource>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusTarget {
    ChatList,
    MessageList,
    Composer,
    ThreadComposer,
    ThreadBroadcast,
    Palette,
    ShortcutsOverlay,
    EmojiPicker,
    Lightbox,
    GiphyPicker,
    VideoPlayer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarChatRow {
    pub chat: Chat,
    pub archive_status: ArchiveStatus,
    pub draft_preview: Option<String>,
    pub bulk_selected: bool,
    pub active: bool,
    pub slot_badge: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct ThreadPanelState {
    pub chat: ChatKey,
    pub root_msg_id: String,
    pub messages: Vec<Message>,
    pub timeline_items: Vec<TimelineItem>,
    pub composer_text: String,
    pub broadcast: bool,
    pub focus_on_broadcast: bool,
    pub exhausted: bool,
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_epoch_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Formats a Unix epoch timestamp (seconds) as UTC RFC3339 (`YYYY-MM-DDTHH:MM:SSZ`)
/// using Howard Hinnant's civil-from-days algorithm without external dependencies.
fn format_utc_rfc3339(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs_of_day = epoch_secs.rem_euclid(86_400);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(month <= 2);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn new_client_id(seq: &mut u64) -> String {
    *seq = seq.saturating_add(1);
    if let Ok(cid) = generate_client_id() {
        return cid;
    }
    let now = now_epoch_millis() as u64;
    format!("{now:016x}{:016x}", *seq)
}

pub(crate) fn now_epoch_secs_public() -> i64 {
    now_epoch_secs()
}

pub(crate) fn now_epoch_millis_public() -> i64 {
    now_epoch_millis()
}

pub(crate) fn format_utc_rfc3339_public(epoch_secs: i64) -> String {
    format_utc_rfc3339(epoch_secs)
}

pub(crate) fn new_client_id_public(seq: &mut u64) -> String {
    new_client_id(seq)
}

/// Pure headless view-model/controller driving Postbox UI state from `Store` and `Keymap`.
/// Compiles and executes without GPU on both Linux and Windows.
pub struct PostboxViewModel {
    pub(crate) store: Store,
    keymap: Keymap,
    version: String,

    hub_status: HubConnectionStatus,
    sync_cmd_tx: Option<async_channel::Sender<SyncCommand>>,
    emitted_sync_commands: Vec<SyncCommand>,

    theme: ThemeChoice,
    font_scale: FontScale,
    view_order: Vec<String>,
    show_archived_by_view: BTreeMap<String, bool>,
    view_selected_chat: BTreeMap<String, ChatKey>,
    pub(crate) dictation_language: DictationLanguage,
    pub(crate) playback_speed_pct: u16,
    slack_threads_by_account: BTreeMap<String, bool>,
    replies_fold_by_chat: BTreeMap<ChatKey, bool>,

    pub(crate) archive_table: ArchiveTable,
    read_state: ReadState,
    pending_reads: PendingReadQueue,
    user_viewed: bool,

    active_view: Option<ViewTab>,
    accounts: Vec<Account>,
    show_archived_only: bool,
    unread_only: bool,
    search_filter: String,

    pub(crate) chats: Vec<Chat>,
    selected_chat_idx: Option<usize>,
    bulk_selected: BTreeSet<ChatKey>,
    pub(crate) drafts: BTreeMap<ChatKey, Draft>,

    pub(crate) messages: Vec<Message>,
    loaded_message_limit: usize,
    pub(crate) outbox_for_chat: Vec<OutboxRecord>,
    timeline_items: Vec<TimelineItem>,
    pub(crate) selected_message_idx: Option<usize>,
    pub(crate) marked_messages: BTreeSet<String>,
    pub(crate) reply_to_msg_id: Option<String>,
    exhausted_chats: BTreeSet<ChatKey>,

    pub(crate) thread_panel: Option<ThreadPanelState>,

    pub(crate) composer_text: String,
    pub(crate) focus: FocusTarget,
    pub(crate) palette: PaletteState,
    shortcuts_open: bool,
    shortcuts_return_focus: FocusTarget,
    ctrl_badges: CtrlBadgeState,
    inbox_chord: InboxChord,
    latest_chord: LatestChord,

    pub(crate) status_notice: Option<String>,
    pub(crate) title_notice: Option<String>,
    quit_requested: bool,
    sync_requested: bool,
    pub(crate) scroll_to_latest_pending: bool,
    refresh_count: u64,
    pub(crate) local_send_seq: u64,
    pub(crate) m3: M3State,
    pub(crate) m4: M4State,
    pub(crate) m5: M5State,
    pub(crate) m6: M6State,
    /// Milestone 7: deep links, the "Chat not found" card and the updater state.
    pub(crate) m7: crate::m7::M7State,
    /// Milestone 7: per-chat high-water mark so a replayed page never toasts twice.
    pub(crate) notification_gate: postbox_core::toast::NotificationGate,
    /// Milestone 7: set by the palette command, cleared by whoever starts the real check.
    pub(crate) update_check_requested: bool,
}

impl PostboxViewModel {
    /// Creates a ViewModel before any view is active (`window_title() == "Postbox v<ver>"`).
    pub fn new_uninitialized(store: Store, keymap: Keymap, version: impl Into<String>) -> Self {
        let settings = store.load_ui_settings().unwrap_or_default();
        let archive_table = store.load_archive_table().unwrap_or_default();
        let read_state = store.load_read_state().unwrap_or_default();
        let pending_reads = store.load_pending_reads().unwrap_or_default();
        let mut drafts = BTreeMap::new();
        if let Ok(loaded) = store.list_drafts() {
            for draft in loaded {
                if !draft.text_body.is_empty() || draft.staged_image_path.is_some() {
                    drafts.insert(draft.chat.clone(), draft);
                }
            }
        }
        let mut show_archived_by_view = BTreeMap::new();
        if settings.show_archived {
            show_archived_by_view.insert("all".to_owned(), true);
        }
        let mut m3 = M3State::default();
        if let Ok(saved_recents) = store.load_emoji_recents() {
            // M5.5 RAM diet: keep the saved emoji strings and only resolve them to catalog
            // indices the first time the user reacts or opens the picker. Startup never builds
            // the 1,923-entry emoji table for a session that never uses emoji.
            m3.pending_emoji_recents = saved_recents;
        }
        let saved_speed = store
            .load_playback_speed()
            .unwrap_or(postbox_core::voice::PlaybackSpeed::X1);
        let saved_dictation_lang = store
            .load_dictation_language()
            .unwrap_or(DictationLanguage::Automatic);
        let m4 = M4State {
            playback_speed: saved_speed,
            played_keys: store.list_played_voice_keys().unwrap_or_default(),
            resume_positions: store.load_all_voice_positions().unwrap_or_default(),
            selected_microphone: store.load_microphone_preference().ok().flatten(),
            ..M4State::default()
        };

        Self {
            store,
            keymap,
            version: version.into(),
            hub_status: HubConnectionStatus::default(),
            sync_cmd_tx: None,
            emitted_sync_commands: Vec::new(),
            theme: settings.theme,
            font_scale: settings.font_scale,
            view_order: settings.view_order,
            show_archived_by_view,
            view_selected_chat: BTreeMap::new(),
            dictation_language: saved_dictation_lang,
            playback_speed_pct: saved_speed.percent(),
            slack_threads_by_account: BTreeMap::new(),
            replies_fold_by_chat: BTreeMap::new(),
            archive_table,
            read_state,
            pending_reads,
            user_viewed: false,
            active_view: None,
            accounts: Vec::new(),
            show_archived_only: false,
            unread_only: false,
            search_filter: String::new(),
            chats: Vec::new(),
            selected_chat_idx: None,
            bulk_selected: BTreeSet::new(),
            drafts,
            messages: Vec::new(),
            loaded_message_limit: MAX_MESSAGES_QUERY,
            outbox_for_chat: Vec::new(),
            timeline_items: Vec::new(),
            selected_message_idx: None,
            marked_messages: BTreeSet::new(),
            reply_to_msg_id: None,
            exhausted_chats: BTreeSet::new(),
            thread_panel: None,
            composer_text: String::new(),
            focus: FocusTarget::ChatList,
            palette: PaletteState::default(),
            shortcuts_open: false,
            shortcuts_return_focus: FocusTarget::ChatList,
            ctrl_badges: CtrlBadgeState::default(),
            inbox_chord: InboxChord::default(),
            latest_chord: LatestChord::default(),
            status_notice: None,
            title_notice: None,
            quit_requested: false,
            sync_requested: false,
            scroll_to_latest_pending: false,
            refresh_count: 0,
            local_send_seq: 0,
            m3,
            m4,
            m5: M5State::default(),
            m6: M6State::default(),
            m7: crate::m7::M7State::default(),
            notification_gate: postbox_core::toast::NotificationGate::new(),
            update_check_requested: false,
        }
    }

    /// Activates `CombinedInbox` and loads accounts, chats, messages, and initial M3
    /// auto-download/avatar/unfurl triggers. Call AFTER `with_hub_status`, `with_data_dir`,
    /// `with_giphy_key_available`, and `attach_sync_sender` when starting from `new_uninitialized`.
    pub fn initialize(&mut self) {
        self.active_view = Some(ViewTab::CombinedInbox);
        self.refresh_all();
    }

    /// Creates a fully initialized ViewModel with `CombinedInbox` active, accounts ordered,
    /// chats loaded, and initial chat messages populated (`user_viewed` remains `false`).
    pub fn new(store: Store, keymap: Keymap, version: impl Into<String>) -> Self {
        let mut vm = Self::new_uninitialized(store, keymap, version);
        vm.initialize();
        vm
    }

    pub fn with_hub_status(mut self, status: HubConnectionStatus) -> Self {
        self.set_hub_status(status);
        self
    }

    pub fn attach_sync_sender(&mut self, tx: async_channel::Sender<SyncCommand>) {
        self.sync_cmd_tx = Some(tx);
    }

    pub fn take_emitted_sync_commands(&mut self) -> Vec<SyncCommand> {
        std::mem::take(&mut self.emitted_sync_commands)
    }

    pub(crate) fn emit_sync_command(&mut self, cmd: SyncCommand) {
        if let Some(tx) = &self.sync_cmd_tx {
            let _ = tx.try_send(cmd.clone());
        }
        if self.emitted_sync_commands.len() >= 128 {
            self.emitted_sync_commands.remove(0);
        }
        self.emitted_sync_commands.push(cmd);
    }

    pub fn hub_status(&self) -> &HubConnectionStatus {
        &self.hub_status
    }

    pub fn set_hub_status(&mut self, status: HubConnectionStatus) {
        self.hub_status = status;
        if self.palette.open {
            self.rebuild_palette_entries();
        }
    }

    /// Applies a `SyncEvent` pushed from the background Tokio Hub worker.
    /// Returns the number of prepended timeline items (for scroll anchor preservation) when applicable.
    pub fn apply_sync_event(&mut self, event: SyncEvent) -> usize {
        match event {
            SyncEvent::StatusChanged(status) => {
                self.set_hub_status(status);
                0
            }
            SyncEvent::ChatsUpdated { .. } => {
                // Reload persisted archive rules and read watermarks written by the sync worker.
                if let Ok(table) = self.store.load_archive_table() {
                    self.archive_table = table;
                }
                if let Ok(rs) = self.store.load_read_state() {
                    self.read_state = rs;
                }
                // Background refresh MUST NOT trigger user_viewed auto mark-read!
                let prev_user_viewed = self.user_viewed;
                self.user_viewed = false;
                self.refresh_accounts();
                self.refresh_chats();
                self.user_viewed = prev_user_viewed;
                if self.palette.open {
                    self.rebuild_palette_entries();
                }
                0
            }
            SyncEvent::MessagesUpdated {
                chat,
                prepended,
                exhausted,
            } => {
                if exhausted {
                    self.exhausted_chats.insert(chat.clone());
                }
                let is_open = self.selected_chat().is_some_and(|c| c.key == chat);
                let mut timeline_prepended = 0usize;
                if is_open {
                    if prepended > 0 {
                        self.loaded_message_limit = self
                            .loaded_message_limit
                            .saturating_add(prepended)
                            .min(MAX_HISTORY_MESSAGES);
                    }
                    let old_len = self.timeline_items.len();
                    self.refresh_messages();
                    if prepended > 0 && self.timeline_items.len() > old_len {
                        timeline_prepended = self.timeline_items.len() - old_len;
                    }
                    if self.user_viewed {
                        self.maybe_mark_selected_chat_read();
                    }
                }
                if self.thread_panel.as_ref().is_some_and(|p| p.chat == chat) {
                    if let Some(panel) = &mut self.thread_panel {
                        panel.exhausted |= exhausted;
                    }
                    self.refresh_thread_panel_messages();
                }
                timeline_prepended
            }
            SyncEvent::OutboxUpdated { chat, notice } => {
                if let Some(n) = notice {
                    self.status_notice = Some(n);
                }
                if self.selected_chat().is_some_and(|c| c.key == chat) {
                    self.refresh_messages();
                }
                if self.thread_panel.as_ref().is_some_and(|p| p.chat == chat) {
                    self.refresh_thread_panel_messages();
                }
                0
            }
            SyncEvent::MediaDownloaded {
                chat,
                msg_id,
                path,
                sha256,
                ..
            } => {
                let pair = (chat.clone(), msg_id.clone());
                self.m3.media_paths.insert(pair.clone(), path.clone());
                self.m3
                    .media_statuses
                    .insert(pair, MediaItemStatus::Ready(path.clone()));
                if let Some(lb) = &mut self.m3.lightbox
                    && lb.chat == chat
                {
                    lb.update_local_path(&msg_id, path.clone());
                }
                self.on_voice_media_downloaded(&chat, &msg_id, sha256);
                0
            }
            SyncEvent::MediaStatusChanged {
                chat,
                msg_id,
                status,
            } => {
                let pair = (chat.clone(), msg_id.clone());
                if let MediaItemStatus::Ready(ref path) = status {
                    self.m3.media_paths.insert(pair.clone(), path.clone());
                }
                if matches!(status, MediaItemStatus::Unavailable | MediaItemStatus::Gone) {
                    if self.m4.pending_auto_advance.as_ref() == Some(&pair) {
                        self.m4.pending_auto_advance = None;
                    }
                    if let Some(lb) = &mut self.m3.lightbox
                        && lb.chat == chat
                    {
                        let _ = lb.mark_failed_and_check_external(&msg_id);
                    }
                }
                self.m3.media_statuses.insert(pair, status);
                0
            }
            SyncEvent::AvatarReady { chat, path } => {
                self.m3.avatar_paths.insert(chat, path);
                0
            }
            SyncEvent::WorkspaceIconReady { team, path } => {
                self.m3.workspace_icon_paths.insert(team, path);
                0
            }
            SyncEvent::SlackEmojiIndexReady { team, index } => {
                let mut to_fetch = Vec::new();
                for msg in &self.messages {
                    if msg.chat.provider == Provider::Slack && msg.chat.account == team {
                        for name in index.referenced_custom_names(&msg.body) {
                            if let Some(postbox_media::avatar::ResolvedSlackEmoji::Custom {
                                name: resolved_name,
                                url,
                            }) = index.resolve(&name)
                                && self
                                    .m3
                                    .slack_emoji_media_requested
                                    .insert((team.clone(), resolved_name.clone()))
                            {
                                to_fetch.push((resolved_name, url));
                            }
                        }
                    }
                }
                self.m3.slack_emoji_indices.insert(team.clone(), index);
                for (name, url) in to_fetch {
                    self.emit_sync_command(SyncCommand::FetchSlackEmojiMedia {
                        team: team.clone(),
                        name,
                        url,
                    });
                }
                0
            }
            SyncEvent::SlackEmojiMediaReady { team, name, path } => {
                self.m3.slack_emoji_paths.insert((team, name), path);
                0
            }
            SyncEvent::ReactionSettled {
                chat,
                msg_id,
                emoji,
                ok,
                http_status,
            } => {
                self.settle_reaction_event(
                    &chat,
                    &msg_id,
                    &emoji,
                    ok,
                    http_status,
                    now_epoch_millis() as u64,
                );
                0
            }
            SyncEvent::UnfurlReady { target, card } => {
                self.m3
                    .unfurl_in_flight
                    .remove(&(target.provider, target.id.clone()));
                if let Some(c) = card {
                    self.m3.unfurl_ring.insert(c);
                }
                0
            }
            SyncEvent::GiphyResults {
                generation,
                target,
                page,
                error,
            } => {
                if let Some(p) = page {
                    self.m3.giphy_picker.accept(generation, &target, p);
                } else if let Some(msg) = error
                    && self.m3.giphy_picker.fail(generation, &target, msg.clone())
                {
                    self.status_notice = Some(msg);
                }
                0
            }
            SyncEvent::GiphyPreviewReady {
                generation,
                index,
                path,
            } => {
                if self.m3.giphy_picker.generation == generation
                    && self.m3.giphy_picker.status == postbox_media::giphy::GiphyStatus::Ready
                {
                    self.m3.giphy_preview_paths.insert(index, path);
                }
                0
            }
            SyncEvent::GiphyDownloadReady {
                generation,
                download_generation,
                target,
                index,
                gif_id,
                bytes,
            } => {
                self.apply_giphy_download_ready(
                    generation,
                    download_generation,
                    &target,
                    index,
                    &gif_id,
                    &bytes,
                );
                0
            }
            SyncEvent::GiphyError {
                generation,
                index,
                download_generation,
                message,
            } => {
                if self
                    .m3
                    .giphy_picker
                    .fail_download(generation, index, download_generation)
                {
                    self.status_notice = Some(message);
                }
                0
            }
            SyncEvent::TranscriptReady {
                chat,
                msg_id,
                sha256,
                raw_text,
                formatted_text,
                language,
            } => {
                self.handle_transcript_ready(
                    chat,
                    msg_id,
                    sha256,
                    raw_text,
                    formatted_text,
                    language,
                );
                0
            }
            SyncEvent::TranscriptFailed {
                chat,
                msg_id,
                error,
            } => {
                self.handle_transcript_failed(chat, msg_id, error);
                0
            }
            SyncEvent::TranscriptFormatted {
                chat,
                msg_id,
                sha256,
                formatted_text,
            } => {
                self.handle_transcript_formatted(chat, msg_id, sha256, formatted_text);
                0
            }
            SyncEvent::VoicePlaybackProgress {
                chat,
                msg_id,
                pos_ms,
                dur_ms,
                waveform,
                playing,
            } => {
                self.handle_voice_playback_progress(
                    chat, msg_id, pos_ms, dur_ms, waveform, playing,
                );
                0
            }
            SyncEvent::VoicePlaybackEnded { chat, msg_id } => {
                self.handle_voice_playback_ended(chat, msg_id);
                0
            }
            SyncEvent::VoicePlaybackFailed {
                chat,
                msg_id,
                message,
            } => {
                self.handle_voice_playback_failed(chat, msg_id, message);
                0
            }
            SyncEvent::VoiceRecordTick {
                generation,
                elapsed_ms,
                bars,
            } => {
                self.handle_voice_record_tick(generation, elapsed_ms, bars);
                0
            }
            SyncEvent::VoiceRecordFinished {
                generation,
                origin_chat,
                ogg_bytes,
                duration_secs,
                waveform,
            } => {
                self.handle_voice_record_finished(
                    generation,
                    origin_chat,
                    ogg_bytes,
                    duration_secs,
                    waveform,
                );
                0
            }
            SyncEvent::VoiceRecordFailed {
                generation,
                message,
            } => {
                self.handle_voice_record_failed(generation, message);
                0
            }
            SyncEvent::MicrophonesUpdated(devices) => {
                self.set_microphones(devices);
                0
            }
            SyncEvent::DictationConnected { generation } => {
                self.handle_dictation_connected(generation);
                0
            }
            SyncEvent::DictationInterim {
                generation,
                revision,
                text,
            } => {
                self.handle_dictation_interim(generation, revision, text);
                0
            }
            SyncEvent::DictationLevel { generation, level } => {
                self.handle_dictation_level(generation, level);
                0
            }
            SyncEvent::DictationDraining { generation } => {
                self.handle_dictation_draining(generation);
                0
            }
            SyncEvent::DictationFinal { generation, text } => {
                self.handle_dictation_final(generation, text);
                0
            }
            SyncEvent::DictationFailed {
                generation,
                message,
            } => {
                self.handle_dictation_failed(generation, message);
                0
            }
            SyncEvent::Notice(msg) => {
                self.status_notice = Some(msg);
                0
            }
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Store {
        &mut self.store
    }

    /// M5.5 RAM diet: releases the SQLite page cache after bulk syncs and on idle/minimize.
    pub fn shrink_store_memory(&self) -> Result<(), postbox_store::StoreError> {
        self.store.shrink_memory()
    }

    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn theme(&self) -> ThemeChoice {
        self.theme
    }

    pub fn font_scale(&self) -> FontScale {
        self.font_scale
    }

    pub fn row_geometry(&self) -> RowGeometry {
        RowGeometry::for_font_scale(self.font_scale)
    }

    pub fn active_view(&self) -> Option<&ViewTab> {
        self.active_view.as_ref()
    }

    pub fn active_view_key(&self) -> String {
        self.active_view
            .as_ref()
            .map(ViewTab::view_key)
            .unwrap_or_else(|| "all".to_owned())
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn view_order(&self) -> &[String] {
        &self.view_order
    }

    pub fn chats(&self) -> &[Chat] {
        &self.chats
    }

    pub fn selected_chat_idx(&self) -> Option<usize> {
        self.selected_chat_idx
    }

    pub fn selected_chat(&self) -> Option<&Chat> {
        self.selected_chat_idx.and_then(|idx| self.chats.get(idx))
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn outbox_for_chat(&self) -> &[OutboxRecord] {
        &self.outbox_for_chat
    }

    pub fn timeline_items(&self) -> &[TimelineItem] {
        &self.timeline_items
    }

    pub fn selected_message_idx(&self) -> Option<usize> {
        self.selected_message_idx
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.selected_message_idx
            .and_then(|idx| self.messages.get(idx))
    }

    pub fn thread_panel(&self) -> Option<&ThreadPanelState> {
        self.thread_panel.as_ref()
    }

    pub fn archive_table(&self) -> &ArchiveTable {
        &self.archive_table
    }

    pub fn read_state(&self) -> &ReadState {
        &self.read_state
    }

    pub fn pending_reads(&self) -> &PendingReadQueue {
        &self.pending_reads
    }

    pub fn user_viewed(&self) -> bool {
        self.user_viewed
    }

    pub fn focus(&self) -> FocusTarget {
        self.focus
    }

    pub fn set_focus(&mut self, focus: FocusTarget) {
        self.focus = focus;
        if let Some(panel) = &mut self.thread_panel {
            match focus {
                FocusTarget::ThreadComposer => panel.focus_on_broadcast = false,
                FocusTarget::ThreadBroadcast => panel.focus_on_broadcast = true,
                _ => {}
            }
        }
        if focus == FocusTarget::Composer {
            self.user_viewed = true;
            self.maybe_mark_selected_chat_read();
        }
    }

    pub fn composer_focused(&self) -> bool {
        self.focus == FocusTarget::Composer
    }

    pub fn set_composer_focused(&mut self, focused: bool) {
        if focused {
            self.focus = FocusTarget::Composer;
            self.user_viewed = true;
            self.maybe_mark_selected_chat_read();
        } else if self.focus == FocusTarget::Composer {
            self.focus = FocusTarget::ChatList;
        }
    }

    pub fn composer_text(&self) -> &str {
        &self.composer_text
    }

    pub(crate) fn set_focus_raw(&mut self, focus: FocusTarget) {
        self.focus = focus;
    }

    pub(crate) fn set_status_notice(&mut self, notice: impl Into<String>) {
        self.status_notice = Some(notice.into());
    }

    pub(crate) fn rebuild_timeline_public(&mut self) {
        self.rebuild_timeline();
    }

    pub(crate) fn refresh_thread_panel_if_chat(&mut self, chat: &ChatKey) {
        if self.thread_panel.as_ref().is_some_and(|p| &p.chat == chat) {
            self.refresh_thread_panel_messages();
        }
    }

    pub(crate) fn set_composer_text_no_popup_recursion(&mut self, text: impl Into<String>) {
        let mut next = text.into();
        if next.as_bytes().contains(&0) {
            next = next.replace('\0', "");
        }
        if validate_draft_text(&next).is_err() {
            let mut end = postbox_core::drafts::MAX_DRAFT_TEXT_BYTES.min(next.len());
            while end > 0 && !next.is_char_boundary(end) {
                end -= 1;
            }
            next.truncate(end);
        }
        self.composer_text = next;
        self.persist_active_draft();
        let current = self.composer_text.clone();
        self.on_composer_user_edit(false, &current);
    }

    pub fn set_composer_text(&mut self, text: impl Into<String>) {
        self.set_composer_text_no_popup_recursion(text);
        let caret = self.composer_text.len();
        self.update_composer_popups_at_caret(caret);
    }

    pub fn set_composer_text_with_caret(&mut self, text: impl Into<String>, caret_byte: usize) {
        self.set_composer_text_no_popup_recursion(text);
        let clamped = caret_byte.min(self.composer_text.len());
        self.update_composer_popups_at_caret(clamped);
    }

    /// Origin-scoped async text append (`drafts.zig:89-100`): appends text to `origin_chat`
    /// without ever leaking into another chat if the user has switched away.
    /// Returns `true` if `origin_chat` is currently open and live `composer_text` was updated.
    pub fn append_to_chat_draft(&mut self, origin_chat: &ChatKey, text: &str) -> bool {
        let is_open = self.selected_chat().is_some_and(|c| &c.key == origin_chat);
        let prev = if is_open {
            self.composer_text.clone()
        } else {
            self.drafts
                .get(origin_chat)
                .map(|d| d.text_body.clone())
                .unwrap_or_default()
        };
        let combined = if !prev.is_empty() && !text.is_empty() {
            format!("{prev} {text}")
        } else {
            format!("{prev}{text}")
        };
        if validate_draft_text(&combined).is_err() {
            return false;
        }
        let draft = Draft {
            chat: origin_chat.clone(),
            text_body: combined.clone(),
            reply_to_id: self
                .drafts
                .get(origin_chat)
                .and_then(|d| d.reply_to_id.clone()),
            staged_image_path: self
                .drafts
                .get(origin_chat)
                .and_then(|d| d.staged_image_path.clone()),
            updated_at: now_epoch_secs(),
        };
        if self.store.save_draft(&draft).is_ok() {
            self.drafts.insert(origin_chat.clone(), draft);
            if is_open {
                self.composer_text = combined;
                return true;
            }
        }
        false
    }

    pub fn draft_for_chat(&self, chat: &ChatKey) -> Option<&Draft> {
        self.drafts.get(chat)
    }

    pub fn reply_to_msg_id(&self) -> Option<&str> {
        self.reply_to_msg_id.as_deref()
    }

    pub fn cancel_reply(&mut self) {
        self.reply_to_msg_id = None;
        self.observe_dictation_context();
    }

    pub fn unread_only(&self) -> bool {
        self.unread_only
    }

    pub fn show_archived_inline(&self) -> bool {
        let key = self.active_view_key();
        if let Some(&cached) = self.show_archived_by_view.get(&key) {
            return cached;
        }
        self.store.get_view_show_archived(&key).unwrap_or(false)
    }

    pub fn show_archived_only(&self) -> bool {
        self.show_archived_only
    }

    pub fn search_filter(&self) -> &str {
        &self.search_filter
    }

    pub fn bulk_selected_count(&self) -> usize {
        self.bulk_selected.len()
    }

    pub fn marked_messages_count(&self) -> usize {
        self.marked_messages.len()
    }

    pub fn status_notice(&self) -> Option<&str> {
        self.status_notice.as_deref()
    }

    pub fn set_title_notice(&mut self, notice: Option<String>) {
        self.title_notice = notice;
    }

    pub fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    pub fn sync_requested(&self) -> bool {
        self.sync_requested
    }

    pub fn refresh_count(&self) -> u64 {
        self.refresh_count
    }

    pub fn take_scroll_to_latest_pending(&mut self) -> bool {
        std::mem::take(&mut self.scroll_to_latest_pending)
    }

    pub fn window_title(&self) -> String {
        let hub_notice = self.hub_status.title_notice();
        let notice = self.title_notice.as_deref().or(hub_notice.as_deref());
        ViewTab::window_title_with_status(
            self.active_view.as_ref(),
            &self.version,
            self.marked_messages.len(),
            notice,
        )
    }

    pub fn ctrl_badges_visible(&self) -> bool {
        self.ctrl_badges.visible()
    }

    pub fn on_ctrl_down(&mut self) -> bool {
        self.ctrl_badges.on_ctrl_down()
    }

    pub fn on_ctrl_up(&mut self) -> bool {
        self.ctrl_badges.on_ctrl_up()
    }

    pub fn on_window_blur(&mut self) -> bool {
        self.inbox_chord.reset();
        self.latest_chord.reset();
        self.ctrl_badges.on_blur()
    }

    pub fn chat_archive_status(&self, chat: &Chat) -> ArchiveStatus {
        if self.archive_table.get(&chat.key).is_some() {
            self.archive_table.status(&chat.key, chat.archived_remote)
        } else if chat.is_archived() {
            ArchiveStatus::Archived
        } else {
            ArchiveStatus::Incoming
        }
    }

    pub fn sidebar_rows(&self) -> Vec<SidebarChatRow> {
        let show_badges = self.ctrl_badges.visible();
        self.chats
            .iter()
            .enumerate()
            .map(|(idx, chat)| {
                let draft_preview = self
                    .drafts
                    .get(&chat.key)
                    .filter(|d| !d.text_body.is_empty())
                    .map(Draft::sidebar_preview);
                let archive_status = self.chat_archive_status(chat);
                SidebarChatRow {
                    chat: chat.clone(),
                    archive_status,
                    draft_preview,
                    bulk_selected: self.bulk_selected.contains(&chat.key),
                    active: self.selected_chat_idx == Some(idx),
                    slot_badge: if show_badges && idx < 9 {
                        Some((idx + 1) as u8)
                    } else {
                        None
                    },
                }
            })
            .collect()
    }

    fn persist_ui_settings(&self) {
        let settings = UiSettings {
            theme: self.theme,
            font_scale: self.font_scale,
            view_order: self.view_order.clone(),
            show_archived: self.show_archived_inline(),
        };
        let _ = self.store.save_ui_settings(&settings);
    }

    pub fn set_theme(&mut self, theme: ThemeChoice) {
        self.theme = theme;
        self.status_notice = Some(format!("Theme: {}", theme.label()));
        self.persist_ui_settings();
    }

    pub fn set_font_scale(&mut self, scale: FontScale) {
        self.font_scale = scale;
        self.status_notice = Some(format!("Text size: {}%", scale.percent()));
        self.persist_ui_settings();
    }

    pub fn font_larger(&mut self) {
        self.set_font_scale(self.font_scale.larger());
    }

    pub fn font_smaller(&mut self) {
        self.set_font_scale(self.font_scale.smaller());
    }

    pub fn font_reset(&mut self) {
        self.set_font_scale(self.font_scale.reset());
    }

    pub fn refresh_all(&mut self) {
        self.refresh_accounts();
        self.refresh_chats();
        self.refresh_count = self.refresh_count.saturating_add(1);
    }

    pub fn request_hub_refresh(&mut self) {
        self.refresh_all();
        let open_chat = self.selected_chat().map(|c| c.key.clone());
        self.emit_sync_command(SyncCommand::RefreshAll { open_chat });
    }

    pub fn request_restart_live_sync(&mut self) {
        self.sync_requested = true;
        self.status_notice = Some("Restarting live sync...".into());
        self.emit_sync_command(SyncCommand::RestartLiveSync);
    }

    /// Merges live accounts with `view_order`, preserving absent/drained saved accounts in exact `Alt+2..9` slots.
    pub fn refresh_accounts(&mut self) {
        let live_accounts = self.store.list_accounts().unwrap_or_default();
        self.accounts = merge_order(&self.view_order, &live_accounts);
        let slack_teams: Vec<String> = self
            .accounts
            .iter()
            .filter(|a| a.provider == Provider::Slack)
            .map(|a| a.account_id.clone())
            .collect();
        for team in slack_teams {
            if self.m3.workspace_icon_requested.insert(team.clone()) {
                self.emit_sync_command(SyncCommand::FetchWorkspaceIcon { team });
            }
        }
    }

    pub fn refresh_chats(&mut self) {
        let prev_key = self.selected_chat().map(|c| c.key.clone());
        let view = self.active_view.clone().unwrap_or(ViewTab::CombinedInbox);
        let include_archived = self.show_archived_inline();
        let mut chats = self
            .store
            .list_chats_filtered(
                &view,
                include_archived,
                self.show_archived_only,
                self.unread_only,
                MAX_CHATS_QUERY,
            )
            .unwrap_or_default();

        if !self.search_filter.trim().is_empty() {
            let needle = self.search_filter.trim().to_ascii_lowercase();
            chats.retain(|c| {
                c.title.to_ascii_lowercase().contains(&needle)
                    || c.key.id.to_ascii_lowercase().contains(&needle)
            });
        }

        self.chats = chats;

        let avatar_targets: Vec<ChatKey> = self
            .chats
            .iter()
            .take(24)
            .filter(|c| postbox_media::avatar::chat_avatar_enabled(c.key.provider))
            .map(|c| c.key.clone())
            .collect();
        for chat_key in avatar_targets {
            if self.m3.avatar_requested.insert(chat_key.clone()) {
                self.emit_sync_command(SyncCommand::FetchAvatar { chat: chat_key });
            }
        }

        let view_key = view.view_key();
        let remembered = self
            .view_selected_chat
            .get(&view_key)
            .cloned()
            .or_else(|| self.store.get_view_selected_chat(&view_key).ok().flatten());

        let fallback = (!self.chats.is_empty()).then_some(0);
        let new_idx = prev_key
            .as_ref()
            .and_then(|key| self.chats.iter().position(|c| &c.key == key))
            .or_else(|| {
                remembered
                    .as_ref()
                    .and_then(|key| self.chats.iter().position(|c| &c.key == key))
            })
            .or(fallback);

        let chat_changed =
            new_idx.and_then(|i| self.chats.get(i)).map(|c| &c.key) != prev_key.as_ref();

        self.selected_chat_idx = new_idx;
        if chat_changed {
            self.cancel_voice_recording_on_chat_switch();
            self.m4.pending_auto_advance = None;
            self.begin_transcription_open();
            self.loaded_message_limit = MAX_MESSAGES_QUERY;
            self.reply_to_msg_id = None;
            self.marked_messages.clear();
            self.m4.marked_anchor_idx = None;
            self.close_thread_panel();
            self.close_lightbox();
            self.close_composer_popup();
            if let Some(new_key) = new_idx
                .and_then(|i| self.chats.get(i))
                .map(|c| c.key.clone())
            {
                self.m3.staged_attachments.guard_chat_match(&new_key);
            } else {
                self.m3.staged_attachments.discard_all();
            }
            self.load_active_draft();
        }
        self.refresh_messages();
    }

    fn effective_replies_fold(&self, chat: &Chat) -> bool {
        if let Some(&override_val) = self.replies_fold_by_chat.get(&chat.key) {
            return override_val;
        }
        if let Ok(Some(saved)) = self.store.get_chat_replies_fold(&chat.key) {
            return saved;
        }
        if chat.key.provider == Provider::Slack {
            let acc_enabled = self
                .slack_threads_by_account
                .get(&chat.key.account)
                .copied()
                .unwrap_or_else(|| {
                    self.store
                        .get_account_slack_threads(Provider::Slack, &chat.key.account)
                        .unwrap_or(true)
                });
            return acc_enabled;
        }
        replies_fold(None, chat.key.provider)
    }

    pub fn chat_folds_replies(&self, chat: &Chat) -> bool {
        self.effective_replies_fold(chat)
    }

    pub fn refresh_messages(&mut self) {
        let Some(chat) = self.selected_chat().cloned() else {
            self.messages.clear();
            self.outbox_for_chat.clear();
            self.timeline_items.clear();
            self.selected_message_idx = None;
            return;
        };

        let prev_msg_id = self.selected_message().map(|m| m.msg_id.clone());
        // M5.5 RAM diet: only the active chat's timeline lives in RAM. Clear the previous
        // chat's vec before loading, so a chat switch never holds two message lists at once.
        self.messages.clear();
        self.messages.shrink_to_fit();
        self.messages = self
            .store
            .list_messages(&chat.key, self.loaded_message_limit, None)
            .unwrap_or_default();
        self.outbox_for_chat = self
            .store
            .list_outbox_for_chat(&chat.key)
            .unwrap_or_default();

        self.selected_message_idx = prev_msg_id
            .as_deref()
            .and_then(|id| self.messages.iter().position(|m| m.msg_id == id))
            .or_else(|| self.messages.len().checked_sub(1));

        self.reapply_pending_reactions(&chat.key);
        self.apply_voice_sent_echoes();
        self.rebuild_timeline();
        self.trigger_open_chat_media_and_metadata();
        self.schedule_chat_transcriptions();
    }

    pub(crate) fn rebuild_timeline(&mut self) {
        let Some(chat) = self.selected_chat() else {
            self.timeline_items.clear();
            return;
        };
        let fold = self.effective_replies_fold(chat);
        let outbox_map: BTreeMap<String, &OutboxRecord> = self
            .outbox_for_chat
            .iter()
            .map(|r| (r.client_id.clone(), r))
            .collect();
        self.timeline_items = build_timeline_items_with_options(
            &self.messages,
            self.selected_message_idx,
            &self.marked_messages,
            &TimelineOptions {
                provider: chat.key.provider,
                is_group: chat.is_group,
                fold_replies: fold,
                outbox_by_client_id: &outbox_map,
            },
        );
    }

    fn persist_active_draft(&mut self) {
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            return;
        };
        let existing_staged = self
            .drafts
            .get(&chat_key)
            .and_then(|d| d.staged_image_path.clone());
        // IGSH-497: preserve whitespace-only drafts; only delete when truly empty and no staged image.
        if self.composer_text.is_empty() && existing_staged.is_none() {
            self.drafts.remove(&chat_key);
            let _ = self.store.delete_draft(&chat_key);
        } else {
            let draft = Draft {
                chat: chat_key.clone(),
                text_body: self.composer_text.clone(),
                reply_to_id: self.reply_to_msg_id.clone(),
                staged_image_path: existing_staged,
                updated_at: now_epoch_secs(),
            };
            if self.store.save_draft(&draft).is_ok() {
                self.drafts.insert(chat_key, draft);
            }
        }
    }

    fn load_active_draft(&mut self) {
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            self.composer_text.clear();
            return;
        };
        if let Some(draft) = self.drafts.get(&chat_key) {
            self.composer_text = draft.text_body.clone();
            self.reply_to_msg_id = draft.reply_to_id.clone();
        } else {
            self.composer_text.clear();
        }
    }

    fn maybe_mark_selected_chat_read(&mut self) {
        let Some(mut chat) = self.selected_chat().cloned() else {
            return;
        };
        let has_unread = chat.unread_count > 0 || self.read_state.is_unread(&chat.key);
        if !should_auto_mark_read(self.user_viewed, has_unread) {
            return;
        }
        let newest_ts = self
            .messages
            .iter()
            .rev()
            .find_map(|m| unix_seconds(&m.ts))
            .or_else(|| unix_seconds(&chat.last_msg_ts))
            .unwrap_or_else(now_epoch_secs);

        if self.read_state.mark_read(&chat.key, newest_ts) {
            let wm = self.read_state.get(&chat.key);
            let _ = self.store.upsert_read_watermark(&chat.key, &wm);
        }

        if chat.unread_count > 0 {
            chat.unread_count = 0;
            let _ = self.store.upsert_chat(&chat, now_epoch_secs());
            if let Some(idx) = self.selected_chat_idx
                && let Some(row) = self.chats.get_mut(idx)
                && row.key == chat.key
            {
                row.unread_count = 0;
            }
        }

        if self.pending_reads.enqueue(chat.key.clone()) {
            let _ = self.store.save_pending_reads(&self.pending_reads);
            self.emit_sync_command(SyncCommand::WakeMarkRead);
        }
    }

    pub fn select_chat_index(&mut self, idx: usize) -> bool {
        if idx >= self.chats.len() {
            return false;
        }
        self.user_viewed = true;
        if self.selected_chat_idx == Some(idx) {
            self.maybe_mark_selected_chat_read();
            return true;
        }
        self.persist_active_draft();
        self.cancel_voice_recording_on_chat_switch();
        self.m4.pending_auto_advance = None;
        self.selected_chat_idx = Some(idx);
        let key = self.chats[idx].key.clone();
        let view_key = self.active_view_key();
        self.view_selected_chat
            .insert(view_key.clone(), key.clone());
        let _ = self.store.set_view_selected_chat(&view_key, Some(&key));

        self.begin_transcription_open();
        self.loaded_message_limit = MAX_MESSAGES_QUERY;
        self.reply_to_msg_id = None;
        self.marked_messages.clear();
        self.m4.marked_anchor_idx = None;
        self.close_thread_panel();
        self.close_lightbox();
        self.close_composer_popup();
        self.m3.staged_attachments.guard_chat_match(&key);
        self.load_active_draft();
        self.refresh_messages();
        self.maybe_mark_selected_chat_read();
        self.emit_sync_command(SyncCommand::FetchChatMessages {
            chat: key,
            limit: OLDER_PAGE_LIMIT,
        });
        self.scroll_to_latest_pending = true;
        self.observe_dictation_context();
        true
    }

    pub fn select_chat_key(&mut self, key: &ChatKey) -> bool {
        if let Some(idx) = self.chats.iter().position(|c| &c.key == key) {
            return self.select_chat_index(idx);
        }
        if let Ok(Some(chat)) = self.store.get_chat(key) {
            self.active_view = Some(ViewTab::CombinedInbox);
            self.search_filter.clear();
            self.unread_only = false;
            self.show_archived_only = chat.is_archived();
            if !chat.is_archived() && chat.unread_count == 0 && self.show_archived_inline() {
                self.show_archived_by_view.insert("all".to_owned(), false);
                let _ = self.store.set_view_show_archived("all", false);
            }
            self.refresh_chats();
            if let Some(idx) = self.chats.iter().position(|c| &c.key == key) {
                return self.select_chat_index(idx);
            }
        }
        false
    }

    pub fn open_chat_slot(&mut self, slot: u8) -> bool {
        if !(1..=9).contains(&slot) {
            return false;
        }
        let idx = (slot - 1) as usize;
        if self.select_chat_index(idx) {
            self.focus = FocusTarget::Composer;
            true
        } else {
            false
        }
    }

    pub fn next_chat(&mut self) {
        if self.chats.is_empty() {
            return;
        }
        let next = self
            .selected_chat_idx
            .map(|i| (i + 1).min(self.chats.len() - 1))
            .unwrap_or(0);
        self.select_chat_index(next);
    }

    pub fn prev_chat(&mut self) {
        if self.chats.is_empty() {
            return;
        }
        let prev = self
            .selected_chat_idx
            .map(|i| i.saturating_sub(1))
            .unwrap_or(0);
        self.select_chat_index(prev);
    }

    pub fn first_chat(&mut self) {
        if !self.chats.is_empty() {
            self.select_chat_index(0);
        }
    }

    pub fn last_chat(&mut self) {
        if let Some(last) = self.chats.len().checked_sub(1) {
            self.select_chat_index(last);
        }
    }

    pub fn page_up_chats(&mut self) {
        if self.chats.is_empty() {
            return;
        }
        let target = self
            .selected_chat_idx
            .unwrap_or(0)
            .saturating_sub(PAGE_STEP_CHATS);
        self.select_chat_index(target);
    }

    pub fn page_down_chats(&mut self) {
        if self.chats.is_empty() {
            return;
        }
        let target = self
            .selected_chat_idx
            .unwrap_or(0)
            .saturating_add(PAGE_STEP_CHATS)
            .min(self.chats.len() - 1);
        self.select_chat_index(target);
    }

    fn save_current_view_selection(&mut self) {
        let view_key = self.active_view_key();
        if let Some(chat) = self.selected_chat() {
            let key = chat.key.clone();
            self.view_selected_chat
                .insert(view_key.clone(), key.clone());
            let _ = self.store.set_view_selected_chat(&view_key, Some(&key));
        }
    }

    pub fn switch_to_combined_inbox(&mut self) {
        self.persist_active_draft();
        self.save_current_view_selection();
        self.user_viewed = false;
        self.active_view = Some(ViewTab::CombinedInbox);
        self.show_archived_only = false;
        self.bulk_selected.clear();
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some("Combined inbox".into());
    }

    pub fn switch_to_account_slot(&mut self, slot: u8) -> bool {
        if !(2..=9).contains(&slot) {
            return false;
        }
        let idx = (slot - 2) as usize;
        let Some(account) = self.accounts.get(idx).cloned() else {
            return false;
        };
        self.persist_active_draft();
        self.save_current_view_selection();
        self.user_viewed = false;
        self.active_view = Some(account.to_view_tab());
        self.show_archived_only = false;
        self.bulk_selected.clear();
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some(account.label);
        true
    }

    pub fn switch_to_view_key(&mut self, view_key: &str) -> bool {
        if view_key == "all" {
            self.switch_to_combined_inbox();
            return true;
        }
        let Some(account) = self
            .accounts
            .iter()
            .find(|a| a.view_key() == view_key)
            .cloned()
        else {
            return false;
        };
        self.persist_active_draft();
        self.save_current_view_selection();
        self.user_viewed = false;
        self.active_view = Some(account.to_view_tab());
        self.show_archived_only = false;
        self.bulk_selected.clear();
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some(account.label);
        true
    }

    pub fn toggle_unread_only(&mut self) {
        self.unread_only = !self.unread_only;
        self.user_viewed = false;
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some(if self.unread_only {
            "Showing unread chats".into()
        } else {
            "Showing all chats".into()
        });
    }

    pub fn toggle_archived_inline(&mut self) {
        let view_key = self.active_view_key();
        let next = !self.show_archived_inline();
        self.show_archived_by_view.insert(view_key.clone(), next);
        let _ = self.store.set_view_show_archived(&view_key, next);
        self.persist_ui_settings();
        self.user_viewed = false;
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some(if next {
            "Archived chats shown in this view".into()
        } else {
            "Archived chats hidden in this view".into()
        });
    }

    pub fn toggle_archived_only(&mut self) {
        self.show_archived_only = !self.show_archived_only;
        self.user_viewed = false;
        self.refresh_chats();
        self.observe_dictation_context();
        self.status_notice = Some(if self.show_archived_only {
            "Showing archived chats".into()
        } else {
            "Showing inbox chats".into()
        });
    }

    pub fn select_all_chats(&mut self) {
        let text_focused = matches!(
            self.focus,
            FocusTarget::Composer | FocusTarget::Palette | FocusTarget::ThreadComposer
        );
        let compose_focused = self.focus == FocusTarget::Composer;
        let compose_empty = self.composer_text.is_empty();
        if !ctrl_a_selects_chats(text_focused, compose_focused, compose_empty) {
            return;
        }
        self.bulk_selected = self.chats.iter().map(|c| c.key.clone()).collect();
        self.status_notice = Some(format!(
            "{} chats selected - Ctrl+E archives, Esc cancels",
            self.bulk_selected.len()
        ));
    }

    pub fn cancel_bulk_selection(&mut self) {
        if !self.bulk_selected.is_empty() {
            self.bulk_selected.clear();
            self.status_notice = Some("Selection cleared".into());
        }
    }

    pub fn archive_selected(&mut self) {
        let now_s = now_epoch_secs();
        let now_ms = now_epoch_millis();
        self.user_viewed = false;

        if !self.bulk_selected.is_empty() {
            let keys: Vec<ChatKey> = self.bulk_selected.iter().cloned().collect();
            let mut wake_wa = false;
            for key in &keys {
                if let Ok(Some(mut chat)) = self.store.get_chat(key) {
                    let baseline = archive_baseline(&chat.last_msg_ts, now_s);
                    self.archive_table.archive(key, &baseline, false);
                    chat.archived_local = true;
                    let _ = self.store.upsert_chat(&chat, now_s);
                    if let Some(entry) = self.archive_table.get(key) {
                        let _ = self.store.upsert_archive_entry(entry);
                    }
                    if key.provider == Provider::Whatsapp {
                        let _ = self.store.enqueue_pending_archive(
                            key,
                            PendingArchiveIntent::ArchivePending,
                            now_ms,
                        );
                        wake_wa = true;
                    }
                }
            }
            let count = keys.len();
            self.bulk_selected.clear();
            self.refresh_chats();
            if wake_wa {
                self.emit_sync_command(SyncCommand::WakeArchive);
            }
            self.status_notice = Some(format!("Archived {count} chats"));
            return;
        }

        let Some(mut chat) = self.selected_chat().cloned() else {
            return;
        };
        let next_archived = !(self.show_archived_only || chat.is_archived());
        if next_archived {
            let baseline = archive_baseline(&chat.last_msg_ts, now_s);
            self.archive_table.archive(&chat.key, &baseline, false);
            chat.archived_local = true;
            if let Some(entry) = self.archive_table.get(&chat.key) {
                let _ = self.store.upsert_archive_entry(entry);
            }
            if chat.key.provider == Provider::Whatsapp {
                let _ = self.store.enqueue_pending_archive(
                    &chat.key,
                    PendingArchiveIntent::ArchivePending,
                    now_ms,
                );
                self.emit_sync_command(SyncCommand::WakeArchive);
            }
        } else {
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
                if chat.archived_remote {
                    self.archive_table.set_local_unarchived(&chat.key);
                    if let Some(entry) = self.archive_table.get(&chat.key) {
                        let _ = self.store.upsert_archive_entry(entry);
                    }
                } else {
                    self.archive_table.remove(&chat.key);
                    let _ = self.store.delete_archive_entry(&chat.key);
                }
            }
            chat.archived_local = false;
            chat.archived_remote = false;
        }

        let _ = self.store.upsert_chat(&chat, now_s);
        self.refresh_chats();
        self.status_notice = Some(if next_archived {
            format!("Archived {}", chat.title)
        } else {
            format!("Unarchived {}", chat.title)
        });
    }

    pub fn keep_archived_selected(&mut self) {
        let now_s = now_epoch_secs();
        let now_ms = now_epoch_millis();
        self.user_viewed = false;

        if !self.bulk_selected.is_empty() {
            let keys: Vec<ChatKey> = self.bulk_selected.iter().cloned().collect();
            let mut wake_wa = false;
            for key in &keys {
                if let Ok(Some(mut chat)) = self.store.get_chat(key) {
                    let baseline = archive_baseline(&chat.last_msg_ts, now_s);
                    self.archive_table.archive(key, &baseline, true);
                    chat.archived_local = true;
                    let _ = self.store.upsert_chat(&chat, now_s);
                    if let Some(entry) = self.archive_table.get(key) {
                        let _ = self.store.upsert_archive_entry(entry);
                    }
                    if key.provider == Provider::Whatsapp {
                        let _ = self.store.enqueue_pending_archive(
                            key,
                            PendingArchiveIntent::ArchivePending,
                            now_ms,
                        );
                        wake_wa = true;
                    }
                }
            }
            let count = keys.len();
            self.bulk_selected.clear();
            self.refresh_chats();
            if wake_wa {
                self.emit_sync_command(SyncCommand::WakeArchive);
            }
            self.status_notice = Some(format!("Kept {count} chats archived"));
            return;
        }

        let Some(mut chat) = self.selected_chat().cloned() else {
            return;
        };
        let baseline = archive_baseline(&chat.last_msg_ts, now_s);
        self.archive_table.archive(&chat.key, &baseline, true);
        chat.archived_local = true;
        let _ = self.store.upsert_chat(&chat, now_s);
        if let Some(entry) = self.archive_table.get(&chat.key) {
            let _ = self.store.upsert_archive_entry(entry);
        }
        if chat.key.provider == Provider::Whatsapp {
            let _ = self.store.enqueue_pending_archive(
                &chat.key,
                PendingArchiveIntent::ArchivePending,
                now_ms,
            );
            self.emit_sync_command(SyncCommand::WakeArchive);
        }
        self.refresh_chats();
        self.status_notice = Some(format!("Kept {} archived", chat.title));
    }

    pub fn toggle_pin_selected(&mut self) {
        let Some(chat) = self.selected_chat().cloned() else {
            return;
        };
        let currently_pinned =
            self.store.is_pinned(&chat.key).unwrap_or(chat.pinned) || chat.pinned;
        if currently_pinned {
            let _ = self.store.unpin_chat(&chat.key);
            self.refresh_chats();
            self.status_notice = Some(format!("Unpinned {}", chat.title));
        } else {
            match self.store.pin_chat(&chat.key, now_epoch_secs()) {
                Ok(_) => {
                    self.refresh_chats();
                    self.status_notice = Some(format!("Pinned {}", chat.title));
                }
                Err(StoreError::PinLimitReached) => {
                    self.status_notice = Some("Pin limit reached (16 chats)".into());
                }
                Err(_) => {
                    self.status_notice = Some("Failed to pin chat".into());
                }
            }
        }
    }

    pub fn select_message_index(&mut self, idx: usize) -> bool {
        if idx >= self.messages.len() {
            return false;
        }
        self.selected_message_idx = Some(idx);
        self.rebuild_timeline();
        true
    }

    pub fn next_message(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let next = self
            .selected_message_idx
            .map(|i| (i + 1).min(self.messages.len() - 1))
            .unwrap_or(0);
        self.select_message_index(next);
    }

    pub fn prev_message(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let prev = self
            .selected_message_idx
            .map(|i| i.saturating_sub(1))
            .unwrap_or(self.messages.len() - 1);
        self.select_message_index(prev);
    }

    pub fn page_up_messages(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let at_top = self.selected_message_idx == Some(0);
        if at_top {
            self.request_older_messages();
            return;
        }
        let target = self
            .selected_message_idx
            .unwrap_or(self.messages.len() - 1)
            .saturating_sub(PAGE_STEP_MESSAGES);
        self.select_message_index(target);
        if target == 0 {
            self.request_older_messages();
        }
    }

    pub fn request_older_messages(&mut self) -> bool {
        let Some(chat) = self.selected_chat().map(|c| c.key.clone()) else {
            return false;
        };
        if self.exhausted_chats.contains(&chat) || self.messages.len() >= MAX_HISTORY_MESSAGES {
            return false;
        }
        let Some(oldest) = self.messages.first() else {
            return false;
        };
        let cursor = MessageCursor::from_message(oldest).to_hub_before_cursor();
        self.emit_sync_command(SyncCommand::FetchOlderMessages {
            chat,
            before_cursor: cursor,
            limit: OLDER_PAGE_LIMIT,
        });
        true
    }

    pub fn page_down_messages(&mut self) {
        if self.messages.is_empty() {
            return;
        }
        let target = self
            .selected_message_idx
            .unwrap_or(0)
            .saturating_add(PAGE_STEP_MESSAGES)
            .min(self.messages.len() - 1);
        self.select_message_index(target);
    }

    pub fn jump_to_latest(&mut self) {
        if let Some(last) = self.messages.len().checked_sub(1) {
            self.selected_message_idx = Some(last);
            self.rebuild_timeline();
        }
        self.scroll_to_latest_pending = true;
        self.status_notice = Some(LatestChord::JUMPED_STATUS.into());
    }

    pub fn toggle_mark_message(&mut self, msg_id: &str) {
        if !self.marked_messages.remove(msg_id) {
            self.marked_messages.insert(msg_id.to_owned());
        }
        if let Some(idx) = self.messages.iter().position(|m| m.msg_id == msg_id) {
            self.m4.marked_anchor_idx = Some(idx);
            self.selected_message_idx = Some(idx);
        }
        self.rebuild_timeline();
    }

    pub fn start_reply_selected(&mut self) {
        if let Some(msg) = self.selected_message().cloned() {
            if let Some(chat) = self.selected_chat()
                && self.effective_replies_fold(chat)
                && chat.key.provider == Provider::Slack
            {
                let root = local_thread_root(&self.messages, &msg, chat.key.provider)
                    .unwrap_or(msg.msg_id.as_str())
                    .to_owned();
                self.open_thread(&root);
                return;
            }
            self.reply_to_msg_id = Some(msg.msg_id);
            self.focus = FocusTarget::Composer;
            self.observe_dictation_context();
        }
    }

    // -------------------------------------------------------------------------
    // Right-Side Slack Thread Panel & Broadcast Toggle (IGSH-1898)
    // -------------------------------------------------------------------------

    pub fn open_thread_for_selected(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            return false;
        };
        let Some(chat) = self.selected_chat().cloned() else {
            return false;
        };
        let root = local_thread_root(&self.messages, &msg, chat.key.provider)
            .unwrap_or(msg.msg_id.as_str())
            .to_owned();
        self.open_thread(&root)
    }

    pub fn open_thread(&mut self, root_msg_id: &str) -> bool {
        let Some(chat) = self.selected_chat().cloned() else {
            return false;
        };
        if root_msg_id.is_empty() {
            return false;
        }
        self.thread_panel = Some(ThreadPanelState {
            chat: chat.key.clone(),
            root_msg_id: root_msg_id.to_owned(),
            messages: Vec::new(),
            timeline_items: Vec::new(),
            composer_text: String::new(),
            broadcast: false,
            focus_on_broadcast: false,
            exhausted: self.exhausted_chats.contains(&chat.key),
        });
        self.refresh_thread_panel_messages();
        self.focus = FocusTarget::ThreadComposer;
        self.observe_dictation_context();
        true
    }

    fn refresh_thread_panel_messages(&mut self) {
        let Some(panel) = self.thread_panel.as_ref() else {
            return;
        };
        let chat_key = panel.chat.clone();
        let root_id = panel.root_msg_id.clone();
        let is_group = self.selected_chat().is_some_and(|c| c.is_group);
        let all_msgs = self
            .store
            .list_messages(&chat_key, MAX_HISTORY_MESSAGES, None)
            .unwrap_or_default();
        let mut thread_msgs: Vec<Message> = all_msgs
            .iter()
            .filter(|m| {
                let parent = m.reply_to_id.as_deref().unwrap_or("");
                in_thread(&m.msg_id, parent, &root_id)
                    || local_thread_root(&all_msgs, m, chat_key.provider) == Some(root_id.as_str())
            })
            .cloned()
            .collect();
        for pending in self.m3.pending_reactions.entries() {
            if pending.chat == chat_key
                && let Some(msg) = thread_msgs.iter_mut().find(|m| m.msg_id == pending.msg_id)
            {
                msg.reactions = postbox_media::emoji::apply_optimistic_reaction(
                    &msg.reactions,
                    &pending.emoji,
                    pending.preserve_chip,
                );
            }
        }
        let outbox = self
            .store
            .list_outbox_for_chat(&chat_key)
            .unwrap_or_default();
        let outbox_map: BTreeMap<String, &OutboxRecord> =
            outbox.iter().map(|r| (r.client_id.clone(), r)).collect();
        let items = build_timeline_items_with_options(
            &thread_msgs,
            None,
            &self.marked_messages,
            &TimelineOptions {
                provider: chat_key.provider,
                is_group,
                fold_replies: false,
                outbox_by_client_id: &outbox_map,
            },
        );
        if let Some(panel) = &mut self.thread_panel {
            panel.messages = thread_msgs;
            panel.timeline_items = items;
        }
    }

    pub fn close_thread_panel(&mut self) {
        if let Some(mut panel) = self.thread_panel.take() {
            panel.broadcast = false;
            if matches!(
                self.focus,
                FocusTarget::ThreadComposer | FocusTarget::ThreadBroadcast
            ) {
                self.focus = FocusTarget::MessageList;
            }
            self.observe_dictation_context();
        }
    }

    pub fn set_thread_composer_text(&mut self, text: impl Into<String>) {
        let next = text.into();
        if let Some(panel) = &mut self.thread_panel {
            panel.composer_text = next.clone();
        }
        self.on_composer_user_edit(true, &next);
    }

    pub fn set_thread_broadcast(&mut self, enabled: bool) {
        let Some(panel) = &mut self.thread_panel else {
            return;
        };
        if enabled && !can_broadcast(panel.chat.provider, Some(panel.root_msg_id.as_str()), false) {
            panel.broadcast = false;
            return;
        }
        panel.broadcast = enabled;
    }

    pub fn toggle_thread_broadcast(&mut self) {
        if let Some(panel) = &self.thread_panel {
            let next = !panel.broadcast;
            self.set_thread_broadcast(next);
        }
    }

    pub fn cycle_thread_focus(&mut self) {
        let Some(panel) = &mut self.thread_panel else {
            return;
        };
        if panel.chat.provider != Provider::Slack {
            panel.focus_on_broadcast = false;
            self.focus = FocusTarget::ThreadComposer;
            return;
        }
        panel.focus_on_broadcast = !panel.focus_on_broadcast;
        self.focus = if panel.focus_on_broadcast {
            FocusTarget::ThreadBroadcast
        } else {
            FocusTarget::ThreadComposer
        };
    }

    pub fn request_older_thread_messages(&mut self) -> bool {
        let Some(panel) = self.thread_panel.as_ref() else {
            return false;
        };
        if panel.exhausted {
            return false;
        }
        let Some(oldest) = panel.messages.first() else {
            return false;
        };
        let chat = panel.chat.clone();
        let cursor = MessageCursor::from_message(oldest).to_hub_before_cursor();
        self.emit_sync_command(SyncCommand::FetchOlderMessages {
            chat,
            before_cursor: cursor,
            limit: OLDER_PAGE_LIMIT,
        });
        true
    }

    pub fn submit_thread_reply(&mut self) -> bool {
        let (chat_key, root_msg_id, text, broadcast) = {
            let Some(panel) = self.thread_panel.as_ref() else {
                return false;
            };
            let trimmed = panel.composer_text.trim();
            if trimmed.is_empty() {
                return false;
            }
            let allow_bc = panel.broadcast
                && can_broadcast(panel.chat.provider, Some(panel.root_msg_id.as_str()), false);
            (
                panel.chat.clone(),
                panel.root_msg_id.clone(),
                trimmed.to_owned(),
                allow_bc,
            )
        };

        let ok = self.enqueue_and_optimistic_send(&chat_key, &text, Some(root_msg_id), broadcast);
        if ok {
            if let Some(panel) = &mut self.thread_panel {
                panel.composer_text.clear();
                // IGSH-1898: auto-reset broadcast toggle after send!
                panel.broadcast = false;
            }
            self.refresh_thread_panel_messages();
        }
        ok
    }

    fn enqueue_and_optimistic_send(
        &mut self,
        chat_key: &ChatKey,
        trimmed: &str,
        reply_to_id: Option<String>,
        reply_broadcast: bool,
    ) -> bool {
        let Ok(Some(mut chat)) = self.store.get_chat(chat_key) else {
            return false;
        };
        self.user_viewed = true;

        let now_ms = now_epoch_millis();
        let now_s = now_epoch_secs();
        let client_id = new_client_id(&mut self.local_send_seq);
        let ts = format_utc_rfc3339(now_s);

        let rec = OutboxRecord::new_text(
            client_id.clone(),
            chat.key.clone(),
            trimmed.to_owned(),
            reply_to_id.clone(),
            reply_broadcast,
            now_ms,
        );
        if self.store.enqueue_outbox(&rec).is_err() {
            self.status_notice = Some("Outbox full; retry or discard failed sends".into());
            return false;
        }

        let msg = Message {
            chat: chat.key.clone(),
            msg_id: client_id,
            sender_id: "me".into(),
            sender_name: "You".into(),
            from_me: true,
            ts: ts.clone(),
            body: trimmed.to_owned(),
            reply_to_id,
            reply_broadcast,
            media_kind: None,
            mime_type: None,
            filename: None,
            file_size: None,
            reactions: Vec::new(),
            read_status: ReadStatus::Sending,
            deleted: false,
        };
        let _ = self.store.upsert_message(&msg);

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
        chat.last_msg_preview = trimmed.to_owned();
        let _ = self.store.upsert_chat(&chat, now_s);
        self.emit_sync_command(SyncCommand::WakeOutbox);
        self.refresh_chats();
        self.scroll_to_latest_pending = true;
        true
    }

    /// Submits the current composer text (and/or staged attachments) into the durable outbox and
    /// appends optimistic outgoing message(s) to SQLite so the timeline updates immediately.
    pub fn submit_composer_message(&mut self) -> bool {
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            return false;
        };
        let trimmed = self.composer_text.trim().to_owned();
        let reply_to = self.reply_to_msg_id.clone();

        if !self.m3.staged_attachments.is_empty() {
            if !self.m3.staged_attachments.guard_chat_match(&chat_key) {
                self.set_status_notice("Pasted image discarded");
            } else {
                if !self.submit_staged_attachments_to_outbox(&chat_key, &trimmed, reply_to) {
                    return false;
                }
                self.composer_text.clear();
                self.reply_to_msg_id = None;
                self.close_composer_popup();
                self.drafts.remove(&chat_key);
                let _ = self.store.delete_draft(&chat_key);
                return true;
            }
        }

        if trimmed.is_empty() {
            return false;
        }
        if !self.enqueue_and_optimistic_send(&chat_key, &trimmed, reply_to, false) {
            return false;
        }
        self.composer_text.clear();
        self.reply_to_msg_id = None;
        self.close_composer_popup();
        self.drafts.remove(&chat_key);
        let _ = self.store.delete_draft(&chat_key);
        true
    }

    /// Explicit user retry of a failed/held outbox message: mints a fresh 32-hex `client_id`
    /// (`SendQueue.tla` `UserRetry`), updates the local optimistic row, and wakes the outbox worker.
    pub fn retry_failed_message(&mut self, old_client_id: &str) -> bool {
        let new_cid = new_client_id(&mut self.local_send_seq);
        let now_ms = now_epoch_millis();
        let Ok(updated) =
            self.store
                .user_retry_outbox_with_new_client_id(old_client_id, &new_cid, now_ms)
        else {
            return false;
        };
        let (body, media_kind, mime_type, filename, file_size) = match updated.kind {
            postbox_core::outbox::OutboxKind::Voice => (
                format!(
                    "Voice message {}",
                    postbox_core::voice::format_voice_clock(
                        u64::from(updated.voice_seconds.max(1)) * 1000
                    )
                ),
                Some(postbox_core::MediaKind::Voice),
                Some("audio/ogg".to_owned()),
                Some(if updated.wire_filename.is_empty() {
                    "voice.ogg".to_owned()
                } else {
                    updated.wire_filename.clone()
                }),
                (updated.file_bytes > 0).then_some(updated.file_bytes),
            ),
            postbox_core::outbox::OutboxKind::File => (
                updated.text_body.clone(),
                Some(postbox_core::MediaKind::Document),
                None,
                (!updated.wire_filename.is_empty()).then(|| updated.wire_filename.clone()),
                (updated.file_bytes > 0).then_some(updated.file_bytes),
            ),
            _ => (updated.text_body.clone(), None, None, None, None),
        };
        let optimistic = Message {
            chat: updated.chat.clone(),
            msg_id: new_cid.clone(),
            sender_id: "me".into(),
            sender_name: "You".into(),
            from_me: true,
            ts: format_utc_rfc3339(now_epoch_secs()),
            body,
            reply_to_id: updated.reply_to_id.clone(),
            reply_broadcast: updated.reply_broadcast,
            media_kind,
            mime_type,
            filename,
            file_size,
            reactions: Vec::new(),
            read_status: ReadStatus::Sending,
            deleted: false,
        };
        let old_pair = (updated.chat.clone(), old_client_id.to_owned());
        let new_pair = (updated.chat.clone(), new_cid);
        if let Some(p) = self.m3.media_paths.remove(&old_pair) {
            self.m3.media_paths.insert(new_pair.clone(), p);
        }
        if let Some(s) = self.m3.media_statuses.remove(&old_pair) {
            self.m3.media_statuses.insert(new_pair.clone(), s);
        }
        if let Some(h) = self.m4.media_sha256.remove(&old_pair) {
            self.m4.media_sha256.insert(new_pair.clone(), h);
        }
        if let Some(w) = self.m4.waveforms.remove(&old_pair) {
            self.m4.waveforms.insert(new_pair, w);
        }
        let _ = self
            .store
            .replace_optimistic_message(&updated.chat, old_client_id, &optimistic);
        self.emit_sync_command(SyncCommand::WakeOutbox);
        self.refresh_messages();
        self.status_notice = Some("Retrying message...".into());
        true
    }

    pub fn retry_selected_failed_message(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            return false;
        };
        self.retry_failed_message(&msg.msg_id)
    }

    pub fn delete_selected_message_for_everyone(&mut self) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            return false;
        };
        if !msg.from_me {
            return false;
        }
        let client_id = new_client_id(&mut self.local_send_seq);
        self.emit_sync_command(SyncCommand::DeleteMessage {
            chat: msg.chat,
            msg_id: msg.msg_id,
            client_id,
        });
        self.status_notice = Some("Deleting message...".into());
        true
    }

    // -------------------------------------------------------------------------
    // Shortcuts Overlay (F1)
    // -------------------------------------------------------------------------

    pub fn shortcuts_open(&self) -> bool {
        self.shortcuts_open
    }

    /// Closes all mutually exclusive modal overlays so at most one modal can ever be open and
    /// `self.focus` always matches the active mounted overlay.
    pub(crate) fn dismiss_all_modals(&mut self) {
        if self.palette.open {
            let was_search = self.palette.mode == PaletteMode::SearchChats;
            self.palette.open = false;
            self.palette.query.clear();
            self.palette.selected_index = 0;
            if was_search && !self.search_filter.is_empty() {
                self.search_filter.clear();
                self.refresh_chats();
            }
        }
        self.shortcuts_open = false;
        self.shortcuts_return_focus = FocusTarget::ChatList;
        self.m3.lightbox = None;
        self.m3.emoji_picker = None;
        if self.m3.giphy_picker.is_open() {
            let gen_id = self.m3.giphy_picker.generation;
            self.m3.giphy_picker.cancel();
            self.m3.giphy_preview_paths.clear();
            self.emit_sync_command(SyncCommand::CancelGiphy { generation: gen_id });
        }
    }

    fn non_modal_focus_or_chat_list(&self) -> FocusTarget {
        match self.focus {
            FocusTarget::ChatList | FocusTarget::MessageList | FocusTarget::Composer => self.focus,
            FocusTarget::ThreadComposer | FocusTarget::ThreadBroadcast
                if self.thread_panel.is_some() =>
            {
                self.focus
            }
            _ => FocusTarget::ChatList,
        }
    }

    pub fn toggle_shortcuts(&mut self) {
        if self.shortcuts_open {
            self.close_shortcuts();
        } else {
            let return_focus = self.non_modal_focus_or_chat_list();
            self.dismiss_all_modals();
            self.shortcuts_return_focus = return_focus;
            self.shortcuts_open = true;
            self.focus = FocusTarget::ShortcutsOverlay;
        }
    }

    pub fn close_shortcuts(&mut self) {
        if self.shortcuts_open {
            self.shortcuts_open = false;
            let target = match self.shortcuts_return_focus {
                FocusTarget::ThreadComposer | FocusTarget::ThreadBroadcast
                    if self.thread_panel.is_none() =>
                {
                    FocusTarget::ChatList
                }
                other => other,
            };
            self.shortcuts_return_focus = FocusTarget::ChatList;
            self.focus = target;
        }
    }

    // -------------------------------------------------------------------------
    // Unified Ctrl+K Palette
    // -------------------------------------------------------------------------

    pub fn palette_open(&self) -> bool {
        self.palette.open
    }

    pub fn palette_mode(&self) -> PaletteMode {
        self.palette.mode
    }

    pub fn palette_query(&self) -> &str {
        &self.palette.query
    }

    pub fn palette_placeholder(&self) -> &'static str {
        self.palette.placeholder()
    }

    pub fn palette_entries(&self) -> &[PaletteEntry] {
        &self.palette.entries
    }

    pub fn palette_sections(&self) -> Vec<PaletteSection> {
        self.palette.sections()
    }

    pub fn palette_selected_index(&self) -> usize {
        self.palette.selected_index
    }

    pub fn open_palette(&mut self) {
        self.open_palette_with_mode(PaletteMode::Normal);
    }

    pub fn open_search_palette(&mut self) {
        self.open_palette_with_mode(PaletteMode::SearchChats);
    }

    pub fn open_sort_views_palette(&mut self) {
        self.open_palette_with_mode(PaletteMode::SortViews);
    }

    pub fn open_links_palette(&mut self) {
        self.open_palette_with_mode(PaletteMode::Links);
    }

    fn open_palette_with_mode(&mut self, mode: PaletteMode) {
        self.dismiss_all_modals();
        self.palette.open = true;
        self.palette.mode = mode;
        self.palette.query.clear();
        self.palette.selected_index = 0;
        self.palette.archive_snapshot = self
            .selected_chat()
            .map(|c| (c.key.clone(), c.is_archived()));
        self.focus = FocusTarget::Palette;
        if mode == PaletteMode::Normal {
            self.emit_sync_command(SyncCommand::RefreshMicrophones);
        }
        self.rebuild_palette_entries();
    }

    pub fn close_palette(&mut self) {
        if !self.palette.open {
            return;
        }
        let was_search = self.palette.mode == PaletteMode::SearchChats;
        self.palette.open = false;
        self.palette.query.clear();
        self.palette.selected_index = 0;
        self.focus = FocusTarget::ChatList;
        if was_search && !self.search_filter.is_empty() {
            self.search_filter.clear();
            self.refresh_chats();
        }
    }

    pub fn set_palette_query(&mut self, query: &str) {
        self.palette.query = query.to_owned();
        self.palette.selected_index = 0;
        if self.palette.mode == PaletteMode::SearchChats {
            self.user_viewed = false;
            self.search_filter = query.trim().to_owned();
            self.refresh_chats();
        }
        self.rebuild_palette_entries();
    }

    pub fn palette_next(&mut self) {
        self.palette.move_next();
    }

    pub fn palette_prev(&mut self) {
        self.palette.move_prev();
    }

    pub fn select_palette_section_row(&mut self, section: usize, row: usize) -> bool {
        if let Some(flat_idx) = self.palette.flat_index_for_section_row(section, row) {
            self.palette.selected_index = flat_idx;
            true
        } else {
            false
        }
    }

    pub fn palette_selected_section_row(&self) -> Option<(usize, usize)> {
        self.palette
            .section_row_for_flat_index(self.palette.selected_index)
    }

    pub fn palette_move_view_up(&mut self) -> bool {
        self.move_palette_view(-1)
    }

    pub fn palette_move_view_down(&mut self) -> bool {
        self.move_palette_view(1)
    }

    fn move_palette_view(&mut self, delta: isize) -> bool {
        if !self.palette.open || self.palette.mode != PaletteMode::SortViews {
            return false;
        }
        let Some(PaletteEntryKind::AccountView { view_key }) =
            self.palette.selected_entry().map(|e| &e.kind).cloned()
        else {
            return false;
        };
        let Some(pos) = self.accounts.iter().position(|a| a.view_key() == view_key) else {
            return false;
        };
        let target = pos as isize + delta;
        if target < 0 || (target as usize) >= self.accounts.len() {
            return false;
        }
        let target = target as usize;
        self.accounts.swap(pos, target);
        self.view_order = save_order(&self.view_order, &self.accounts);
        let _ = self.store.save_view_order(&self.view_order);
        self.persist_ui_settings();
        self.rebuild_palette_entries();
        if let Some(new_sel) = self.palette.entries.iter().position(|e| {
            matches!(
                &e.kind,
                PaletteEntryKind::AccountView { view_key: k } if k == &view_key
            )
        }) {
            self.palette.selected_index = new_sel;
        }
        true
    }

    pub(crate) fn rebuild_palette_entries(&mut self) {
        let selected_chat = self.selected_chat();
        let slack_threads_enabled = selected_chat
            .filter(|c| c.key.provider == Provider::Slack)
            .map(|c| {
                self.slack_threads_by_account
                    .get(&c.key.account)
                    .copied()
                    .unwrap_or_else(|| {
                        self.store
                            .get_account_slack_threads(Provider::Slack, &c.key.account)
                            .unwrap_or(true)
                    })
            })
            .unwrap_or(true);
        let slack_reply_style = selected_chat
            .map(|c| self.effective_replies_fold(c))
            .unwrap_or(false);

        let catalog_ctx = PaletteCatalogContext {
            version: &self.version,
            hub_status: &self.hub_status,
            theme: self.theme,
            font_scale: self.font_scale,
            dictation_language: self.dictation_language,
            playback_speed_pct: self.playback_speed_pct,
            show_archived_inline: self.show_archived_inline(),
            show_archived_only: self.show_archived_only,
            slack_threads_enabled,
            slack_reply_style,
            accounts: &self.accounts,
            microphones: &self.m4.microphones,
            selected_microphone: self.m4.selected_microphone.as_deref(),
            selected_chat,
            selected_message: self.selected_message(),
            bulk_selected_count: self.bulk_selected.len(),
            marked_messages_count: self.marked_voice_messages_count(),
            update_label: self.m7.update_label(),
        };
        let commands = build_palette_items(&catalog_ctx);

        let trimmed = self.palette.query.trim();
        let archived_chats = if !trimmed.is_empty()
            && matches!(
                self.palette.mode,
                PaletteMode::Normal | PaletteMode::SearchChats
            ) {
            self.store
                .list_chats_filtered(&ViewTab::CombinedInbox, true, true, false, MAX_CHATS_QUERY)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let fts_hits = if !trimmed.is_empty()
            && matches!(
                self.palette.mode,
                PaletteMode::Normal | PaletteMode::SearchChats
            ) {
            self.store
                .search_messages_fts(trimmed, MAX_FTS_HITS)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let links = if self.palette.mode == PaletteMode::Links {
            self.selected_message()
                .map(extract_links_and_media)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        self.palette.entries = filter_palette(PaletteFilterInput {
            mode: self.palette.mode,
            query: &self.palette.query,
            commands: &commands,
            visible_chats: &self.chats,
            archived_chats: &archived_chats,
            fts_hits: &fts_hits,
            accounts: &self.accounts,
            links: &links,
        });
        if self.palette.selected_index >= self.palette.entries.len() {
            self.palette.selected_index = 0;
        }
    }

    pub fn execute_palette_selected(&mut self) -> bool {
        let Some(entry) = self.palette.selected_entry().cloned() else {
            return false;
        };
        self.execute_palette_entry(&entry)
    }

    pub fn execute_palette_section_row(&mut self, section: usize, row: usize) -> bool {
        let Some(entry) = self.palette.entry_for_section_row(section, row).cloned() else {
            return false;
        };
        self.execute_palette_entry(&entry)
    }

    pub fn execute_palette_entry(&mut self, entry: &PaletteEntry) -> bool {
        let archive_snapshot = self.palette.archive_snapshot.clone();
        match &entry.kind {
            PaletteEntryKind::Chat { key, .. } => {
                self.palette.open = false;
                self.palette.query.clear();
                self.search_filter.clear();
                self.refresh_chats();
                self.focus = FocusTarget::Composer;
                self.select_chat_key(key)
            }
            PaletteEntryKind::MessageHit { chat, msg_id, .. } => {
                self.palette.open = false;
                self.palette.query.clear();
                self.search_filter.clear();
                self.refresh_chats();
                self.select_chat_key(chat);
                if let Some(idx) = self.messages.iter().position(|m| m.msg_id == *msg_id) {
                    self.select_message_index(idx);
                }
                self.focus = FocusTarget::MessageList;
                true
            }
            PaletteEntryKind::AccountView { view_key } => {
                self.palette.open = false;
                self.palette.query.clear();
                self.focus = FocusTarget::ChatList;
                self.switch_to_view_key(view_key)
            }
            PaletteEntryKind::Link { target } => {
                self.palette.open = false;
                self.palette.query.clear();
                self.focus = FocusTarget::MessageList;
                self.status_notice = Some(format!("Selected link: {target}"));
                true
            }
            PaletteEntryKind::Command(cmd) => {
                self.execute_palette_command(cmd, archive_snapshot.as_ref())
            }
        }
    }

    fn execute_palette_command(
        &mut self,
        cmd: &PaletteCommand,
        archive_snapshot: Option<&(ChatKey, bool)>,
    ) -> bool {
        match cmd {
            PaletteCommand::SearchChats => {
                self.open_search_palette();
                return true;
            }
            PaletteCommand::SortViews => {
                self.open_sort_views_palette();
                return true;
            }
            PaletteCommand::OpenLinksInMessage => {
                self.open_links_palette();
                return true;
            }
            PaletteCommand::ShowShortcuts => {
                let return_focus = self.non_modal_focus_or_chat_list();
                self.dismiss_all_modals();
                self.shortcuts_return_focus = return_focus;
                self.shortcuts_open = true;
                self.focus = FocusTarget::ShortcutsOverlay;
                return true;
            }
            PaletteCommand::Dictate => {
                self.palette.open = false;
                self.palette.query.clear();
                self.toggle_dictation(true);
                return true;
            }
            _ => {
                self.palette.open = false;
                self.palette.query.clear();
                self.focus = FocusTarget::ChatList;
            }
        }

        match cmd {
            PaletteCommand::ComposeMessage => {
                self.set_focus(FocusTarget::Composer);
            }
            PaletteCommand::ToggleSlackThreadsForAccount => {
                if let Some(chat) = self.selected_chat().cloned()
                    && chat.key.provider == Provider::Slack
                {
                    let cur = self
                        .slack_threads_by_account
                        .get(&chat.key.account)
                        .copied()
                        .unwrap_or_else(|| {
                            self.store
                                .get_account_slack_threads(Provider::Slack, &chat.key.account)
                                .unwrap_or(true)
                        });
                    let next = !cur;
                    self.slack_threads_by_account
                        .insert(chat.key.account.clone(), next);
                    let _ = self.store.set_account_slack_threads(
                        Provider::Slack,
                        &chat.key.account,
                        next,
                    );
                    self.rebuild_timeline();
                    self.status_notice = Some(if next {
                        "Slack threads enabled".into()
                    } else {
                        "Slack threads disabled".into()
                    });
                }
            }
            PaletteCommand::VersionInfo => {
                self.status_notice = Some(format!("Postbox v{}", self.version));
            }
            PaletteCommand::HubStatusInfo => match &self.hub_status {
                HubConnectionStatus::Connected { .. } => {
                    self.request_hub_refresh();
                    self.status_notice = Some("Refreshed Hub".into());
                }
                HubConnectionStatus::Reconnecting { .. } | HubConnectionStatus::Error { .. } => {
                    self.request_restart_live_sync();
                }
                HubConnectionStatus::Connecting { url } => {
                    self.status_notice = Some(format!("Connecting to {url}..."));
                }
                HubConnectionStatus::NotConfigured { .. } => {
                    self.status_notice = Some(
                        "Set POSTBOX_HUB_URL / WAZIG_HUB_URL and POSTBOX_HUB_TOKEN / WAZIG_HUB_TOKEN"
                            .into(),
                    );
                }
            },
            PaletteCommand::Dictate => {
                self.toggle_dictation(true);
            }
            PaletteCommand::RecordVoiceNote => {
                self.toggle_voice_recording(true);
            }
            PaletteCommand::SetDictationLanguage(lang) => {
                self.set_dictation_language(*lang);
            }
            PaletteCommand::SelectMicrophoneDefault => {
                self.select_microphone(None);
            }
            PaletteCommand::SelectMicrophone(name) => {
                self.select_microphone(Some(name.clone()));
            }
            PaletteCommand::SetPlaybackSpeed(speed) => {
                self.set_playback_speed(postbox_core::voice::PlaybackSpeed::from_percent(*speed));
            }
            PaletteCommand::FontSmaller => self.font_smaller(),
            PaletteCommand::FontLarger => self.font_larger(),
            PaletteCommand::FontReset => self.font_reset(),
            PaletteCommand::SetFontPreset(scale) => self.set_font_scale(*scale),
            PaletteCommand::SetTheme(theme) => self.set_theme(*theme),
            PaletteCommand::ToggleUnreadChats => self.toggle_unread_only(),
            PaletteCommand::ShowCombinedInbox => self.switch_to_combined_inbox(),
            PaletteCommand::ShowAccountView(view_key) => {
                self.switch_to_view_key(view_key);
            }
            PaletteCommand::TogglePinSelected => self.toggle_pin_selected(),
            PaletteCommand::ToggleArchivedInline => self.toggle_archived_inline(),
            PaletteCommand::ArchiveBulkSelected => self.archive_selected(),
            PaletteCommand::CancelBulkSelection => self.cancel_bulk_selection(),
            PaletteCommand::SetReplyStyleSlack(slack) => {
                if let Some(chat) = self.selected_chat().cloned() {
                    self.replies_fold_by_chat.insert(chat.key.clone(), *slack);
                    let _ = self.store.set_chat_replies_fold(&chat.key, Some(*slack));
                    self.rebuild_timeline();
                }
                self.status_notice = Some(if *slack {
                    "Replies: Slack style".into()
                } else {
                    "Replies: WhatsApp style".into()
                });
            }
            PaletteCommand::ToggleArchivedOnlyView => self.toggle_archived_only(),
            PaletteCommand::ArchiveOrUnarchiveSelected { expected_archived } => {
                let current_matches = self.selected_chat().is_some_and(|chat| {
                    let same_chat = archive_snapshot
                        .map(|(k, _)| k == &chat.key)
                        .unwrap_or(true);
                    same_chat && chat.is_archived() == *expected_archived
                });
                if current_matches {
                    self.archive_selected();
                } else {
                    self.status_notice = Some("Archive state changed; skipped stale action".into());
                }
            }
            PaletteCommand::KeepSelectedArchived => self.keep_archived_selected(),
            PaletteCommand::ReplyToSelected => self.start_reply_selected(),
            PaletteCommand::ResendFailedMessage => {
                if !self.retry_selected_failed_message() {
                    self.status_notice = Some("No failed message selected".into());
                }
            }
            PaletteCommand::DeleteMessageForEveryone => {
                if !self.delete_selected_message_for_everyone() {
                    self.status_notice = Some("Select one of your own messages to delete".into());
                }
            }
            PaletteCommand::SelectNextMessage => self.next_message(),
            PaletteCommand::SelectPrevMessage => self.prev_message(),
            PaletteCommand::JumpToLatest => self.jump_to_latest(),
            PaletteCommand::NextChat => self.next_chat(),
            PaletteCommand::PrevChat => self.prev_chat(),
            PaletteCommand::SendMessage => {
                self.submit_composer_message();
            }
            PaletteCommand::Refresh => {
                self.request_hub_refresh();
                self.status_notice = Some("Refreshed".into());
            }
            PaletteCommand::RestartLiveSync => {
                self.request_restart_live_sync();
            }
            PaletteCommand::ManageAccounts => {
                self.status_notice = Some("Manage accounts on the hub".into());
            }
            PaletteCommand::Quit => {
                self.quit_requested = true;
            }
            PaletteCommand::PickMention => {
                if !self.accept_composer_popup() {
                    self.status_notice = Some("No active mention to pick".into());
                }
            }
            PaletteCommand::ReactToSelected => {
                self.open_reaction_picker();
            }
            PaletteCommand::InsertEmoji => {
                self.open_insert_emoji_picker();
            }
            PaletteCommand::SendGif => {
                self.open_giphy_picker();
            }
            PaletteCommand::QuickReact(emoji_opt) => {
                if let Some(msg) = self.selected_message().cloned() {
                    let emoji = emoji_opt.unwrap_or("");
                    self.react_to_message_with_chip(&msg.chat, &msg.msg_id, emoji, false);
                }
            }
            PaletteCommand::OpenImageExternal => {
                self.open_selected_image_externally();
            }
            PaletteCommand::CopyMessageText => {
                self.copy_selected_message_text();
            }
            PaletteCommand::CopyTranscript => {
                self.copy_selected_transcript();
            }
            PaletteCommand::CopyAllTranscripts => {
                self.copy_all_marked_transcripts();
            }
            PaletteCommand::PlayPauseVoice => {
                self.play_or_pause_selected_voice();
            }
            PaletteCommand::ToggleTranscript => {
                self.toggle_selected_transcript();
            }
            PaletteCommand::PlayVideoInline => {
                self.request_video_player_open_selected();
            }
            PaletteCommand::CheckForUpdates => {
                // The desktop shell kicks off the real check; headless keeps a visible notice.
                self.status_notice = Some(self.m7.update_label().to_owned());
                self.update_check_requested = true;
            }
            PaletteCommand::CopyMessageLink
            | PaletteCommand::OpenVideoExternal
            | PaletteCommand::AttachFile => {
                self.status_notice = Some("Command queued".into());
            }
            PaletteCommand::SearchChats
            | PaletteCommand::SortViews
            | PaletteCommand::OpenLinksInMessage
            | PaletteCommand::ShowShortcuts => unreachable!(),
        }
        true
    }

    // -------------------------------------------------------------------------
    // Chords, Esc Cascade & Action Dispatch
    // -------------------------------------------------------------------------

    pub fn handle_inbox_chord_key(
        &mut self,
        key: char,
        now_ms: u64,
        repeated: bool,
        modifiers_active: bool,
    ) -> bool {
        let allowed = !modifiers_active
            && !self.palette.open
            && !self.shortcuts_open
            && self.m3.lightbox.is_none()
            && self.m3.emoji_picker.is_none()
            && !self.m3.giphy_picker.is_open()
            && matches!(self.focus, FocusTarget::ChatList | FocusTarget::MessageList);
        if self.inbox_chord.on_key(key, now_ms, allowed, repeated) {
            self.switch_to_combined_inbox();
            true
        } else {
            false
        }
    }

    pub fn handle_alt_g(&mut self, now_ms: u64, repeated: bool) -> LatestChordOutcome {
        let outcome = self.latest_chord.on_alt_g(now_ms, repeated);
        match outcome {
            LatestChordOutcome::Ignored => {}
            LatestChordOutcome::ArmedHint => {
                self.status_notice = Some(LatestChord::HINT_STATUS.into());
            }
            LatestChordOutcome::JumpLatest => {
                self.jump_to_latest();
            }
        }
        outcome
    }

    pub fn reset_latest_chord(&mut self) {
        self.latest_chord.reset();
    }

    pub fn reset_inbox_chord(&mut self) {
        self.inbox_chord.reset();
    }

    pub fn reset_chords(&mut self) {
        self.inbox_chord.reset();
        self.latest_chord.reset();
    }

    // -------------------------------------------------------------------------
    // Milestone 7: deep links, status cards and the updater
    // -------------------------------------------------------------------------

    /// Applies a `postbox://` link or a `focus` signal and reports what the shell should do.
    ///
    /// Selecting the chat is done here, so the caller only has to raise the window. A link to
    /// a chat that is not on disk leaves a dismissible card instead of failing the launch.
    pub fn open_deep_link(&mut self, line: &str, now_ms: u64) -> crate::m7::LinkAction {
        let action = self.m7.accept_link(line, now_ms, |target| {
            self.store.get_chat(&target.chat).ok().flatten().is_some()
                || self.chats.iter().any(|chat| chat.key == target.chat)
        });
        if let Some(target) = action.open.clone() {
            self.focus_deep_link_chat(&target);
        }
        action
    }

    fn focus_deep_link_chat(&mut self, target: &DeepLinkTarget) {
        // A cross-account link must switch the view first, otherwise the chat list cannot
        // contain the chat and `select_chat_key` gives up.
        let view_key = format!("{}:{}", target.chat.provider.as_str(), target.chat.account);
        let already_in_view = self
            .active_view()
            .map(|view| view.view_key() == view_key)
            .unwrap_or(false);
        if !already_in_view {
            let _ = self.switch_to_view_key(&view_key);
        }
        if !self.select_chat_key(&target.chat) {
            // Fall back to the combined inbox, which lists every account.
            self.switch_to_combined_inbox();
            let _ = self.select_chat_key(&target.chat);
        }
        if let Some(message) = target.message.as_deref()
            && let Some(idx) = self
                .messages
                .iter()
                .position(|msg| msg.msg_id == message && msg.chat == target.chat)
        {
            self.select_message_index(idx);
            self.marked_messages.insert(message.to_owned());
            self.rebuild_timeline();
            self.status_notice = Some("Opening linked message".into());
        }
        self.focus = FocusTarget::MessageList;
    }

    /// The status card currently shown, if any.
    pub fn status_card(&self) -> Option<&crate::m7::StatusCard> {
        self.m7.card.as_ref()
    }

    /// Dismisses the status card. Returns `true` when something was dismissed.
    pub fn close_status_card(&mut self) -> bool {
        self.m7.close_card()
    }

    /// Expires a stale card; the shell calls this on its render tick.
    pub fn tick_status_card(&mut self, now_ms: u64) -> bool {
        self.m7.tick(now_ms)
    }

    /// Records the newest exit record so the shell can offer a crash note.
    pub fn set_last_exit_record(&mut self, record: Option<postbox_core::crash::ExitRecord>) {
        self.m7.last_exit_record = record;
    }

    /// `true` once, when the user asked for an update check from `Ctrl+K`.
    pub fn take_update_check_request(&mut self) -> bool {
        std::mem::take(&mut self.update_check_requested)
    }

    /// The palette label for the update command, so it reads as state rather than an action
    /// once a check has run.
    pub fn update_command_label(&self) -> &'static str {
        self.m7.update_label()
    }

    pub fn m7(&self) -> &crate::m7::M7State {
        &self.m7
    }

    pub fn m7_mut(&mut self) -> &mut crate::m7::M7State {
        &mut self.m7
    }

    /// Applies an update manifest and returns what to do next.
    pub fn check_for_updates(
        &mut self,
        manifest: &postbox_core::update::UpdateManifest,
    ) -> postbox_core::update::CheckOutcome {
        let current = postbox_core::update::Version::parse(self.version())
            .unwrap_or_else(|| postbox_core::update::Version::new(0, 0, 0));
        let outcome = self.m7.apply_manifest(manifest, current);
        self.status_notice = self.m7.update_note.clone();
        outcome
    }

    /// `true` when the current chat is the one an arriving message belongs to and the window
    /// has focus, so the toast policy can stay silent.
    pub fn focused_chat_key(&self) -> Option<&ChatKey> {
        self.selected_chat().map(|chat| &chat.key)
    }

    /// Raises a toast for a new message unless the policy says it is redundant.
    pub fn notify_incoming(
        &mut self,
        message: &Message,
        ts_seconds: i64,
        window_focused: bool,
        notifier: &dyn postbox_platform::Notifier,
    ) -> bool {
        let focused_chat = self.selected_chat().map(|chat| chat.key.clone());
        let focus = postbox_core::toast::ToastFocus {
            window_focused,
            chat: focused_chat.as_ref(),
        };
        if self
            .notification_gate
            .check(&message.chat, ts_seconds, message.from_me, focus)
            .is_some()
        {
            return false;
        }
        let title = message.sender_name.trim();
        let body = postbox_core::toast::preview_body(title, &message.body);
        notifier.notify_message(&message.chat, title, &body).is_ok()
    }

    /// Seeds the notification gate from the chats already on screen, so a fresh launch does
    /// not replay a toast for every unread chat.
    pub fn prime_notifications(&mut self) {
        for chat in &self.chats {
            if let Some(ts) = postbox_core::archive::unix_seconds(&chat.last_msg_ts) {
                self.notification_gate.prime(&chat.key, ts);
            }
        }
    }

    /// Exact Postbox `Esc` cascade (`main.zig:21127-21132`, `main.zig:21364-21513`).
    pub fn handle_escape(&mut self) {
        // M7: a status card is the outermost, least modal thing on screen.
        if self.m7.close_card() {
            return;
        }
        if self.m3.lightbox.is_some() {
            self.close_lightbox();
            return;
        }
        if self.m3.emoji_picker.is_some() {
            self.close_emoji_picker();
            return;
        }
        if self.m3.giphy_picker.is_open() {
            self.close_giphy_picker();
            return;
        }
        if self.close_composer_popup() {
            return;
        }
        if self.shortcuts_open {
            self.close_shortcuts();
            return;
        }
        if self.palette.open {
            self.close_palette();
            return;
        }
        if self.m4.recording.is_active() {
            self.cancel_voice_recording("Voice note cancelled");
            return;
        }
        if self.is_dictation_active() {
            self.cancel_dictation();
            return;
        }
        if self.thread_panel.is_some() {
            self.close_thread_panel();
            return;
        }
        if !self.marked_messages.is_empty() {
            self.marked_messages.clear();
            self.rebuild_timeline();
            return;
        }
        if !self.bulk_selected.is_empty() {
            self.bulk_selected.clear();
            self.status_notice = Some("Selection cleared".into());
            return;
        }
        if self.chats.is_empty()
            && (!self.search_filter.is_empty()
                || self.unread_only
                || self.show_archived_only
                || !matches!(self.active_view, Some(ViewTab::CombinedInbox)))
        {
            self.search_filter.clear();
            self.unread_only = false;
            self.show_archived_only = false;
            self.active_view = Some(ViewTab::CombinedInbox);
            self.refresh_chats();
            return;
        }
        if self.reply_to_msg_id.is_some() {
            self.reply_to_msg_id = None;
            self.status_notice = Some("Reply cancelled".into());
            return;
        }
        if self.discard_staged_attachments() {
            return;
        }
        if !self.search_filter.is_empty() {
            self.search_filter.clear();
            self.refresh_chats();
            return;
        }
        if self.focus == FocusTarget::Composer {
            self.focus = FocusTarget::ChatList;
        }
    }

    pub fn handle_action(&mut self, action: Action, now_ms: u64) {
        match action {
            Action::OpenPalette => self.open_palette(),
            Action::ShowShortcutsHelp => self.toggle_shortcuts(),
            Action::SearchChats => self.open_search_palette(),
            Action::ClearSearchOrBack => self.handle_escape(),
            Action::ToggleUnread => self.toggle_unread_only(),
            Action::ToggleArchived => self.toggle_archived_inline(),
            Action::ArchiveSelected => self.archive_selected(),
            Action::KeepArchivedSelected => self.keep_archived_selected(),
            Action::SelectAllChats => self.select_all_chats(),
            Action::Refresh => self.request_hub_refresh(),
            Action::RestartLiveSync => self.request_restart_live_sync(),
            Action::Quit => self.quit_requested = true,
            Action::NextChat => self.next_chat(),
            Action::PrevChat => self.prev_chat(),
            Action::FirstChat => self.first_chat(),
            Action::LastChat => self.last_chat(),
            Action::PageUpChats => self.page_up_chats(),
            Action::PageDownChats => self.page_down_chats(),
            Action::OpenChatSlot(slot) => {
                self.open_chat_slot(slot);
            }
            Action::SwitchCombinedInbox => self.switch_to_combined_inbox(),
            Action::SwitchAccountView(slot) => {
                self.switch_to_account_slot(slot);
            }
            Action::NextMessage => self.next_message(),
            Action::PrevMessage => self.prev_message(),
            Action::PageUpMessages => self.page_up_messages(),
            Action::PageDownMessages => self.page_down_messages(),
            Action::JumpLatest => self.jump_to_latest(),
            Action::ReactToMessage => {
                self.open_reaction_picker();
            }
            Action::ReplyToMessage => self.start_reply_selected(),
            Action::OpenThread => {
                self.open_thread_for_selected();
            }
            Action::ResendFailedMessage => {
                self.retry_selected_failed_message();
            }
            Action::OpenImageLightbox => {
                self.open_selected_image_lightbox(now_ms);
            }
            Action::CloseLightbox => self.close_lightbox(),
            Action::PrevLightboxImage => {
                self.lightbox_prev();
            }
            Action::NextLightboxImage => {
                self.lightbox_next();
            }
            Action::EmojiMoveUp => {
                if self.m3.giphy_picker.is_open() {
                    self.move_giphy_picker(-1, 0);
                } else {
                    self.move_emoji_picker(-1, 0);
                }
            }
            Action::EmojiMoveDown => {
                if self.m3.giphy_picker.is_open() {
                    self.move_giphy_picker(1, 0);
                } else {
                    self.move_emoji_picker(1, 0);
                }
            }
            Action::EmojiMoveLeft => {
                if self.m3.giphy_picker.is_open() {
                    self.move_giphy_picker(0, -1);
                } else {
                    self.move_emoji_picker(0, -1);
                }
            }
            Action::EmojiMoveRight => {
                if self.m3.giphy_picker.is_open() {
                    self.move_giphy_picker(0, 1);
                } else {
                    self.move_emoji_picker(0, 1);
                }
            }
            Action::EmojiActivate => {
                if self.m3.giphy_picker.is_open() {
                    self.activate_giphy_enter();
                } else {
                    self.activate_emoji_picker();
                }
            }
            Action::CloseEmojiPicker => {
                if self.m3.giphy_picker.is_open() {
                    self.close_giphy_picker();
                } else {
                    self.close_emoji_picker();
                }
            }
            Action::ShortcodePrev => {
                self.composer_popup_prev();
            }
            Action::ShortcodeNext => {
                self.composer_popup_next();
            }
            Action::ShortcodeAccept => {
                self.accept_composer_popup();
            }
            Action::CloseShortcodePopup => {
                self.close_composer_popup();
            }
            Action::CloseThreadPanel => self.close_thread_panel(),
            Action::CycleThreadFocus => self.cycle_thread_focus(),
            Action::ToggleThreadBroadcast => self.toggle_thread_broadcast(),
            Action::SendThreadReply => {
                self.submit_thread_reply();
            }
            Action::OpenLinksAndMedia => self.open_links_palette(),
            Action::FocusComposer => self.set_focus(FocusTarget::Composer),
            Action::SendMessage => {
                if self.has_composer_popup() {
                    self.accept_composer_popup();
                } else {
                    self.submit_composer_message();
                }
            }
            Action::FontSmaller => self.font_smaller(),
            Action::FontLarger => self.font_larger(),
            Action::FontReset => self.font_reset(),
            Action::PaletteNext => self.palette_next(),
            Action::PalettePrev => self.palette_prev(),
            Action::PaletteActivate => {
                self.execute_palette_selected();
            }
            Action::ClosePalette => self.close_palette(),
            Action::PlayPauseVoice => {
                self.play_or_pause_selected_voice();
            }
            Action::ToggleTranscript => {
                self.toggle_selected_transcript();
            }
            Action::ToggleVoiceRecord => {
                self.toggle_voice_recording(true);
            }
            Action::FinishVoiceRecord => {
                self.finish_voice_recording();
            }
            Action::CancelVoiceRecord => {
                self.cancel_voice_recording("Voice note cancelled");
            }
            Action::ToggleDictation => {
                self.toggle_dictation(true);
            }
            Action::FinishDictation => {
                self.finish_dictation_and_auto_send();
            }
            Action::CancelDictation => {
                self.cancel_dictation();
            }
            Action::CopyMessageText | Action::SmartCopy => {
                self.copy_selected_message_text();
            }
            Action::MoveViewUp => {
                self.palette_move_view_up();
            }
            Action::MoveViewDown => {
                self.palette_move_view_down();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::PaletteGroupKind;

    fn seeded_store() -> Store {
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

        let c1 = Chat {
            key: ChatKey::new(Provider::Whatsapp, "main", "alice@s.whatsapp.net"),
            title: "Alice Light".into(),
            is_group: false,
            archived_local: false,
            archived_remote: false,
            pinned: false,
            unread_count: 2,
            last_msg_ts: "2025-01-15T12:00:00Z".into(),
            last_msg_preview: "See you soon".into(),
            avatar_hash: None,
        };
        let c2 = Chat {
            key: ChatKey::new(Provider::Slack, "T1", "C-eng"),
            title: "Engineering".into(),
            is_group: true,
            archived_local: false,
            archived_remote: false,
            pinned: true,
            unread_count: 0,
            last_msg_ts: "2025-01-15T11:00:00Z".into(),
            last_msg_preview: "Build is green".into(),
            avatar_hash: None,
        };
        let c3 = Chat {
            key: ChatKey::new(Provider::Slack, "T1", "C-old"),
            title: "Light Archive Room".into(),
            is_group: true,
            archived_local: true,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2025-01-14T10:00:00Z".into(),
            last_msg_preview: "Old notes".into(),
            avatar_hash: None,
        };
        store.upsert_chat(&c1, 100).unwrap();
        store.upsert_chat(&c2, 100).unwrap();
        store.upsert_chat(&c3, 100).unwrap();

        store
            .upsert_message(&Message {
                chat: c1.key.clone(),
                msg_id: "m1".into(),
                sender_id: "alice".into(),
                sender_name: "Alice".into(),
                from_me: false,
                ts: "2025-01-14T18:00:00Z".into(),
                body: "Check https://postbox.example/release".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();
        store
            .upsert_message(&Message {
                chat: c1.key.clone(),
                msg_id: "m2".into(),
                sender_id: "me".into(),
                sender_name: "You".into(),
                from_me: true,
                ts: "2025-01-15T12:00:00Z".into(),
                body: "Light mode contrast verified".into(),
                reply_to_id: None,
                reply_broadcast: false,
                media_kind: None,
                mime_type: None,
                filename: None,
                file_size: None,
                reactions: vec![],
                read_status: ReadStatus::Read,
                deleted: false,
            })
            .unwrap();

        store
    }

    #[test]
    fn window_title_enforces_pre_view_combined_account_marked_and_notice() {
        let store = seeded_store();
        let vm_uninit =
            PostboxViewModel::new_uninitialized(store, Keymap::postbox_defaults(), "0.1.0");
        assert_eq!(vm_uninit.window_title(), "Postbox v0.1.0");

        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        assert_eq!(vm.window_title(), "Combined inbox - Postbox v0.1.0");

        vm.toggle_mark_message("m1");
        assert_eq!(
            vm.window_title(),
            "Combined inbox \u{00B7} 1 messages marked - Postbox v0.1.0"
        );
        vm.set_title_notice(Some("Copied transcript".into()));
        assert_eq!(
            vm.window_title(),
            "Combined inbox \u{00B7} 1 messages marked \u{00B7} Copied transcript - Postbox v0.1.0"
        );

        assert!(vm.switch_to_account_slot(3));
        assert_eq!(
            vm.window_title(),
            "Acme Slack \u{00B7} Copied transcript - Postbox v0.1.0"
        );
        vm.toggle_mark_message("slack-m1");
        assert_eq!(
            vm.window_title(),
            "Acme Slack \u{00B7} 1 messages marked \u{00B7} Copied transcript - Postbox v0.1.0"
        );
    }

    #[test]
    fn theme_and_font_scale_persist_to_sqlite_settings() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        assert_eq!(vm.theme(), ThemeChoice::ZincDark);
        assert_eq!(vm.font_scale(), FontScale::default());

        vm.set_theme(ThemeChoice::Amoled);
        vm.font_larger();
        vm.font_larger();
        assert_eq!(vm.font_scale().percent(), 120);

        let persisted = vm.store().load_ui_settings().unwrap();
        assert_eq!(persisted.theme, ThemeChoice::Amoled);
        assert_eq!(persisted.font_scale.percent(), 120);

        vm.font_reset();
        assert_eq!(vm.font_scale().percent(), 100);
    }

    #[test]
    fn hold_ctrl_badges_appear_clear_on_blur_and_slot_jumps_focus_composer() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        assert!(!vm.ctrl_badges_visible());
        assert!(vm.sidebar_rows().iter().all(|r| r.slot_badge.is_none()));

        assert!(vm.on_ctrl_down());
        let rows = vm.sidebar_rows();
        assert_eq!(rows[0].slot_badge, Some(1));
        assert_eq!(rows[1].slot_badge, Some(2));

        assert!(vm.open_chat_slot(2));
        assert_eq!(vm.selected_chat().unwrap().title, "Engineering");
        assert_eq!(vm.focus(), FocusTarget::Composer);

        assert!(vm.on_window_blur());
        assert!(!vm.ctrl_badges_visible());
        assert!(vm.sidebar_rows().iter().all(|r| r.slot_badge.is_none()));
    }

    #[test]
    fn ctrl_k_palette_ranks_chats_above_fts_above_commands_and_executes_theme() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        vm.open_palette();
        assert!(vm.palette_open());

        vm.set_palette_query("light");
        let entries = vm.palette_entries();
        let first_chat = entries
            .iter()
            .position(|e| e.group == PaletteGroupKind::Chats)
            .unwrap();
        let first_msg = entries
            .iter()
            .position(|e| e.group == PaletteGroupKind::Messages)
            .unwrap();
        let first_cmd = entries
            .iter()
            .position(|e| e.group == PaletteGroupKind::Commands)
            .unwrap();
        assert!(first_chat < first_msg);
        assert!(first_msg < first_cmd);

        vm.set_palette_query("Theme: Light");
        assert!(vm.execute_palette_selected());
        assert!(!vm.palette_open());
        assert_eq!(vm.theme(), ThemeChoice::Light);
    }

    #[test]
    fn sort_views_palette_reorders_accounts_and_persists_view_order() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        assert_eq!(vm.accounts()[0].label, "WhatsApp");
        assert_eq!(vm.accounts()[1].label, "Acme Slack");

        vm.open_sort_views_palette();
        assert_eq!(vm.palette_mode(), PaletteMode::SortViews);
        assert!(vm.palette_move_view_down());
        assert_eq!(vm.accounts()[0].label, "Acme Slack");
        assert_eq!(vm.accounts()[1].label, "WhatsApp");

        let persisted = vm.store().load_ui_settings().unwrap();
        assert_eq!(
            persisted.view_order,
            vec!["slack:T1".to_owned(), "whatsapp:main".to_owned()]
        );
    }

    #[test]
    fn chords_f1_links_composer_and_escape_cascade_work_end_to_end() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");

        assert!(vm.switch_to_account_slot(3));
        assert_eq!(vm.active_view().unwrap().label(), "Acme Slack");
        assert!(!vm.handle_inbox_chord_key('g', 1000, false, false));
        assert!(vm.handle_inbox_chord_key('i', 1400, false, false));
        assert_eq!(vm.active_view(), Some(&ViewTab::CombinedInbox));

        assert!(vm.select_chat_index(0));
        vm.select_message_index(0);
        assert_eq!(vm.selected_message_idx(), Some(0));
        assert_eq!(vm.handle_alt_g(2000, false), LatestChordOutcome::ArmedHint);
        assert_eq!(vm.handle_alt_g(2500, false), LatestChordOutcome::JumpLatest);
        assert_eq!(vm.selected_message_idx(), Some(1));
        assert!(vm.take_scroll_to_latest_pending());

        vm.select_message_index(0);
        vm.open_links_palette();
        assert_eq!(vm.palette_entries().len(), 1);
        assert_eq!(
            vm.palette_entries()[0].label,
            "https://postbox.example/release"
        );
        vm.handle_escape();
        assert!(!vm.palette_open());

        vm.toggle_shortcuts();
        assert!(vm.shortcuts_open());
        vm.handle_escape();
        assert!(!vm.shortcuts_open());

        vm.set_focus(FocusTarget::Composer);
        vm.set_composer_text("Hello from M1");
        assert!(vm.submit_composer_message());
        assert!(vm.composer_text().is_empty());
        assert_eq!(vm.messages().last().unwrap().body, "Hello from M1");
    }

    #[test]
    fn ctrl_f_confirm_chat_and_message_hit_restores_full_sidebar_list() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        assert_eq!(vm.chats().len(), 2);

        vm.open_search_palette();
        vm.set_palette_query("Engineering");
        assert_eq!(vm.chats().len(), 1);
        assert!(vm.execute_palette_selected());
        assert!(!vm.palette_open());
        assert!(vm.search_filter().is_empty());
        assert_eq!(vm.chats().len(), 2);
        assert_eq!(vm.selected_chat().unwrap().title, "Engineering");
        assert_eq!(vm.focus(), FocusTarget::Composer);

        vm.open_search_palette();
        vm.set_palette_query("Alice");
        assert_eq!(vm.chats().len(), 1);
        let msg_hit_entry = vm
            .palette_entries()
            .iter()
            .find(|e| matches!(e.kind, PaletteEntryKind::MessageHit { .. }))
            .cloned()
            .expect("expected FTS5 message hit for Alice");
        assert!(vm.execute_palette_entry(&msg_hit_entry));
        assert!(!vm.palette_open());
        assert!(vm.search_filter().is_empty());
        assert_eq!(vm.chats().len(), 2);
        assert_eq!(vm.selected_chat().unwrap().title, "Alice Light");
        assert_eq!(vm.focus(), FocusTarget::MessageList);
    }

    #[test]
    fn format_utc_rfc3339_and_submit_composer_use_real_utc_timestamps() {
        assert_eq!(format_utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc_rfc3339(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_utc_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc_rfc3339(951_868_800), "2000-03-01T00:00:00Z");
        assert_eq!(format_utc_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(format_utc_rfc3339(1_795_731_482), "2026-11-26T22:18:02Z");

        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        let before = format_utc_rfc3339(now_epoch_secs());
        vm.set_composer_text("Timestamped message");
        assert!(vm.submit_composer_message());
        let after = format_utc_rfc3339(now_epoch_secs());
        let sent = vm.messages().last().unwrap();
        assert_eq!(sent.body, "Timestamped message");
        assert_eq!(sent.ts.len(), 20);
        assert!(sent.ts.ends_with('Z'));
        assert!(
            sent.ts.as_str() >= before.as_str() && sent.ts.as_str() <= after.as_str(),
            "expected {} to fall within [{}, {}]",
            sent.ts,
            before,
            after
        );
    }

    #[test]
    fn palette_section_row_sync_and_chord_resets_work() {
        let mut vm = PostboxViewModel::new(seeded_store(), Keymap::postbox_defaults(), "0.1.0");
        vm.open_sort_views_palette();
        assert_eq!(vm.palette_selected_section_row(), Some((0, 0)));
        assert!(vm.select_palette_section_row(0, 1));
        assert_eq!(vm.palette_selected_index(), 1);
        assert_eq!(vm.palette_selected_section_row(), Some((0, 1)));
        assert!(vm.palette_move_view_up());
        assert_eq!(vm.accounts()[0].label, "Acme Slack");
        assert_eq!(vm.palette_selected_section_row(), Some((0, 0)));
        vm.close_palette();

        assert!(vm.switch_to_account_slot(2));
        assert!(!vm.handle_inbox_chord_key('g', 1000, false, false));
        vm.reset_inbox_chord();
        assert!(!vm.handle_inbox_chord_key('i', 1200, false, false));
        assert_ne!(vm.active_view(), Some(&ViewTab::CombinedInbox));

        assert_eq!(vm.handle_alt_g(2000, false), LatestChordOutcome::ArmedHint);
        vm.reset_latest_chord();
        assert_eq!(vm.handle_alt_g(2200, false), LatestChordOutcome::ArmedHint);
    }
}

#[cfg(test)]
mod ram_diet_tests {
    use super::*;
    use postbox_core::{Account, Chat, Message, Provider};

    fn seed(store: &mut Store) {
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
        for (id, title, ts) in [
            ("a@s.whatsapp.net", "Alice", "2025-01-15T12:00:00Z"),
            ("b@s.whatsapp.net", "Bob", "2025-01-15T11:00:00Z"),
        ] {
            let key = ChatKey::new(Provider::Whatsapp, "main", id);
            store
                .upsert_chat(
                    &Chat {
                        key: key.clone(),
                        title: title.into(),
                        is_group: false,
                        archived_local: false,
                        archived_remote: false,
                        pinned: false,
                        unread_count: 0,
                        last_msg_ts: ts.into(),
                        last_msg_preview: "hi".into(),
                        avatar_hash: None,
                    },
                    100,
                )
                .unwrap();
            for n in 0..50 {
                store
                    .upsert_message(&Message {
                        chat: key.clone(),
                        msg_id: format!("{id}-{n}"),
                        sender_id: "u1".into(),
                        sender_name: title.into(),
                        from_me: false,
                        ts: format!("2025-01-15T12:{n:02}:00Z"),
                        body: format!("{title} message {n}"),
                        reply_to_id: None,
                        reply_broadcast: false,
                        media_kind: None,
                        mime_type: None,
                        filename: None,
                        file_size: None,
                        reactions: vec![],
                        read_status: postbox_core::ReadStatus::Read,
                        deleted: false,
                    })
                    .unwrap();
            }
        }
    }

    #[test]
    fn chat_switch_reloads_only_the_active_timeline_from_sqlite_within_5ms() {
        let mut store = Store::open_in_memory().unwrap();
        seed(&mut store);
        let mut vm =
            PostboxViewModel::new(store, postbox_keymap::Keymap::postbox_defaults(), "0.1.0");
        vm.initialize();

        assert!(vm.select_chat_index(0));
        let first_chat = vm.selected_chat().unwrap().key.clone();
        let first_msgs = vm.messages().len();
        assert_eq!(first_msgs, 50);
        assert!(vm.messages().iter().all(|m| m.chat == first_chat));

        // Switching chat replaces the vec: no multi-chat message map, no accumulation.
        let started = std::time::Instant::now();
        assert!(vm.select_chat_index(1));
        let switch_ms = started.elapsed().as_secs_f64() * 1000.0;

        let second_chat = vm.selected_chat().unwrap().key.clone();
        assert_ne!(second_chat, first_chat);
        assert_eq!(vm.messages().len(), 50);
        assert!(
            vm.messages().iter().all(|m| m.chat == second_chat),
            "only the active chat's messages may stay in RAM"
        );
        assert!(
            switch_ms < 5.0,
            "chat switch + SQLite reload took {switch_ms:.3} ms, budget is 5 ms"
        );

        // Standing memory diet: the same `shrink_memory` the idle hook calls must work here too.
        assert!(vm.shrink_store_memory().is_ok());
    }
}
