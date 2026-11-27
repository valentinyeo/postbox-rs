//! Persists [`ExitRecord`] lines in the app data directory and installs the crash hooks.
//!
//! The panic hook records the sanitised panic message and location, then calls the previous hook
//! so the normal message still reaches stderr. On Windows the native exception filter records a
//! process that dies outside the Rust runtime. A dirty-session sentinel covers everything else.
//! Nothing here reads chat data.

use crate::PlatformError;
use postbox_core::crash::{
    CRASH_REPORT_STATE_FILE, CrashReport, CrashReportState, EXIT_RECORD_FILE, ExitKind, ExitRecord,
    NATIVE_EXCEPTION_FILE, RUNNING_SESSION_FILE, RunningSession, rotated,
};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Append-only JSONL log of clean exits and panics.
#[derive(Debug, Clone)]
pub struct ExitRecordStore {
    dir: PathBuf,
}

impl ExitRecordStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(EXIT_RECORD_FILE)
    }

    /// Appends one record, rotating first when the log is over its bound.
    pub fn record(&self, record: &ExitRecord) -> Result<(), PlatformError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path();
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        if let Some(trimmed) = rotated(&existing) {
            std::fs::write(&path, trimmed)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(record.to_line().as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    pub fn record_clean(&self, version: &str, uptime_ms: u64) -> Result<(), PlatformError> {
        self.record(&ExitRecord::clean(version, uptime_ms, now_unix_ms()))
    }

    /// Everything on disk, oldest first. A corrupt line is skipped, never fatal.
    pub fn read_all(&self) -> Result<Vec<ExitRecord>, PlatformError> {
        let text = match std::fs::read_to_string(self.path()) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(PlatformError::Io(err)),
        };
        Ok(text.lines().filter_map(ExitRecord::parse).collect())
    }

    /// The newest record, for a "last run ended in a panic" banner.
    pub fn last(&self) -> Result<Option<ExitRecord>, PlatformError> {
        Ok(self.read_all()?.pop())
    }

    /// Path of the dirty-session sentinel.
    pub fn session_path(&self) -> PathBuf {
        self.dir.join(RUNNING_SESSION_FILE)
    }

    /// Path of the native exception lines the Windows filter appends to.
    pub fn native_exception_path(&self) -> PathBuf {
        self.dir.join(NATIVE_EXCEPTION_FILE)
    }

    /// Path of the upload bookkeeping file.
    pub fn report_state_path(&self) -> PathBuf {
        self.dir.join(CRASH_REPORT_STATE_FILE)
    }

    /// The sentinel a previous, unfinished run left behind, if any.
    pub fn session(&self) -> Option<RunningSession> {
        let text = std::fs::read_to_string(self.session_path()).ok()?;
        RunningSession::parse(&text)
    }

    /// Writes the sentinel atomically, so a crash mid-write cannot leave a half line.
    pub fn write_session(&self, session: &RunningSession) -> Result<(), PlatformError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.session_path();
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, session.to_line())?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    /// Updates the stage the process has reached, keeping the start time and pid.
    pub fn update_stage(&self, stage: &str) -> Result<(), PlatformError> {
        let Some(mut session) = self.session() else {
            return Ok(());
        };
        session.last_stage = postbox_core::crash::sanitize_stage(stage);
        self.write_session(&session)
    }

    /// Removes the sentinel, so the next startup knows this run ended on purpose.
    pub fn clear_session(&self) {
        let _ = std::fs::remove_file(self.session_path());
    }

    /// Folds the native exception lines into the JSONL log and removes the raw file.
    ///
    /// Returns how many records were appended. A line that cannot be parsed is dropped: the
    /// filter may have been interrupted mid-write.
    pub fn fold_native_exceptions(&self) -> usize {
        let path = self.native_exception_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return 0;
        };
        let mut written = 0;
        for line in text.lines() {
            if let Some(record) = ExitRecord::from_native_line(line)
                && self.record(&record).is_ok()
            {
                written += 1;
            }
        }
        let _ = std::fs::remove_file(&path);
        written
    }

    /// Starts the run: folds native exceptions, converts a leftover sentinel into a
    /// [`ExitKind::SuddenExit`] record, then writes this process' sentinel.
    ///
    /// Returns the synthesised record, which is also appended to the log.
    pub fn begin_session(&self, version: &str, now_unix_ms: u64) -> Option<ExitRecord> {
        self.fold_native_exceptions();
        let synthesized = self.session().and_then(|previous| {
            // A panic or native exception record from the dead run already explains the death.
            let explained = self.read_all().unwrap_or_default().iter().any(|record| {
                matches!(record.kind, ExitKind::Panic | ExitKind::NativeException)
                    && record.timestamp_unix_ms >= previous.started_at_unix_ms
            });
            if explained {
                return None;
            }
            let record = ExitRecord::sudden_exit(&previous, now_unix_ms);
            self.record(&record).ok().map(|()| record)
        });
        let session = RunningSession::new(version, now_unix_ms, std::process::id());
        let _ = self.write_session(&session);
        synthesized
    }

    /// Upload bookkeeping, empty when nothing has been sent yet.
    pub fn report_state(&self) -> CrashReportState {
        std::fs::read_to_string(self.report_state_path())
            .ok()
            .and_then(|text| CrashReportState::parse(&text))
            .unwrap_or_default()
    }

    pub fn save_report_state(&self, state: &CrashReportState) -> Result<(), PlatformError> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(self.report_state_path(), state.to_line())?;
        Ok(())
    }

    /// Crash reports that have not been delivered and still have attempts left, newest last.
    pub fn pending_reports(&self, state: &CrashReportState) -> Vec<CrashReport> {
        let mut seen: Vec<String> = Vec::new();
        let mut pending = Vec::new();
        for record in self.read_all().unwrap_or_default() {
            let Some(report) = record.report() else {
                continue;
            };
            if state.is_settled(&report.report_id) {
                continue;
            }
            if seen.contains(&report.report_id) {
                continue;
            }
            seen.push(report.report_id.clone());
            pending.push(report);
        }
        pending
    }
}

/// Install the native (non-panic) crash filter. A no-op outside Windows.
pub fn install_native_exception_filter(dir: &Path, version: &'static str) {
    #[cfg(windows)]
    {
        crate::win::crash::install(dir, version);
    }
    #[cfg(not(windows))]
    {
        let _ = (dir, version);
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Installs a panic hook that writes a crash record and then runs the previous hook.
///
/// Returns nothing on purpose: a hook that cannot be installed must never stop the app from
/// starting, and a hook that cannot write must never stop a panic from being reported.
pub fn install_panic_hook(store: ExitRecordStore, version: &'static str, started_at: Instant) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()));
        let record = ExitRecord::panic(
            version,
            started_at.elapsed().as_millis() as u64,
            now_unix_ms(),
            message.as_deref(),
            location.as_deref(),
        );
        // The sentinel must not outlive a caught panic on an already-dead run: the record
        // itself is the marker the next startup looks for.
        let _ = store.record(&record);
        previous(info);
    }));
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> Option<String> {
    if let Some(text) = info.payload().downcast_ref::<&str>() {
        Some((*text).to_owned())
    } else {
        info.payload().downcast_ref::<String>().cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::crash::{EXIT_RECORD_KEEP, ExitKind};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-exit-{tag}-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn clean_and_crash_records_are_written_and_read_back_in_order() {
        let dir = temp_dir("order");
        let store = ExitRecordStore::new(&dir);
        store.record_clean("0.1.0", 4_000).unwrap();
        store
            .record(&ExitRecord::panic(
                "0.1.0",
                9_000,
                now_unix_ms(),
                Some("attempt to subtract with overflow"),
                Some("crates/postbox-ui/src/desktop.rs:120:9"),
            ))
            .unwrap();

        let records = store.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, ExitKind::Clean);
        assert_eq!(records[0].uptime_ms, 4_000);
        assert_eq!(records[1].kind, ExitKind::Panic);
        assert_eq!(
            records[1].message.as_deref(),
            Some("attempt to subtract with overflow")
        );
        assert_eq!(records[1].location.as_deref(), Some("desktop.rs:120:9"));
        assert_eq!(store.last().unwrap().unwrap().kind, ExitKind::Panic);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_log_reads_as_empty_instead_of_failing() {
        let store = ExitRecordStore::new(temp_dir("missing").join("nested"));
        assert!(store.read_all().unwrap().is_empty());
        assert_eq!(store.last().unwrap(), None);
    }

    #[test]
    fn a_corrupt_line_is_skipped_and_the_rest_still_parses() {
        let dir = temp_dir("corrupt");
        let store = ExitRecordStore::new(&dir);
        store.record_clean("0.1.0", 10).unwrap();
        let mut text = std::fs::read_to_string(store.path()).unwrap();
        text.push_str("{not json}\n");
        std::fs::write(store.path(), text).unwrap();
        store.record_clean("0.1.0", 20).unwrap();

        let records = store.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].uptime_ms, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_log_is_bounded_by_rotation() {
        let dir = temp_dir("rotate");
        let store = ExitRecordStore::new(&dir);
        let total = EXIT_RECORD_KEEP * 12;
        for i in 0..total {
            store
                .record(&ExitRecord::panic(
                    "0.1.0",
                    i as u64,
                    now_unix_ms(),
                    // Words, not one long run: `sanitize` redacts long alphanumeric runs, and
                    // this test needs realistically sized lines to exercise rotation.
                    Some(&"crash while drawing the message timeline ".repeat(6)),
                    None,
                ))
                .unwrap();
            let size = std::fs::metadata(store.path()).unwrap().len() as usize;
            assert!(
                size <= postbox_core::crash::EXIT_RECORD_MAX_BYTES + 1024,
                "the log must stay bounded, saw {size} bytes after {} records",
                i + 1
            );
        }
        let records = store.read_all().unwrap();
        assert!(
            records.len() < total,
            "rotation must have dropped old records, kept {} of {total}",
            records.len()
        );
        assert!(
            records.len() >= EXIT_RECORD_KEEP,
            "rotation keeps at least the newest {} records, kept {}",
            EXIT_RECORD_KEEP,
            records.len()
        );
        // The newest record survives, which is the one a bug report needs.
        assert_eq!(records.last().unwrap().uptime_ms, (total - 1) as u64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_keep_no_message_content_or_credentials() {
        let dir = temp_dir("privacy");
        let store = ExitRecordStore::new(&dir);
        store
            .record(&ExitRecord::panic(
                "0.1.0",
                1,
                now_unix_ms(),
                Some("send failed for ada@example.com with token=abcdefghijklmnopqrstuvwxyz012345"),
                None,
            ))
            .unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz012345"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        // The email address survives as prose-free context only if it is short: assert the
        // record still parses and stays one line.
        assert_eq!(text.lines().count(), 1);
        assert!(ExitRecord::parse(text.lines().next().unwrap()).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installing_the_hook_records_a_caught_panic() {
        let dir = temp_dir("hook");
        let store = ExitRecordStore::new(&dir);
        let started = Instant::now();
        // Capture the panic without polluting the test output.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook(store.clone(), "0.1.0", started);
        let caught = std::panic::catch_unwind(|| {
            panic!("simulated render panic");
        });
        std::panic::set_hook(previous);
        assert!(caught.is_err());

        let record = store.last().unwrap().expect("a crash record was written");
        assert_eq!(record.kind, ExitKind::Panic);
        assert_eq!(record.version, "0.1.0");
        assert_eq!(record.message.as_deref(), Some("simulated render panic"));
        assert!(record.location.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
