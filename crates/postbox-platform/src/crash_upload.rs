//! Automatic crash report delivery.
//!
//! Startup calls [`upload_pending_reports`] from a background thread, after the first frame.
//! The function is deliberately transport-agnostic: the caller hands in a `deliver` closure so
//! this crate keeps no Hub dependency, and every failure is swallowed. A user must never see an
//! error, banner or modal because a crash report could not be uploaded.

use crate::exit_records::ExitRecordStore;
use postbox_core::crash::CrashReport;

/// What one delivery attempt told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The Hub accepted the report; it is never sent again.
    Delivered,
    /// Offline, 404, 5xx or anything else. Quietly retried later, up to the attempt cap.
    Failed,
}

/// Counts from one upload pass, for tests and telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadSummary {
    pub attempted: usize,
    pub delivered: usize,
    pub failed: usize,
}

/// Delivers every pending, unused-attempt crash report exactly once per call.
///
/// Reports that reach the Hub are recorded as sent. Reports that fail get one attempt used, and
/// are dropped once they hit the cap in [`postbox_core::crash::MAX_REPORT_UPLOAD_ATTEMPTS`].
pub fn upload_pending_reports(
    store: &ExitRecordStore,
    mut deliver: impl FnMut(&CrashReport) -> Delivery,
) -> UploadSummary {
    let mut state = store.report_state();
    let pending = store.pending_reports(&state);
    let mut summary = UploadSummary::default();
    for report in &pending {
        summary.attempted += 1;
        match deliver(report) {
            Delivery::Delivered => {
                summary.delivered += 1;
                state.record_sent(&report.report_id);
            }
            Delivery::Failed => {
                summary.failed += 1;
                state.record_attempt(&report.report_id);
            }
        }
    }
    if summary.attempted > 0 {
        // A read-only data directory must not turn a crash report into a startup failure.
        let _ = store.save_report_state(&state);
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::crash::{CrashReportKind, ExitRecord, MAX_REPORT_UPLOAD_ATTEMPTS};
    use std::path::PathBuf;

    fn now() -> u64 {
        crate::exit_records::now_unix_ms()
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-upload-{tag}-{}-{}",
            std::process::id(),
            now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn seeded_store(tag: &str, records: usize) -> (ExitRecordStore, PathBuf) {
        let dir = temp_dir(tag);
        let store = ExitRecordStore::new(&dir);
        for i in 0..records {
            store
                .record(&ExitRecord::panic(
                    "0.1.1",
                    i as u64,
                    now(),
                    Some("boom"),
                    Some("desktop.rs:1:1"),
                ))
                .unwrap();
        }
        (store, dir)
    }

    #[test]
    fn every_pending_report_is_delivered_once_and_never_again() {
        let (store, dir) = seeded_store("once", 2);
        let mut sent = Vec::new();
        let first = upload_pending_reports(&store, |report| {
            sent.push(report.report_id.clone());
            Delivery::Delivered
        });
        assert_eq!(first.attempted, 2);
        assert_eq!(first.delivered, 2);
        assert_eq!(first.failed, 0);
        assert_eq!(sent.len(), 2);

        let second = upload_pending_reports(&store, |_| {
            panic!("a delivered report must never be sent again")
        });
        assert_eq!(second.attempted, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_exits_are_never_uploaded() {
        let dir = temp_dir("clean");
        let store = ExitRecordStore::new(&dir);
        store.record_clean("0.1.1", 5_000).unwrap();
        let summary = upload_pending_reports(&store, |_| panic!("nothing to send"));
        assert_eq!(summary, UploadSummary::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_hub_uses_the_attempt_cap_then_stops_quietly() {
        let (store, dir) = seeded_store("cap", 1);
        for _ in 0..MAX_REPORT_UPLOAD_ATTEMPTS {
            let summary = upload_pending_reports(&store, |_| Delivery::Failed);
            assert_eq!(summary.failed, 1);
        }
        let summary = upload_pending_reports(&store, |_| Delivery::Failed);
        assert_eq!(summary.attempted, 0, "the cap must stop the retries");
        assert_eq!(store.report_state().attempts.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_later_success_clears_the_attempt_history() {
        let (store, dir) = seeded_store("recover", 1);
        upload_pending_reports(&store, |_| Delivery::Failed);
        let recovered = upload_pending_reports(&store, |_| Delivery::Delivered);
        assert_eq!(recovered.delivered, 1);
        assert!(store.report_state().attempts.is_empty());
        assert_eq!(
            upload_pending_reports(&store, |_| Delivery::Failed).attempted,
            0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn three_kinds_of_death_all_become_reports() {
        let dir = temp_dir("kinds");
        let store = ExitRecordStore::new(&dir);
        store
            .record(&ExitRecord::panic("0.1.1", 1, 10, Some("boom"), None))
            .unwrap();
        store
            .record(&ExitRecord::native_exception(
                "0.1.1",
                20,
                "0xC0000005",
                Some("postbox.exe+0x1000"),
            ))
            .unwrap();
        let session = postbox_core::crash::RunningSession {
            version: "0.1.1".to_owned(),
            started_at_unix_ms: 1,
            pid: 99,
            last_stage: "ready".to_owned(),
        };
        store
            .record(&ExitRecord::sudden_exit(&session, 30))
            .unwrap();

        let mut kinds = Vec::new();
        let summary = upload_pending_reports(&store, |report| {
            kinds.push(report.kind);
            Delivery::Delivered
        });
        assert_eq!(summary.delivered, 3);
        assert!(kinds.contains(&CrashReportKind::Panic));
        assert!(kinds.contains(&CrashReportKind::NativeException));
        assert!(kinds.contains(&CrashReportKind::SuddenExit));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
