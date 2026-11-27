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
/// Card id for updater feedback after a manual `Ctrl+K` check.
pub const UPDATE_STATUS_CARD_ID: &str = "update-status";

/// `v0.1.1` for both `0.1.1` and `v0.1.1`, so every surface reads the same tag.
pub fn version_tag(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with('v') {
        trimmed.to_owned()
    } else {
        format!("v{trimmed}")
    }
}

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

/// Updater state machine, driven by the background scheduler and the `Ctrl+K` command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum UpdatePhase {
    /// Nothing has been checked in this run.
    #[default]
    Idle,
    /// A manifest check is running.
    Checking,
    /// A newer release is downloading and being staged in the background.
    Downloading,
    /// The download is verified and staged; the restart card is offered.
    Ready,
    /// The restart click is swapping the staged binary in.
    Installing,
    /// The running build is the newest.
    UpToDate,
    /// A manual update failed; the installed build is untouched.
    Failed,
}

/// The non-modal "Restart to update" card, shown once an update is staged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdatePrompt {
    pub tag: String,
    pub notes: String,
    pub created_ms: u64,
}

impl UpdatePrompt {
    pub fn new(tag: impl Into<String>, notes: impl Into<String>, now_ms: u64) -> Self {
        let notes: String = notes.into().chars().take(MAX_CARD_NOTE_CHARS).collect();
        Self {
            tag: tag.into(),
            notes,
            created_ms: now_ms,
        }
    }

    /// `Postbox v0.2.0 is ready`.
    pub fn headline(&self) -> String {
        format!("Postbox {} is ready", self.tag)
    }
}

/// The whole M7 shell state.
#[derive(Debug, Default)]
pub struct M7State {
    pub card: Option<StatusCard>,
    pub last_exit_record: Option<ExitRecord>,
    pub update_phase: UpdatePhase,
    pub pending_asset: Option<Box<UpdateAsset>>,
    pub update_note: Option<String>,
    /// The staged release the restart card offers, if one is ready.
    pub update_prompt: Option<UpdatePrompt>,
    /// Version dismissed this session, so the same card never nags twice.
    pub dismissed_update_tag: Option<String>,
    pub launched_from_link: bool,
}

impl M7State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version_label(&self, version: &str) -> String {
        match self.update_phase {
            UpdatePhase::Idle => format!("Postbox v{version}"),
            UpdatePhase::Checking => format!("Postbox v{version} (checking for updates...)"),
            UpdatePhase::Downloading => format!("Postbox v{version} (downloading update...)"),
            UpdatePhase::Ready => {
                if let Some(prompt) = &self.update_prompt {
                    format!("Postbox v{version} ({} ready to install)", prompt.tag)
                } else {
                    format!("Postbox v{version} (update ready)")
                }
            }
            UpdatePhase::Installing => format!("Postbox v{version} (installing update...)"),
            UpdatePhase::UpToDate => format!("Postbox v{version} (up to date)"),
            UpdatePhase::Failed => format!("Postbox v{version} (update check failed)"),
        }
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

    /// Enters the checking state. `false` when a cycle is already running.
    ///
    /// A manual `Ctrl+K` check also un-dismisses a card the user hid earlier, which is the
    /// one way to bring it back without a newer release.
    pub fn begin_check(&mut self, manual: bool) -> bool {
        if self.busy() {
            return false;
        }
        if manual {
            self.reshow_update_prompt();
        }
        self.update_phase = UpdatePhase::Checking;
        self.update_note = Some("Checking for updates...".to_owned());
        true
    }

    /// Folds a manifest into the updater state.
    pub fn apply_manifest(&mut self, manifest: &UpdateManifest, current: Version) -> CheckOutcome {
        let outcome = postbox_core::update::evaluate(manifest, current);
        match &outcome {
            CheckOutcome::UpToDate { current } => {
                self.mark_up_to_date(current, true);
            }
            CheckOutcome::Available { asset } => {
                self.offer(asset.as_ref().clone(), true);
            }
        }
        outcome
    }

    /// The running build is the newest. A staged card, if any, stays offered.
    pub fn mark_up_to_date(&mut self, current: &str, manual: bool) {
        self.pending_asset = None;
        self.update_phase = if self.update_prompt.is_some() {
            UpdatePhase::Ready
        } else {
            UpdatePhase::UpToDate
        };
        self.update_note = manual.then(|| self.up_to_date_notice(current));
    }

    /// `Postbox v0.1.1 is up to date`, the notice a manual check leaves behind.
    pub fn up_to_date_notice(&self, current: &str) -> String {
        format!("Postbox {} is up to date", version_tag(current))
    }

    /// A dismissible card for updater feedback, so a manual check leaves proof on screen.
    ///
    /// Manual checks only: a background cycle stays quiet, because the card would nag about
    /// something the user did not ask for.
    pub fn show_update_card(
        &mut self,
        title: impl Into<String>,
        note: impl Into<String>,
        now_ms: u64,
    ) {
        self.card = Some(StatusCard::new(UPDATE_STATUS_CARD_ID, title, note, now_ms));
    }

    /// A newer release was found. `true` when it still needs downloading and staging.
    ///
    /// An already staged release with the same tag is surfaced again instead of downloaded
    /// twice, which is what a manual check does.
    pub fn offer(&mut self, asset: UpdateAsset, manual: bool) -> bool {
        let already_staged = self.staged_tag() == Some(asset.tag.as_str());
        if already_staged {
            self.update_phase = UpdatePhase::Ready;
            if manual {
                self.dismissed_update_tag = None;
            }
            self.update_note = Some(format!(
                "Postbox {} is ready. Restart to update.",
                asset.tag
            ));
            self.pending_asset = Some(Box::new(asset));
            return false;
        }
        self.update_phase = UpdatePhase::Downloading;
        self.update_note = Some("Downloading the update in the background...".to_owned());
        self.pending_asset = Some(Box::new(asset));
        true
    }

    /// The download is verified and extracted beside the live binary.
    pub fn mark_staged(&mut self, tag: &str, notes: &str, now_ms: u64) {
        self.update_phase = UpdatePhase::Ready;
        self.update_prompt = Some(UpdatePrompt::new(tag, notes, now_ms));
        self.update_note = Some(format!("Postbox {tag} is ready. Restart to update."));
    }

    pub fn mark_installing(&mut self) {
        self.update_phase = UpdatePhase::Installing;
        self.update_note = Some("Installing the update...".to_owned());
    }

    pub fn mark_installed(&mut self, tag: &str) {
        self.update_phase = UpdatePhase::UpToDate;
        self.pending_asset = None;
        self.update_prompt = None;
        self.update_note = Some(format!("Updated to Postbox {tag}. Restarting."));
    }

    /// A failure the user asked for is reported; a background one fails quietly.
    pub fn mark_failed(&mut self, reason: &str, manual: bool) {
        if manual {
            self.update_phase = UpdatePhase::Failed;
            self.update_note = Some(format!(
                "Update failed: {reason}. Your installed version is untouched."
            ));
        } else {
            self.update_phase = UpdatePhase::Idle;
            self.update_note = None;
        }
    }

    /// Tag of the staged release, whether or not the card is currently shown.
    pub fn staged_tag(&self) -> Option<&str> {
        self.update_prompt
            .as_ref()
            .map(|prompt| prompt.tag.as_str())
    }

    /// The card to paint, when the user has not dismissed this version.
    pub fn visible_update_prompt(&self) -> Option<&UpdatePrompt> {
        let prompt = self.update_prompt.as_ref()?;
        if self.dismissed_update_tag.as_deref() == Some(prompt.tag.as_str()) {
            return None;
        }
        Some(prompt)
    }

    pub fn prompt_visible(&self) -> bool {
        self.visible_update_prompt().is_some()
    }

    /// Hides the card for this version for the rest of the session.
    pub fn dismiss_update_prompt(&mut self) -> bool {
        let Some(prompt) = self.visible_update_prompt() else {
            return false;
        };
        self.dismissed_update_tag = Some(prompt.tag.clone());
        true
    }

    /// Brings a dismissed card back, for a manual `Ctrl+K` check.
    pub fn reshow_update_prompt(&mut self) -> bool {
        if self.update_prompt.is_none() {
            return false;
        }
        self.dismissed_update_tag = None;
        self.update_phase = UpdatePhase::Ready;
        true
    }

    /// Extra `Ctrl+K` entry once a restart is waiting, naming the staged release.
    pub fn restart_label(&self) -> Option<String> {
        let prompt = self.update_prompt.as_ref()?;
        Some(format!(
            "Restart to install Postbox {}",
            version_tag(&prompt.tag)
        ))
    }

    /// Text the `Ctrl+K` check entry shows. A staged update keeps this entry as the check,
    /// because [`M7State::restart_label`] adds the separate restart entry beside it.
    pub fn update_label(&self) -> &'static str {
        match self.update_phase {
            UpdatePhase::Idle | UpdatePhase::UpToDate => "Check for updates",
            UpdatePhase::Checking => "Checking for updates...",
            UpdatePhase::Downloading => "Downloading the update...",
            UpdatePhase::Ready => "Check for updates now",
            UpdatePhase::Installing => "Installing update...",
            UpdatePhase::Failed => "Retry the update check",
        }
    }

    /// Second line of the `Ctrl+K` check entry: the running version plus live updater state.
    ///
    /// `v0.1.1` alone before the first check, then `v0.1.1 \u{00B7} Up to date`,
    /// `v0.1.1 \u{00B7} Downloading v0.1.2...` and so on.
    pub fn update_hint(&self, current: &str) -> String {
        let running = version_tag(current);
        let status = match self.update_phase {
            UpdatePhase::Idle => return running,
            UpdatePhase::Checking => "Checking...".to_owned(),
            UpdatePhase::Downloading => match &self.pending_asset {
                Some(asset) => format!("Downloading {}...", version_tag(&asset.tag)),
                None => "Downloading...".to_owned(),
            },
            UpdatePhase::Ready => match self.update_prompt.as_ref() {
                Some(prompt) => format!("{} ready to install", version_tag(&prompt.tag)),
                None => "Ready to install".to_owned(),
            },
            UpdatePhase::Installing => "Installing...".to_owned(),
            UpdatePhase::UpToDate => "Up to date".to_owned(),
            UpdatePhase::Failed => "Update check failed".to_owned(),
        };
        format!("{running} \u{00B7} {status}")
    }

    pub fn busy(&self) -> bool {
        matches!(
            self.update_phase,
            UpdatePhase::Checking | UpdatePhase::Downloading | UpdatePhase::Installing
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
    fn version_label_reports_the_current_version_and_update_phase() {
        for (phase, expected) in [
            (UpdatePhase::Idle, "Postbox v0.1.42"),
            (
                UpdatePhase::Checking,
                "Postbox v0.1.42 (checking for updates...)",
            ),
            (
                UpdatePhase::Downloading,
                "Postbox v0.1.42 (downloading update...)",
            ),
            (UpdatePhase::Ready, "Postbox v0.1.42 (update ready)"),
            (
                UpdatePhase::Installing,
                "Postbox v0.1.42 (installing update...)",
            ),
            (UpdatePhase::UpToDate, "Postbox v0.1.42 (up to date)"),
            (UpdatePhase::Failed, "Postbox v0.1.42 (update check failed)"),
        ] {
            let state = M7State {
                update_phase: phase,
                ..M7State::new()
            };
            assert_eq!(state.version_label("0.1.42"), expected, "{phase:?}");
        }
    }

    #[test]
    fn version_label_includes_the_staged_release_tag_when_ready() {
        let state = M7State {
            update_phase: UpdatePhase::Ready,
            update_prompt: Some(UpdatePrompt::new("v0.1.43", "", 0)),
            ..M7State::new()
        };
        assert_eq!(
            state.version_label("0.1.42"),
            "Postbox v0.1.42 (v0.1.43 ready to install)"
        );
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
    fn a_newer_manifest_starts_the_background_download() {
        let mut state = M7State::new();
        let outcome = state.apply_manifest(&manifest("v0.2.0"), Version::new(0, 1, 0));
        assert!(matches!(outcome, CheckOutcome::Available { .. }));
        assert_eq!(state.update_phase, UpdatePhase::Downloading);
        assert!(state.pending_asset.is_some());
        assert_eq!(state.update_label(), "Downloading the update...");
        assert!(state.busy());
    }

    #[test]
    fn the_installed_version_reads_as_up_to_date() {
        let mut state = M7State::new();
        let outcome = state.apply_manifest(&manifest("v0.1.0"), Version::new(0, 1, 0));
        assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
        assert_eq!(state.update_phase, UpdatePhase::UpToDate);
        assert!(state.pending_asset.is_none());
        assert_eq!(state.update_label(), "Check for updates");
        assert_eq!(state.update_hint("0.1.0"), "v0.1.0 \u{00B7} Up to date");
        assert!(state.update_note.is_some());
    }

    #[test]
    fn a_staged_update_shows_the_restart_card_and_dismissal_hides_that_version() {
        let mut state = M7State::new();
        assert!(state.begin_check(false));
        assert!(state.busy());
        state.mark_staged("v0.2.0", "- Toasts open the right chat", 1_000);

        assert_eq!(state.update_phase, UpdatePhase::Ready);
        assert_eq!(
            state.update_label(),
            "Check for updates now",
            "the check entry stays a check"
        );
        assert_eq!(
            state.restart_label().as_deref(),
            Some("Restart to install Postbox v0.2.0")
        );
        assert_eq!(
            state.update_hint("0.1.0"),
            "v0.1.0 \u{00B7} v0.2.0 ready to install"
        );
        let prompt = state.visible_update_prompt().expect("the card is offered");
        assert_eq!(prompt.headline(), "Postbox v0.2.0 is ready");
        assert_eq!(prompt.notes, "- Toasts open the right chat");

        assert!(state.dismiss_update_prompt());
        assert!(!state.prompt_visible());
        assert!(
            !state.dismiss_update_prompt(),
            "dismissing twice is harmless"
        );
        assert_eq!(
            state.staged_tag(),
            Some("v0.2.0"),
            "it stays staged on disk"
        );
    }

    #[test]
    fn a_dismissed_card_comes_back_for_a_manual_check_or_a_newer_version() {
        let mut state = M7State::new();
        state.mark_staged("v0.2.0", "", 1_000);
        assert!(state.dismiss_update_prompt());
        assert!(!state.prompt_visible());

        // `Ctrl+K` asks for a check: the same staged version is surfaced again.
        assert!(state.begin_check(true));
        assert!(state.prompt_visible(), "the manual check surfaces the card");
        state.mark_up_to_date("0.1.0", true);
        assert!(state.dismiss_update_prompt());

        // A newer release replaces the staged one and re-prompts on its own.
        let asset = manifest("v0.3.0").select_asset().expect("the newer asset");
        assert!(state.offer(asset, false), "a new tag needs a download");
        assert_eq!(state.update_phase, UpdatePhase::Downloading);
        state.mark_staged("v0.3.0", "", 2_000);
        assert!(state.prompt_visible(), "a newer version re-prompts");
        assert_eq!(state.staged_tag(), Some("v0.3.0"));
    }

    #[test]
    fn a_manual_check_of_an_already_staged_version_does_not_download_again() {
        let mut state = M7State::new();
        state.mark_staged("v0.2.0", "", 1_000);
        state.dismiss_update_prompt();
        let asset = manifest("v0.2.0").select_asset().expect("the asset");

        assert!(!state.offer(asset, true), "the staged bytes are reused");
        assert_eq!(state.update_phase, UpdatePhase::Ready);
        assert!(state.prompt_visible(), "the card is surfaced again");
    }

    #[test]
    fn a_background_failure_is_quiet_while_a_manual_one_is_reported() {
        let mut state = M7State::new();
        assert!(state.begin_check(false));
        state.mark_failed("offline", false);
        assert_eq!(state.update_phase, UpdatePhase::Idle);
        assert!(state.update_note.is_none(), "no nagging status line");

        assert!(state.begin_check(true));
        state.mark_failed("digest mismatch", true);
        assert_eq!(state.update_phase, UpdatePhase::Failed);
        let note = state.update_note.as_deref().unwrap();
        assert!(note.contains("digest mismatch"), "{note}");
        assert!(note.contains("untouched"), "{note}");
        assert_eq!(state.update_label(), "Retry the update check");
    }

    #[test]
    fn a_failed_restart_says_the_installed_version_is_untouched() {
        let mut state = M7State::new();
        state.mark_staged("v0.2.0", "", 1_000);
        state.mark_installing();
        assert!(state.busy());
        state.mark_failed("another copy is running", true);
        assert_eq!(state.update_phase, UpdatePhase::Failed);
        let note = state.update_note.as_deref().unwrap();
        assert!(note.contains("untouched"), "{note}");
        assert!(state.prompt_visible(), "the card still offers a retry");
    }

    #[test]
    fn a_running_cycle_is_not_started_twice() {
        let mut state = M7State::new();
        assert!(state.begin_check(false));
        assert!(
            !state.begin_check(false),
            "a second tick does not stack checks"
        );
        state.mark_installed("v0.2.0");
        assert!(!state.busy());
        assert_eq!(state.update_phase, UpdatePhase::UpToDate);
        assert!(state.visible_update_prompt().is_none());
        assert!(state.restart_label().is_none());
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
    fn every_surface_spells_the_version_as_one_tag() {
        // The build string has no `v`; the manifest tag already has one. Both read the same.
        assert_eq!(version_tag("0.1.1"), "v0.1.1");
        assert_eq!(version_tag("v0.1.1"), "v0.1.1");
        assert_eq!(version_tag(" v0.1.1 "), "v0.1.1");

        let mut state = M7State::new();
        assert_eq!(state.update_hint("0.1.1"), "v0.1.1", "quiet before a check");
        state.mark_staged("v0.1.2", "", 1_000);
        assert_eq!(
            state.restart_label().as_deref(),
            Some("Restart to install Postbox v0.1.2")
        );
        assert_eq!(
            state.update_hint("0.1.1"),
            "v0.1.1 \u{00B7} v0.1.2 ready to install"
        );
    }

    #[test]
    fn a_manual_up_to_date_check_leaves_a_card_the_user_can_dismiss() {
        let mut state = M7State::new();
        state.mark_up_to_date("0.1.1", true);
        assert_eq!(
            state.update_note.as_deref(),
            Some("Postbox v0.1.1 is up to date")
        );
        state.show_update_card("Postbox is up to date", "Postbox v0.1.1 is up to date", 10);
        let card = state.card.as_ref().expect("the card is offered");
        assert_eq!(card.id, UPDATE_STATUS_CARD_ID);
        assert!(!card.is_missing_chat());
        assert!(state.close_card());
        assert!(state.card.is_none());

        // A background cycle says nothing at all.
        let mut quiet = M7State::new();
        quiet.mark_up_to_date("0.1.1", false);
        assert!(quiet.update_note.is_none());
        assert!(quiet.card.is_none());
    }

    #[test]
    fn card_notes_are_clipped_so_the_layout_stays_stable() {
        let card = StatusCard::new("x", "t", "y".repeat(1_000), 0);
        assert_eq!(card.note.chars().count(), MAX_CARD_NOTE_CHARS);
    }
}
