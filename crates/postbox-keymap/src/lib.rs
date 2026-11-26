use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    Lightbox,
    VideoPlayer,
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
    SelectAllChats,
    Refresh,
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
    JumpLatest,
    ReactToMessage,
    ReplyToMessage,
    CopyMessageText,
    PlayPauseVoice,
    ToggleTranscript,
    OpenLinksAndMedia,
    FocusComposer,
    SendMessage,
    InsertNewline,
    PasteImage,
    ToggleVoiceRecord,
    ToggleDictation,
    FontSmaller,
    FontLarger,
    FontReset,
    ToggleVideoFullscreen,
    CloseLightbox,
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
                "ctrl-shift-c",
                Action::CopyMessageText,
                "Copy selected message text",
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
                "ctrl-0",
                Action::FontReset,
                "Reset font size",
            ),
            b(
                KeyContext::VideoPlayer,
                "f11",
                Action::ToggleVideoFullscreen,
                "Toggle fullscreen inline video",
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn inbox_chord_respects_900ms_window_and_repeat_guard() {
        let mut chord = InboxChord::default();
        assert!(!chord.on_key('g', 100, true, false));
        assert!(!chord.on_key('g', 500, true, true));
        assert!(!chord.on_key('i', 1001, true, false));

        assert!(!chord.on_key('g', 2000, true, false));
        assert!(chord.on_key('i', 2900, true, false));
    }

    #[test]
    fn ctrl_a_in_palette_never_selects_chats() {
        assert!(!ctrl_a_selects_chats(true, false, true));
        assert!(ctrl_a_selects_chats(true, true, true));
        assert!(!ctrl_a_selects_chats(true, true, false));
    }
}
