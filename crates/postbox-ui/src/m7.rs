//! Milestone 7 headless state: deep-link routing, the non-modal "Chat not found" card and
//! the "Check for updates" flow.
//!
//! Nothing here paints. `desktop.rs` calls [`M7State::accept_link`], renders
//! [`M7State::card`], and starts the updater task the state asks for.

use postbox_core::crash::ExitRecord;
use postbox_core::deep_link::{self, DeepLinkTarget};
use postbox_core::update::{CheckOutcome, UpdateAsset, UpdateManifest, Version};
use postbox_platform::os_integration::{LinkOutcome, route_line};

/// How long a status card stays up before the shell may hide it.
pub const CARD_DISMISS_AFTER_MS: u64 = 12_000;
/// Longest note shown in the card, in characters.
pub const MAX_CARD_NOTE_CHARS: usize = 160;

/// A dismissible, non-blocking card in the top of the chat pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusCard {
    pub id: String,
    pub title: String,
    pub note: String,
    pub created_ms: u64,
}

impl StatusCard {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        note: impl Into<String>,
        now_ms: u64,
    ) -> Self {
        let note: String = note.into().chars().take(MAX_CARD_NOTE_CHARS).collect();
        Self {
            id: id.into(),
            title: title.into(),
            note,
            created_ms: now_ms,
        }
    }

    /// The "Chat not found" card, using the provider-specific note from `postbox_core`.
    pub fn missing_chat(provider: postbox_core::Provider, now_ms: u64) -> Self {
        Self::new(
            "chat-not-found",
            "Chat not found",
            deep_link::missing_chat_note(provider),
            now_ms,
        )
    }

    /// `true` when the card is the deep-link "Chat not found" one.
    pub fn is_missing_chat(&self) -> bool {
        self.id == "chat-not-found"
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.created_ms) >= CARD_DISMISS_AFTER_MS
    }
}

/// What the shell should do after a link arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkAction {
    pub outcome: LinkOutcome,
    /// Chat to open and focus, when the link resolved.
    pub open: Option<DeepLinkTarget>,
    /// `true` when the window must be raised even if it already has focus.
    pub raise_window: bool,
}

/// Updater state machine, driven from the `Ctrl+K` command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    /// Nothing has been checked in this run.
    #[default]
    Idle,
    /// A check or a download is running.
    Busy,
    /// The running build is the newest.
    UpToDate,
    /// A newer build is available and waiting for the user's say-so.
    Available,
    /// The update is being applied.
    Installing,
    /// The update failed; the installed build is untouched.
    Failed,
}

/// The whole M7 shell state.
#[derive(Debug, Default)]
pub struct M7State {
    pub card: Option<StatusCard>,
    pub last_exit_record: Option<ExitRecord>,
    pub update_phase: UpdatePhase,
    pub pending_asset: Option<Box<UpdateAsset>>,
    pub update_note: Option<String>,
    pub launched_from_link: bool,
}

impl M7State {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parses a `postbox://` URL or a bare `focus` signal from the link pipe or the command
    /// line, and reports what the shell must do.
    pub fn accept_link(
        &mut self,
        line: &str,
        now_ms: u64,
        can_open: impl Fn(&DeepLinkTarget) -> bool,
    ) -> LinkAction {
        let outcome = route_line(line, can_open);
        self.close_card();
        match &outcome {
            LinkOutcome::Focused { .. } => {
                self.launched_from_link = true;
                LinkAction {
                    outcome: outcome.clone(),
                    open: deep_link::parse(line).ok(),
                    raise_window: true,
                }
            }
            LinkOutcome::MissingChat { note } => {
                self.card = Some(StatusCard::new(
                    "chat-not-found",
                    "Chat not found",
                    note.clone(),
                    now_ms,
                ));
                LinkAction {
                    outcome,
                    open: None,
                    raise_window: true,
                }
            }
            LinkOutcome::Rejected { note } => {
                self.card = Some(StatusCard::new(
                    "link-rejected",
                    "Link not understood",
                    note.clone(),
                    now_ms,
                ));
                LinkAction {
                    outcome,
                    open: None,
                    raise_window: true,
                }
            }
            LinkOutcome::FocusRaised => LinkAction {
                outcome,
                open: None,
                raise_window: true,
            },
        }
    }

    /// Dismisses the card. The `Esc` cascade calls this before anything else.
    pub fn close_card(&mut self) -> bool {
        self.card.take().is_some()
    }

    /// Drops the card once it is old enough. Called on every render tick.
    pub fn tick(&mut self, now_ms: u64) -> bool {
        match &self.card {
            Some(card) if card.expired(now_ms) => {
                self.card = None;
                true
            }
            _ => false,
        }
    }

    /// Folds a manifest into the updater state.
    pub fn apply_manifest(&mut self, manifest: &UpdateManifest, current: Version) -> CheckOutcome {
        let outcome = postbox_core::update::evaluate(manifest, current);
        match &outcome {
            CheckOutcome::UpToDate { current } => {
                self.update_phase = UpdatePhase::UpToDate;
                self.pending_asset = None;
                self.update_note = Some(format!("Postbox {current} is the newest version."));
            }
            CheckOutcome::Available { asset } => {
                self.update_phase = UpdatePhase::Available;
                self.pending_asset = Some(asset.clone());
                self.update_note = Some(format!("Postbox {} is available.", asset.tag));
            }
        }
        outcome
    }

    pub fn mark_installing(&mut self) {
        self.update_phase = UpdatePhase::Installing;
        self.update_note = Some("Installing the update...".to_owned());
    }

    pub fn mark_installed(&mut self, tag: &str) {
        self.update_phase = UpdatePhase::UpToDate;
        self.pending_asset = None;
        self.update_note = Some(format!("Updated to Postbox {tag}. Restarting."));
    }

    pub fn mark_failed(&mut self, reason: &str) {
        self.update_phase = UpdatePhase::Failed;
        self.update_note = Some(format!(
            "Update failed: {reason}. Your installed version is untouched."
        ));
    }

    /// Text the `Ctrl+K` palette entry shows.
    pub fn update_label(&self) -> &'static str {
        match self.update_phase {
            UpdatePhase::Idle => "Check for updates now",
            UpdatePhase::Busy => "Checking for updates...",
            UpdatePhase::UpToDate => "Postbox is up to date",
            UpdatePhase::Available => "Install the available update",
            UpdatePhase::Installing => "Installing update...",
            UpdatePhase::Failed => "Retry the update check",
        }
    }

    pub fn busy(&self) -> bool {
        matches!(
            self.update_phase,
            UpdatePhase::Busy | UpdatePhase::Installing
        )
    }

    /// `true` when the shell should show the "last run crashed" banner.
    pub fn should_report_last_crash(&self) -> bool {
        matches!(
            self.last_exit_record.as_ref().map(|record| record.kind),
            Some(postbox_core::crash::ExitKind::Panic)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::Provider;
    use postbox_core::update::digest_hex;

    const WA: &str = "postbox://whatsapp/biz/491234%40s.whatsapp.net";

    fn manifest(tag: &str) -> UpdateManifest {
        let body = format!(
            r#"{{"tag":"{tag}","sha256":"{}","asset":"postbox-windows-x64.zip","assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/postbox-windows-x64.zip","size":10}}]}}"#,
            digest_hex(b"payload")
        );
        UpdateManifest::parse(&body).unwrap()
    }

    #[test]
    fn a_known_link_asks_the_window_to_open_that_chat() {
        let mut state = M7State::new();
        let action = state.accept_link(WA, 1_000, |_| true);
        assert!(matches!(action.outcome, LinkOutcome::Focused { .. }));
        assert!(action.raise_window);
        let target = action.open.expect("the chat to open");
        assert_eq!(target.chat.provider, Provider::Whatsapp);
        assert_eq!(target.chat.account, "biz");
        assert_eq!(target.chat.id, "491234@s.whatsapp.net");
        assert!(state.card.is_none());
        assert!(state.launched_from_link);
    }

    #[test]
    fn a_missing_chat_shows_a_dismissible_never_blocking_card() {
        let mut state = M7State::new();
        let action = state.accept_link(WA, 5_000, |_| false);
        assert!(matches!(action.outcome, LinkOutcome::MissingChat { .. }));
        let card = state.card.clone().expect("the card is shown");
        assert_eq!(card.title, "Chat not found");
        assert_eq!(card.note, "Could not find that WhatsApp chat.");
        assert_eq!(card.created_ms, 5_000);
        // Nothing blocks input: the card is state only, and `close_card` clears it.
        assert!(state.close_card());
        assert!(state.card.is_none());
        assert!(!state.close_card(), "closing twice is harmless");
    }

    #[test]
    fn the_card_expires_on_its_own() {
        let mut state = M7State::new();
        state.accept_link(WA, 0, |_| false);
        assert!(state.card.is_some());
        assert!(!state.tick(CARD_DISMISS_AFTER_MS - 1));
        assert!(state.card.is_some());
        assert!(state.tick(CARD_DISMISS_AFTER_MS));
        assert!(state.card.is_none());
    }

    #[test]
    fn each_provider_gets_its_own_missing_chat_note() {
        for (url, expected) in [
            (
                "postbox://whatsapp/a/c",
                "Could not find that WhatsApp chat.",
            ),
            ("postbox://slack/a/c", "Could not find that Slack channel."),
            (
                "postbox://telegram/a/c",
                "Could not find that Telegram chat.",
            ),
            ("postbox://signal/a/c", "Could not find that Signal chat."),
        ] {
            let mut state = M7State::new();
            state.accept_link(url, 0, |_| false);
            assert_eq!(state.card.as_ref().unwrap().note, expected);
        }
    }

    #[test]
    fn a_focus_signal_raises_the_window_without_a_card() {
        let mut state = M7State::new();
        let action = state.accept_link("focus", 0, |_| true);
        assert_eq!(action.outcome, LinkOutcome::FocusRaised);
        assert!(action.raise_window);
        assert!(state.card.is_none());
    }

    #[test]
    fn a_broken_link_shows_a_card_instead_of_failing_the_launch() {
        let mut state = M7State::new();
        let action = state.accept_link("postbox://nope/a/b", 0, |_| true);
        assert!(matches!(action.outcome, LinkOutcome::Rejected { .. }));
        assert_eq!(state.card.as_ref().unwrap().title, "Link not understood");
    }

    #[test]
    fn a_new_link_replaces_an_old_card() {
        let mut state = M7State::new();
        state.accept_link(WA, 0, |_| false);
        assert_eq!(state.card.as_ref().unwrap().id, "chat-not-found");
        state.accept_link("https://example.com", 10, |_| true);
        assert_eq!(state.card.as_ref().unwrap().id, "link-rejected");
    }

    #[test]
    fn a_newer_manifest_moves_the_updater_to_available() {
        let mut state = M7State::new();
        let outcome = state.apply_manifest(&manifest("v0.2.0"), Version::new(0, 1, 0));
        assert!(matches!(outcome, CheckOutcome::Available { .. }));
        assert_eq!(state.update_phase, UpdatePhase::Available);
        assert!(state.pending_asset.is_some());
        assert_eq!(state.update_label(), "Install the available update");
        assert!(!state.busy());
    }

    #[test]
    fn the_installed_version_reads_as_up_to_date() {
        let mut state = M7State::new();
        let outcome = state.apply_manifest(&manifest("v0.1.0"), Version::new(0, 1, 0));
        assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
        assert_eq!(state.update_phase, UpdatePhase::UpToDate);
        assert!(state.pending_asset.is_none());
        assert_eq!(state.update_label(), "Postbox is up to date");
    }

    #[test]
    fn a_failed_update_says_the_installed_version_is_untouched() {
        let mut state = M7State::new();
        state.apply_manifest(&manifest("v0.2.0"), Version::new(0, 1, 0));
        state.mark_installing();
        assert!(state.busy());
        state.mark_failed("digest mismatch");
        assert_eq!(state.update_phase, UpdatePhase::Failed);
        let note = state.update_note.as_deref().unwrap();
        assert!(note.contains("digest mismatch"), "{note}");
        assert!(note.contains("untouched"), "{note}");
        assert_eq!(state.update_label(), "Retry the update check");
    }

    #[test]
    fn only_a_panic_record_deserves_the_crash_banner() {
        let mut state = M7State::new();
        assert!(!state.should_report_last_crash());
        state.last_exit_record = Some(ExitRecord::clean("0.1.0", 10, 20));
        assert!(!state.should_report_last_crash());
        state.last_exit_record = Some(ExitRecord::panic("0.1.0", 10, 20, Some("boom"), None));
        assert!(state.should_report_last_crash());
    }

    #[test]
    fn card_notes_are_clipped_so_the_layout_stays_stable() {
        let card = StatusCard::new("x", "t", "y".repeat(1_000), 0);
        assert_eq!(card.note.chars().count(), MAX_CARD_NOTE_CHARS);
    }
}
