//! `postbox://` registration, the single-instance guard and the link forwarder.
//!
//! Every OS call sits behind a trait so the wiring is exercised on Linux CI with in-memory
//! doubles. Only `crate::win::os` touches `HKCU`, the mutex and the pipe.
//!
//! Startup order matters:
//! 1. If a `postbox://` URL arrived on the command line, parse it first.
//! 2. Try to take the single-instance lock.
//! 3. If the lock was already held, forward the URL (or a focus signal) over the link pipe
//!    and exit without ever creating a window.
//! 4. Otherwise register the protocol handler and start serving the link pipe.

use crate::PlatformError;
use postbox_core::deep_link::{self, DeepLinkTarget, LINK_PIPE_PREFIX};
use postbox_core::os_integration::{
    COMMAND_PLACEHOLDER, FOCUS_SIGNAL, SINGLE_INSTANCE_MUTEX_PREFIX, url_protocol_values,
};
use std::collections::HashMap;
use std::sync::Mutex;

/// Registry writes needed to own `postbox://` for the current user.
pub trait UrlHandler: Send + Sync {
    /// Writes the `HKCU\Software\Classes\postbox` values and returns `true` when it changed
    /// anything (so startup only writes the registry once).
    fn register(&self, exe_path: &str) -> Result<bool, PlatformError>;

    /// Reads back the stored `shell\open\command` value.
    fn registered_command(&self) -> Result<Option<String>, PlatformError>;

    /// Removes the values, used by the uninstall path and by tests.
    fn unregister(&self) -> Result<(), PlatformError>;
}

/// Per-user single-instance guard.
pub trait SingleInstance: Send + Sync {
    /// `true` when this process owns the lock, `false` when another instance already does.
    fn try_acquire(&self, user: &str) -> Result<bool, PlatformError>;

    /// Retries [`SingleInstance::try_acquire`] until it wins or `wait` runs out.
    ///
    /// The self-updater relaunches itself while the previous process is still shutting down,
    /// so the fresh build must wait for the old one to release the lock instead of
    /// forwarding an empty link and exiting.
    fn try_acquire_waiting(
        &self,
        user: &str,
        wait: std::time::Duration,
    ) -> Result<bool, PlatformError> {
        let deadline = std::time::Instant::now() + wait;
        loop {
            if self.try_acquire(user)? {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// The `\\.\pipe\postbox-link-<user>` forwarder.
pub trait LinkPipe: Send + Sync {
    /// Sends one line to the running instance. `Ok(true)` means it was delivered.
    fn forward(&self, user: &str, payload: &str) -> Result<bool, PlatformError>;
}

/// `true` when `arg` is the placeholder the registry command passes through unchanged, or a
/// `postbox://` URL. Windows passes `%1` verbatim when the handler is invoked without one.
pub fn link_from_args<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = String>,
{
    for arg in args {
        let trimmed = arg.trim();
        if trimmed.is_empty() || trimmed == COMMAND_PLACEHOLDER || trimmed.starts_with('-') {
            continue;
        }
        if deep_link::is_postbox_url(trimmed) {
            return Some(trimmed.to_owned());
        }
    }
    None
}

/// What the app does with the command line at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupRole {
    /// Open a window; carries the link this launch should open.
    Primary { link: Option<DeepLinkTarget> },
    /// Another instance is running; the link was handed over and this process exits.
    Forwarded { link: Option<DeepLinkTarget> },
}

/// Runs the startup handshake against the injected OS pieces.
///
/// `exe_path` registration is attempted for both roles so a second launch repairs a broken
/// registration, but the forwarded role never creates a window.
pub fn startup(
    exe_path: &str,
    user: &str,
    link_url: Option<&str>,
    registry: &dyn UrlHandler,
    instance: &dyn SingleInstance,
    pipe: &dyn LinkPipe,
) -> Result<StartupRole, PlatformError> {
    let _ = registry.register(exe_path)?;
    let link = match link_url {
        Some(raw) => deep_link::parse(raw).ok(),
        None => None,
    };
    if instance.try_acquire(user)? {
        return Ok(StartupRole::Primary { link });
    }
    let payload = match link_url {
        Some(raw) => raw.to_owned(),
        None => FOCUS_SIGNAL.to_owned(),
    };
    let _ = pipe.forward(user, &payload)?;
    Ok(StartupRole::Forwarded { link })
}

/// Command-line flag the updater passes to the build it launches.
pub const UPDATE_RELAUNCH_ARG: &str = "--update-relaunch";

/// How long a fresh build waits for the process it replaced to release the single-instance
/// lock before deciding another copy really is running.
pub const UPDATE_RELAUNCH_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Startup for the build the updater just swapped in.
///
/// The old process is still exiting, so take the lock with a bounded wait instead of
/// treating its mutex as a second instance. If the wait still loses, this is a genuine
/// second launch and the caller exits without a window.
pub fn startup_relaunch(
    exe_path: &str,
    user: &str,
    registry: &dyn UrlHandler,
    instance: &dyn SingleInstance,
    wait: std::time::Duration,
) -> Result<StartupRole, PlatformError> {
    let _ = registry.register(exe_path)?;
    if instance.try_acquire_waiting(user, wait)? {
        Ok(StartupRole::Primary { link: None })
    } else {
        Ok(StartupRole::Forwarded { link: None })
    }
}

/// Line the link pipe sends when a URL is opened on a running instance.
pub fn link_payload(target: &DeepLinkTarget) -> String {
    postbox_core::toast::launch_url(&target.chat, target.message.as_deref())
}

/// `true` when `line` is only asking for the window to be raised.
pub fn is_focus_signal(line: &str) -> bool {
    line.trim().eq_ignore_ascii_case(FOCUS_SIGNAL)
}

/// Result of handing one link-pipe line to the shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The shell found the chat and focused it.
    Focused { chat: postbox_core::ChatKey },
    /// The window was raised without changing the chat.
    FocusRaised,
    /// The URL did not parse; the shell shows the note without knowing the provider.
    Rejected { note: String },
    /// The chat is not in the store; the shell shows a non-modal card.
    MissingChat { note: String },
}

/// Turns one link-pipe line into a [`LinkOutcome`], given a "can the shell open this chat"
/// predicate. Pure, so the routing rules are unit-tested without a window.
pub fn route_line(line: &str, can_open: impl Fn(&DeepLinkTarget) -> bool) -> LinkOutcome {
    let trimmed = line.trim();
    if is_focus_signal(trimmed) {
        return LinkOutcome::FocusRaised;
    }
    match deep_link::parse(trimmed) {
        Ok(target) => {
            if can_open(&target) {
                LinkOutcome::Focused { chat: target.chat }
            } else {
                LinkOutcome::MissingChat {
                    note: deep_link::missing_chat_note(target.chat.provider).to_owned(),
                }
            }
        }
        Err(_) => LinkOutcome::Rejected {
            note: "That Postbox link is not valid.".to_owned(),
        },
    }
}

// ---------------------------------------------------------------------------
// Portable implementations used by tests and non-Windows builds
// ---------------------------------------------------------------------------

/// In-memory registry double: same values the Windows writer produces.
#[derive(Debug, Default)]
pub struct MockUrlHandler {
    values: Mutex<HashMap<(String, Option<String>), String>>,
    writes: Mutex<Vec<String>>,
    fail: Mutex<bool>,
}

impl MockUrlHandler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail(&self, fail: bool) {
        if let Ok(mut guard) = self.fail.lock() {
            *guard = fail;
        }
    }

    pub fn value(&self, subkey: &str, name: Option<&str>) -> Option<String> {
        let guard = self.values.lock().ok()?;
        guard
            .get(&(subkey.to_owned(), name.map(str::to_owned)))
            .cloned()
    }

    /// Number of register calls that actually wrote something.
    pub fn write_count(&self) -> usize {
        self.writes.lock().map(|w| w.len()).unwrap_or(0)
    }

    pub fn is_registered(&self) -> bool {
        self.value(
            postbox_core::os_integration::URL_PROTOCOL_SUBKEY,
            Some("URL Protocol"),
        )
        .is_some()
    }
}

impl UrlHandler for MockUrlHandler {
    fn register(&self, exe_path: &str) -> Result<bool, PlatformError> {
        if self.fail.lock().map(|f| *f).unwrap_or(false) {
            return Err(PlatformError::OsUnavailable);
        }
        let values = url_protocol_values(exe_path);
        let already = values.iter().all(|value| {
            self.value(&value.subkey, value.name.as_deref()).as_deref()
                == Some(value.value.as_str())
        });
        if already {
            return Ok(false);
        }
        let mut guard = self
            .values
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        for value in &values {
            guard.insert(
                (value.subkey.clone(), value.name.clone()),
                value.value.clone(),
            );
        }
        drop(guard);
        if let Ok(mut writes) = self.writes.lock() {
            writes.push(exe_path.to_owned());
        }
        Ok(true)
    }

    fn registered_command(&self) -> Result<Option<String>, PlatformError> {
        Ok(self.value(
            postbox_core::os_integration::URL_PROTOCOL_COMMAND_SUBKEY,
            None,
        ))
    }

    fn unregister(&self) -> Result<(), PlatformError> {
        let mut guard = self
            .values
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        guard.clear();
        Ok(())
    }
}

/// In-memory single-instance double: the first `try_acquire` wins, later ones lose.
#[derive(Debug, Default)]
pub struct MockSingleInstance {
    owner: Mutex<Option<String>>,
    /// Attempts still refused before the simulated old process lets go.
    release_after: Mutex<Option<usize>>,
}

impl MockSingleInstance {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulates another process already holding the lock.
    pub fn held_by(user: &str) -> Self {
        Self {
            owner: Mutex::new(Some(user.to_owned())),
            release_after: Mutex::new(None),
        }
    }

    /// Simulates a process that lets go after `attempts` refused acquisitions.
    pub fn held_by_until(user: &str, attempts: usize) -> Self {
        Self {
            owner: Mutex::new(Some(user.to_owned())),
            release_after: Mutex::new(Some(attempts)),
        }
    }

    pub fn owner(&self) -> Option<String> {
        self.owner.lock().ok().and_then(|g| g.clone())
    }
}

impl SingleInstance for MockSingleInstance {
    fn try_acquire(&self, user: &str) -> Result<bool, PlatformError> {
        let mut guard = self
            .owner
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?;
        if let Ok(mut release) = self.release_after.lock()
            && let Some(remaining) = *release
        {
            if remaining > 0 {
                *release = Some(remaining - 1);
                return Ok(false);
            }
            *release = None;
            *guard = None;
        }
        if guard.is_some() {
            return Ok(false);
        }
        *guard = Some(user.to_owned());
        Ok(true)
    }
}

/// Records forwarded link lines.
#[derive(Debug, Default)]
pub struct MockLinkPipe {
    sent: Mutex<Vec<(String, String)>>,
    reachable: Mutex<bool>,
}

impl MockLinkPipe {
    pub fn new() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
            reachable: Mutex::new(true),
        }
    }

    pub fn set_reachable(&self, reachable: bool) {
        if let Ok(mut guard) = self.reachable.lock() {
            *guard = reachable;
        }
    }

    pub fn sent(&self) -> Vec<(String, String)> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn last_payload(&self) -> Option<String> {
        self.sent().pop().map(|(_, payload)| payload)
    }
}

impl LinkPipe for MockLinkPipe {
    fn forward(&self, user: &str, payload: &str) -> Result<bool, PlatformError> {
        if !self.reachable.lock().map(|r| *r).unwrap_or(true) {
            return Ok(false);
        }
        self.sent
            .lock()
            .map_err(|_| PlatformError::OsUnavailable)?
            .push((user.to_owned(), payload.to_owned()));
        Ok(true)
    }
}

/// `\\.\pipe\postbox-link-<user>` for the real link pipe.
pub fn link_pipe_name(user: &str) -> String {
    format!("{LINK_PIPE_PREFIX}{user}")
}

/// `Local\postbox-single-<user>` for the real mutex.
pub fn mutex_name(user: &str) -> String {
    format!("{SINGLE_INSTANCE_MUTEX_PREFIX}{user}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const EXE: &str = r"C:\Program Files\Postbox\postbox.exe";
    const WA: &str = "postbox://whatsapp/biz/491234%40s.whatsapp.net";

    #[test]
    fn the_relaunched_build_waits_for_the_old_process_lock_instead_of_forwarding() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by_until("valentin", 2);

        let role =
            startup_relaunch(EXE, "valentin", &registry, &instance, UPDATE_RELAUNCH_WAIT).unwrap();

        assert!(matches!(role, StartupRole::Primary { link: None }));
        assert_eq!(instance.owner().as_deref(), Some("valentin"));
    }

    #[test]
    fn a_relaunch_that_never_gets_the_lock_still_exits_without_a_window() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by("valentin");

        let role = startup_relaunch(EXE, "valentin", &registry, &instance, Duration::ZERO).unwrap();

        assert!(matches!(role, StartupRole::Forwarded { link: None }));
    }

    #[test]
    fn first_launch_registers_the_scheme_and_opens_a_window() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::new();
        let pipe = MockLinkPipe::new();
        let role = startup(EXE, "valentin", Some(WA), &registry, &instance, &pipe).unwrap();

        assert!(matches!(role, StartupRole::Primary { link: Some(_) }));
        assert!(registry.is_registered());
        assert_eq!(
            registry.registered_command().unwrap().as_deref(),
            Some(r#""C:\Program Files\Postbox\postbox.exe" "%1""#)
        );
        assert!(pipe.sent().is_empty(), "a primary launch forwards nothing");
    }

    #[test]
    fn a_second_launch_forwards_the_url_and_does_not_take_the_lock() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by("valentin");
        let pipe = MockLinkPipe::new();

        let role = startup(EXE, "valentin", Some(WA), &registry, &instance, &pipe).unwrap();
        match role {
            StartupRole::Forwarded { link } => {
                let link = link.expect("the URL parsed");
                assert_eq!(link.chat.id, "491234@s.whatsapp.net");
            }
            other => panic!("expected a forwarded launch, got {other:?}"),
        }
        assert_eq!(pipe.last_payload().as_deref(), Some(WA));
        assert_eq!(instance.owner().as_deref(), Some("valentin"));
    }

    #[test]
    fn a_second_launch_without_a_url_only_asks_for_focus() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by("valentin");
        let pipe = MockLinkPipe::new();
        let role = startup(EXE, "valentin", None, &registry, &instance, &pipe).unwrap();
        assert_eq!(role, StartupRole::Forwarded { link: None });
        assert_eq!(pipe.last_payload().as_deref(), Some(FOCUS_SIGNAL));
    }

    #[test]
    fn registration_is_idempotent_so_startup_writes_the_registry_once() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::new();
        let pipe = MockLinkPipe::new();
        startup(EXE, "valentin", None, &registry, &instance, &pipe).unwrap();
        assert_eq!(registry.write_count(), 1);
        registry.register(EXE).unwrap();
        assert_eq!(registry.write_count(), 1, "the second write is skipped");
    }

    #[test]
    fn a_registry_failure_fails_the_launch_closed() {
        let registry = MockUrlHandler::new();
        registry.set_fail(true);
        let instance = MockSingleInstance::new();
        let pipe = MockLinkPipe::new();
        let err = startup(EXE, "valentin", None, &registry, &instance, &pipe).unwrap_err();
        assert!(matches!(err, PlatformError::OsUnavailable));
    }

    #[test]
    fn an_unreachable_running_instance_still_reports_a_forwarded_role() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by("valentin");
        let pipe = MockLinkPipe::new();
        pipe.set_reachable(false);
        let role = startup(EXE, "valentin", Some(WA), &registry, &instance, &pipe).unwrap();
        assert!(matches!(role, StartupRole::Forwarded { .. }));
        assert!(pipe.sent().is_empty());
    }

    #[test]
    fn command_line_links_are_found_and_the_placeholder_is_ignored() {
        assert_eq!(
            link_from_args(vec!["postbox".into(), WA.into()]),
            Some(WA.to_owned())
        );
        assert_eq!(link_from_args(vec!["postbox".into(), "%1".into()]), None);
        assert_eq!(
            link_from_args(vec!["postbox".into(), "--in-memory".into()]),
            None
        );
        assert_eq!(
            link_from_args(vec!["postbox".into(), "postbox://slack/T01/C02".into()]),
            Some("postbox://slack/T01/C02".to_owned())
        );
    }

    #[test]
    fn routing_focuses_a_known_chat_and_cards_a_missing_one() {
        let focused = route_line(WA, |_| true);
        match focused {
            LinkOutcome::Focused { chat } => {
                assert_eq!(chat.provider, postbox_core::Provider::Whatsapp);
                assert_eq!(chat.account, "biz");
            }
            other => panic!("expected focus, got {other:?}"),
        }

        let missing = route_line(WA, |_| false);
        assert_eq!(
            missing,
            LinkOutcome::MissingChat {
                note: "Could not find that WhatsApp chat.".to_owned()
            }
        );

        let slack_missing = route_line("postbox://slack/T01/C99", |_| false);
        assert_eq!(
            slack_missing,
            LinkOutcome::MissingChat {
                note: "Could not find that Slack channel.".to_owned()
            }
        );
    }

    #[test]
    fn routing_raises_focus_and_rejects_junk() {
        assert_eq!(route_line("focus", |_| true), LinkOutcome::FocusRaised);
        assert_eq!(route_line("  FOCUS \n", |_| true), LinkOutcome::FocusRaised);
        assert!(matches!(
            route_line("https://example.com", |_| true),
            LinkOutcome::Rejected { .. }
        ));
        assert!(matches!(
            route_line("postbox://unknown/acct/chat", |_| true),
            LinkOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn link_payload_round_trips_a_message_id() {
        let target = postbox_core::deep_link::parse(&format!("{WA}/MSG-1")).unwrap();
        let payload = link_payload(&target);
        assert_eq!(payload, format!("{WA}/MSG-1"));
        assert_eq!(
            postbox_core::deep_link::parse(&payload)
                .unwrap()
                .message
                .as_deref(),
            Some("MSG-1")
        );
    }

    #[test]
    fn pipe_and_mutex_names_carry_the_user() {
        assert_eq!(
            link_pipe_name("valentin"),
            r"\\.\pipe\postbox-link-valentin"
        );
        assert_eq!(mutex_name("valentin"), r"Local\postbox-single-valentin");
    }
}
