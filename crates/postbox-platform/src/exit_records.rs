//! Persists [`ExitRecord`] lines in the app data directory and installs the panic hook.
//!
//! The hook records the sanitised panic message and location, then calls the previous hook so
//! the normal message still reaches stderr. Nothing here reads chat data.

use crate::PlatformError;
use postbox_core::crash::{EXIT_RECORD_FILE, ExitRecord, rotated};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const PENDING_CRASH_FILE: &str = "pending-crash.json";

// An upload of an older report must not remove a panic written while HTTP was in flight.
static PENDING_CRASH_LOCK: Mutex<()> = Mutex::new(());

pub fn read_pending_crash(data_dir: &Path) -> Option<String> {
    let _guard = PENDING_CRASH_LOCK.lock().ok()?;
    std::fs::read_to_string(data_dir.join(PENDING_CRASH_FILE)).ok()
}

pub fn clear_pending_crash(data_dir: &Path) {
    if let Ok(_guard) = PENDING_CRASH_LOCK.lock() {
        let _ = std::fs::remove_file(data_dir.join(PENDING_CRASH_FILE));
    }
}

pub fn clear_pending_crash_if_matches(data_dir: &Path, uploaded: &str) {
    if let Ok(_guard) = PENDING_CRASH_LOCK.lock() {
        let path = data_dir.join(PENDING_CRASH_FILE);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(uploaded) {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn write_pending_crash(data_dir: &Path, record: &ExitRecord, backtrace: &str) {
    let mut end = backtrace.len().min(4096);
    while !backtrace.is_char_boundary(end) {
        end -= 1;
    }
    let report = serde_json::json!({
        "kind": "panic",
        "version": record.version,
        "os": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "uptime_ms": record.uptime_ms,
        "message": record.message,
        "location": record.location,
        "backtrace": &backtrace[..end],
        "timestamp_ms": record.timestamp_unix_ms,
    });
    if let Ok(_guard) = PENDING_CRASH_LOCK.lock() {
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(data_dir.join(PENDING_CRASH_FILE), report.to_string());
    }
}

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
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        write_pending_crash(store.dir(), &record, &backtrace);
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
    fn pending_report_round_trips_sanitized_payload_and_clears() {
        let dir = temp_dir("pending").join("nested");
        assert_eq!(read_pending_crash(&dir), None);
        clear_pending_crash(&dir);
        let record = ExitRecord::panic(
            "0.1.2",
            42,
            123,
            Some("panic with token=abcdefghijklmnopqrstuvwxyz012345"),
            Some("crates/postbox-ui/src/desktop.rs:3:1"),
        );
        write_pending_crash(&dir, &record, "frame one\nframe two");
        let text = read_pending_crash(&dir).unwrap();
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["kind"], "panic");
        assert_eq!(report["version"], "0.1.2");
        assert_eq!(
            report["os"],
            format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
        );
        assert_eq!(report["uptime_ms"], 42);
        assert_eq!(report["timestamp_ms"], 123);
        assert_eq!(report["message"], "panic with <redacted>");
        assert_eq!(report["location"], "desktop.rs:3:1");
        assert_eq!(report["backtrace"], "frame one\nframe two");
        assert!(!text.contains("abcdefghijklmnopqrstuvwxyz012345"));
        clear_pending_crash_if_matches(&dir, "an older upload");
        assert!(read_pending_crash(&dir).is_some());
        clear_pending_crash_if_matches(&dir, &text);
        assert_eq!(read_pending_crash(&dir), None);
        write_pending_crash(&dir, &record, "another trace");
        clear_pending_crash(&dir);
        assert_eq!(read_pending_crash(&dir), None);
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn backtrace_is_bounded_without_splitting_a_utf8_character() {
        let dir = temp_dir("backtrace");
        let record = ExitRecord::panic("0.1.0", 1, 2, None, None);
        let backtrace = format!("{}émore frames", "a".repeat(4095));
        write_pending_crash(&dir, &record, &backtrace);
        let text = read_pending_crash(&dir).unwrap();
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["backtrace"].as_str().unwrap().len(), 4095);
        assert!(text.len() <= 16 * 1024);
        let _ = std::fs::remove_dir_all(dir);
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
        let text = read_pending_crash(&dir).expect("the hook also writes a pending report");
        let report: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(report["message"], "simulated render panic");
        assert_eq!(report["version"], "0.1.0");
        assert!(!report["backtrace"].as_str().unwrap().is_empty());
        assert!(report["backtrace"].as_str().unwrap().len() <= 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
