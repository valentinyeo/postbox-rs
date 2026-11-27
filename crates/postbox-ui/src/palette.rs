use crate::sync_bridge::HubConnectionStatus;
use postbox_core::dictation::DictationLanguage;
use postbox_core::voice::AudioDeviceInfo;
use postbox_core::{Account, Chat, ChatKey, FontScale, Message, Provider, ThemeChoice};
use postbox_store::SearchHit;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Normal,
    SearchChats,
    SortViews,
    Links,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommand {
    SearchChats,
    ComposeMessage,
    ToggleSlackThreadsForAccount,
    ShowShortcuts,
    VersionInfo,
    HubStatusInfo,
    Dictate,
    RecordVoiceNote,
    SetDictationLanguage(DictationLanguage),
    SelectMicrophoneDefault,
    SelectMicrophone(String),
    SetPlaybackSpeed(u16), // 100 = 1x, 150 = 1.5x, 200 = 2x
    FontSmaller,
    FontLarger,
    FontReset,
    SetFontPreset(FontScale),
    SetTheme(ThemeChoice),
    ToggleUnreadChats,
    ShowCombinedInbox,
    SortViews,
    ShowAccountView(String),
    TogglePinSelected,
    ToggleArchivedInline,
    ArchiveBulkSelected,
    CancelBulkSelection,
    PickMention,
    SetReplyStyleSlack(bool),
    ToggleArchivedOnlyView,
    ArchiveOrUnarchiveSelected { expected_archived: bool },
    KeepSelectedArchived,
    ReplyToSelected,
    ResendFailedMessage,
    DeleteMessageForEveryone,
    ReactToSelected,
    CopyMessageText,
    CopyMessageLink,
    CopyTranscript,
    CopyAllTranscripts,
    PlayPauseVoice,
    ToggleTranscript,
    SelectNextMessage,
    SelectPrevMessage,
    JumpToLatest,
    OpenLinksInMessage,
    NextChat,
    PrevChat,
    InsertEmoji,
    SendGif,
    SendMessage,
    QuickReact(Option<&'static str>),
    Refresh,
    RestartLiveSync,
    ManageAccounts,
    OpenVideoExternal,
    PlayVideoInline,
    OpenImageExternal,
    AttachFile,
    CheckForUpdates,
    RestartToUpdate,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteEntryKind {
    Command(PaletteCommand),
    Chat {
        key: ChatKey,
        archived: bool,
    },
    MessageHit {
        chat: ChatKey,
        msg_id: String,
        snippet: String,
    },
    AccountView {
        view_key: String,
    },
    Link {
        target: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteGroupKind {
    Chats,
    Messages,
    Commands,
    Views,
    Links,
}

impl PaletteGroupKind {
    pub fn heading(self) -> &'static str {
        match self {
            Self::Chats => "Chats",
            Self::Messages => "Messages",
            Self::Commands => "Commands",
            Self::Views => "Account views (Ctrl+Up / Ctrl+Down to reorder)",
            Self::Links => "Links & media",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub label: String,
    pub subtitle: Option<String>,
    pub shortcut: Option<String>,
    pub group: PaletteGroupKind,
    pub kind: PaletteEntryKind,
    pub score: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteSection {
    pub group: PaletteGroupKind,
    pub heading: String,
    pub items: Vec<PaletteEntry>,
}

#[derive(Debug, Clone)]
pub struct PaletteState {
    pub open: bool,
    pub mode: PaletteMode,
    pub query: String,
    pub selected_index: usize,
    pub entries: Vec<PaletteEntry>,
    pub archive_snapshot: Option<(ChatKey, bool)>,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self {
            open: false,
            mode: PaletteMode::Normal,
            query: String::new(),
            selected_index: 0,
            entries: Vec::new(),
            archive_snapshot: None,
        }
    }
}

impl PaletteState {
    pub fn placeholder(&self) -> &'static str {
        match self.mode {
            PaletteMode::Normal => "Type a command or search chats and messages...",
            PaletteMode::SearchChats => "Search chats and messages...",
            PaletteMode::SortViews => "Reorder account views (Ctrl+Up / Ctrl+Down)...",
            PaletteMode::Links => "Filter links & media (J/K to navigate)...",
        }
    }

    pub fn sections(&self) -> Vec<PaletteSection> {
        let mut sections: Vec<PaletteSection> = Vec::new();
        for entry in &self.entries {
            if let Some(last) = sections.last_mut()
                && last.group == entry.group
            {
                last.items.push(entry.clone());
            } else {
                sections.push(PaletteSection {
                    group: entry.group,
                    heading: entry.group.heading().to_owned(),
                    items: vec![entry.clone()],
                });
            }
        }
        sections
    }

    pub fn flat_index_for_section_row(&self, section_idx: usize, row_idx: usize) -> Option<usize> {
        let mut cur_section = 0usize;
        let mut cur_row = 0usize;
        let mut prev_group: Option<PaletteGroupKind> = None;
        for (flat_idx, entry) in self.entries.iter().enumerate() {
            match prev_group {
                Some(g) if g == entry.group => {
                    cur_row += 1;
                }
                Some(_) => {
                    cur_section += 1;
                    cur_row = 0;
                }
                None => {}
            }
            prev_group = Some(entry.group);
            if cur_section == section_idx && cur_row == row_idx {
                return Some(flat_idx);
            }
        }
        None
    }

    pub fn section_row_for_flat_index(&self, flat_idx: usize) -> Option<(usize, usize)> {
        let mut cur_section = 0usize;
        let mut cur_row = 0usize;
        let mut prev_group: Option<PaletteGroupKind> = None;
        for (idx, entry) in self.entries.iter().enumerate() {
            match prev_group {
                Some(g) if g == entry.group => {
                    cur_row += 1;
                }
                Some(_) => {
                    cur_section += 1;
                    cur_row = 0;
                }
                None => {}
            }
            prev_group = Some(entry.group);
            if idx == flat_idx {
                return Some((cur_section, cur_row));
            }
        }
        None
    }

    pub fn entry_for_section_row(
        &self,
        section_idx: usize,
        row_idx: usize,
    ) -> Option<&PaletteEntry> {
        let flat_idx = self.flat_index_for_section_row(section_idx, row_idx)?;
        self.entries.get(flat_idx)
    }

    pub fn selected_entry(&self) -> Option<&PaletteEntry> {
        self.entries.get(self.selected_index)
    }

    pub fn move_next(&mut self) {
        if !self.entries.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.entries.len();
        }
    }

    pub fn move_prev(&mut self) {
        if !self.entries.is_empty() {
            if self.selected_index == 0 {
                self.selected_index = self.entries.len() - 1;
            } else {
                self.selected_index -= 1;
            }
        }
    }
}

/// Fuzzy subsequence score matching Zig `main.zig:15952-15974` (`paletteScore`):
/// `+1` per matched char, `+4` contiguous match, `-min(gap, 8)` non-contiguous,
/// `+3` at start of label or immediately after `' '` or `':'`.
pub fn fuzzy_score(label: &str, query: &str) -> Option<i32> {
    let label_chars: Vec<char> = label.chars().collect();
    let mut score: i32 = 0;
    let mut idx: usize = 0;
    let mut prev: Option<usize> = None;

    for qch in query.chars() {
        let wanted = qch.to_ascii_lowercase();
        let mut matched = false;
        while idx < label_chars.len() {
            if label_chars[idx].to_ascii_lowercase() == wanted {
                score += 1;
                if let Some(p) = prev {
                    if idx == p + 1 {
                        score += 4;
                    } else {
                        score -= (idx - p).min(8) as i32;
                    }
                }
                if idx == 0 || matches!(label_chars[idx - 1], ' ' | ':') {
                    score += 3;
                }
                prev = Some(idx);
                idx += 1;
                matched = true;
                break;
            }
            idx += 1;
        }
        if !matched {
            return None;
        }
    }
    Some(score)
}

/// Chat name match quality matching Zig `search_rank.zig:7-22`:
/// Strips any `"  ·  "` suffix (such as `"  ·  Archived"`) before matching.
/// Returns `3` (exact), `2` (prefix), `1` (inner substring), or `0` (no name match / JID only).
pub fn chat_name_quality(label: &str, query: &str) -> i32 {
    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return 0;
    }
    let clean_name = label.split("  \u{00B7}  ").next().unwrap_or(label).trim();
    let name_lower = clean_name.to_ascii_lowercase();
    let query_lower = trimmed_query.to_ascii_lowercase();

    if name_lower == query_lower {
        3
    } else if name_lower.starts_with(&query_lower) {
        2
    } else if name_lower.contains(&query_lower) {
        1
    } else {
        0
    }
}

pub struct PaletteCatalogContext<'a> {
    pub version: &'a str,
    pub version_label: &'a str,
    pub hub_status: &'a HubConnectionStatus,
    pub theme: ThemeChoice,
    pub font_scale: FontScale,
    pub dictation_language: DictationLanguage,
    pub playback_speed_pct: u16,
    pub show_archived_inline: bool,
    pub show_archived_only: bool,
    pub slack_threads_enabled: bool,
    pub slack_reply_style: bool,
    pub accounts: &'a [Account],
    pub microphones: &'a [AudioDeviceInfo],
    pub selected_microphone: Option<&'a str>,
    pub selected_chat: Option<&'a Chat>,
    pub selected_message: Option<&'a Message>,
    pub bulk_selected_count: usize,
    pub marked_messages_count: usize,
    /// Milestone 7: the update command reads as state ("Checking for updates...") once a check
    /// has run.
    pub update_label: &'a str,
    /// Milestone 7: the running version plus live updater state, shown under the check entry.
    pub update_hint: &'a str,
    /// Milestone 7: the second entry that appears once a restart is waiting, as in
    /// `main.zig:15705`.
    pub restart_label: Option<&'a str>,
}

fn cmd_entry(
    label: impl Into<String>,
    shortcut: Option<&str>,
    cmd: PaletteCommand,
) -> PaletteEntry {
    cmd_entry_with_subtitle(label, None, shortcut, cmd)
}

fn cmd_entry_with_subtitle(
    label: impl Into<String>,
    subtitle: Option<&str>,
    shortcut: Option<&str>,
    cmd: PaletteCommand,
) -> PaletteEntry {
    PaletteEntry {
        label: label.into(),
        subtitle: subtitle.map(str::to_owned),
        shortcut: shortcut.map(str::to_owned),
        group: PaletteGroupKind::Commands,
        kind: PaletteEntryKind::Command(cmd),
        score: 0,
    }
}

/// Builds the complete hub-mode command catalog matching `buildPaletteItems` (`main.zig:15831-15928`).
pub fn build_palette_items(ctx: &PaletteCatalogContext<'_>) -> Vec<PaletteEntry> {
    let mut items = Vec::with_capacity(64);

    items.push(cmd_entry(
        "Search chats",
        Some("Ctrl+F"),
        PaletteCommand::SearchChats,
    ));
    items.push(cmd_entry(
        "Compose message",
        Some("C"),
        PaletteCommand::ComposeMessage,
    ));

    if ctx
        .selected_chat
        .is_some_and(|c| c.key.provider == Provider::Slack)
    {
        let label = if ctx.slack_threads_enabled {
            "Slack threads like Slack for this account: On"
        } else {
            "Slack threads like Slack for this account: Off"
        };
        items.push(cmd_entry(
            label,
            None,
            PaletteCommand::ToggleSlackThreadsForAccount,
        ));
    }

    items.push(cmd_entry(
        "Keyboard shortcuts",
        Some("F1"),
        PaletteCommand::ShowShortcuts,
    ));
    items.push(cmd_entry(
        ctx.version_label,
        None,
        PaletteCommand::VersionInfo,
    ));
    items.push(cmd_entry(
        ctx.hub_status.palette_label(),
        None,
        PaletteCommand::HubStatusInfo,
    ));
    items.push(cmd_entry(
        "Dictate",
        Some("Ctrl+D"),
        PaletteCommand::Dictate,
    ));
    items.push(cmd_entry(
        "Record a voice note",
        Some("Ctrl+Shift+M"),
        PaletteCommand::RecordVoiceNote,
    ));

    for (lang, name) in [
        (DictationLanguage::Automatic, "Automatic"),
        (DictationLanguage::English, "English"),
        (DictationLanguage::German, "German"),
    ] {
        let marker = if ctx.dictation_language == lang {
            " (active)"
        } else {
            ""
        };
        items.push(cmd_entry(
            format!("Dictation language: {name}{marker}"),
            None,
            PaletteCommand::SetDictationLanguage(lang),
        ));
    }

    let default_mic_marker = if ctx.selected_microphone.is_none() {
        " (in use)"
    } else {
        ""
    };
    items.push(cmd_entry(
        format!("Microphone: System default{default_mic_marker}"),
        None,
        PaletteCommand::SelectMicrophoneDefault,
    ));
    for mic in ctx.microphones.iter().take(16) {
        let active = ctx.selected_microphone == Some(mic.friendly_name.as_str());
        let marker = if active { " (in use)" } else { "" };
        items.push(cmd_entry(
            format!("Microphone: {}{marker}", mic.friendly_name),
            None,
            PaletteCommand::SelectMicrophone(mic.friendly_name.clone()),
        ));
    }

    for (speed, label) in [(100u16, "1x"), (150, "1.5x"), (200, "2x")] {
        let marker = if ctx.playback_speed_pct == speed {
            " (active)"
        } else {
            ""
        };
        items.push(cmd_entry(
            format!("Playback speed: {label}{marker}"),
            None,
            PaletteCommand::SetPlaybackSpeed(speed),
        ));
    }

    items.push(cmd_entry(
        "Make text smaller",
        Some("Ctrl+-"),
        PaletteCommand::FontSmaller,
    ));
    items.push(cmd_entry(
        "Make text larger",
        Some("Ctrl++"),
        PaletteCommand::FontLarger,
    ));
    items.push(cmd_entry(
        "Reset text size",
        Some("Ctrl+0"),
        PaletteCommand::FontReset,
    ));

    for (preset, name) in [
        (FontScale::preset_small(), "Small (80%)"),
        (FontScale::default(), "Default (100%)"),
        (FontScale::preset_large(), "Large (125%)"),
        (FontScale::preset_larger(), "Larger (150%)"),
    ] {
        let marker = if ctx.font_scale == preset {
            " (active)"
        } else {
            ""
        };
        items.push(cmd_entry(
            format!("Text size: {name}{marker}"),
            None,
            PaletteCommand::SetFontPreset(preset),
        ));
    }

    for theme in [
        ThemeChoice::ZincDark,
        ThemeChoice::Amoled,
        ThemeChoice::Light,
    ] {
        let marker = if ctx.theme == theme { " (active)" } else { "" };
        items.push(cmd_entry(
            format!("Theme: {}{marker}", theme.label()),
            None,
            PaletteCommand::SetTheme(theme),
        ));
    }

    items.push(cmd_entry(
        "Toggle unread chats",
        Some("U"),
        PaletteCommand::ToggleUnreadChats,
    ));
    items.push(cmd_entry(
        "Show combined inbox",
        Some("Alt+1 / Alt+I"),
        PaletteCommand::ShowCombinedInbox,
    ));
    items.push(cmd_entry("Sort views", None, PaletteCommand::SortViews));

    for (idx, account) in ctx.accounts.iter().enumerate() {
        let shortcut = if idx < 8 {
            Some(format!("Alt+{}", idx + 2))
        } else {
            None
        };
        items.push(cmd_entry(
            format!("Show {}", account.label),
            shortcut.as_deref(),
            PaletteCommand::ShowAccountView(account.view_key()),
        ));
    }

    let is_pinned = ctx.selected_chat.is_some_and(|c| c.pinned);
    items.push(cmd_entry(
        if is_pinned {
            "Unpin selected chat"
        } else {
            "Pin chat to top"
        },
        None,
        PaletteCommand::TogglePinSelected,
    ));

    items.push(cmd_entry(
        if ctx.show_archived_inline {
            "Toggle archived chats in this view: On"
        } else {
            "Toggle archived chats in this view: Off"
        },
        Some("Alt+A"),
        PaletteCommand::ToggleArchivedInline,
    ));

    if ctx.bulk_selected_count > 0 {
        items.push(cmd_entry(
            format!("Archive {} selected chats", ctx.bulk_selected_count),
            Some("Ctrl+E"),
            PaletteCommand::ArchiveBulkSelected,
        ));
        items.push(cmd_entry(
            "Cancel selection",
            Some("Esc"),
            PaletteCommand::CancelBulkSelection,
        ));
    }

    items.push(cmd_entry(
        "Pick the mention",
        None,
        PaletteCommand::PickMention,
    ));

    if ctx.slack_reply_style {
        items.push(cmd_entry(
            "Replies: Slack style (active)",
            None,
            PaletteCommand::SetReplyStyleSlack(true),
        ));
        items.push(cmd_entry(
            "Replies: WhatsApp style",
            None,
            PaletteCommand::SetReplyStyleSlack(false),
        ));
    } else {
        items.push(cmd_entry(
            "Replies: WhatsApp style (active)",
            None,
            PaletteCommand::SetReplyStyleSlack(false),
        ));
        items.push(cmd_entry(
            "Replies: Slack style",
            None,
            PaletteCommand::SetReplyStyleSlack(true),
        ));
    }

    items.push(cmd_entry(
        if ctx.show_archived_only {
            "Show inbox chats"
        } else {
            "Show archived chats"
        },
        None,
        PaletteCommand::ToggleArchivedOnlyView,
    ));

    let selected_archived = ctx.selected_chat.is_some_and(Chat::is_archived);
    items.push(cmd_entry(
        if selected_archived {
            "Unarchive selected chat"
        } else {
            "Archive selected chat"
        },
        Some("Ctrl+E"),
        PaletteCommand::ArchiveOrUnarchiveSelected {
            expected_archived: selected_archived,
        },
    ));
    items.push(cmd_entry(
        "Keep selected chat archived",
        Some("Ctrl+Shift+E"),
        PaletteCommand::KeepSelectedArchived,
    ));

    items.push(cmd_entry(
        "Reply to selected message",
        Some("Ctrl+Shift+R"),
        PaletteCommand::ReplyToSelected,
    ));
    items.push(cmd_entry(
        "Resend failed message",
        None,
        PaletteCommand::ResendFailedMessage,
    ));
    if ctx.selected_message.is_some() {
        items.push(cmd_entry(
            "Delete message for everyone",
            None,
            PaletteCommand::DeleteMessageForEveryone,
        ));
    }
    items.push(cmd_entry(
        "React to selected message",
        Some("Ctrl+R"),
        PaletteCommand::ReactToSelected,
    ));
    items.push(cmd_entry(
        "Copy message text",
        Some("Ctrl+C"),
        PaletteCommand::CopyMessageText,
    ));
    items.push(cmd_entry(
        "Copy message link",
        None,
        PaletteCommand::CopyMessageLink,
    ));
    items.push(cmd_entry(
        "Copy transcript",
        None,
        PaletteCommand::CopyTranscript,
    ));
    if ctx.marked_messages_count > 0 {
        items.push(cmd_entry(
            "Copy all transcripts",
            None,
            PaletteCommand::CopyAllTranscripts,
        ));
    }

    items.push(cmd_entry(
        "Play or pause voice message",
        Some("Ctrl+P"),
        PaletteCommand::PlayPauseVoice,
    ));
    items.push(cmd_entry(
        "Show or hide transcript",
        Some("Ctrl+T"),
        PaletteCommand::ToggleTranscript,
    ));
    items.push(cmd_entry(
        "Select next message",
        Some("Alt+J"),
        PaletteCommand::SelectNextMessage,
    ));
    items.push(cmd_entry(
        "Select previous message",
        Some("Alt+K"),
        PaletteCommand::SelectPrevMessage,
    ));
    items.push(cmd_entry(
        "Jump to latest message",
        Some("Alt+G"),
        PaletteCommand::JumpToLatest,
    ));
    items.push(cmd_entry(
        "Open links in message",
        Some("Ctrl+O"),
        PaletteCommand::OpenLinksInMessage,
    ));
    items.push(cmd_entry(
        "Next chat",
        Some("Ctrl+Tab"),
        PaletteCommand::NextChat,
    ));
    items.push(cmd_entry(
        "Previous chat",
        Some("Ctrl+Shift+Tab"),
        PaletteCommand::PrevChat,
    ));
    items.push(cmd_entry("Insert emoji", None, PaletteCommand::InsertEmoji));
    items.push(cmd_entry("Send a GIF", None, PaletteCommand::SendGif));
    items.push(cmd_entry(
        "Send message",
        Some("Enter"),
        PaletteCommand::SendMessage,
    ));

    for (emoji, name) in [
        ("👍", "Like"),
        ("❤️", "Love"),
        ("😂", "Laugh"),
        ("😮", "Surprised"),
        ("😢", "Sad"),
        ("🙏", "Thanks"),
    ] {
        items.push(cmd_entry(
            format!("React to message: {emoji} {name}"),
            None,
            PaletteCommand::QuickReact(Some(emoji)),
        ));
    }
    items.push(cmd_entry(
        "Remove reaction",
        None,
        PaletteCommand::QuickReact(None),
    ));

    items.push(cmd_entry("Refresh", Some("R"), PaletteCommand::Refresh));
    items.push(cmd_entry(
        "Restart live sync",
        Some("S"),
        PaletteCommand::RestartLiveSync,
    ));
    items.push(cmd_entry(
        "Manage accounts",
        None,
        PaletteCommand::ManageAccounts,
    ));
    items.push(cmd_entry(
        "Play video inline",
        None,
        PaletteCommand::PlayVideoInline,
    ));
    items.push(cmd_entry(
        "Open video in external player",
        None,
        PaletteCommand::OpenVideoExternal,
    ));
    items.push(cmd_entry(
        "Open image in Windows viewer",
        None,
        PaletteCommand::OpenImageExternal,
    ));
    items.push(cmd_entry(
        "Attach photo or file",
        None,
        PaletteCommand::AttachFile,
    ));
    items.push(cmd_entry_with_subtitle(
        ctx.update_label,
        Some(ctx.update_hint),
        None,
        PaletteCommand::CheckForUpdates,
    ));
    if let Some(label) = ctx.restart_label {
        items.push(cmd_entry(label, None, PaletteCommand::RestartToUpdate));
    }
    items.push(cmd_entry("Quit Postbox", Some("Q"), PaletteCommand::Quit));

    items
}

fn chat_matches_query(chat: &Chat, query_lower: &str) -> Option<i32> {
    let quality = chat_name_quality(&chat.title, query_lower);
    if quality > 0 {
        return Some(quality * 100);
    }
    if chat.key.id.to_ascii_lowercase().contains(query_lower) {
        return Some(10);
    }
    None
}

pub struct PaletteFilterInput<'a> {
    pub mode: PaletteMode,
    pub query: &'a str,
    pub commands: &'a [PaletteEntry],
    pub visible_chats: &'a [Chat],
    pub archived_chats: &'a [Chat],
    pub fts_hits: &'a [SearchHit],
    pub accounts: &'a [Account],
    pub links: &'a [String],
}

/// Filters and ranks palette entries according to `mode` and `query`.
/// Invariant: on non-empty query (or `SearchChats` mode), matching **Chats (including archived)**
/// always rank ABOVE **Message FTS5 Hits** which always rank ABOVE **Commands**.
pub fn filter_palette(input: PaletteFilterInput<'_>) -> Vec<PaletteEntry> {
    let PaletteFilterInput {
        mode,
        query,
        commands,
        visible_chats,
        archived_chats,
        fts_hits,
        accounts,
        links,
    } = input;
    let trimmed = query.trim();
    let query_lower = trimmed.to_ascii_lowercase();

    match mode {
        PaletteMode::SortViews => accounts
            .iter()
            .enumerate()
            .filter(|(_, acc)| {
                trimmed.is_empty()
                    || fuzzy_score(&acc.label, trimmed).is_some()
                    || acc.view_key().to_ascii_lowercase().contains(&query_lower)
            })
            .map(|(idx, acc)| PaletteEntry {
                label: acc.label.clone(),
                subtitle: Some(acc.view_key()),
                shortcut: if idx < 8 {
                    Some(format!("Alt+{}", idx + 2))
                } else {
                    None
                },
                group: PaletteGroupKind::Views,
                kind: PaletteEntryKind::AccountView {
                    view_key: acc.view_key(),
                },
                score: 0,
            })
            .collect(),

        PaletteMode::Links => links
            .iter()
            .filter(|target| {
                trimmed.is_empty() || target.to_ascii_lowercase().contains(&query_lower)
            })
            .map(|target| PaletteEntry {
                label: target.clone(),
                subtitle: None,
                shortcut: None,
                group: PaletteGroupKind::Links,
                kind: PaletteEntryKind::Link {
                    target: target.clone(),
                },
                score: 0,
            })
            .collect(),

        PaletteMode::Normal if trimmed.is_empty() => {
            let mut out = commands.to_vec();
            for chat in visible_chats {
                out.push(PaletteEntry {
                    label: chat.title.clone(),
                    subtitle: Some(chat.last_msg_preview.clone()),
                    shortcut: None,
                    group: PaletteGroupKind::Chats,
                    kind: PaletteEntryKind::Chat {
                        key: chat.key.clone(),
                        archived: chat.is_archived(),
                    },
                    score: 0,
                });
            }
            out
        }

        PaletteMode::SearchChats if trimmed.is_empty() => visible_chats
            .iter()
            .map(|chat| PaletteEntry {
                label: chat.title.clone(),
                subtitle: Some(chat.last_msg_preview.clone()),
                shortcut: None,
                group: PaletteGroupKind::Chats,
                kind: PaletteEntryKind::Chat {
                    key: chat.key.clone(),
                    archived: chat.is_archived(),
                },
                score: 0,
            })
            .collect(),

        PaletteMode::Normal | PaletteMode::SearchChats => {
            // 1. Matching visible + archived chats (deduped by ChatKey)
            let mut seen_chats: BTreeSet<ChatKey> = BTreeSet::new();
            let mut chat_matches: Vec<(i32, usize, PaletteEntry)> = Vec::new();

            for (order, chat) in visible_chats.iter().enumerate() {
                if let Some(score) = chat_matches_query(chat, &query_lower) {
                    seen_chats.insert(chat.key.clone());
                    let label = if chat.is_archived() {
                        format!("{}  \u{00B7}  Archived", chat.title)
                    } else {
                        chat.title.clone()
                    };
                    chat_matches.push((
                        score,
                        order,
                        PaletteEntry {
                            label,
                            subtitle: Some(chat.last_msg_preview.clone()),
                            shortcut: None,
                            group: PaletteGroupKind::Chats,
                            kind: PaletteEntryKind::Chat {
                                key: chat.key.clone(),
                                archived: chat.is_archived(),
                            },
                            score,
                        },
                    ));
                }
            }

            let offset = visible_chats.len();
            for (i, chat) in archived_chats.iter().enumerate() {
                if seen_chats.contains(&chat.key) {
                    continue;
                }
                if let Some(score) = chat_matches_query(chat, &query_lower) {
                    seen_chats.insert(chat.key.clone());
                    chat_matches.push((
                        score.saturating_sub(1),
                        offset + i,
                        PaletteEntry {
                            label: format!("{}  \u{00B7}  Archived", chat.title),
                            subtitle: Some(chat.last_msg_preview.clone()),
                            shortcut: None,
                            group: PaletteGroupKind::Chats,
                            kind: PaletteEntryKind::Chat {
                                key: chat.key.clone(),
                                archived: true,
                            },
                            score,
                        },
                    ));
                }
            }

            chat_matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

            // 2. FTS5 message hits (deduped by (chat, msg_id))
            let mut seen_msgs: BTreeSet<(ChatKey, String)> = BTreeSet::new();
            let mut msg_matches: Vec<PaletteEntry> = Vec::new();
            for hit in fts_hits {
                if seen_msgs.insert((hit.chat.clone(), hit.msg_id.clone())) {
                    msg_matches.push(PaletteEntry {
                        label: format!("{}: {}", hit.sender_name, hit.snippet),
                        subtitle: Some(format!("{}:{}", hit.chat.provider.as_str(), hit.chat.id)),
                        shortcut: None,
                        group: PaletteGroupKind::Messages,
                        kind: PaletteEntryKind::MessageHit {
                            chat: hit.chat.clone(),
                            msg_id: hit.msg_id.clone(),
                            snippet: hit.snippet.clone(),
                        },
                        score: 0,
                    });
                }
            }

            // 3. Fuzzy commands
            let mut cmd_matches: Vec<(i32, usize, PaletteEntry)> = Vec::new();
            if mode == PaletteMode::Normal {
                for (order, cmd) in commands.iter().enumerate() {
                    if let Some(score) = fuzzy_score(&cmd.label, trimmed) {
                        let mut entry = cmd.clone();
                        entry.score = score;
                        cmd_matches.push((score, order, entry));
                    }
                }
                cmd_matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            }

            // Grouping invariant: Chats ABOVE Message FTS5 Hits ABOVE Commands
            let mut out =
                Vec::with_capacity(chat_matches.len() + msg_matches.len() + cmd_matches.len());
            out.extend(chat_matches.into_iter().map(|(_, _, e)| e));
            out.extend(msg_matches);
            out.extend(cmd_matches.into_iter().map(|(_, _, e)| e));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_chat(id: &str, title: &str, archived: bool) -> Chat {
        Chat {
            key: ChatKey::new(Provider::Whatsapp, "main", id),
            title: title.into(),
            is_group: false,
            archived_local: archived,
            archived_remote: false,
            pinned: false,
            unread_count: 0,
            last_msg_ts: "2026-11-26T12:00:00Z".into(),
            last_msg_preview: "hello".into(),
            avatar_hash: None,
        }
    }

    #[test]
    fn fuzzy_score_prefers_contiguous_word_start_matches() {
        let exact = fuzzy_score("Theme: Light", "light").unwrap();
        let scattered = fuzzy_score("Line item ghost hint test", "light").unwrap();
        assert!(exact > scattered);
        assert!(fuzzy_score("Quit Postbox", "xyz").is_none());
    }

    #[test]
    fn chat_name_quality_strips_archived_suffix() {
        assert_eq!(
            chat_name_quality("Alice  \u{00B7}  Archived", "Archived"),
            0
        );
        assert_eq!(chat_name_quality("Alice  \u{00B7}  Archived", "alice"), 3);
        assert_eq!(chat_name_quality("Alice Smith", "Ali"), 2);
        assert_eq!(chat_name_quality("Team Alice", "Ali"), 1);
    }

    #[test]
    fn the_restart_entry_appears_only_while_a_staged_update_waits() {
        let hub_status = HubConnectionStatus::NotConfigured {
            missing_url: true,
            missing_token: true,
            configured_url: None,
            detail: None,
        };
        let mut ctx = PaletteCatalogContext {
            version: "0.1.0",
            version_label: "Postbox v0.1.0",
            hub_status: &hub_status,
            theme: ThemeChoice::ZincDark,
            font_scale: FontScale::default(),
            dictation_language: DictationLanguage::Automatic,
            playback_speed_pct: 100,
            show_archived_inline: false,
            show_archived_only: false,
            slack_threads_enabled: false,
            slack_reply_style: false,
            accounts: &[],
            microphones: &[],
            selected_microphone: None,
            selected_chat: None,
            selected_message: None,
            bulk_selected_count: 0,
            marked_messages_count: 0,
            update_label: "Check for updates",
            update_hint: "v0.1.0",
            restart_label: None,
        };
        let idle = build_palette_items(&ctx);
        assert!(
            !idle.iter().any(|entry| matches!(
                entry.kind,
                PaletteEntryKind::Command(PaletteCommand::RestartToUpdate)
            )),
            "no restart entry before anything is staged"
        );

        ctx.restart_label = Some("Restart to install Postbox v0.2.0");
        let staged = build_palette_items(&ctx);
        let restart = staged
            .iter()
            .find(|entry| {
                matches!(
                    entry.kind,
                    PaletteEntryKind::Command(PaletteCommand::RestartToUpdate)
                )
            })
            .expect("the restart entry is listed");
        assert_eq!(restart.label, "Restart to install Postbox v0.2.0");
    }

    #[test]
    fn version_entry_uses_the_live_version_label() {
        let hub_status = HubConnectionStatus::Connected {
            url: "https://hub.example".into(),
            last_event_id: Some(1),
        };
        let mut ctx = PaletteCatalogContext {
            version: "0.1.42",
            version_label: "Postbox v0.1.42",
            hub_status: &hub_status,
            theme: ThemeChoice::ZincDark,
            font_scale: FontScale::default(),
            dictation_language: DictationLanguage::Automatic,
            playback_speed_pct: 100,
            show_archived_inline: false,
            show_archived_only: false,
            slack_threads_enabled: false,
            slack_reply_style: false,
            accounts: &[],
            microphones: &[],
            selected_microphone: None,
            selected_chat: None,
            selected_message: None,
            bulk_selected_count: 0,
            marked_messages_count: 0,
            update_label: "Check for updates now",
            update_hint: "v0.1.42",
            restart_label: None,
        };
        for label in [
            "Postbox v0.1.42",
            "Postbox v0.1.42 (checking for updates...)",
            "Postbox v0.1.42 (downloading update...)",
            "Postbox v0.1.42 (v0.1.43 ready to install)",
            "Postbox v0.1.42 (update ready)",
            "Postbox v0.1.42 (installing update...)",
            "Postbox v0.1.42 (up to date)",
            "Postbox v0.1.42 (update check failed)",
        ] {
            ctx.version_label = label;
            let items = build_palette_items(&ctx);
            let version_entry = items
                .iter()
                .find(|entry| {
                    matches!(
                        entry.kind,
                        PaletteEntryKind::Command(PaletteCommand::VersionInfo)
                    )
                })
                .expect("the version entry is listed");
            assert_eq!(version_entry.label, label);
        }
    }

    #[test]
    fn non_empty_query_ranks_chats_above_messages_above_commands() {
        let hub_status = HubConnectionStatus::Connected {
            url: "https://hub.example".into(),
            last_event_id: Some(1),
        };
        let ctx = PaletteCatalogContext {
            version: "0.1.0",
            version_label: "Postbox v0.1.0",
            hub_status: &hub_status,
            theme: ThemeChoice::ZincDark,
            font_scale: FontScale::default(),
            dictation_language: DictationLanguage::Automatic,
            playback_speed_pct: 100,
            show_archived_inline: false,
            show_archived_only: false,
            slack_threads_enabled: false,
            slack_reply_style: false,
            accounts: &[],
            microphones: &[],
            selected_microphone: None,
            selected_chat: None,
            selected_message: None,
            bulk_selected_count: 0,
            marked_messages_count: 0,
            update_label: "Check for updates",
            update_hint: "v0.1.0",
            restart_label: None,
        };
        let commands = build_palette_items(&ctx);
        let visible = vec![test_chat("c1", "Light Design", false)];
        let archived = vec![test_chat("c2", "Light Archive", true)];
        let hits = vec![SearchHit {
            chat: ChatKey::new(Provider::Slack, "T1", "C9"),
            msg_id: "m1".into(),
            sender_name: "Bob".into(),
            snippet: "Turn on [Light] mode".into(),
        }];

        let results = filter_palette(PaletteFilterInput {
            mode: PaletteMode::Normal,
            query: "light",
            commands: &commands,
            visible_chats: &visible,
            archived_chats: &archived,
            fts_hits: &hits,
            accounts: &[],
            links: &[],
        });

        let first_chat = results
            .iter()
            .position(|e| e.group == PaletteGroupKind::Chats)
            .unwrap();
        let first_msg = results
            .iter()
            .position(|e| e.group == PaletteGroupKind::Messages)
            .unwrap();
        let first_cmd = results
            .iter()
            .position(|e| e.group == PaletteGroupKind::Commands)
            .unwrap();

        assert!(first_chat < first_msg);
        assert!(first_msg < first_cmd);
        assert!(
            results
                .iter()
                .any(|e| matches!(e.kind, PaletteEntryKind::Chat { archived: true, .. }))
        );
    }
}
