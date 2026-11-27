#![forbid(unsafe_code)]
//! Loopback HTTP tests: automatic crash report upload against a mock Hub.

use postbox_core::crash::{
    CrashReport, CrashReportKind, ExitRecord, MAX_CRASH_REPORT_BYTES, MAX_REPORT_UPLOAD_ATTEMPTS,
    RunningSession,
};
use postbox_e2e::MockHubServer;
use postbox_hub::{Endpoint, HubClient};
use postbox_platform::exit_records::{ExitRecordStore, now_unix_ms};
use postbox_platform::{Delivery, UploadSummary, upload_pending_reports};
use serde_json::Value;
use std::path::PathBuf;

const REPORT_PATH: &str = "/v1/wazig/crash-reports";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "postbox-e2e-crash-{tag}-{}-{}",
        std::process::id(),
        now_unix_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn client(hub: &MockHubServer) -> HubClient {
    HubClient::new(hub.endpoint(), hub.token()).expect("loopback client builds")
}

fn upload(store: &ExitRecordStore, client: &HubClient) -> UploadSummary {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    upload_pending_reports(store, |report| {
        match rt.block_on(client.send_crash_report(report)) {
            Ok(()) => Delivery::Delivered,
            Err(_) => Delivery::Failed,
        }
    })
}

fn seed_three_kinds(store: &ExitRecordStore) {
    store
        .record(&ExitRecord::panic(
            "0.1.1",
            1_200,
            now_unix_ms(),
            Some("index out of bounds while drawing the timeline"),
            Some("crates/postbox-ui/src/desktop.rs:42:9"),
        ))
        .unwrap();
    store
        .record(&ExitRecord::native_exception(
            "0.1.1",
            now_unix_ms(),
            "0xC0000005",
            Some("postbox.exe+0x1a2b"),
        ))
        .unwrap();
    store
        .record(&ExitRecord::sudden_exit(
            &RunningSession {
                version: "0.1.1".to_owned(),
                started_at_unix_ms: 1,
                pid: 4242,
                last_stage: "ready".to_owned(),
            },
            now_unix_ms(),
        ))
        .unwrap();
}

#[test]
fn pending_reports_reach_the_hub_once_and_are_never_sent_again() {
    let dir = temp_dir("once");
    let store = ExitRecordStore::new(&dir);
    seed_three_kinds(&store);

    let hub = MockHubServer::start("crash-token");
    let client = client(&hub);
    let first = upload(&store, &client);
    assert_eq!(first.attempted, 3, "{first:?}");
    assert_eq!(first.delivered, 3, "{first:?}");
    assert_eq!(first.failed, 0, "{first:?}");

    assert_eq!(hub.request_count("POST", REPORT_PATH), 3);
    let mut accepted = hub.accepted_crash_report_ids();
    accepted.sort();
    let mut expected: Vec<String> = store
        .read_all()
        .unwrap()
        .iter()
        .filter_map(|record| record.report().map(|r| r.report_id))
        .collect();
    expected.sort();
    assert_eq!(accepted, expected);

    // Every request carried the Bearer token and a JSON body.
    let expected_auth = format!("Bearer {}", hub.token());
    for recorded in hub.recorded_requests() {
        assert_eq!(
            recorded.authorization.as_deref(),
            Some(expected_auth.as_str())
        );
        assert!(
            recorded
                .content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("application/json")),
            "{:?}",
            recorded.content_type
        );
    }

    // Second pass: nothing is re-sent, even though the records are still on disk.
    let second = upload(&store, &client);
    assert_eq!(second.attempted, 0, "{second:?}");
    assert_eq!(hub.request_count("POST", REPORT_PATH), 3);
    assert_eq!(store.report_state().sent.len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn crash_report_bodies_carry_only_the_allowed_fields_and_no_private_data() {
    let dir = temp_dir("privacy");
    let store = ExitRecordStore::new(&dir);
    store
        .record(&ExitRecord::panic(
            "0.1.1",
            900,
            now_unix_ms(),
            Some(
                "send failed Bearer abcdef123456 for Ada <ada@example.com> token=xyztopsecretvalue \
                 chat: Family group with the summer trip photos",
            ),
            Some("C:\\Users\\ada\\postbox\\desktop.rs:42:9"),
        ))
        .unwrap();
    let hub = MockHubServer::start("privacy-token");
    let client = client(&hub);
    let summary = upload(&store, &client);
    assert_eq!(summary.delivered, 1, "{summary:?}");

    let requests = hub.recorded_requests();
    assert_eq!(requests.len(), 1);
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(
        body.len() <= MAX_CRASH_REPORT_BYTES,
        "the body must stay under the cap, {} bytes",
        body.len()
    );
    let value: Value = serde_json::from_str(&body).expect("the body is JSON");
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort();
    let allowed = [
        "arch",
        "exception_code",
        "kind",
        "last_stage",
        "location",
        "os",
        "report_id",
        "summary",
        "timestamp_unix_ms",
        "version",
    ];
    for key in &keys {
        assert!(
            allowed.contains(key),
            "{key} is not an allowed crash report field"
        );
    }
    assert!(keys.contains(&"report_id"));
    assert_eq!(value["kind"], "panic");

    for leaked in [
        "ada@example.com",
        "abcdef123456",
        "xyztopsecretvalue",
        "Users",
    ] {
        assert!(!body.contains(leaked), "{leaked} leaked into {body}");
    }
    // Long user text never ships whole: the summary is clipped at the message bound.
    let summary = value["summary"].as_str().unwrap_or_default();
    assert!(
        summary.chars().count() <= postbox_core::crash::MAX_MESSAGE_CHARS,
        "{summary}"
    );
    assert_eq!(value["location"], "desktop.rs:42:9");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_or_broken_endpoint_is_quiet_and_capped() {
    let dir = temp_dir("faults");
    let store = ExitRecordStore::new(&dir);
    store
        .record(&ExitRecord::panic(
            "0.1.1",
            900,
            now_unix_ms(),
            Some("boom"),
            None,
        ))
        .unwrap();
    let hub = MockHubServer::start("fault-token");
    let client = client(&hub);

    // 404: the Hub has no such route yet. Quiet failure, one attempt used.
    hub.push_crash_report_fault(404);
    let first = upload(&store, &client);
    assert_eq!(
        first,
        UploadSummary {
            attempted: 1,
            delivered: 0,
            failed: 1
        }
    );

    // 500: the Hub is broken. Same quiet failure.
    hub.push_crash_report_fault(500);
    let second = upload(&store, &client);
    assert_eq!(
        second,
        UploadSummary {
            attempted: 1,
            delivered: 0,
            failed: 1
        }
    );

    // The last allowed attempt, then the report is dropped for good.
    hub.push_crash_report_fault(503);
    let third = upload(&store, &client);
    assert_eq!(third.failed, 1, "{third:?}");
    assert_eq!(
        hub.request_count("POST", REPORT_PATH),
        MAX_REPORT_UPLOAD_ATTEMPTS as usize
    );

    let after_cap = upload(&store, &client);
    assert_eq!(after_cap.attempted, 0, "{after_cap:?}");
    assert_eq!(
        hub.request_count("POST", REPORT_PATH),
        MAX_REPORT_UPLOAD_ATTEMPTS as usize,
        "the cap stops the retries"
    );
    assert!(hub.accepted_crash_report_ids().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_offline_hub_fails_without_a_word() {
    let dir = temp_dir("offline");
    let store = ExitRecordStore::new(&dir);
    seed_three_kinds(&store);
    // Port 1 on loopback is closed: every attempt is a transport error.
    let endpoint = Endpoint::parse("http://127.0.0.1:1", true).unwrap();
    let client = HubClient::new(endpoint, "offline-token").expect("client builds");
    let summary = upload(&store, &client);
    assert_eq!(summary.failed, summary.attempted, "{summary:?}");
    assert!(summary.attempted > 0);
    assert!(store.report_state().sent.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_report_the_hub_accepts_is_marked_so_it_is_never_re_sent() {
    let dir = temp_dir("dedup");
    let store = ExitRecordStore::new(&dir);
    seed_three_kinds(&store);
    let hub = MockHubServer::start("dedup-token");
    let client = client(&hub);
    let _ = upload(&store, &client);
    let state = store.report_state();
    let reports: Vec<CrashReport> = store
        .read_all()
        .unwrap()
        .iter()
        .filter_map(|record| record.report())
        .collect();
    assert_eq!(reports.len(), 3);
    for report in reports {
        assert!(state.is_settled(&report.report_id));
        assert!(matches!(
            report.kind,
            CrashReportKind::Panic | CrashReportKind::NativeException | CrashReportKind::SuddenExit
        ));
    }
    let _ = std::fs::remove_dir_all(&dir);
}
