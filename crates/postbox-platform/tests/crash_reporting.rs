#![forbid(unsafe_code)]
//! Crash capture: panic records, native exception lines, and the dirty-session sentinel.

use postbox_core::crash::{
    CrashReportKind, ExitKind, ExitRecord, NATIVE_EXCEPTION_FILE, RUNNING_SESSION_FILE,
    RunningSession,
};
use postbox_platform::exit_records::{
    ExitRecordStore, install_native_exception_filter, install_panic_hook, now_unix_ms,
};
use std::path::PathBuf;
use std::time::Instant;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "postbox-crash-{tag}-{}-{}",
        std::process::id(),
        now_unix_ms()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn the_panic_hook_records_a_reportable_crash_with_an_id() {
    let dir = temp_dir("panic");
    let store = ExitRecordStore::new(&dir);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    install_panic_hook(store.clone(), "0.1.1", Instant::now());
    let caught = std::panic::catch_unwind(|| panic!("render panic while drawing a bubble"));
    std::panic::set_hook(previous);
    assert!(caught.is_err());

    let record = store.last().unwrap().expect("a crash record was written");
    assert_eq!(record.kind, ExitKind::Panic);
    assert_eq!(record.version, "0.1.1");
    assert!(record.location.is_some(), "the panic location is recorded");
    assert!(!record.report_id.is_empty(), "the record carries an id");
    let report = record.report().expect("a panic is reportable");
    assert_eq!(report.kind, CrashReportKind::Panic);
    assert_eq!(report.report_id, record.report_id);
    assert_eq!(report.summary, "render panic while drawing a bubble");
    assert!(!report.os.is_empty() && !report.arch.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_native_exception_record_formats_code_module_and_offset() {
    let record = ExitRecord::native_exception(
        "0.1.1",
        now_unix_ms(),
        "c0000409",
        Some("C:\\Program Files\\Postbox\\postbox.exe+0x1234"),
    );
    assert_eq!(record.kind, ExitKind::NativeException);
    assert_eq!(record.exception_code.as_deref(), Some("0xC0000409"));
    assert_eq!(record.location.as_deref(), Some("postbox.exe+0x1234"));
    let report = record.report().expect("a native exception is reportable");
    assert!(report.report_id.starts_with("native-"));
}

#[test]
fn native_exception_lines_are_folded_into_the_log_and_the_raw_file_removed() {
    let dir = temp_dir("fold");
    let store = ExitRecordStore::new(&dir);
    std::fs::write(
        store.native_exception_path(),
        "native-exception ts=1700000000123 code=c0000005 module=postbox.exe offset=0x1a2b version=0.1.0\n\
         garbage line that the filter left half-written\n\
         native-exception ts=1700000000456 code=c0000409 module=ntdll.dll offset=0x99 version=0.1.0\n",
    )
    .unwrap();

    let synthesized = store.begin_session("0.1.1", now_unix_ms());
    assert!(synthesized.is_none(), "the exceptions explain the death");
    assert!(
        !store.native_exception_path().exists(),
        "the raw exception file is consumed"
    );

    let records = store.read_all().unwrap();
    let native: Vec<&ExitRecord> = records
        .iter()
        .filter(|r| r.kind == ExitKind::NativeException)
        .collect();
    assert_eq!(native.len(), 2);
    assert_eq!(native[0].exception_code.as_deref(), Some("0xC0000005"));
    assert_eq!(native[0].location.as_deref(), Some("postbox.exe+0x1a2b"));
    assert_eq!(native[1].exception_code.as_deref(), Some("0xC0000409"));
    assert_eq!(native[1].version, "0.1.0");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_leftover_sentinel_becomes_a_sudden_exit_record() {
    let dir = temp_dir("sudden");
    let store = ExitRecordStore::new(&dir);
    store.record_clean("0.1.0", 4_000).unwrap();

    let dead = RunningSession {
        version: "0.1.0".to_owned(),
        started_at_unix_ms: 1_700_000_000_000,
        pid: 4242,
        last_stage: "ready".to_owned(),
    };
    store.write_session(&dead).unwrap();
    assert_eq!(store.session().as_ref(), Some(&dead));

    let synthesized = store
        .begin_session("0.1.1", 1_700_000_060_000)
        .expect("the vanished run is recorded");
    assert_eq!(synthesized.kind, ExitKind::SuddenExit);
    assert_eq!(synthesized.version, "0.1.0");
    assert_eq!(synthesized.pid, Some(4242));
    assert_eq!(synthesized.started_at_unix_ms, Some(1_700_000_000_000));
    assert_eq!(synthesized.last_stage.as_deref(), Some("ready"));

    // The log holds it, and it is reportable.
    let log = std::fs::read_to_string(store.path()).unwrap();
    assert!(log.contains("\"kind\":\"sudden-exit\""), "{log}");
    let pending = store.pending_reports(&store.report_state());
    assert!(
        pending
            .iter()
            .any(|report| report.kind == CrashReportKind::SuddenExit),
        "{pending:?}"
    );

    // The fresh sentinel belongs to this run, so the stage can be updated without touching it.
    let current = store.session().expect("a new sentinel exists");
    assert_ne!(current, dead);
    assert_eq!(current.last_stage, "startup");
    store.update_stage("ready").unwrap();
    assert_eq!(store.session().unwrap().last_stage, "ready");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_clean_exit_clears_the_sentinel_and_fakes_no_crash() {
    let dir = temp_dir("clean");
    let store = ExitRecordStore::new(&dir);
    let _ = store.begin_session("0.1.1", now_unix_ms());
    assert_eq!(store.session().unwrap().last_stage, "startup");

    store.record_clean("0.1.1", 9_000).unwrap();
    store.clear_session();
    assert_eq!(store.session(), None, "the sentinel is gone");

    // Next launch: no leftover sentinel, so nothing to synthesise.
    let synthesized = store.begin_session("0.1.1", now_unix_ms() + 1_000);
    assert!(synthesized.is_none());
    let records = store.read_all().unwrap();
    assert!(
        !records.iter().any(|r| r.kind == ExitKind::SuddenExit),
        "{records:?}"
    );
    assert!(
        store.pending_reports(&store.report_state()).is_empty(),
        "a clean exit is never uploaded"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_panic_after_the_sentinel_suppresses_the_sudden_exit_record() {
    let dir = temp_dir("explained");
    let store = ExitRecordStore::new(&dir);
    let started = 1_700_000_000_000;
    store
        .write_session(&RunningSession {
            version: "0.1.0".to_owned(),
            started_at_unix_ms: started,
            pid: 7,
            last_stage: "ready".to_owned(),
        })
        .unwrap();
    store
        .record(&ExitRecord::panic(
            "0.1.0",
            2_000,
            started + 2_000,
            Some("boom"),
            Some("desktop.rs:1:1"),
        ))
        .unwrap();

    assert!(store.begin_session("0.1.1", started + 10_000).is_none());
    assert!(
        !store
            .read_all()
            .unwrap()
            .iter()
            .any(|r| r.kind == ExitKind::SuddenExit),
        "one death produces one record"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_native_filter_install_is_a_safe_no_op_off_windows() {
    let dir = temp_dir("filter");
    install_native_exception_filter(&dir, "0.1.1");
    // Off Windows nothing is written; on Windows the filter only logs on a real exception.
    assert!(!dir.join(NATIVE_EXCEPTION_FILE).exists());
    assert!(dir.is_dir());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_sentinel_file_lives_beside_the_exit_log() {
    let dir = temp_dir("paths");
    let store = ExitRecordStore::new(&dir);
    assert_eq!(store.session_path(), dir.join(RUNNING_SESSION_FILE));
    assert_eq!(
        store.native_exception_path(),
        dir.join(NATIVE_EXCEPTION_FILE)
    );
    assert_eq!(
        store.report_state_path(),
        dir.join("crash-reports-state.json")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
