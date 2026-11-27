//! Milestone 7 desktop wiring: toasts, the link pipe, the control server, the status card and
//! the "Check for updates" command.
//!
//! The headless rules live in `crate::m7` and `crate::control`; this module only moves bytes
//! and paints. Every OS piece is optional, so the shell still runs in tests with none of them.

use crate::control::LiveTelemetry;
use crate::desktop::PostboxShell;
use crate::geometry::{TYPE_LABEL_PX, TYPE_TIME_PX};
use crate::m7::{M7State, StatusCard};
use crate::theme::ShadcnPalette;
use crate::{FONT_MONO, FONT_SANS};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::{
    AsyncWindowContext, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, WeakEntity, Window, div, px,
    rgb,
};
use postbox_core::control::{ControlRequest, ControlResponse, decode_request, encode_response};
use postbox_core::crash::ExitKind;
use postbox_platform::Relauncher as _;
use postbox_platform::control_pipe::Transport;
use postbox_platform::os_integration::route_line;
use postbox_platform::{LinkOutcome, exit_records::ExitRecordStore};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the link pipe is polled for a forwarded `postbox://` URL.
pub const LINK_POLL_MS: u64 = 120;
/// How often the control pipe is polled for a `postboxctl` line.
pub const CONTROL_POLL_MS: u64 = 60;
/// Longest queue of pending link lines kept before dropping the oldest.
pub const MAX_PENDING_LINKS: usize = 16;

/// One command for the shell, produced off the UI thread.
#[derive(Debug, Clone, PartialEq)]
pub enum OsCommand {
    /// A `postbox://` URL or the `focus` signal arrived on the link pipe.
    Link(String),
    /// A `postboxctl` request arrived on the control pipe.
    Control(ControlRequest),
}

/// One queued job: the command plus, for a control request, where the answer goes.
#[derive(Debug, Clone)]
pub struct OsJob {
    pub command: OsCommand,
    pub reply: Option<ControlReply>,
}

impl OsJob {
    pub fn link(line: impl Into<String>) -> Self {
        Self {
            command: OsCommand::Link(line.into()),
            reply: None,
        }
    }

    pub fn control(request: ControlRequest, reply: ControlReply) -> Self {
        Self {
            command: OsCommand::Control(request),
            reply: Some(reply),
        }
    }
}

/// Shared queue the background threads fill and the UI thread drains.
#[derive(Debug, Clone, Default)]
pub struct OsCommandQueue {
    inner: Arc<Mutex<VecDeque<OsJob>>>,
}

impl OsCommandQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, job: OsJob) -> bool {
        let Ok(mut queue) = self.inner.lock() else {
            return false;
        };
        if queue.len() >= MAX_PENDING_LINKS {
            queue.pop_front();
        }
        queue.push_back(job);
        true
    }

    pub fn push_link(&self, line: impl Into<String>) -> bool {
        self.push(OsJob::link(line))
    }

    pub fn drain(&self) -> Vec<OsJob> {
        let Ok(mut queue) = self.inner.lock() else {
            return Vec::new();
        };
        queue.drain(..).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Reply slot the control server waits on while the UI thread handles the request.
#[derive(Debug, Clone)]
pub struct ControlReply {
    slot: Arc<Mutex<Option<String>>>,
}

impl Default for ControlReply {
    fn default() -> Self {
        Self {
            slot: Arc::new(Mutex::new(None)),
        }
    }
}

impl ControlReply {
    /// Stores the answer the UI thread produced.
    pub fn set(&self, line: String) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = Some(line);
        }
    }

    /// Takes the answer, if the UI thread already produced one.
    pub fn take(&self) -> Option<String> {
        self.slot.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// The running OS services for one window.
pub struct OsRuntime {
    pub queue: OsCommandQueue,
    pub exit_records: Option<ExitRecordStore>,
    pub control_endpoint: String,
    link_endpoint: String,
    _servers: Vec<postbox_platform::control_pipe::ServerHandle>,
}

impl std::fmt::Debug for OsRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OsRuntime")
            .field("control_endpoint", &self.control_endpoint)
            .field("link_endpoint", &self.link_endpoint)
            .field("servers", &self._servers.len())
            .finish()
    }
}

impl OsRuntime {
    /// The control endpoint for this user: the Windows named pipe, or a per-user socket.
    pub fn control_transport(data_dir: &std::path::Path, user: &str) -> Transport {
        if cfg!(windows) {
            Transport::pipe(&postbox_core::os_integration::control_pipe_name(user))
        } else {
            Transport::socket(postbox_platform::control_pipe::socket_path(data_dir, user))
        }
    }

    /// The link endpoint for this user: the Windows named pipe, or a per-user socket.
    pub fn link_transport(data_dir: &std::path::Path, user: &str) -> Transport {
        if cfg!(windows) {
            Transport::pipe(&postbox_core::os_integration::link_pipe_name(user))
        } else {
            Transport::socket(postbox_platform::control_pipe::socket_path(
                data_dir,
                &format!("link-{user}"),
            ))
        }
    }

    /// Starts the control server and the link-pipe server for this user.
    ///
    /// Both are best effort: a host without pipes, or without permission to bind, leaves the
    /// app fully usable with no `postboxctl` and no link forwarding.
    pub fn start(data_dir: &std::path::Path, user: &str) -> Self {
        let queue = OsCommandQueue::new();
        let mut servers = Vec::new();

        let control_transport = Self::control_transport(data_dir, user);
        let control_endpoint = control_transport.display();
        let control_queue = queue.clone();
        if let Ok(handle) = postbox_platform::control_pipe::serve(&control_transport, move |line| {
            match decode_request(line) {
                Ok(request) => {
                    let reply = ControlReply::default();
                    let _ = control_queue.push(OsJob::control(request, reply.clone()));
                    reply_wait(&reply)
                }
                Err(err) => encode_response(&ControlResponse::error(err)),
            }
        }) {
            servers.push(handle);
        }

        let link_transport = Self::link_transport(data_dir, user);
        let link_endpoint = link_transport.display();
        let link_queue = queue.clone();
        if let Ok(handle) = postbox_platform::control_pipe::serve(&link_transport, move |line| {
            let _ = link_queue.push_link(line.to_owned());
            // The link client only needs to know the line arrived.
            encode_response(&ControlResponse::with_message("ok"))
        }) {
            servers.push(handle);
        }

        Self {
            queue,
            exit_records: Some(ExitRecordStore::new(data_dir)),
            control_endpoint,
            link_endpoint,
            _servers: servers,
        }
    }

    /// A runtime with nothing running, for tests and for `--no-os-integration`.
    pub fn disabled(data_dir: &std::path::Path) -> Self {
        Self {
            queue: OsCommandQueue::new(),
            exit_records: Some(ExitRecordStore::new(data_dir)),
            control_endpoint: "disabled".to_owned(),
            link_endpoint: "disabled".to_owned(),
            _servers: Vec::new(),
        }
    }

    /// Endpoint a second launch should forward its `postbox://` URL to.
    pub fn link_endpoint(&self) -> &str {
        &self.link_endpoint
    }

    /// Pushes a link line as if it had arrived on the pipe.
    pub fn deliver_link(&self, line: impl Into<String>) -> bool {
        self.queue.push_link(line)
    }
}

fn reply_wait(reply: &ControlReply) -> String {
    for _ in 0..100 {
        if let Some(line) = reply.take() {
            return line;
        }
        std::thread::sleep(Duration::from_millis(CONTROL_POLL_MS));
    }
    encode_response(&ControlResponse::error("Postbox did not answer in time"))
}

impl PostboxShell {
    /// Installs the M7 OS pieces on a freshly built shell.
    pub fn with_os_integration(
        mut self,
        os: OsRuntime,
        notifier: Option<Arc<dyn postbox_platform::Notifier>>,
    ) -> Self {
        if let Some(records) = &os.exit_records
            && let Ok(last) = records.last()
        {
            self.vm.set_last_exit_record(last);
        }
        self.os_runtime = Some(os);
        self.notifier = notifier;
        self
    }

    /// Directory the running executable lives in, so the updater can stage beside it.
    pub fn with_install_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.install_dir = dir;
        self
    }

    /// The manifest/archive source. `None` keeps the shell offline and fully usable, which is
    /// what the tests and the headless CI check use.
    pub fn with_release_source(
        mut self,
        source: Option<std::sync::Arc<dyn postbox_platform::ReleaseSource>>,
    ) -> Self {
        self.release_source = source;
        self
    }

    /// Starts the timer that drains the OS queue into UI work, once per window.
    pub fn start_os_pump(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<()>> {
        if self.os_pump_task.is_some() || self.os_runtime.is_none() {
            return None;
        }
        Some(spawn_os_pump(window, cx))
    }

    /// Kicks off one update cycle on the background executor and folds the result back in.
    ///
    /// `postboxapp` injects the GitHub Releases source, so this crate never links an HTTP
    /// stack. A shell without a source stays offline and says so.
    pub fn spawn_update_check(&mut self, cx: &mut Context<Self>) {
        if self.vm.m7().busy() {
            return;
        }
        let Some(source) = self.release_source.clone() else {
            self.vm
                .set_status_notice("Update checks are unavailable in this build");
            return;
        };
        let install_dir = self.install_dir.clone();
        let version = self.vm.version().to_owned();
        let current = postbox_core::update::Version::parse(&version)
            .unwrap_or_else(|| postbox_core::update::Version::new(0, 0, 0));
        self.vm.m7_mut().update_phase = crate::m7::UpdatePhase::Busy;

        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    run_update(
                        source.as_ref(),
                        &install_dir,
                        postbox_platform::updater::EXE_ASSET_NAME,
                        current,
                    )
                })
                .await;
            let _ = this.update(cx, |shell, cx| {
                shell.apply_update_outcome(outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// Folds an update outcome into the shell state.
    pub fn apply_update_outcome(&mut self, outcome: UpdateOutcome) {
        match outcome {
            UpdateOutcome::UpToDate { current } => {
                self.vm.m7_mut().update_phase = crate::m7::UpdatePhase::UpToDate;
                self.vm.m7_mut().update_note =
                    Some(format!("Postbox {current} is the newest version."));
            }
            UpdateOutcome::Installed { tag, exe } => {
                self.vm.m7_mut().mark_installed(&tag);
                // Relaunch detached, so the swap survives the old process leaving.
                let _ = postbox_platform::ProcessRelauncher.relaunch(&exe, &[]);
            }
            UpdateOutcome::Failed { reason } => {
                self.vm.m7_mut().mark_failed(&reason);
            }
        }
        if let Some(note) = self.vm.m7().update_note.clone() {
            self.vm.set_status_notice(note);
        }
    }

    /// Applies a startup `postbox://` URL, from the command line or a second launch.
    pub fn apply_startup_link(&mut self, line: &str) -> LinkOutcome {
        let now_ms = crate::viewmodel::now_epoch_millis_public() as u64;
        apply_startup_link(self, line, now_ms)
    }

    /// Polls the OS queue and applies everything it found. Called from the render tick.
    pub fn pump_os_commands(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(os) = self.os_runtime.as_ref() else {
            return;
        };
        let jobs = os.queue.drain();
        if jobs.is_empty() {
            return;
        }
        let now_ms = crate::viewmodel::now_epoch_millis_public() as u64;
        let mut changed = false;
        for job in jobs {
            match job.command {
                OsCommand::Link(line) => {
                    let action = self.vm.open_deep_link(&line, now_ms);
                    if action.raise_window {
                        window.activate_window();
                    }
                    changed = true;
                }
                OsCommand::Control(request) => {
                    let telemetry = self.live_telemetry(cx);
                    let response = crate::control::handle(&mut self.vm, &request, &telemetry);
                    if let Some(reply) = job.reply {
                        reply.set(encode_response(&response));
                    }
                    changed = true;
                }
            }
        }
        if changed {
            self.sync_after_vm_change(window, cx);
        }
    }

    /// Live counters for `postboxctl status`.
    pub fn live_telemetry(&self, _cx: &mut Context<Self>) -> LiveTelemetry {
        let mut counters = postbox_core::control::TelemetryCounters {
            frames_rendered: self.frames_rendered,
            working_set_kb: 0,
            private_usage_kb: 0,
            gpu_texture_bytes: self.gpu_textures.total_vram_bytes() as u64,
            pending_outbox: self
                .vm
                .store()
                .list_outbox()
                .map(|rows| rows.len())
                .unwrap_or(0) as u32,
        };
        if let Some(platform) = &self.memory_platform
            && let Ok(stats) = platform.read_process_memory_stats()
        {
            counters.working_set_kb = stats.working_set_kb();
            counters.private_usage_kb = stats.private_usage_kb();
        }
        LiveTelemetry::new(counters, self.fps_meter.fps())
    }

    /// Records the clean exit, so the next run knows the last one ended normally.
    pub fn record_clean_exit(&self) {
        if let Some(os) = self.os_runtime.as_ref()
            && let Some(records) = &os.exit_records
        {
            let uptime_ms = self.started_at.elapsed().as_millis() as u64;
            let _ = records.record_clean(self.vm.version(), uptime_ms);
        }
    }

    /// Raises a toast for a new incoming message unless the policy stays silent.
    pub fn maybe_toast(&mut self, message: &postbox_core::Message, window_focused: bool) -> bool {
        let Some(notifier) = self.notifier.clone() else {
            return false;
        };
        let Some(ts) = postbox_core::archive::unix_seconds(&message.ts) else {
            return false;
        };
        self.vm
            .notify_incoming(message, ts, window_focused, notifier.as_ref())
    }

    /// The dismissible status card, when one is up.
    pub fn render_status_card(
        &self,
        p: &ShadcnPalette,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        let card = self.vm.status_card()?.clone();
        let is_missing = card.is_missing_chat();
        let accent = if is_missing { p.destructive } else { p.border };
        Some(
            div()
                .id(SharedString::from(format!("status-card-{}", card.id)))
                .test_support()
                .max_w(px(540.0))
                .w_full()
                .mx_auto()
                .mb_2()
                .p_3()
                .rounded_lg()
                .bg(rgb(p.card))
                .border_1()
                .border_color(rgb(accent))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .font_family(FONT_SANS)
                                .text_size(px(TYPE_LABEL_PX))
                                .text_color(rgb(p.foreground))
                                .child(card.title.clone()),
                        )
                        .child(
                            div()
                                .font_family(FONT_SANS)
                                .text_size(px(TYPE_TIME_PX))
                                .text_color(rgb(p.muted_foreground))
                                .child(card.note.clone()),
                        ),
                )
                .child(
                    div()
                        .id("status-card-close")
                        .test_support()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(rgb(p.secondary))
                        .font_family(FONT_MONO)
                        .text_size(px(TYPE_TIME_PX))
                        .text_color(rgb(p.foreground))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _event, window, cx| {
                            if this.vm.close_status_card() {
                                this.sync_after_vm_change(window, cx);
                            }
                        }))
                        .child("Esc"),
                )
                .into_any_element(),
        )
    }
}

impl M7State {
    /// Text for the crash banner when the previous run ended in a panic.
    pub fn crash_banner(&self) -> Option<String> {
        let record = self.last_exit_record.as_ref()?;
        if record.kind != ExitKind::Panic {
            return None;
        }
        let when = record.timestamp_unix_ms / 1000;
        Some(match &record.message {
            Some(message) => format!(
                "Postbox {} stopped unexpectedly at {when}: {message}",
                record.version
            ),
            None => format!("Postbox {} stopped unexpectedly at {when}.", record.version),
        })
    }
}

/// Runs one update cycle off the UI thread: fetch, verify, install, relaunch.
///
/// Returns the state to apply and, when the install succeeded, the binary to relaunch. The
/// live executable is only replaced after the digest matches, and a failed swap puts the
/// previous binary back, so a run of this can never leave the app uninstalled.
pub fn run_update(
    source: &dyn postbox_platform::ReleaseSource,
    install_dir: &std::path::Path,
    exe_name: &str,
    current: postbox_core::update::Version,
) -> UpdateOutcome {
    let manifest = match source.fetch_manifest() {
        Ok(manifest) => manifest,
        Err(err) => {
            return UpdateOutcome::Failed {
                reason: err.to_string(),
            };
        }
    };
    match postbox_core::update::evaluate(&manifest, current) {
        postbox_core::update::CheckOutcome::UpToDate { current } => {
            UpdateOutcome::UpToDate { current }
        }
        postbox_core::update::CheckOutcome::Available { asset } => {
            let bytes = match source.download(&asset) {
                Ok(bytes) => bytes,
                Err(err) => {
                    return UpdateOutcome::Failed {
                        reason: err.to_string(),
                    };
                }
            };
            // A bad hash never reaches `install`, so the live binary is untouched.
            if let Err(err) = postbox_platform::verify_download(&bytes, &asset) {
                return UpdateOutcome::Failed {
                    reason: err.to_string(),
                };
            }
            let replacer = native_replacer();
            match postbox_platform::install(
                &asset,
                &bytes,
                install_dir,
                exe_name,
                &native_extractor(),
                replacer.as_ref(),
            ) {
                Ok(report) => UpdateOutcome::Installed {
                    tag: asset.tag.clone(),
                    exe: report.installed_exe,
                },
                Err(err) => UpdateOutcome::Failed {
                    reason: err.to_string(),
                },
            }
        }
    }
}

/// What one update cycle produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    UpToDate {
        current: String,
    },
    Installed {
        tag: String,
        exe: std::path::PathBuf,
    },
    Failed {
        reason: String,
    },
}

fn native_replacer() -> Box<dyn postbox_platform::AtomicReplacer> {
    #[cfg(windows)]
    {
        Box::new(postbox_platform::win::updater::WindowsReplacer)
    }
    #[cfg(not(windows))]
    {
        Box::new(postbox_platform::NativeReplacer)
    }
}

fn native_extractor() -> Box<dyn postbox_platform::ArchiveExtractor> {
    #[cfg(windows)]
    {
        Box::new(postbox_platform::win::updater::ZipExtractor)
    }
    #[cfg(not(windows))]
    {
        Box::new(postbox_platform::PlainFileExtractor)
    }
}

/// Frames-per-second meter shared between the render tick and `postboxctl status`.
#[derive(Debug, Clone, Default)]
pub struct SharedFpsMeter {
    inner: Arc<Mutex<postbox_core::control::FpsMeter>>,
    frames: Arc<std::sync::atomic::AtomicU64>,
    opened_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl SharedFpsMeter {
    /// Counts one painted frame; the first frame of a run opens the window.
    pub fn on_frame(&self, now_ms: u64, frames_rendered: u64) {
        if let Ok(mut meter) = self.inner.lock() {
            meter.on_frame(now_ms);
        }
        self.frames
            .store(frames_rendered, std::sync::atomic::Ordering::Relaxed);
        self.opened_ms
            .store(now_ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Completed-window frames per second; `0.0` while nothing has painted twice.
    pub fn fps(&self) -> f64 {
        self.inner.lock().map(|meter| meter.fps()).unwrap_or(0.0)
    }
}

/// Spawns the timer that turns the OS queue into UI work.
pub fn spawn_os_pump(window: &mut Window, cx: &mut Context<PostboxShell>) -> Task<()> {
    cx.spawn_in(
        window,
        async move |this: WeakEntity<PostboxShell>, cx: &mut AsyncWindowContext| loop {
            cx.background_executor()
                .timer(Duration::from_millis(CONTROL_POLL_MS))
                .await;
            if this
                .update_in(cx, |shell, window, cx| {
                    shell.pump_os_commands(window, cx);
                    cx.notify();
                })
                .is_err()
            {
                break;
            }
        },
    )
}

/// Routes one link line straight to the shell's view model, for tests and for the startup path.
pub fn apply_startup_link(shell: &mut PostboxShell, line: &str, now_ms: u64) -> LinkOutcome {
    let outcome = route_line(line, |target| {
        shell
            .vm
            .store()
            .get_chat(&target.chat)
            .ok()
            .flatten()
            .is_some()
    });
    let _ = shell.vm.open_deep_link(line, now_ms);
    outcome
}

impl PostboxShell {
    /// Applies a link line and returns the routing outcome.
    pub fn open_link_now(&mut self, line: &str, now_ms: u64) -> LinkOutcome {
        let action = self.vm.open_deep_link(line, now_ms);
        action.outcome
    }
}

/// Status card for a run that ended in a panic.
pub fn crash_card(state: &M7State, now_ms: u64) -> Option<StatusCard> {
    state
        .crash_banner()
        .map(|note| StatusCard::new("last-crash", "Postbox closed unexpectedly", note, now_ms))
}
