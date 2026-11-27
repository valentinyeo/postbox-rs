use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyContext {
    Workspace,
    ChatList,
    MessageList,
    Composer,
    ComposerEmpty,
    Palette,
    EmojiPicker,
    ThreadPanel,
    Lightbox,
    VideoPlayer,
    Dictating,
    VoiceRecording,
    ShortcodePopup,
    GiphyPicker,
}

impl KeyContext {
    pub fn as_gpui_context(self) -> &'static str {
        match self {
            Self::Workspace => "PostboxWorkspace",
            Self::ChatList => "PostboxChatList",
            Self::MessageList => "PostboxMessageList",
            Self::Composer => "PostboxComposer",
            Self::ComposerEmpty => "PostboxComposerEmpty",
            Self::Palette => "PostboxPalette",
            Self::EmojiPicker => "PostboxEmojiPicker",
            Self::ThreadPanel => "PostboxThreadPanel",
            Self::Lightbox => "PostboxLightbox",
            Self::VideoPlayer => "PostboxVideoPlayer",
            Self::Dictating => "PostboxDictating",
            Self::VoiceRecording => "PostboxVoiceRecording",
            Self::ShortcodePopup => "PostboxShortcodePopup",
            Self::GiphyPicker => "PostboxGiphyPicker",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    OpenPalette,
    ShowShortcutsHelp,
    SearchChats,
    ClearSearchOrBack,
    ToggleUnread,
    ToggleArchived,
    ArchiveSelected,
    KeepArchivedSelected,
    SelectAllChats,
    Refresh,
    RestartLiveSync,
    Quit,
    NextChat,
    PrevChat,
    FirstChat,
    LastChat,
    PageUpChats,
    PageDownChats,
    OpenChatSlot(u8),
    SwitchCombinedInbox,
    SwitchAccountView(u8),
    NextMessage,
    PrevMessage,
    PageUpMessages,
    PageDownMessages,
    JumpLatest,
    ReactToMessage,
    ReplyToMessage,
    OpenThread,
    ResendFailedMessage,
    OpenImageLightbox,
    CopyMessageText,
    SmartCopy,
    PlayPauseVoice,
    ToggleTranscript,
    OpenLinksAndMedia,
    FocusComposer,
    SendMessage,
    InsertNewline,
    DeleteWordBackward,
    PasteImage,
    ToggleVoiceRecord,
    ToggleDictation,
    FontSmaller,
    FontLarger,
    FontReset,
    ToggleVideoFullscreen,
    CloseVideoPlayer,
    CloseLightbox,
    PrevLightboxImage,
    NextLightboxImage,
    PaletteNext,
    PalettePrev,
    PaletteActivate,
    ClosePalette,
    MoveViewUp,
    MoveViewDown,
    EmojiMoveUp,
    EmojiMoveDown,
    EmojiMoveLeft,
    EmojiMoveRight,
    EmojiActivate,
    CloseEmojiPicker,
    SendThreadReply,
    CloseThreadPanel,
    CycleThreadFocus,
    ToggleThreadBroadcast,
    FinishDictation,
    CancelDictation,
    FinishVoiceRecord,
    CancelVoiceRecord,
    ShortcodePrev,
    ShortcodeNext,
    ShortcodeAccept,
    CloseShortcodePopup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub context: KeyContext,
    pub stroke: String,
    pub action: Action,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeymapOverride {
    #[serde(default)]
    pub overrides: Vec<BindingOverride>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingOverride {
    pub context: KeyContext,
    pub stroke: String,
    pub action: Action,
}

#[derive(Debug, Error)]
pub enum KeymapError {
    #[error("invalid_keymap_json")]
    InvalidJson(#[from] serde_json::Error),
    #[error("keymap_io_error")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Keymap {
    /// Postbox default muscle-memory keymap matching the existing Zig client.
    pub fn postbox_defaults() -> Self {
        let mut bindings = vec![
            b(
                KeyContext::Workspace,
                "ctrl-k",
                Action::OpenPalette,
                "Open command & search palette",
            ),
            b(
                KeyContext::Workspace,
                "f1",
                Action::ShowShortcutsHelp,
                "Show keyboard shortcuts overlay",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-f",
                Action::SearchChats,
                "Search chats in the command palette",
            ),
            b(
                KeyContext::ChatList,
                "/",
                Action::SearchChats,
                "Search chats in the command palette",
            ),
            b(
                KeyContext::Workspace,
                "escape",
                Action::ClearSearchOrBack,
                "Clear search, cancel reply, or return to chats",
            ),
            b(
                KeyContext::Workspace,
                "alt-a",
                Action::ToggleArchived,
                "Show or hide archived chats in this view",
            ),
            b(
                KeyContext::ChatList,
                "u",
                Action::ToggleUnread,
                "Toggle unread chats",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-e",
                Action::ArchiveSelected,
                "Archive or unarchive the selected chat",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-e",
                Action::KeepArchivedSelected,
                "Keep selected chat archived",
            ),
            b(
                KeyContext::ChatList,
                "e",
                Action::ArchiveSelected,
                "Archive or unarchive the selected chat",
            ),
            b(
                KeyContext::ChatList,
                "ctrl-a",
                Action::SelectAllChats,
                "Select all visible chats",
            ),
            b(
                KeyContext::ComposerEmpty,
                "ctrl-a",
                Action::SelectAllChats,
                "Select all visible chats when composer is empty",
            ),
            b(KeyContext::ChatList, "r", Action::Refresh, "Refresh"),
            b(
                KeyContext::ChatList,
                "s",
                Action::RestartLiveSync,
                "Restart live sync",
            ),
            b(KeyContext::ChatList, "q", Action::Quit, "Quit Postbox"),
            b(
                KeyContext::Workspace,
                "ctrl-tab",
                Action::NextChat,
                "Next chat",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-tab",
                Action::PrevChat,
                "Previous chat",
            ),
            b(KeyContext::ChatList, "j", Action::NextChat, "Next chat"),
            b(KeyContext::ChatList, "k", Action::PrevChat, "Previous chat"),
            b(
                KeyContext::ChatList,
                "enter",
                Action::FocusComposer,
                "Focus the composer",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-home",
                Action::FirstChat,
                "Jump to the first chat",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-end",
                Action::LastChat,
                "Jump to the last chat",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-pageup",
                Action::PageUpChats,
                "Move chat selection up a page",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-pagedown",
                Action::PageDownChats,
                "Move chat selection down a page",
            ),
            b(
                KeyContext::Workspace,
                "alt-i",
                Action::SwitchCombinedInbox,
                "Combined inbox",
            ),
            b(
                KeyContext::Workspace,
                "alt-1",
                Action::SwitchCombinedInbox,
                "Combined inbox",
            ),
            b(
                KeyContext::Workspace,
                "alt-j",
                Action::NextMessage,
                "Select the next message",
            ),
            b(
                KeyContext::Workspace,
                "alt-k",
                Action::PrevMessage,
                "Select the previous message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-down",
                Action::NextMessage,
                "Select the next message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-up",
                Action::PrevMessage,
                "Select the previous message",
            ),
            b(
                KeyContext::Workspace,
                "pageup",
                Action::PageUpMessages,
                "Scroll messages up a screen",
            ),
            b(
                KeyContext::Workspace,
                "pagedown",
                Action::PageDownMessages,
                "Scroll messages down a screen",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-end",
                Action::JumpLatest,
                "Jump back to the newest message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-r",
                Action::ReactToMessage,
                "React to message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-r",
                Action::ReplyToMessage,
                "Reply to selected message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-c",
                Action::SmartCopy,
                "Copy composer selection or selected message text",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-c",
                Action::CopyMessageText,
                "Copy selected message text",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-backspace",
                Action::DeleteWordBackward,
                "Delete word before caret",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-p",
                Action::PlayPauseVoice,
                "Play or pause selected voice message",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-t",
                Action::ToggleTranscript,
                "Show or hide voice transcript",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-o",
                Action::OpenLinksAndMedia,
                "Open links and media in selected message",
            ),
            b(
                KeyContext::ChatList,
                "c",
                Action::FocusComposer,
                "Focus the composer",
            ),
            b(
                KeyContext::Composer,
                "enter",
                Action::SendMessage,
                "Send message",
            ),
            b(
                KeyContext::Composer,
                "shift-enter",
                Action::InsertNewline,
                "Insert a new line",
            ),
            b(
                KeyContext::Composer,
                "ctrl-v",
                Action::PasteImage,
                "Paste a copied image to send",
            ),
            b(
                KeyContext::Composer,
                "ctrl-shift-v",
                Action::PasteImage,
                "Paste a copied image to send",
            ),
            b(
                KeyContext::Composer,
                "shift-insert",
                Action::PasteImage,
                "Paste a copied image to send",
            ),
            b(
                KeyContext::ComposerEmpty,
                "ctrl-v",
                Action::PasteImage,
                "Paste a copied image into an empty composer",
            ),
            b(
                KeyContext::ComposerEmpty,
                "ctrl-shift-v",
                Action::PasteImage,
                "Paste a copied image into an empty composer",
            ),
            b(
                KeyContext::ComposerEmpty,
                "shift-insert",
                Action::PasteImage,
                "Paste a copied image into an empty composer",
            ),
            b(
                KeyContext::MessageList,
                "enter",
                Action::OpenImageLightbox,
                "Open selected image in lightbox",
            ),
            b(
                KeyContext::ComposerEmpty,
                "up",
                Action::PrevMessage,
                "Walk message highlight up when composer is empty",
            ),
            b(
                KeyContext::ComposerEmpty,
                "down",
                Action::NextMessage,
                "Walk message highlight down when composer is empty",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-shift-m",
                Action::ToggleVoiceRecord,
                "Start or finish a voice note",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-d",
                Action::ToggleDictation,
                "Toggle live dictation",
            ),
            b(
                KeyContext::Workspace,
                "ctrl--",
                Action::FontSmaller,
                "Decrease font size",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-=",
                Action::FontLarger,
                "Increase font size",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-+",
                Action::FontLarger,
                "Increase font size",
            ),
            b(
                KeyContext::Workspace,
                "ctrl-0",
                Action::FontReset,
                "Reset font size",
            ),
            b(
                KeyContext::Palette,
                "up",
                Action::PalettePrev,
                "Move palette selection up",
            ),
            b(
                KeyContext::Palette,
                "down",
                Action::PaletteNext,
                "Move palette selection down",
            ),
            b(
                KeyContext::Palette,
                "j",
                Action::PaletteNext,
                "Move palette selection down in links mode",
            ),
            b(
                KeyContext::Palette,
                "k",
                Action::PalettePrev,
                "Move palette selection up in links mode",
            ),
            b(
                KeyContext::Palette,
                "enter",
                Action::PaletteActivate,
                "Run selected palette item",
            ),
            b(
                KeyContext::Palette,
                "escape",
                Action::ClosePalette,
                "Close command palette",
            ),
            b(
                KeyContext::Palette,
                "ctrl-up",
                Action::MoveViewUp,
                "Move selected account view up",
            ),
            b(
                KeyContext::Palette,
                "ctrl-down",
                Action::MoveViewDown,
                "Move selected account view down",
            ),
            b(
                KeyContext::EmojiPicker,
                "up",
                Action::EmojiMoveUp,
                "Move emoji selection up",
            ),
            b(
                KeyContext::EmojiPicker,
                "down",
                Action::EmojiMoveDown,
                "Move emoji selection down",
            ),
            b(
                KeyContext::EmojiPicker,
                "left",
                Action::EmojiMoveLeft,
                "Move emoji selection left",
            ),
            b(
                KeyContext::EmojiPicker,
                "right",
                Action::EmojiMoveRight,
                "Move emoji selection right",
            ),
            b(
                KeyContext::EmojiPicker,
                "enter",
                Action::EmojiActivate,
                "Insert or react with selected emoji",
            ),
            b(
                KeyContext::EmojiPicker,
                "escape",
                Action::CloseEmojiPicker,
                "Close emoji picker",
            ),
            b(
                KeyContext::GiphyPicker,
                "up",
                Action::EmojiMoveUp,
                "Move GIF selection up",
            ),
            b(
                KeyContext::GiphyPicker,
                "down",
                Action::EmojiMoveDown,
                "Move GIF selection down",
            ),
            b(
                KeyContext::GiphyPicker,
                "left",
                Action::EmojiMoveLeft,
                "Move GIF selection left",
            ),
            b(
                KeyContext::GiphyPicker,
                "right",
                Action::EmojiMoveRight,
                "Move GIF selection right",
            ),
            b(
                KeyContext::GiphyPicker,
                "enter",
                Action::EmojiActivate,
                "Search or select highlighted GIF",
            ),
            b(
                KeyContext::GiphyPicker,
                "escape",
                Action::CloseEmojiPicker,
                "Close GIPHY picker",
            ),
            b(
                KeyContext::ThreadPanel,
                "enter",
                Action::SendThreadReply,
                "Send thread reply",
            ),
            b(
                KeyContext::ThreadPanel,
                "escape",
                Action::CloseThreadPanel,
                "Close thread panel",
            ),
            b(
                KeyContext::ThreadPanel,
                "tab",
                Action::CycleThreadFocus,
                "Cycle focus between thread composer and broadcast toggle",
            ),
            b(
                KeyContext::ThreadPanel,
                "space",
                Action::ToggleThreadBroadcast,
                "Toggle broadcast reply to channel",
            ),
            b(
                KeyContext::VideoPlayer,
                "f11",
                Action::ToggleVideoFullscreen,
                "Toggle fullscreen inline video",
            ),
            b(
                KeyContext::VideoPlayer,
                "escape",
                Action::CloseVideoPlayer,
                "Exit fullscreen or close inline video player",
            ),
            b(
                KeyContext::Lightbox,
                "escape",
                Action::CloseLightbox,
                "Close image viewer",
            ),
            b(
                KeyContext::Lightbox,
                "space",
                Action::CloseLightbox,
                "Close image viewer",
            ),
            b(
                KeyContext::Lightbox,
                "enter",
                Action::CloseLightbox,
                "Close image viewer",
            ),
            b(
                KeyContext::Lightbox,
                "left",
                Action::PrevLightboxImage,
                "Previous image in chat gallery",
            ),
            b(
                KeyContext::Lightbox,
                "right",
                Action::NextLightboxImage,
                "Next image in chat gallery",
            ),
            b(
                KeyContext::Dictating,
                "ctrl-enter",
                Action::FinishDictation,
                "Finish dictation and send message",
            ),
            b(
                KeyContext::Dictating,
                "escape",
                Action::CancelDictation,
                "Cancel dictation and restore text",
            ),
            b(
                KeyContext::VoiceRecording,
                "enter",
                Action::FinishVoiceRecord,
                "Finish and send voice note",
            ),
            b(
                KeyContext::VoiceRecording,
                "escape",
                Action::CancelVoiceRecord,
                "Cancel voice recording",
            ),
            b(
                KeyContext::ShortcodePopup,
                "up",
                Action::ShortcodePrev,
                "Previous shortcode suggestion",
            ),
            b(
                KeyContext::ShortcodePopup,
                "down",
                Action::ShortcodeNext,
                "Next shortcode suggestion",
            ),
            b(
                KeyContext::ShortcodePopup,
                "enter",
                Action::ShortcodeAccept,
                "Accept shortcode suggestion",
            ),
            b(
                KeyContext::ShortcodePopup,
                "tab",
                Action::ShortcodeAccept,
                "Accept shortcode suggestion",
            ),
            b(
                KeyContext::ShortcodePopup,
                "escape",
                Action::CloseShortcodePopup,
                "Close shortcode popup",
            ),
        ];

        for slot in 1..=9u8 {
            bindings.push(b(
                KeyContext::Workspace,
                &format!("ctrl-{slot}"),
                Action::OpenChatSlot(slot),
                &format!("Open chat #{slot}"),
            ));
        }
        for view in 2..=9u8 {
            bindings.push(b(
                KeyContext::Workspace,
                &format!("alt-{view}"),
                Action::SwitchAccountView(view),
                &format!("Switch to account view #{view}"),
            ));
        }

        Self { bindings }
    }

    pub fn apply_override_json(&mut self, raw_json: &str) -> Result<(), KeymapError> {
        let parsed: KeymapOverride = serde_json::from_str(raw_json)?;
        let mut by_key: BTreeMap<(KeyContext, String), usize> = self
            .bindings
            .iter()
            .enumerate()
            .map(|(i, item)| ((item.context, item.stroke.clone()), i))
            .collect();

        for ov in parsed.overrides {
            let stroke = ov.stroke.to_ascii_lowercase();
            let lookup = (ov.context, stroke.clone());
            if let Some(&idx) = by_key.get(&lookup) {
                self.bindings[idx].action = ov.action;
            } else {
                let idx = self.bindings.len();
                self.bindings.push(Binding {
                    context: ov.context,
                    stroke: stroke.clone(),
                    action: ov.action,
                    description: "User override".to_owned(),
                });
                by_key.insert((ov.context, stroke), idx);
            }
        }
        Ok(())
    }

    /// Loads and applies overrides from `path`. Returns `Ok(false)` when the file does not exist.
    pub fn load_override_file(&mut self, path: impl AsRef<Path>) -> Result<bool, KeymapError> {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                self.apply_override_json(&raw)?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(KeymapError::Io(err)),
        }
    }

    pub fn resolve(&self, context: KeyContext, stroke: &str) -> Option<Action> {
        let needle = stroke.to_ascii_lowercase();
        self.bindings
            .iter()
            .find(|item| item.context == context && item.stroke == needle)
            .map(|item| item.action)
    }

    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }
}

fn b(context: KeyContext, stroke: &str, action: Action, description: &str) -> Binding {
    Binding {
        context,
        stroke: stroke.to_owned(),
        action,
        description: description.to_owned(),
    }
}

/// Smart Ctrl+A rule: selects chats when no text field is focused or when the main composer is empty.
pub fn ctrl_a_selects_chats(
    text_field_focused: bool,
    compose_focused: bool,
    compose_empty: bool,
) -> bool {
    !text_field_focused || (compose_focused && compose_empty)
}

/// Smart Ctrl+C rule: copies selected message text unless the composer has an active text selection.
pub fn ctrl_c_copies_message(composer_focused: bool, has_selection: bool) -> bool {
    !(composer_focused && has_selection)
}

/// `G then I` chord (within 900 ms) to jump to the combined inbox.
#[derive(Debug, Default)]
pub struct InboxChord {
    started_ms: Option<u64>,
}

impl InboxChord {
    pub const WINDOW_MS: u64 = 900;

    pub fn on_key(&mut self, key: char, now_ms: u64, allowed: bool, repeated: bool) -> bool {
        if !allowed {
            self.started_ms = None;
            return false;
        }
        let upper = key.to_ascii_uppercase();
        if repeated && upper == 'G' {
            return false;
        }
        let started = self.started_ms.take();
        if !repeated
            && upper == 'I'
            && started.is_some_and(|t| now_ms.saturating_sub(t) <= Self::WINDOW_MS)
        {
            return true;
        }
        if !repeated && upper == 'G' {
            self.started_ms = Some(now_ms);
        }
        false
    }

    pub fn reset(&mut self) {
        self.started_ms = None;
    }

    pub fn is_armed(&self) -> bool {
        self.started_ms.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatestChordOutcome {
    Ignored,
    ArmedHint,
    JumpLatest,
}

/// `Alt+G, Alt+G` double-tap chord (within 900 ms) to jump to the latest message.
#[derive(Debug, Default)]
pub struct LatestChord {
    started_ms: Option<u64>,
}

impl LatestChord {
    pub const WINDOW_MS: u64 = 900;
    pub const HINT_STATUS: &'static str = "Press Alt+G again to jump to the latest message";
    pub const JUMPED_STATUS: &'static str = "Jumped to latest message";

    pub fn on_key(
        &mut self,
        key: char,
        alt: bool,
        control: bool,
        shift: bool,
        now_ms: u64,
        repeated: bool,
    ) -> LatestChordOutcome {
        let is_alt_g = key.eq_ignore_ascii_case(&'g') && alt && !control && !shift;
        if !is_alt_g {
            self.started_ms = None;
            return LatestChordOutcome::Ignored;
        }
        self.on_alt_g(now_ms, repeated)
    }

    pub fn on_alt_g(&mut self, now_ms: u64, repeated: bool) -> LatestChordOutcome {
        if repeated {
            return LatestChordOutcome::Ignored;
        }
        if let Some(started) = self.started_ms
            && now_ms.saturating_sub(started) <= Self::WINDOW_MS
        {
            self.started_ms = None;
            return LatestChordOutcome::JumpLatest;
        }
        self.started_ms = Some(now_ms);
        LatestChordOutcome::ArmedHint
    }

    pub fn reset(&mut self) {
        self.started_ms = None;
    }

    pub fn is_armed(&self) -> bool {
        self.started_ms.is_some()
    }
}

/// Visibility state machine for hold-`Ctrl` `1..9` sidebar slot badges.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CtrlBadgeState {
    visible: bool,
}

impl CtrlBadgeState {
    /// Marks `Ctrl` held down. Returns `true` only if visibility changed.
    pub fn on_ctrl_down(&mut self) -> bool {
        if self.visible {
            false
        } else {
            self.visible = true;
            true
        }
    }

    /// Marks `Ctrl` released. Returns `true` only if visibility changed.
    pub fn on_ctrl_up(&mut self) -> bool {
        if self.visible {
            self.visible = false;
            true
        } else {
            false
        }
    }

    /// Clears badge visibility when the window loses focus. Returns `true` only if visibility changed.
    pub fn on_blur(&mut self) -> bool {
        self.on_ctrl_up()
    }

    pub fn visible(&self) -> bool {
        self.visible
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageListKey {
    Enter,
    R,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MessageListKeyContext {
    pub has_selected_message: bool,
    pub composer_focused: bool,
    pub composer_empty: bool,
    pub thread_open: bool,
    pub modal_open: bool,
    pub slack_threads_enabled: bool,
    pub local_slack_style: bool,
    pub selected_from_me: bool,
    pub selected_is_failed: bool,
    pub selected_is_normal_send_state: bool,
    pub selected_is_image: bool,
    pub has_staged_images: bool,
}

/// 4-tier priority cascade for `Enter` / `R` on a selected message (`main.zig:21322-21363`):
/// 1. `Enter` on Slack thread-eligible chat -> `OpenThread`
/// 2. `Enter` or `R` on own failed message -> `ResendFailedMessage`
/// 3. `Enter` on image message (with no staged images) -> `OpenImageLightbox`
/// 4. `Enter` or `R` (when composer not focused for `R`) on normal message -> `ReplyToMessage`
pub fn resolve_message_list_action(
    key: MessageListKey,
    ctx: MessageListKeyContext,
) -> Option<Action> {
    if !ctx.has_selected_message || !ctx.composer_empty || ctx.modal_open {
        return None;
    }
    if key == MessageListKey::Enter
        && !ctx.thread_open
        && ctx.slack_threads_enabled
        && ctx.local_slack_style
    {
        return Some(Action::OpenThread);
    }
    if ctx.selected_from_me && ctx.selected_is_failed {
        return Some(Action::ResendFailedMessage);
    }
    if key == MessageListKey::Enter && ctx.selected_is_image && !ctx.has_staged_images {
        return Some(Action::OpenImageLightbox);
    }
    if !ctx.thread_open
        && ctx.selected_is_normal_send_state
        && (key == MessageListKey::Enter || !ctx.composer_focused)
    {
        return Some(Action::ReplyToMessage);
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutEntry {
    pub key: &'static str,
    pub desc: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutGroup {
    pub name: &'static str,
    pub items: &'static [ShortcutEntry],
}

const CHATS_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Ctrl+1 to Ctrl+9",
        desc: "Open one of the top nine chats",
    },
    ShortcutEntry {
        key: "Ctrl+Tab / Ctrl+Shift+Tab",
        desc: "Next / previous chat",
    },
    ShortcutEntry {
        key: "Ctrl+Shift+Home",
        desc: "Jump to the first chat",
    },
    ShortcutEntry {
        key: "Ctrl+Shift+End",
        desc: "Jump to the last chat",
    },
    ShortcutEntry {
        key: "Ctrl+Page Up / Ctrl+Page Down",
        desc: "Move the selection by a page",
    },
    ShortcutEntry {
        key: "Mouse wheel over the list",
        desc: "Scroll the chat list",
    },
    ShortcutEntry {
        key: "Ctrl+F or /",
        desc: "Search chats in the command palette",
    },
    ShortcutEntry {
        key: "Esc",
        desc: "Clear the search and show all chats",
    },
    ShortcutEntry {
        key: "Alt+A",
        desc: "Show or hide archived chats in this view",
    },
    ShortcutEntry {
        key: "U",
        desc: "Toggle unread chats",
    },
    ShortcutEntry {
        key: "Ctrl+E or E",
        desc: "Archive or unarchive the selected chat",
    },
    ShortcutEntry {
        key: "Ctrl+A in the chat list or empty message box",
        desc: "Select all chats; Ctrl+E archives, Esc cancels",
    },
    ShortcutEntry {
        key: "R",
        desc: "Refresh",
    },
    ShortcutEntry {
        key: "Q",
        desc: "Quit Postbox",
    },
];

const MESSAGES_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Alt+J / Alt+K",
        desc: "Select the next / previous message",
    },
    ShortcutEntry {
        key: "Up / Down",
        desc: "Walk the message highlight (composer empty)",
    },
    ShortcutEntry {
        key: "Ctrl+Up / Ctrl+Down",
        desc: "Walk the message highlight",
    },
    ShortcutEntry {
        key: "Page Up / Page Down",
        desc: "Scroll the chat a screen at a time",
    },
    ShortcutEntry {
        key: "Ctrl+End",
        desc: "Jump back to the newest message",
    },
    ShortcutEntry {
        key: "Alt+G, Alt+G",
        desc: "Jump to the latest message (press twice)",
    },
    ShortcutEntry {
        key: "Ctrl+R",
        desc: "React to message (search any emoji)",
    },
    ShortcutEntry {
        key: "Ctrl+Shift+R",
        desc: "Reply to the selected message",
    },
    ShortcutEntry {
        key: "Enter or R on a focused message",
        desc: "Start a reply to it",
    },
    ShortcutEntry {
        key: "Ctrl+C or Ctrl+Shift+C",
        desc: "Copy the selected message text",
    },
    ShortcutEntry {
        key: "Ctrl+P",
        desc: "Play or pause the selected voice message",
    },
    ShortcutEntry {
        key: "Ctrl+T",
        desc: "Show or hide the transcript",
    },
    ShortcutEntry {
        key: "Enter or R on a failed message",
        desc: "Resend it",
    },
];

const COMPOSER_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Enter",
        desc: "Send the message, or focus the composer if it isn't focused",
    },
    ShortcutEntry {
        key: "Shift+Enter",
        desc: "Insert a new line",
    },
    ShortcutEntry {
        key: "Ctrl+A",
        desc: "Select all text, or all visible chats if the message box is empty",
    },
    ShortcutEntry {
        key: "C",
        desc: "Focus the composer",
    },
    ShortcutEntry {
        key: "Ctrl+V",
        desc: "Paste a copied image to send",
    },
    ShortcutEntry {
        key: "Ctrl+Shift+M",
        desc: "Start or finish a voice note",
    },
    ShortcutEntry {
        key: "Esc",
        desc: "Cancel the reply or discard a pasted image",
    },
    ShortcutEntry {
        key: "Arrows / Enter / Esc, emoji picker open",
        desc: "Move, insert or close the emoji picker",
    },
];

const MESSENGERS_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Ctrl+K > Connect Slack workspace",
        desc: "Sign in with Slack in the browser",
    },
    ShortcutEntry {
        key: "Ctrl+K > Slack sign-in settings",
        desc: "Change the Slack Client ID and Socket Mode token",
    },
    ShortcutEntry {
        key: "Ctrl+K > Disconnect Slack workspace",
        desc: "Remove a workspace and its saved credentials",
    },
    ShortcutEntry {
        key: "Alt+1 / Alt+I / G then I",
        desc: "Combined inbox",
    },
    ShortcutEntry {
        key: "Alt+2..9",
        desc: "Accounts in saved view order",
    },
];

const OPEN_AND_VIEW_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Ctrl+O",
        desc: "Open links and media in the selected message",
    },
    ShortcutEntry {
        key: "Double-click a video",
        desc: "Play it inline",
    },
    ShortcutEntry {
        key: "F11",
        desc: "Toggle fullscreen for the inline video player",
    },
    ShortcutEntry {
        key: "Esc, Space, Enter or a click",
        desc: "Close the image viewer",
    },
];

const APP_SHORTCUTS: &[ShortcutEntry] = &[
    ShortcutEntry {
        key: "Ctrl+K",
        desc: "Open the command palette",
    },
    ShortcutEntry {
        key: "F1",
        desc: "Show this keyboard shortcuts overlay",
    },
    ShortcutEntry {
        key: "Up / Down / J / K, palette open",
        desc: "Move the palette selection",
    },
    ShortcutEntry {
        key: "Enter / Esc, palette open",
        desc: "Run the selected item, or close the palette",
    },
    ShortcutEntry {
        key: "Ctrl+D",
        desc: "Toggle dictation",
    },
    ShortcutEntry {
        key: "Ctrl+- / Ctrl++ / Ctrl+0",
        desc: "Make text smaller, larger, or reset it",
    },
];

const F1_SHORTCUT_GROUPS: &[ShortcutGroup] = &[
    ShortcutGroup {
        name: "Chats",
        items: CHATS_SHORTCUTS,
    },
    ShortcutGroup {
        name: "Messages",
        items: MESSAGES_SHORTCUTS,
    },
    ShortcutGroup {
        name: "Composer",
        items: COMPOSER_SHORTCUTS,
    },
    ShortcutGroup {
        name: "Messengers",
        items: MESSENGERS_SHORTCUTS,
    },
    ShortcutGroup {
        name: "Open and view",
        items: OPEN_AND_VIEW_SHORTCUTS,
    },
    ShortcutGroup {
        name: "App",
        items: APP_SHORTCUTS,
    },
];

pub fn f1_shortcut_groups() -> &'static [ShortcutGroup] {
    F1_SHORTCUT_GROUPS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn default_keymap_resolves_postbox_core_shortcuts() {
        let map = Keymap::postbox_defaults();
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-k"),
            Some(Action::OpenPalette)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-1"),
            Some(Action::OpenChatSlot(1))
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "alt-2"),
            Some(Action::SwitchAccountView(2))
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-d"),
            Some(Action::ToggleDictation)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-shift-m"),
            Some(Action::ToggleVoiceRecord)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-shift-e"),
            Some(Action::KeepArchivedSelected)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-c"),
            Some(Action::SmartCopy)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-backspace"),
            Some(Action::DeleteWordBackward)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "pageup"),
            Some(Action::PageUpMessages)
        );
        assert_eq!(
            map.resolve(KeyContext::Workspace, "pagedown"),
            Some(Action::PageDownMessages)
        );
        assert_eq!(
            map.resolve(KeyContext::ComposerEmpty, "up"),
            Some(Action::PrevMessage)
        );
        assert_eq!(
            map.resolve(KeyContext::ComposerEmpty, "down"),
            Some(Action::NextMessage)
        );
        assert_eq!(
            map.resolve(KeyContext::Palette, "ctrl-up"),
            Some(Action::MoveViewUp)
        );
        assert_eq!(
            map.resolve(KeyContext::Palette, "j"),
            Some(Action::PaletteNext)
        );
        assert_eq!(
            map.resolve(KeyContext::ThreadPanel, "space"),
            Some(Action::ToggleThreadBroadcast)
        );
        assert_eq!(
            map.resolve(KeyContext::Dictating, "ctrl-enter"),
            Some(Action::FinishDictation)
        );
        assert_eq!(
            map.resolve(KeyContext::VoiceRecording, "enter"),
            Some(Action::FinishVoiceRecord)
        );
        assert_eq!(
            map.resolve(KeyContext::EmojiPicker, "left"),
            Some(Action::EmojiMoveLeft)
        );
        assert_eq!(
            map.resolve(KeyContext::ShortcodePopup, "tab"),
            Some(Action::ShortcodeAccept)
        );
        assert_eq!(
            map.resolve(KeyContext::Lightbox, "right"),
            Some(Action::NextLightboxImage)
        );
        assert_eq!(
            map.resolve(KeyContext::VideoPlayer, "escape"),
            Some(Action::CloseVideoPlayer)
        );
    }

    #[test]
    fn default_keymap_has_zero_duplicate_context_stroke_pairs() {
        let map = Keymap::postbox_defaults();
        let mut seen = BTreeSet::new();
        for binding in map.bindings() {
            let key = (binding.context, binding.stroke.clone());
            assert!(
                seen.insert(key.clone()),
                "duplicate default binding for {:?}",
                key
            );
        }
    }

    #[test]
    fn user_keymap_json_overrides_default_binding() {
        let mut map = Keymap::postbox_defaults();
        map.apply_override_json(
            r#"{"overrides":[{"context":"workspace","stroke":"ctrl-k","action":"search_chats"}]}"#,
        )
        .unwrap();
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-k"),
            Some(Action::SearchChats)
        );
    }

    #[test]
    fn load_override_file_handles_missing_valid_and_invalid_files() {
        let mut map = Keymap::postbox_defaults();
        let missing = std::env::temp_dir().join("postbox-missing-keymap-nonexistent.json");
        assert!(!map.load_override_file(&missing).unwrap());

        let valid_path =
            std::env::temp_dir().join(format!("postbox-keymap-test-{}.json", std::process::id()));
        std::fs::write(
            &valid_path,
            r#"{"overrides":[{"context":"workspace","stroke":"ctrl-k","action":"quit"}]}"#,
        )
        .unwrap();
        assert!(map.load_override_file(&valid_path).unwrap());
        assert_eq!(
            map.resolve(KeyContext::Workspace, "ctrl-k"),
            Some(Action::Quit)
        );

        std::fs::write(&valid_path, "{ not valid json").unwrap();
        assert!(matches!(
            map.load_override_file(&valid_path),
            Err(KeymapError::InvalidJson(_))
        ));
        let _ = std::fs::remove_file(&valid_path);
    }

    #[test]
    fn inbox_chord_respects_900ms_window_and_repeat_guard() {
        let mut chord = InboxChord::default();
        assert!(!chord.on_key('g', 100, true, false));
        assert!(chord.is_armed());
        assert!(!chord.on_key('g', 500, true, true));
        assert!(!chord.on_key('i', 1001, true, false));

        assert!(!chord.on_key('g', 2000, true, false));
        assert!(chord.on_key('i', 2900, true, false));

        assert!(!chord.on_key('g', 3000, true, false));
        chord.reset();
        assert!(!chord.is_armed());
        assert!(!chord.on_key('i', 3100, true, false));
    }

    #[test]
    fn latest_chord_enforces_900ms_window_repeat_guard_and_reset() {
        let mut chord = LatestChord::default();
        assert_eq!(
            chord.on_key('g', true, false, false, 100, false),
            LatestChordOutcome::ArmedHint
        );
        assert!(chord.is_armed());
        // Key repeat must NOT trigger jump or extend window
        assert_eq!(
            chord.on_key('g', true, false, false, 950, true),
            LatestChordOutcome::Ignored
        );
        // Past 900 ms window -> re-arms instead of jumping
        assert_eq!(
            chord.on_key('g', true, false, false, 1001, false),
            LatestChordOutcome::ArmedHint
        );
        // Within 900 ms -> JumpLatest
        assert_eq!(
            chord.on_key('g', true, false, false, 1901, false),
            LatestChordOutcome::JumpLatest
        );
        assert!(!chord.is_armed());

        // Arm then interrupt with another key or extra modifier
        assert_eq!(
            chord.on_key('g', true, false, false, 2000, false),
            LatestChordOutcome::ArmedHint
        );
        assert_eq!(
            chord.on_key('x', true, false, false, 2100, false),
            LatestChordOutcome::Ignored
        );
        assert!(!chord.is_armed());

        assert_eq!(
            chord.on_key('g', true, false, false, 3000, false),
            LatestChordOutcome::ArmedHint
        );
        assert_eq!(
            chord.on_key('g', true, true, false, 3100, false),
            LatestChordOutcome::Ignored
        );
        assert!(!chord.is_armed());
    }

    #[test]
    fn ctrl_badge_state_transitions_and_blur_clear() {
        let mut badges = CtrlBadgeState::default();
        assert!(!badges.visible());
        assert!(badges.on_ctrl_down());
        assert!(badges.visible());
        assert!(!badges.on_ctrl_down());
        assert!(badges.on_ctrl_up());
        assert!(!badges.visible());
        assert!(!badges.on_ctrl_up());

        assert!(badges.on_ctrl_down());
        assert!(badges.on_blur());
        assert!(!badges.visible());
    }

    #[test]
    fn ctrl_a_in_palette_never_selects_chats() {
        assert!(!ctrl_a_selects_chats(true, false, true));
        assert!(ctrl_a_selects_chats(true, true, true));
        assert!(!ctrl_a_selects_chats(true, true, false));
    }

    #[test]
    fn ctrl_c_copies_message_truth_table() {
        assert!(ctrl_c_copies_message(false, false));
        assert!(ctrl_c_copies_message(false, true));
        assert!(ctrl_c_copies_message(true, false));
        assert!(!ctrl_c_copies_message(true, true));
    }

    #[test]
    fn message_list_cascade_respects_four_tier_priority() {
        let base = MessageListKeyContext {
            has_selected_message: true,
            composer_focused: false,
            composer_empty: true,
            thread_open: false,
            modal_open: false,
            slack_threads_enabled: true,
            local_slack_style: true,
            selected_from_me: true,
            selected_is_failed: true,
            selected_is_normal_send_state: true,
            selected_is_image: true,
            has_staged_images: false,
        };
        // Tier 1: Slack thread wins on Enter
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, base),
            Some(Action::OpenThread)
        );
        // Tier 2: Resend failed wins on R (and on Enter when Slack threads disabled)
        assert_eq!(
            resolve_message_list_action(MessageListKey::R, base),
            Some(Action::ResendFailedMessage)
        );
        let no_thread = MessageListKeyContext {
            slack_threads_enabled: false,
            ..base
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, no_thread),
            Some(Action::ResendFailedMessage)
        );
        // Tier 3: Image lightbox wins on Enter when not failed
        let img_ctx = MessageListKeyContext {
            selected_is_failed: false,
            ..no_thread
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, img_ctx),
            Some(Action::OpenImageLightbox)
        );
        // Tier 4: Reply when not image (or staged images present)
        let reply_ctx = MessageListKeyContext {
            selected_is_image: false,
            ..img_ctx
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, reply_ctx),
            Some(Action::ReplyToMessage)
        );
        assert_eq!(
            resolve_message_list_action(MessageListKey::R, reply_ctx),
            Some(Action::ReplyToMessage)
        );
        // R with composer focused does NOT start reply
        let composer_focused = MessageListKeyContext {
            composer_focused: true,
            ..reply_ctx
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::R, composer_focused),
            None
        );
        // Non-empty composer always returns None
        let non_empty = MessageListKeyContext {
            composer_empty: false,
            ..base
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, non_empty),
            None
        );
    }

    #[test]
    fn message_list_cascade_blocks_all_four_tiers_when_modal_open() {
        let tier1 = MessageListKeyContext {
            has_selected_message: true,
            composer_focused: false,
            composer_empty: true,
            thread_open: false,
            modal_open: true,
            slack_threads_enabled: true,
            local_slack_style: true,
            selected_from_me: true,
            selected_is_failed: true,
            selected_is_normal_send_state: true,
            selected_is_image: true,
            has_staged_images: false,
        };
        // Tier 1 (OpenThread)
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, tier1),
            None
        );
        // Tier 2 (ResendFailedMessage via R and Enter)
        assert_eq!(resolve_message_list_action(MessageListKey::R, tier1), None);
        let tier2_enter = MessageListKeyContext {
            slack_threads_enabled: false,
            ..tier1
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, tier2_enter),
            None
        );
        // Tier 3 (OpenImageLightbox)
        let tier3 = MessageListKeyContext {
            selected_is_failed: false,
            ..tier2_enter
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, tier3),
            None
        );
        // Tier 4 (ReplyToMessage)
        let tier4 = MessageListKeyContext {
            selected_is_image: false,
            ..tier3
        };
        assert_eq!(
            resolve_message_list_action(MessageListKey::Enter, tier4),
            None
        );
        assert_eq!(resolve_message_list_action(MessageListKey::R, tier4), None);
    }

    #[test]
    fn f1_shortcut_groups_match_six_group_catalog_without_duplicates() {
        let groups = f1_shortcut_groups();
        let names: Vec<&str> = groups.iter().map(|g| g.name).collect();
        assert_eq!(
            names,
            vec![
                "Chats",
                "Messages",
                "Composer",
                "Messengers",
                "Open and view",
                "App",
            ]
        );
        for group in groups {
            assert!(!group.items.is_empty(), "empty group: {}", group.name);
            let mut keys = BTreeSet::new();
            for item in group.items {
                assert!(!item.key.trim().is_empty());
                assert!(!item.desc.trim().is_empty());
                assert!(
                    keys.insert(item.key),
                    "duplicate F1 key '{}' in group '{}'",
                    item.key,
                    group.name
                );
            }
        }
    }
}
