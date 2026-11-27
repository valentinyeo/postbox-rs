//! Milestone 3 view-model state and controller methods for `PostboxViewModel`:
//! - #18 Auto-download on chat open (newest-first, 100 MiB cap, `410` gone persistence)
//! - #19 Inline image/WebP/GIF state
//! - #20 Avatars, workspace icons & Slack custom emoji
//! - #21 Clipboard paste-to-attach (`Ctrl+V`/`Shift+Insert`) & drag-and-drop staging (cap 16)
//! - #22 Image lightbox gallery navigation & extension-allowlisted external fallback
//! - #23 `Ctrl+R` 8×6 reaction picker, clickable reaction chips, WA bubble double-click 👍, `:shortcode:` popup
//! - #24 `@mention` autocomplete (takes priority over `:shortcode:`)
//! - #25 SSRF-safe link unfurl cards
//! - #26 Giphy 4×3 picker with 5-field target race guard

use crate::sync_bridge::{MediaDownloadTarget, SyncCommand};
use crate::viewmodel::{
    FocusTarget, PostboxViewModel, new_client_id_public, now_epoch_millis_public,
    now_epoch_secs_public,
};
use postbox_core::outbox::OutboxRecord;
use postbox_core::{ChatKey, MediaKind, Message, Provider, ReadStatus};
use postbox_media::avatar::{SlackEmojiIndex, chat_avatar_enabled, workspace_avatar_key};
use postbox_media::drain::{
    AttachmentState, MediaItemStatus, attachment_label, is_downloadable_message, is_oversized,
    media_cache_key,
};
use postbox_media::emoji::{
    DoubleClickDecision, EmojiCatalog, EmojiPickerMode, EmojiPickerState, PendingReactionQueue,
    ReactionChipDecision, ReactionSettleOutcome, ShortcodeMatch, ShortcodeTyping,
    apply_optimistic_reaction, decide_bubble_double_click, decide_reaction_chip_click,
    detect_shortcode_at_caret, format_reactions_chip, push_recent, replace_shortcode_token,
};
use postbox_media::giphy::{
    GIPHY_MSG_ATTACH_FAILED, GIPHY_MSG_DOWNLOADING, GIPHY_MSG_NEEDS_KEY, GIPHY_MSG_SAVE_FAILED,
    GiphyPickerState, GiphyStatus, GiphyTarget,
};
use postbox_media::lightbox::{LightboxState, external_preview_allowed, is_viewer_image};
use postbox_media::mention::{
    MAX_MENTION_POPUP_ROWS, MentionCandidate, MentionMatch, MentionSpan, detect_mention_at_caret,
    format_mention_insertion, format_mention_status, rank_mentions, replace_mention_token,
    roster_from_messages,
};
use postbox_media::paste::{PasteStageResult, SendCapability, StagedAttachments};
use postbox_media::unfurl::{
    ClassifiedUrl, UnfurlCard, UnfurlProvider, UnfurlRingCache, extract_first_unfurl_url,
};
use postbox_platform::{ClipboardPayload, open_external_image};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionPopupState {
    pub span: MentionSpan,
    pub matches: Vec<MentionMatch>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcodePopupState {
    pub typing: ShortcodeTyping,
    pub matches: Vec<ShortcodeMatch>,
    pub selected: usize,
}

/// Consolidated Milestone 3 UI state owned by `PostboxViewModel`.
#[derive(Debug)]
pub struct M3State {
    pub data_dir: PathBuf,
    pub giphy_key_available: bool,
    pub media_paths: HashMap<(ChatKey, String), PathBuf>,
    pub media_statuses: HashMap<(ChatKey, String), MediaItemStatus>,
    pub avatar_paths: HashMap<ChatKey, Option<PathBuf>>,
    pub avatar_requested: HashSet<ChatKey>,
    pub workspace_icon_paths: HashMap<String, Option<PathBuf>>,
    pub workspace_icon_requested: HashSet<String>,
    pub slack_emoji_indices: HashMap<String, SlackEmojiIndex>,
    pub slack_emoji_index_requested: HashSet<String>,
    pub slack_emoji_paths: HashMap<(String, String), PathBuf>,
    pub slack_emoji_media_requested: HashSet<(String, String)>,
    pub staged_attachments: StagedAttachments,
    pub lightbox: Option<LightboxState>,
    pub emoji_recents: Vec<usize>,
    pub emoji_picker: Option<EmojiPickerState>,
    pub pending_reactions: PendingReactionQueue,
    pub mention_popup: Option<MentionPopupState>,
    pub shortcode_popup: Option<ShortcodePopupState>,
    pub extra_mention_candidates: Vec<MentionCandidate>,
    pub unfurl_ring: UnfurlRingCache,
    pub unfurl_in_flight: HashSet<(UnfurlProvider, String)>,
    pub giphy_picker: GiphyPickerState,
    pub giphy_preview_paths: HashMap<usize, PathBuf>,
}

impl Default for M3State {
    fn default() -> Self {
        Self {
            data_dir: std::env::temp_dir().join("postbox-media-runtime"),
            giphy_key_available: false,
            media_paths: HashMap::new(),
            media_statuses: HashMap::new(),
            avatar_paths: HashMap::new(),
            avatar_requested: HashSet::new(),
            workspace_icon_paths: HashMap::new(),
            workspace_icon_requested: HashSet::new(),
            slack_emoji_indices: HashMap::new(),
            slack_emoji_index_requested: HashSet::new(),
            slack_emoji_paths: HashMap::new(),
            slack_emoji_media_requested: HashSet::new(),
            staged_attachments: StagedAttachments::new(),
            lightbox: None,
            emoji_recents: Vec::new(),
            emoji_picker: None,
            pending_reactions: PendingReactionQueue::new(),
            mention_popup: None,
            shortcode_popup: None,
            extra_mention_candidates: Vec::new(),
            unfurl_ring: UnfurlRingCache::new(),
            unfurl_in_flight: HashSet::new(),
            giphy_picker: GiphyPickerState::default(),
            giphy_preview_paths: HashMap::new(),
        }
    }
}

impl M3State {
    pub fn paste_dir(&self) -> PathBuf {
        self.data_dir.join("paste")
    }

    pub fn hub_media_dir(&self) -> PathBuf {
        self.data_dir.join("hub-media")
    }
}

impl PostboxViewModel {
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.m3.data_dir = data_dir;
        self
    }

    pub fn with_giphy_key_available(mut self, available: bool) -> Self {
        self.m3.giphy_key_available = available;
        self
    }

    pub fn set_giphy_key_available(&mut self, available: bool) {
        self.m3.giphy_key_available = available;
    }

    pub fn m3(&self) -> &M3State {
        &self.m3
    }

    pub fn data_dir(&self) -> &Path {
        &self.m3.data_dir
    }

    // -------------------------------------------------------------------------
    // #18 Auto-Download Media on Chat Open + Statuses
    // -------------------------------------------------------------------------

    pub fn media_path_for(&self, chat: &ChatKey, msg_id: &str) -> Option<&PathBuf> {
        self.m3.media_paths.get(&(chat.clone(), msg_id.to_owned()))
    }

    pub fn media_status_for(&self, chat: &ChatKey, msg_id: &str) -> MediaItemStatus {
        let key = (chat.clone(), msg_id.to_owned());
        if let Some(p) = self.m3.media_paths.get(&key) {
            return MediaItemStatus::Ready(p.clone());
        }
        self.m3
            .media_statuses
            .get(&key)
            .cloned()
            .unwrap_or(MediaItemStatus::Idle)
    }

    pub fn attachment_display_label(&self, msg: &Message) -> String {
        let name =
            msg.filename
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(match msg.media_kind {
                    Some(MediaKind::Voice) => "Voice message",
                    Some(MediaKind::Audio) => "Audio",
                    Some(MediaKind::Image) => "Image",
                    Some(MediaKind::Gif) => "GIF",
                    Some(MediaKind::Sticker) => "Sticker",
                    Some(MediaKind::Video) => "Video",
                    _ => "Attachment",
                });
        let status = self.media_status_for(&msg.chat, &msg.msg_id);
        let state = if matches!(status, MediaItemStatus::Idle) {
            if msg.file_size.is_some_and(is_oversized) {
                AttachmentState::Oversized
            } else {
                AttachmentState::Downloading
            }
        } else {
            status.to_attachment_state()
        };
        attachment_label(name, msg.file_size.unwrap_or(0), state)
    }

    /// Product invariant (#18): immediately triggers auto-download newest-first for all
    /// uncached downloadable media (`<= 100 MiB`) whenever a chat is opened or refreshed,
    /// plus queues avatars, workspace icons, Slack custom emoji, and link unfurls.
    pub(crate) fn trigger_open_chat_media_and_metadata(&mut self) {
        let Some(chat) = self.selected_chat().cloned() else {
            return;
        };

        // 1. Chat avatar (WA / TG) or Slack workspace icon
        if chat_avatar_enabled(chat.key.provider)
            && self.m3.avatar_requested.insert(chat.key.clone())
        {
            self.emit_sync_command(SyncCommand::FetchAvatar {
                chat: chat.key.clone(),
            });
        }
        if chat.key.provider == Provider::Slack
            && let Some(ws_key) = workspace_avatar_key(&chat.key.id)
        {
            let team = ws_key
                .trim_start_matches("slack:")
                .trim_end_matches(':')
                .to_owned();
            if self.m3.workspace_icon_requested.insert(team.clone()) {
                self.emit_sync_command(SyncCommand::FetchWorkspaceIcon { team: team.clone() });
            }
            if self.m3.slack_emoji_index_requested.insert(team.clone()) {
                self.emit_sync_command(SyncCommand::EnsureSlackEmojiIndex { team: team.clone() });
            } else if let Some(index) = self.m3.slack_emoji_indices.get(&team).cloned() {
                let mut media_cmds = Vec::new();
                for msg in &self.messages {
                    if msg.chat.provider != Provider::Slack {
                        continue;
                    }
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
                            media_cmds.push(SyncCommand::FetchSlackEmojiMedia {
                                team: team.clone(),
                                name: resolved_name,
                                url,
                            });
                        }
                    }
                }
                for cmd in media_cmds {
                    self.emit_sync_command(cmd);
                }
            }
        }

        // 2. Load SQLite `gone_media` + resolve existing `media_cache` hits, collect newest-first downloads
        let gone_ids: HashSet<String> = self
            .store
            .list_gone_media_for_chat(&chat.key)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let hub_media_dir = self.m3.hub_media_dir();
        let mut download_targets = Vec::new();
        let mut unfurl_targets = Vec::new();

        for msg in self.messages.iter().rev() {
            let pair = (chat.key.clone(), msg.msg_id.clone());

            if is_downloadable_message(msg) && !msg.msg_id.is_empty() {
                if gone_ids.contains(&msg.msg_id) {
                    self.m3.media_statuses.insert(pair, MediaItemStatus::Gone);
                } else if let Some(entry) = self
                    .store
                    .get_media_cache(&media_cache_key(&chat.key, &msg.msg_id))
                    .ok()
                    .flatten()
                {
                    let full = hub_media_dir.join(&entry.rel_path);
                    if full.is_file() {
                        let cache_key = media_cache_key(&chat.key, &msg.msg_id);
                        let _ = self
                            .store
                            .touch_media_cache(&cache_key, now_epoch_secs_public());
                        postbox_media::age::touch_file_mtime(&full);
                        self.m3.media_paths.insert(pair.clone(), full.clone());
                        if !entry.sha256.is_empty() {
                            self.m4.media_sha256.insert(pair.clone(), entry.sha256);
                        }
                        self.m3
                            .media_statuses
                            .insert(pair, MediaItemStatus::Ready(full));
                    } else if msg.file_size.is_some_and(is_oversized) {
                        self.m3
                            .media_statuses
                            .insert(pair, MediaItemStatus::Oversized);
                    } else {
                        self.m3
                            .media_statuses
                            .insert(pair, MediaItemStatus::Downloading);
                        download_targets.push(MediaDownloadTarget {
                            msg_id: msg.msg_id.clone(),
                            filename: msg.filename.clone(),
                            mime_type: msg.mime_type.clone(),
                            media_kind: msg.media_kind,
                            file_size: msg.file_size,
                        });
                    }
                } else if self.m3.media_paths.contains_key(&pair) {
                    // Already in memory
                } else if msg.file_size.is_some_and(is_oversized) {
                    self.m3
                        .media_statuses
                        .insert(pair, MediaItemStatus::Oversized);
                } else {
                    let existing = self.m3.media_statuses.get(&pair);
                    if !matches!(
                        existing,
                        Some(
                            MediaItemStatus::Downloading
                                | MediaItemStatus::Unavailable
                                | MediaItemStatus::Gone
                        )
                    ) {
                        self.m3
                            .media_statuses
                            .insert(pair, MediaItemStatus::Downloading);
                        download_targets.push(MediaDownloadTarget {
                            msg_id: msg.msg_id.clone(),
                            filename: msg.filename.clone(),
                            mime_type: msg.mime_type.clone(),
                            media_kind: msg.media_kind,
                            file_size: msg.file_size,
                        });
                    }
                }
            }

            // 3. Collect link unfurl targets for messages containing URLs
            if !msg.deleted
                && let Some(cls) = extract_first_unfurl_url(&msg.body, msg.media_kind.is_some())
            {
                let key = (cls.provider, cls.id.clone());
                if self.m3.unfurl_ring.get(cls.provider, &cls.id).is_none()
                    && self.m3.unfurl_in_flight.insert(key)
                {
                    unfurl_targets.push(cls);
                }
            }
        }

        if !download_targets.is_empty() {
            self.emit_sync_command(SyncCommand::DownloadChatMedia {
                chat: chat.key.clone(),
                items: download_targets,
            });
        }
        for target in unfurl_targets {
            self.emit_sync_command(SyncCommand::UnfurlUrl { target });
        }
    }

    /// Queues an explicit user-initiated media download (overrides oversized/unavailable/failed).
    pub fn request_explicit_media_download(&mut self, msg_id: &str) -> bool {
        let Some(msg) = self.messages.iter().find(|m| m.msg_id == msg_id).cloned() else {
            return false;
        };
        let pair = (msg.chat.clone(), msg.msg_id.clone());
        self.m3
            .media_statuses
            .insert(pair, MediaItemStatus::Downloading);
        self.emit_sync_command(SyncCommand::DownloadMediaExplicit {
            chat: msg.chat,
            target: MediaDownloadTarget {
                msg_id: msg.msg_id,
                filename: msg.filename,
                mime_type: msg.mime_type,
                media_kind: msg.media_kind,
                file_size: msg.file_size,
            },
        });
        true
    }

    // -------------------------------------------------------------------------
    // #20 Avatars, Workspace Icons & Slack Custom Emoji
    // -------------------------------------------------------------------------

    pub fn avatar_path_for(&self, chat: &ChatKey) -> Option<&PathBuf> {
        self.m3.avatar_paths.get(chat).and_then(Option::as_ref)
    }

    pub fn workspace_icon_path_for(&self, team: &str) -> Option<&PathBuf> {
        self.m3
            .workspace_icon_paths
            .get(team)
            .and_then(Option::as_ref)
    }

    pub fn slack_emoji_path_for(&self, team: &str, name: &str) -> Option<&PathBuf> {
        self.m3
            .slack_emoji_paths
            .get(&(team.to_owned(), name.to_owned()))
    }

    pub fn render_message_text_with_slack_emoji(&self, msg: &Message) -> String {
        if msg.chat.provider != Provider::Slack {
            return msg.body.clone();
        }
        let team = msg.chat.account.as_str();
        if let Some(index) = self.m3.slack_emoji_indices.get(team) {
            index.replace_text(&msg.body, |custom_name| Some(format!(":{custom_name}:")))
        } else {
            let empty = SlackEmojiIndex::default();
            empty.replace_text(&msg.body, |_| None)
        }
    }

    // -------------------------------------------------------------------------
    // #21 Paste-to-Attach & Drag-and-Drop Staging (Capacity 16)
    // -------------------------------------------------------------------------

    pub fn staged_attachments(&self) -> &StagedAttachments {
        &self.m3.staged_attachments
    }

    /// Handles `Ctrl+V` / `Ctrl+Shift+V` / `Shift+Insert` clipboard payload.
    /// Returns `PasteStageResult::PropagateTextPaste` when clipboard holds text so normal text
    /// paste is never hijacked!
    pub fn handle_paste_clipboard_payload(
        &mut self,
        payload: ClipboardPayload,
    ) -> PasteStageResult {
        if matches!(payload, ClipboardPayload::Text) {
            return PasteStageResult::PropagateTextPaste;
        }
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            self.set_status_notice("Select a chat first");
            return PasteStageResult::NoImage {
                notice: "Select a chat first",
            };
        };
        let paste_dir = self.m3.paste_dir();
        let res = self
            .m3
            .staged_attachments
            .apply_clipboard_payload(&chat_key, &paste_dir, payload);
        match &res {
            PasteStageResult::PropagateTextPaste => {}
            PasteStageResult::NoImage { notice }
            | PasteStageResult::RejectedExtension { notice } => {
                self.set_status_notice(*notice);
            }
            PasteStageResult::Staged { notice, .. } => {
                self.set_status_notice(*notice);
                self.observe_dictation_context();
            }
        }
        res
    }

    pub fn stage_dropped_files(&mut self, paths: &[PathBuf]) -> usize {
        let Some(chat_key) = self.selected_chat().map(|c| c.key.clone()) else {
            self.set_status_notice("Select a chat first");
            return 0;
        };
        let paste_dir = self.m3.paste_dir();
        let mut staged = 0usize;
        for path in paths {
            if self
                .m3
                .staged_attachments
                .stage_disk_file(&chat_key, &paste_dir, path)
                .is_err()
            {
                break;
            }
            staged += 1;
        }
        if staged > 0 {
            self.set_status_notice("File attached - Enter sends, Esc discards");
            self.observe_dictation_context();
        }
        staged
    }

    pub fn remove_staged_attachment(&mut self, index: usize) -> bool {
        let removed = self.m3.staged_attachments.remove(index, true).is_some();
        if removed {
            self.observe_dictation_context();
        }
        removed
    }

    pub fn discard_staged_attachments(&mut self) -> bool {
        if self.m3.staged_attachments.is_empty() {
            return false;
        }
        self.m3.staged_attachments.discard_all();
        self.set_status_notice("Pasted image discarded");
        self.observe_dictation_context();
        true
    }

    /// Submits staged attachments (with optional caption on first file per `SendCapability::SingleFile`)
    /// into the durable M2 SQLite outbox (`OutboxRecord::new_file`).
    pub(crate) fn submit_staged_attachments_to_outbox(
        &mut self,
        chat_key: &ChatKey,
        caption: &str,
        reply_to: Option<String>,
    ) -> bool {
        if !self.m3.staged_attachments.guard_chat_match(chat_key) {
            self.set_status_notice("Pasted image discarded");
            return false;
        }
        let plan = self
            .m3
            .staged_attachments
            .send_plan(caption, SendCapability::SingleFile);
        if plan.is_empty() {
            return false;
        }
        let items = self.m3.staged_attachments.take_for_outbox();
        let now_ms = now_epoch_millis_public();
        let now_s = now_epoch_secs_public();
        let ts = crate::viewmodel::format_utc_rfc3339_public(now_s);

        let mut enqueued = 0usize;
        for step in plan {
            let Some(item) = items.get(step.first_index) else {
                continue;
            };
            let client_id = new_client_id_public(&mut self.local_send_seq);
            let rec = OutboxRecord::new_file(
                client_id.clone(),
                chat_key.clone(),
                step.caption.clone(),
                if step.first_index == 0 {
                    reply_to.clone()
                } else {
                    None
                },
                item.path.to_string_lossy().into_owned(),
                item.filename.clone(),
                item.sha256,
                item.byte_size,
                now_ms,
            );
            if self.store.enqueue_outbox(&rec).is_err() {
                self.set_status_notice("Outbox full; retry or discard failed sends");
                break;
            }
            let optimistic = Message {
                chat: chat_key.clone(),
                msg_id: client_id,
                sender_id: "me".into(),
                sender_name: "You".into(),
                from_me: true,
                ts: ts.clone(),
                body: step.caption,
                reply_to_id: if step.first_index == 0 {
                    reply_to.clone()
                } else {
                    None
                },
                reply_broadcast: false,
                media_kind: Some(if item.is_image {
                    MediaKind::Image
                } else {
                    MediaKind::Document
                }),
                mime_type: None,
                filename: Some(item.filename.clone()),
                file_size: Some(item.byte_size),
                reactions: Vec::new(),
                read_status: ReadStatus::Sending,
                deleted: false,
            };
            let _ = self.store.upsert_message(&optimistic);
            enqueued += 1;
        }
        if enqueued > 0 {
            self.emit_sync_command(SyncCommand::WakeOutbox);
            self.refresh_chats();
            self.scroll_to_latest_pending = true;
            true
        } else {
            false
        }
    }

    // -------------------------------------------------------------------------
    // #22 Full-Window Image Lightbox & Chat Gallery Navigation
    // -------------------------------------------------------------------------

    pub fn lightbox(&self) -> Option<&LightboxState> {
        self.m3.lightbox.as_ref()
    }

    pub fn open_lightbox_for_message(&mut self, msg_id: &str, now_ms: u64) -> bool {
        let Some(chat) = self.selected_chat().cloned() else {
            return false;
        };
        let mut paths = HashMap::new();
        for ((k, id), p) in &self.m3.media_paths {
            if *k == chat.key {
                paths.insert(id.clone(), p.clone());
            }
        }
        let Some(lb) =
            LightboxState::open_for_message(&chat.key, &self.messages, msg_id, &paths, now_ms)
        else {
            return false;
        };
        let needs_dl = lb
            .current()
            .filter(|c| c.local_path.is_none())
            .map(|c| c.msg_id.clone());
        self.dismiss_all_modals();
        self.m3.lightbox = Some(lb);
        self.set_focus_raw(FocusTarget::Lightbox);
        if let Some(dl_id) = needs_dl {
            self.request_explicit_media_download(&dl_id);
        }
        true
    }

    pub fn open_selected_image_lightbox(&mut self, now_ms: u64) -> bool {
        let Some(msg) = self.selected_message().cloned() else {
            return false;
        };
        if !is_viewer_image(&msg) {
            return false;
        }
        self.open_lightbox_for_message(&msg.msg_id, now_ms)
    }

    pub fn lightbox_prev(&mut self) -> bool {
        let moved_id = if let Some(lb) = &mut self.m3.lightbox {
            if lb.step_prev() {
                lb.current()
                    .filter(|c| c.local_path.is_none())
                    .map(|c| c.msg_id.clone())
            } else {
                return false;
            }
        } else {
            return false;
        };
        if let Some(id) = moved_id {
            self.request_explicit_media_download(&id);
        }
        true
    }

    pub fn lightbox_next(&mut self) -> bool {
        let moved_id = if let Some(lb) = &mut self.m3.lightbox {
            if lb.step_next() {
                lb.current()
                    .filter(|c| c.local_path.is_none())
                    .map(|c| c.msg_id.clone())
            } else {
                return false;
            }
        } else {
            return false;
        };
        if let Some(id) = moved_id {
            self.request_explicit_media_download(&id);
        }
        true
    }

    pub fn close_lightbox(&mut self) {
        if self.m3.lightbox.take().is_some() {
            self.set_focus_raw(FocusTarget::MessageList);
        }
    }

    pub fn close_lightbox_on_click(&mut self, now_ms: u64) -> bool {
        if let Some(lb) = &self.m3.lightbox
            && lb.should_close_on_click(now_ms)
        {
            self.close_lightbox();
            true
        } else {
            false
        }
    }

    pub fn open_selected_image_externally(&mut self) -> bool {
        let path = self
            .m3
            .lightbox
            .as_ref()
            .and_then(|lb| lb.current())
            .and_then(|c| c.local_path.clone())
            .or_else(|| {
                self.selected_message()
                    .and_then(|m| self.media_path_for(&m.chat, &m.msg_id).cloned())
            });
        let Some(p) = path else {
            self.set_status_notice("No downloaded image selected");
            return false;
        };
        if !external_preview_allowed(&p) {
            self.set_status_notice("External preview blocked for unsafe file extension");
            return false;
        }
        match open_external_image(&p) {
            Ok(()) => {
                self.close_lightbox();
                true
            }
            Err(_) => {
                self.set_status_notice("Windows could not open the attachment");
                false
            }
        }
    }

    // -------------------------------------------------------------------------
    // #23 Reaction Picker (Ctrl+R), Reaction Chips, WA Double-Click & Shortcodes
    // -------------------------------------------------------------------------

    pub fn emoji_picker(&self) -> Option<&EmojiPickerState> {
        self.m3.emoji_picker.as_ref()
    }

    pub fn emoji_recents(&self) -> &[usize] {
        &self.m3.emoji_recents
    }

    pub fn is_reaction_pending(&self, chat: &ChatKey, msg_id: &str) -> bool {
        self.m3.pending_reactions.is_pending(chat, msg_id)
    }

    pub fn open_reaction_picker(&mut self) -> bool {
        let Some(chat) = self.selected_chat().cloned() else {
            return false;
        };
        if self.messages.is_empty() {
            return false;
        }
        let target_msg_id = if let Some(msg) = self.selected_message() {
            msg.msg_id.clone()
        } else if let Some(incoming) = self.messages.iter().rev().find(|m| !m.from_me) {
            incoming.msg_id.clone()
        } else {
            self.set_status_notice("No incoming message to react to");
            return false;
        };
        self.dismiss_all_modals();
        self.m3.emoji_picker = Some(EmojiPickerState::open(
            EmojiPickerMode::React {
                chat: chat.key,
                msg_id: target_msg_id,
            },
            &self.m3.emoji_recents,
        ));
        self.set_focus_raw(FocusTarget::EmojiPicker);
        true
    }

    pub fn open_insert_emoji_picker(&mut self) -> bool {
        self.dismiss_all_modals();
        let origin_chat = self.selected_chat().map(|c| c.key.clone());
        self.m3.emoji_picker = Some(EmojiPickerState::open(
            EmojiPickerMode::InsertIntoComposer { origin_chat },
            &self.m3.emoji_recents,
        ));
        self.set_focus_raw(FocusTarget::EmojiPicker);
        true
    }

    pub fn close_emoji_picker(&mut self) {
        if self.m3.emoji_picker.take().is_some() {
            self.set_focus_raw(FocusTarget::Composer);
        }
    }

    pub fn set_emoji_picker_query(&mut self, query: impl Into<String>) {
        if let Some(picker) = &mut self.m3.emoji_picker {
            picker.set_query(query.into(), &self.m3.emoji_recents);
        }
    }

    pub fn move_emoji_picker(&mut self, delta_rows: isize, delta_cols: isize) {
        if let Some(picker) = &mut self.m3.emoji_picker {
            picker.move_grid(delta_rows, delta_cols);
        }
    }

    pub fn activate_emoji_picker_index(&mut self, match_index: usize) -> bool {
        if let Some(picker) = &mut self.m3.emoji_picker
            && match_index < picker.matches.len()
        {
            picker.selected = match_index;
        }
        self.activate_emoji_picker()
    }

    pub fn activate_emoji_picker(&mut self) -> bool {
        let Some(picker) = self.m3.emoji_picker.take() else {
            return false;
        };
        let Some(entry) = picker.selected_entry() else {
            self.set_focus_raw(FocusTarget::Composer);
            return false;
        };
        let emoji = entry.emoji.to_owned();
        self.record_emoji_recent(entry.index);

        match picker.mode {
            EmojiPickerMode::React { chat, msg_id } => {
                self.set_focus_raw(FocusTarget::MessageList);
                self.react_to_message_with_chip(&chat, &msg_id, &emoji, false)
            }
            EmojiPickerMode::InsertIntoComposer { origin_chat } => {
                let current_chat = self.selected_chat().map(|c| c.key.clone());
                if let Some(orig) = origin_chat
                    && current_chat.as_ref() != Some(&orig)
                {
                    self.append_to_chat_draft(&orig, &emoji);
                    self.set_focus_raw(FocusTarget::Composer);
                    return true;
                }
                let mut text = self.composer_text().to_owned();
                text.push_str(&emoji);
                self.set_composer_text(text);
                self.set_focus_raw(FocusTarget::Composer);
                true
            }
        }
    }

    fn record_emoji_recent(&mut self, catalog_index: usize) {
        push_recent(&mut self.m3.emoji_recents, catalog_index);
        let formatted = EmojiCatalog::global().format_recents(&self.m3.emoji_recents);
        let _ = self.store.save_emoji_recents(&formatted);
    }

    /// Optimistically applies a reaction toggle (`emoji == ""` removes) with `preserve_chip`,
    /// enqueues in `PendingReactionQueue`, and dispatches `SyncCommand::ReactToMessage`.
    pub fn react_to_message_with_chip(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        emoji: &str,
        preserve_chip: bool,
    ) -> bool {
        if self.selected_chat().map(|c| &c.key) != Some(chat) {
            self.set_status_notice("The chat changed; reaction not sent");
            return false;
        }
        let Some(pos) = self.messages.iter().position(|m| m.msg_id == msg_id) else {
            self.set_status_notice("That message is no longer loaded");
            return false;
        };
        let prev_reactions = self.messages[pos].reactions.clone();
        let client_id = new_client_id_public(&mut self.local_send_seq);
        let baseline_reactions = match self.m3.pending_reactions.enqueue(
            chat.clone(),
            msg_id.to_owned(),
            emoji.to_owned(),
            client_id.clone(),
            prev_reactions,
            preserve_chip,
        ) {
            Ok(entry) => entry.previous_reactions.clone(),
            Err(err_msg) => {
                self.set_status_notice(err_msg);
                return false;
            }
        };
        if !emoji.is_empty()
            && let Some(idx) = EmojiCatalog::global().index_of(emoji)
        {
            self.record_emoji_recent(idx);
        }
        let next_reactions = apply_optimistic_reaction(&baseline_reactions, emoji, preserve_chip);
        self.messages[pos].reactions = next_reactions.clone();
        let _ = self
            .store
            .update_message_reactions(chat, msg_id, &next_reactions);
        self.rebuild_timeline_public();
        self.refresh_thread_panel_if_chat(chat);

        self.set_status_notice(if emoji.is_empty() {
            "Removing reaction..."
        } else {
            "Reacting..."
        });
        self.emit_sync_command(SyncCommand::ReactToMessage {
            chat: chat.clone(),
            msg_id: msg_id.to_owned(),
            emoji: emoji.to_owned(),
            client_id,
        });
        true
    }

    pub fn click_reaction_chip(&mut self, msg_id: &str, segment_index: usize) -> bool {
        let Some(msg) = self.messages.iter().find(|m| m.msg_id == msg_id).cloned() else {
            return false;
        };
        let chip_text = format_reactions_chip(&msg.reactions);
        let my_reaction = msg
            .reactions
            .iter()
            .find(|r| r.from_me)
            .map(|r| r.emoji.as_str())
            .unwrap_or("");
        match decide_reaction_chip_click(&chip_text, segment_index, my_reaction) {
            Some(ReactionChipDecision::Add(emoji)) => {
                self.react_to_message_with_chip(&msg.chat, &msg.msg_id, &emoji, true)
            }
            Some(ReactionChipDecision::Remove) => {
                self.react_to_message_with_chip(&msg.chat, &msg.msg_id, "", true)
            }
            None => false,
        }
    }

    pub fn double_click_message_bubble(&mut self, msg_id: &str) -> bool {
        let Some(msg) = self.messages.iter().find(|m| m.msg_id == msg_id).cloned() else {
            return false;
        };
        let my_reaction = msg
            .reactions
            .iter()
            .find(|r| r.from_me)
            .map(|r| r.emoji.as_str())
            .unwrap_or("");
        match decide_bubble_double_click(msg.chat.provider, msg.from_me, my_reaction) {
            DoubleClickDecision::Ignore => false,
            DoubleClickDecision::AddThumbsUp => {
                self.react_to_message_with_chip(&msg.chat, &msg.msg_id, "👍", false)
            }
            DoubleClickDecision::RemoveThumbsUp => {
                self.react_to_message_with_chip(&msg.chat, &msg.msg_id, "", false)
            }
        }
    }

    pub fn settle_reaction_event(
        &mut self,
        chat: &ChatKey,
        msg_id: &str,
        emoji: &str,
        ok: bool,
        http_status: Option<u16>,
        now_ms: u64,
    ) {
        let pending_preserve_chip = self
            .m3
            .pending_reactions
            .entries()
            .iter()
            .find(|e| &e.chat == chat && e.msg_id == msg_id && e.emoji == emoji)
            .map(|e| e.preserve_chip)
            .unwrap_or(false);
        let outcome =
            self.m3
                .pending_reactions
                .settle(chat, msg_id, emoji, ok, http_status, now_ms);
        match outcome {
            ReactionSettleOutcome::IgnoredStale => {}
            ReactionSettleOutcome::Confirmed { notice }
            | ReactionSettleOutcome::UnknownDelivery409 { notice } => {
                if let Some(msg) = self
                    .messages
                    .iter()
                    .find(|m| &m.chat == chat && m.msg_id == msg_id)
                {
                    let reactions = msg.reactions.clone();
                    let _ = self
                        .store
                        .update_message_reactions(chat, msg_id, &reactions);
                } else if let Ok(Some(stored)) = self.store.get_message_reactions(chat, msg_id) {
                    let already_applied = if emoji.is_empty() {
                        stored.iter().all(|r| !r.from_me)
                    } else {
                        stored.iter().any(|r| r.emoji == emoji && r.from_me)
                            && stored.iter().all(|r| !r.from_me || r.emoji == emoji)
                    };
                    if !already_applied {
                        let next = apply_optimistic_reaction(&stored, emoji, pending_preserve_chip);
                        let _ = self.store.update_message_reactions(chat, msg_id, &next);
                    }
                }
                self.set_status_notice(notice);
                self.refresh_thread_panel_if_chat(chat);
            }
            ReactionSettleOutcome::RetryScheduled { notice, .. } => {
                self.set_status_notice(notice);
                self.refresh_thread_panel_if_chat(chat);
            }
            ReactionSettleOutcome::RevertedTerminal { previous, notice } => {
                if let Some(msg) = self
                    .messages
                    .iter_mut()
                    .find(|m| &m.chat == chat && m.msg_id == msg_id)
                {
                    msg.reactions = previous.clone();
                }
                let _ = self.store.update_message_reactions(chat, msg_id, &previous);
                self.rebuild_timeline_public();
                self.refresh_thread_panel_if_chat(chat);
                self.set_status_notice(notice);
            }
        }
    }

    pub fn retry_due_reactions(&mut self, now_ms: u64) {
        let due = self.m3.pending_reactions.due_retries(now_ms);
        for entry in due {
            self.emit_sync_command(SyncCommand::ReactToMessage {
                chat: entry.chat,
                msg_id: entry.msg_id,
                emoji: entry.emoji,
                client_id: entry.client_id,
            });
        }
    }

    pub fn next_reaction_retry_at_ms(&self) -> Option<u64> {
        self.m3.pending_reactions.next_retry_at_ms()
    }

    pub fn next_reaction_retry_delay_ms(&self, now_ms: u64) -> Option<u64> {
        self.next_reaction_retry_at_ms()
            .map(|at| at.saturating_sub(now_ms))
    }

    /// Re-applies active in-flight / retry-scheduled optimistic reactions onto freshly loaded
    /// messages (`IGSH-674`, `main.zig:9649, 12979-12987`). Safe & idempotent on repeated calls.
    pub(crate) fn reapply_pending_reactions(&mut self, chat: &ChatKey) {
        if self.m3.pending_reactions.is_empty() {
            return;
        }
        for pending in self.m3.pending_reactions.entries() {
            if &pending.chat != chat {
                continue;
            }
            if let Some(msg) = self
                .messages
                .iter_mut()
                .find(|m| m.msg_id == pending.msg_id)
            {
                msg.reactions = apply_optimistic_reaction(
                    &msg.reactions,
                    &pending.emoji,
                    pending.preserve_chip,
                );
            }
        }
    }

    // -------------------------------------------------------------------------
    // #23 & #24 Composer `@mention` & `:shortcode:` Popups (`@mention` takes priority!)
    // -------------------------------------------------------------------------

    pub fn mention_popup(&self) -> Option<&MentionPopupState> {
        self.m3.mention_popup.as_ref()
    }

    pub fn shortcode_popup(&self) -> Option<&ShortcodePopupState> {
        self.m3.shortcode_popup.as_ref()
    }

    pub fn has_composer_popup(&self) -> bool {
        self.m3.mention_popup.is_some() || self.m3.shortcode_popup.is_some()
    }

    pub fn set_extra_mention_candidates(&mut self, candidates: Vec<MentionCandidate>) {
        self.m3.extra_mention_candidates = candidates;
    }

    /// Evaluates composer `@mention` (priority 1) and `:shortcode:` (priority 2) popups at
    /// `caret_byte`. Closed `:code:` shortcodes auto-replace immediately (`main.zig:13041-13050`).
    pub fn update_composer_popups_at_caret(&mut self, caret_byte: usize) {
        let text = self.composer_text().to_owned();
        let provider = self
            .selected_chat()
            .map(|c| c.key.provider)
            .unwrap_or(Provider::Whatsapp);

        // Priority 1: @mention autocomplete
        if let Some(span) = detect_mention_at_caret(&text, caret_byte) {
            self.m3.shortcode_popup = None;
            let mut roster = roster_from_messages(&self.messages);
            for extra in &self.m3.extra_mention_candidates {
                if !roster.iter().any(|r| r.display_name == extra.display_name) {
                    roster.push(extra.clone());
                }
            }
            let mut matches = rank_mentions(&span.query, &roster, provider);
            matches.truncate(MAX_MENTION_POPUP_ROWS);
            if matches.is_empty() {
                self.m3.mention_popup = None;
            } else {
                let status = format_mention_status(&matches);
                self.set_status_notice(status);
                self.m3.mention_popup = Some(MentionPopupState {
                    span,
                    matches,
                    selected: 0,
                });
            }
            return;
        }
        self.m3.mention_popup = None;

        // Priority 2: :shortcode: autocomplete
        if let Some(typing) = detect_shortcode_at_caret(&text, caret_byte) {
            let catalog = EmojiCatalog::global();
            if typing.closed {
                self.m3.shortcode_popup = None;
                if let Some(emoji) = catalog.resolve_shortcode(&typing.query) {
                    let (replaced, _new_caret) = replace_shortcode_token(&text, &typing, emoji);
                    self.set_composer_text_no_popup_recursion(replaced);
                }
                return;
            }
            let matches = catalog.match_shortcodes(&typing.query);
            if matches.is_empty() {
                self.m3.shortcode_popup = None;
            } else {
                self.m3.shortcode_popup = Some(ShortcodePopupState {
                    typing,
                    matches,
                    selected: 0,
                });
            }
        } else {
            self.m3.shortcode_popup = None;
        }
    }

    pub fn composer_popup_prev(&mut self) -> bool {
        if let Some(m) = &mut self.m3.mention_popup {
            let len = m.matches.len();
            if len > 0 {
                m.selected = (m.selected + len - 1) % len;
            }
            return true;
        }
        if let Some(s) = &mut self.m3.shortcode_popup {
            let len = s.matches.len();
            if len > 0 {
                s.selected = (s.selected + len - 1) % len;
            }
            return true;
        }
        false
    }

    pub fn composer_popup_next(&mut self) -> bool {
        if let Some(m) = &mut self.m3.mention_popup {
            let len = m.matches.len();
            if len > 0 {
                m.selected = (m.selected + 1) % len;
            }
            return true;
        }
        if let Some(s) = &mut self.m3.shortcode_popup {
            let len = s.matches.len();
            if len > 0 {
                s.selected = (s.selected + 1) % len;
            }
            return true;
        }
        false
    }

    pub fn accept_composer_popup(&mut self) -> bool {
        let provider = self
            .selected_chat()
            .map(|c| c.key.provider)
            .unwrap_or(Provider::Whatsapp);
        if let Some(popup) = self.m3.mention_popup.take()
            && let Some(chosen) = popup.matches.get(popup.selected)
        {
            let replacement = format_mention_insertion(provider, chosen);
            let text = self.composer_text().to_owned();
            let (replaced, _) = replace_mention_token(&text, &popup.span, &replacement);
            self.set_composer_text_no_popup_recursion(replaced);
            self.set_status_notice("Mention added");
            return true;
        }
        if let Some(popup) = self.m3.shortcode_popup.take()
            && let Some(chosen) = popup.matches.get(popup.selected)
        {
            let text = self.composer_text().to_owned();
            let (replaced, _) = replace_shortcode_token(&text, &popup.typing, chosen.emoji);
            self.set_composer_text_no_popup_recursion(replaced);
            return true;
        }
        false
    }

    pub fn close_composer_popup(&mut self) -> bool {
        let had = self.m3.mention_popup.is_some() || self.m3.shortcode_popup.is_some();
        self.m3.mention_popup = None;
        self.m3.shortcode_popup = None;
        had
    }

    // -------------------------------------------------------------------------
    // #25 Link Unfurl Cards
    // -------------------------------------------------------------------------

    pub fn unfurl_card_for_message(&self, msg: &Message) -> Option<(&UnfurlCard, ClassifiedUrl)> {
        if msg.deleted {
            return None;
        }
        let cls = extract_first_unfurl_url(&msg.body, msg.media_kind.is_some())?;
        if cls.provider == UnfurlProvider::Image {
            return None;
        }
        let card = self.m3.unfurl_ring.get(cls.provider, &cls.id)?;
        Some((card, cls))
    }

    // -------------------------------------------------------------------------
    // #26 Giphy Picker (`Ctrl+K > Send a GIF`, 4×3 grid + 5-field target guard)
    // -------------------------------------------------------------------------

    pub fn giphy_picker(&self) -> &GiphyPickerState {
        &self.m3.giphy_picker
    }

    pub fn giphy_preview_path(&self, index: usize) -> Option<&PathBuf> {
        self.m3.giphy_preview_paths.get(&index)
    }

    pub fn current_giphy_target(&self) -> Option<GiphyTarget> {
        let chat = self.selected_chat()?.key.clone();
        let reply_id = self.reply_to_msg_id();
        let reply_sender = reply_id.and_then(|id| {
            self.messages
                .iter()
                .find(|m| m.msg_id == id)
                .map(|m| m.sender_name.as_str())
        });
        Some(GiphyTarget::new(chat, reply_id, reply_sender))
    }

    pub fn open_giphy_picker(&mut self) -> bool {
        if !self.m3.giphy_key_available {
            self.set_status_notice(GIPHY_MSG_NEEDS_KEY);
            return false;
        }
        let Some(target) = self.current_giphy_target() else {
            self.set_status_notice("Select a chat first");
            return false;
        };
        self.dismiss_all_modals();
        self.m3.giphy_preview_paths.clear();
        let gen_id = self
            .m3
            .giphy_picker
            .begin(target.clone(), true, String::new());
        self.set_focus_raw(FocusTarget::GiphyPicker);
        self.emit_sync_command(SyncCommand::SearchGiphy {
            query: String::new(),
            offset: 0,
            generation: gen_id,
            target,
        });
        true
    }

    pub fn set_giphy_query(&mut self, query: impl Into<String>) {
        // Typing in Giphy search box does NOT fire a network call on every keystroke (`main.zig:13472`).
        self.m3.giphy_picker.query = query.into();
    }

    pub fn submit_giphy_search(&mut self) -> bool {
        let Some(target) = self.current_giphy_target() else {
            return false;
        };
        let q = self.m3.giphy_picker.query.trim_matches(' ').to_owned();
        self.m3.giphy_preview_paths.clear();
        let gen_id =
            self.m3
                .giphy_picker
                .begin(target.clone(), self.m3.giphy_key_available, q.clone());
        if self.m3.giphy_picker.status == GiphyStatus::Loading {
            self.emit_sync_command(SyncCommand::SearchGiphy {
                query: q,
                offset: 0,
                generation: gen_id,
                target,
            });
            true
        } else {
            false
        }
    }

    pub fn activate_giphy_enter(&mut self) -> bool {
        if self.m3.giphy_picker.enter_should_search() {
            return self.submit_giphy_search();
        }
        let idx = self.m3.giphy_picker.selected;
        self.request_giphy_tile_download(idx)
    }

    pub fn request_giphy_tile_download(&mut self, index: usize) -> bool {
        if self.m3.giphy_picker.status != GiphyStatus::Ready {
            return false;
        }
        let Some(item) = self.m3.giphy_picker.items.get(index).cloned() else {
            return false;
        };
        let Some(target) = self.m3.giphy_picker.target.clone() else {
            return false;
        };
        let Some(download_generation) = self.m3.giphy_picker.begin_download(index) else {
            return false;
        };
        let gen_id = self.m3.giphy_picker.generation;
        self.set_status_notice(GIPHY_MSG_DOWNLOADING);
        self.emit_sync_command(SyncCommand::DownloadGiphyGif {
            gif_id: item.id,
            url: item.original.url,
            index,
            generation: gen_id,
            download_generation,
            target,
        });
        true
    }

    pub fn move_giphy_picker(&mut self, delta_rows: isize, delta_cols: isize) {
        self.m3.giphy_picker.move_grid(delta_rows, delta_cols);
    }

    pub fn close_giphy_picker(&mut self) {
        if self.m3.giphy_picker.is_open() {
            let gen_id = self.m3.giphy_picker.generation;
            self.m3.giphy_picker.cancel();
            self.m3.giphy_preview_paths.clear();
            self.emit_sync_command(SyncCommand::CancelGiphy { generation: gen_id });
            self.set_focus_raw(FocusTarget::Composer);
        }
    }

    /// Completes a downloaded GIPHY GIF with the 5-field target race guard (`main.zig:13943-13980`).
    pub(crate) fn apply_giphy_download_ready(
        &mut self,
        generation: u64,
        download_generation: u64,
        reply_target: &GiphyTarget,
        index: usize,
        gif_id: &str,
        bytes: &[u8],
    ) {
        // Silently drop stale callbacks when picker is closed or generation moved on.
        if !self.m3.giphy_picker.is_open() || self.m3.giphy_picker.generation != generation {
            return;
        }
        let live_target = self.current_giphy_target();
        if live_target.as_ref() != Some(reply_target) {
            self.m3.giphy_picker.cancel();
            self.m3.giphy_preview_paths.clear();
            self.set_focus_raw(FocusTarget::Composer);
            self.set_status_notice(postbox_media::giphy::GIPHY_MSG_TARGET_CHANGED);
            return;
        }
        // Drop superseded, ABA, or unsolicited download callbacks (e.g., rapid tile 0 -> tile 2
        // or tile 0 -> tile 2 -> tile 0).
        if !self
            .m3
            .giphy_picker
            .is_active_download(generation, index, download_generation)
        {
            return;
        }
        match self
            .m3
            .giphy_picker
            .select_for_staging(generation, live_target.as_ref(), index)
        {
            Err(notice) => {
                self.m3.giphy_picker.cancel();
                self.m3.giphy_preview_paths.clear();
                self.set_focus_raw(FocusTarget::Composer);
                self.set_status_notice(notice);
            }
            Ok(_chosen) => {
                let Some(chat) = self.selected_chat().map(|c| c.key.clone()) else {
                    self.m3.giphy_picker.cancel();
                    self.m3.giphy_preview_paths.clear();
                    self.set_focus_raw(FocusTarget::Composer);
                    self.set_status_notice(GIPHY_MSG_SAVE_FAILED);
                    return;
                };
                let paste_dir = self.m3.paste_dir();
                let filename = format!("{gif_id}.gif");
                match self
                    .m3
                    .staged_attachments
                    .stage_bytes(&chat, &paste_dir, &filename, "gif", bytes)
                {
                    Ok(_) => {
                        self.m3.giphy_picker.cancel();
                        self.m3.giphy_preview_paths.clear();
                        self.set_focus_raw(FocusTarget::Composer);
                        self.set_status_notice("Image ready... Enter sends, Esc discards");
                    }
                    Err(postbox_media::MediaError::CapacityFull) => {
                        self.m3.giphy_picker.cancel();
                        self.m3.giphy_preview_paths.clear();
                        self.set_focus_raw(FocusTarget::Composer);
                        self.set_status_notice(GIPHY_MSG_ATTACH_FAILED);
                    }
                    Err(_) => {
                        self.m3.giphy_picker.cancel();
                        self.m3.giphy_preview_paths.clear();
                        self.set_focus_raw(FocusTarget::Composer);
                        self.set_status_notice(GIPHY_MSG_SAVE_FAILED);
                    }
                }
            }
        }
    }
}
