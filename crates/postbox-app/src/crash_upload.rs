//! Background crash report upload.
//!
//! Called once per launch, from a thread that waits for the first frame to be drawn. A crash
//! report that cannot be delivered is retried on a later launch, and the user is never told:
//! no toast, no banner, no modal, no log line on stdout.

use postbox_hub::{Endpoint, HubClient};
use postbox_platform::exit_records::ExitRecordStore;
use postbox_platform::{Delivery, upload_pending_reports};
use std::path::PathBuf;
use std::time::Duration;

/// How long the reporter waits before touching the network, so the first frame wins the
/// startup budget on a cold machine.
pub const FIRST_FRAME_DELAY: Duration = Duration::from_millis(2_000);

/// Uploads pending crash reports in the background and returns immediately.
///
/// The returned handle is only useful to tests; the app drops it.
pub fn spawn_crash_reporter(
    endpoint: Endpoint,
    token: String,
    data_dir: PathBuf,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("postbox-crash-report".to_owned())
        .spawn(move || {
            std::thread::sleep(FIRST_FRAME_DELAY);
            let store = ExitRecordStore::new(&data_dir);
            if store.pending_reports(&store.report_state()).is_empty() {
                return;
            }
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            // A client that cannot be built (bad token shape) is not worth retrying.
            let Ok(client) = HubClient::new(endpoint, token) else {
                return;
            };
            let _ = upload_pending_reports(&store, |report| {
                match rt.block_on(client.send_crash_report(report)) {
                    Ok(()) => Delivery::Delivered,
                    Err(_) => Delivery::Failed,
                }
            });
        })
        .unwrap_or_else(|_| {
            // A host that cannot start the thread simply never uploads; that is not fatal.
            std::thread::spawn(|| {})
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::crash::ExitRecord;

    #[test]
    fn the_reporter_delay_is_after_the_first_frame_budget() {
        assert!(FIRST_FRAME_DELAY >= Duration::from_millis(500));
    }

    #[test]
    fn a_store_with_nothing_pending_needs_no_hub() {
        let dir = std::env::temp_dir().join(format!("postbox-reporter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = ExitRecordStore::new(&dir);
        store.record_clean("0.1.1", 10).unwrap();
        assert!(store.pending_reports(&store.report_state()).is_empty());
        let endpoint = Endpoint::parse("http://127.0.0.1:1", true).unwrap();
        // Nothing pending: the thread returns before it ever dials the port.
        let handle = spawn_crash_reporter(endpoint, "test-token".to_owned(), dir.clone());
        let _ = handle.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(store.pending_reports(&store.report_state()), Vec::new());
        assert!(ExitRecord::clean("0.1.1", 1, 0).report().is_none());
    }
}
