//! Structured exit and crash records.
//!
//! One JSON object per line in `exit-records.jsonl` inside the app data directory, so a
//! bug report can say "it died at 14:03 while showing a video" without shipping any message
//! content. The record carries the version, when it happened, how long the process had been
//! up, a unique `report_id`, and a sanitised panic or exception message plus source location.
//!
//! Three kinds of death are recorded:
//!
//! - [`ExitKind::Clean`]: the window closed and the process exited on purpose.
//! - [`ExitKind::Panic`]: a Rust panic unwound (or aborted) the process; the panic hook writes it.
//! - [`ExitKind::NativeException`]: a native Windows exception (`STATUS_ACCESS_VIOLATION` and
//!   friends) where no Rust panic hook ran; the unhandled-exception filter writes it.
//! - [`ExitKind::SuddenExit`]: the process vanished without either of the above, such as
//!   `TerminateProcess`, a fast-fail (`0xC0000409`), or a GPU driver crash. The next startup
//!   finds the leftover `running-session.json` and synthesises the record.
//!
//! [`CrashReport`] is the only shape allowed to leave the machine. It carries identity and
//! failure shape, never chat titles, message bodies, contact names or credentials.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Filename written inside the app data directory.
pub const EXIT_RECORD_FILE: &str = "exit-records.jsonl";
/// Rotate once the log is bigger than this.
pub const EXIT_RECORD_MAX_BYTES: usize = 64 * 1024;
/// How many of the newest records a rotation keeps.
pub const EXIT_RECORD_KEEP: usize = 50;
/// Longest sanitised message kept, in characters.
pub const MAX_MESSAGE_CHARS: usize = 200;
/// Dirty-session sentinel written on startup and removed on a clean exit.
pub const RUNNING_SESSION_FILE: &str = "running-session.json";
/// Native exception lines written by the Windows unhandled-exception filter.
pub const NATIVE_EXCEPTION_FILE: &str = "native-exceptions.log";
/// Upload bookkeeping: which crash reports already reached the Hub, and how often each failed.
pub const CRASH_REPORT_STATE_FILE: &str = "crash-reports-state.json";
/// How many delivery attempts one report gets before it is dropped quietly.
pub const MAX_REPORT_UPLOAD_ATTEMPTS: u32 = 3;
/// How many report ids the upload state file remembers.
pub const MAX_TRACKED_REPORTS: usize = 200;
/// Hard ceiling for one serialised crash report body.
pub const MAX_CRASH_REPORT_BYTES: usize = 4 * 1024;
/// Stage name written into the sentinel before the window exists.
pub const SESSION_STAGE_STARTUP: &str = "startup";
/// Stage name written once the window and Hub sync are up.
pub const SESSION_STAGE_READY: &str = "ready";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitKind {
    /// The window closed and the process exited on purpose.
    Clean,
    /// A panic unwound (or aborted) the process.
    Panic,
    /// A native Windows exception killed the process outside the Rust panic machinery.
    NativeException,
    /// The process vanished with no record at all; inferred from the leftover sentinel.
    SuddenExit,
}

impl ExitKind {
    /// Short prefix used inside generated `report_id` values.
    pub fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Panic => "panic",
            Self::NativeException => "native",
            Self::SuddenExit => "sudden",
        }
    }
}

/// One line of `exit-records.jsonl`.
///
/// Deliberately carries no chat, message, contact or credential detail: `message` is run
/// through [`sanitize`] and `location` is a file/line pair from the compiler or a module
/// offset from the Windows exception record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub kind: ExitKind,
    pub version: String,
    pub timestamp_unix_ms: u64,
    pub uptime_ms: u64,
    /// Stable identity of this record; the upload bookkeeping keys off it.
    #[serde(default)]
    pub report_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// `0xC0000005`-shaped native exception code, for [`ExitKind::NativeException`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_code: Option<String>,
    /// The last lifecycle stage the process reached, from the dirty-session sentinel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stage: Option<String>,
    /// Process id of the run this record describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// When the dead session started, for a synthesised [`ExitKind::SuddenExit`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
}

impl ExitRecord {
    fn base(kind: ExitKind, version: &str, uptime_ms: u64, now_unix_ms: u64) -> Self {
        Self {
            kind,
            version: version.to_owned(),
            timestamp_unix_ms: now_unix_ms,
            uptime_ms,
            report_id: new_report_id(kind),
            message: None,
            location: None,
            exception_code: None,
            last_stage: None,
            pid: Some(std::process::id()),
            started_at_unix_ms: None,
        }
    }

    pub fn clean(version: &str, uptime_ms: u64, now_unix_ms: u64) -> Self {
        Self::base(ExitKind::Clean, version, uptime_ms, now_unix_ms)
    }

    pub fn panic(
        version: &str,
        uptime_ms: u64,
        now_unix_ms: u64,
        message: Option<&str>,
        location: Option<&str>,
    ) -> Self {
        let mut record = Self::base(ExitKind::Panic, version, uptime_ms, now_unix_ms);
        record.message = message.map(sanitize).filter(|m| !m.is_empty());
        record.location = location.map(sanitize_location).filter(|l| !l.is_empty());
        record
    }

    /// A native Windows exception that bypassed the Rust panic hook.
    pub fn native_exception(
        version: &str,
        now_unix_ms: u64,
        exception_code: &str,
        location: Option<&str>,
    ) -> Self {
        let mut record = Self::base(ExitKind::NativeException, version, 0, now_unix_ms);
        record.exception_code = Some(normalize_exception_code(exception_code));
        record.location = location.map(sanitize_location).filter(|l| !l.is_empty());
        record
    }

    /// A process that died with no panic and no exception record: inferred from the sentinel.
    pub fn sudden_exit(session: &RunningSession, now_unix_ms: u64) -> Self {
        let mut record = Self::base(ExitKind::SuddenExit, &session.version, 0, now_unix_ms);
        record.pid = Some(session.pid);
        record.started_at_unix_ms = Some(session.started_at_unix_ms);
        record.last_stage = Some(sanitize_stage(&session.last_stage));
        record
    }

    pub fn to_line(&self) -> String {
        // `ExitRecord` is plain owned data, so serialisation cannot fail here.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }

    /// Parses one line written by the Windows unhandled-exception filter.
    ///
    /// The filter runs inside a fatal exception and cannot allocate, so it writes a fixed
    /// space-separated line instead of JSON:
    /// `native-exception ts=<unix_ms> code=0xC0000005 module=postbox.exe offset=0x1234 version=0.1.1`
    pub fn from_native_line(line: &str) -> Option<Self> {
        let mut parts = line.split_whitespace();
        if parts.next()? != "native-exception" {
            return None;
        }
        let mut timestamp = None;
        let mut code = None;
        let mut module = String::new();
        let mut offset = None;
        let mut version = None;
        for field in parts {
            let (key, value) = field.split_once('=')?;
            match key {
                "ts" => timestamp = value.parse::<u64>().ok(),
                "code" => code = Some(value.to_owned()),
                "module" => {
                    module = value
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
                        .take(40)
                        .collect()
                }
                "offset" => offset = Some(value.to_owned()),
                "version" => {
                    version = Some(
                        value
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
                            .take(32)
                            .collect::<String>(),
                    )
                }
                _ => {}
            }
        }
        let code = code?;
        let version = version.filter(|v| !v.is_empty())?;
        let location = match (module.is_empty(), offset) {
            (false, Some(offset)) => Some(format!("{module}+{offset}")),
            (false, None) => Some(module),
            (true, _) => None,
        };
        let mut record = Self::native_exception(
            &version,
            timestamp.unwrap_or_default(),
            &code,
            location.as_deref(),
        );
        if let Some(timestamp) = timestamp {
            record.timestamp_unix_ms = timestamp;
        }
        Some(record)
    }

    /// The uploadable report for this record, or `None` for a clean exit.
    pub fn report(&self) -> Option<CrashReport> {
        let kind = CrashReportKind::from_exit_kind(self.kind)?;
        let report_id = if self.report_id.is_empty() {
            new_report_id(self.kind)
        } else {
            self.report_id.clone()
        };
        Some(
            CrashReport {
                report_id,
                kind,
                version: self.version.clone(),
                os: std::env::consts::OS.to_owned(),
                arch: std::env::consts::ARCH.to_owned(),
                timestamp_unix_ms: self.timestamp_unix_ms,
                location: self.location.clone(),
                exception_code: self.exception_code.clone(),
                last_stage: self.last_stage.clone(),
                summary: self.message.clone().unwrap_or_default(),
            }
            .redacted(),
        )
    }
}

/// The kind of failure a [`CrashReport`] describes. Clean exits are never uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashReportKind {
    Panic,
    NativeException,
    SuddenExit,
}

impl CrashReportKind {
    pub fn from_exit_kind(kind: ExitKind) -> Option<Self> {
        match kind {
            ExitKind::Clean => None,
            ExitKind::Panic => Some(Self::Panic),
            ExitKind::NativeException => Some(Self::NativeException),
            ExitKind::SuddenExit => Some(Self::SuddenExit),
        }
    }
}

/// The only crash shape allowed to leave the machine.
///
/// Every field is either an identity token, a product fact or text that has been through
/// [`sanitize`]: no chat titles, message bodies, contact names or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReport {
    pub report_id: String,
    pub kind: CrashReportKind,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub timestamp_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exception_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_stage: Option<String>,
    #[serde(default)]
    pub summary: String,
}

impl CrashReport {
    /// Re-applies every redaction rule. Callers that build a report by hand still cannot leak.
    pub fn redacted(mut self) -> Self {
        self.report_id = sanitize_report_id(&self.report_id);
        self.version = self
            .version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            .take(32)
            .collect();
        self.os = self
            .os
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .take(24)
            .collect();
        self.arch = self
            .arch
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .take(24)
            .collect();
        self.location = self
            .location
            .as_deref()
            .map(sanitize_location)
            .filter(|l| !l.is_empty());
        self.exception_code = self
            .exception_code
            .as_deref()
            .map(normalize_exception_code)
            .filter(|c| c != "0x00000000");
        self.last_stage = self
            .last_stage
            .as_deref()
            .map(sanitize_stage)
            .filter(|s| !s.is_empty());
        self.summary = sanitize(&self.summary);
        self
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// What the app writes on startup and deletes on a clean exit.
///
/// A file still on disk at the next startup means the previous process died without either
/// a panic record or a native exception record, so the exit was sudden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningSession {
    pub version: String,
    pub started_at_unix_ms: u64,
    pub pid: u32,
    pub last_stage: String,
}

impl RunningSession {
    pub fn new(version: &str, started_at_unix_ms: u64, pid: u32) -> Self {
        Self {
            version: version.to_owned(),
            started_at_unix_ms,
            pid,
            last_stage: SESSION_STAGE_STARTUP.to_owned(),
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// Upload bookkeeping for crash reports, persisted beside the records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashReportState {
    /// Reports the Hub accepted; they are never sent twice.
    #[serde(default)]
    pub sent: Vec<String>,
    /// Failed delivery attempts per report id.
    #[serde(default)]
    pub attempts: BTreeMap<String, u32>,
}

impl CrashReportState {
    /// `true` when this report is done: delivered, or out of attempts.
    pub fn is_settled(&self, report_id: &str) -> bool {
        self.sent.iter().any(|id| id == report_id)
            || self
                .attempts
                .get(report_id)
                .is_some_and(|count| *count >= MAX_REPORT_UPLOAD_ATTEMPTS)
    }

    pub fn record_sent(&mut self, report_id: &str) {
        if !self.sent.iter().any(|id| id == report_id) {
            self.sent.push(report_id.to_owned());
        }
        self.attempts.remove(report_id);
        let excess = self.sent.len().saturating_sub(MAX_TRACKED_REPORTS);
        if excess > 0 {
            self.sent.drain(..excess);
        }
    }

    pub fn record_attempt(&mut self, report_id: &str) {
        let entry = self.attempts.entry(report_id.to_owned()).or_insert(0);
        *entry = entry.saturating_add(1);
        if self.attempts.len() > MAX_TRACKED_REPORTS {
            let drop: Vec<String> = self
                .attempts
                .keys()
                .take(self.attempts.len() - MAX_TRACKED_REPORTS)
                .cloned()
                .collect();
            for id in drop {
                self.attempts.remove(&id);
            }
        }
    }

    pub fn to_line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// A random 64-bit report identity with a kind prefix, so two records never collide.
pub fn new_report_id(kind: ExitKind) -> String {
    let mut bytes = [0u8; 8];
    let hex = match getrandom::fill(&mut bytes) {
        Ok(()) => format!(
            "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        ),
        Err(_) => {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            format!("{nanos:016x}")
        }
    };
    format!("{}-{hex}", kind.label())
}

/// Keeps a report id to the generated shape so it cannot smuggle text out.
fn sanitize_report_id(report_id: &str) -> String {
    let kept: String = report_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(48)
        .collect();
    if kept.len() < 6 {
        new_report_id(ExitKind::Panic)
    } else {
        kept
    }
}

/// Normalises any exception code spelling to `0x` plus eight upper-case hex digits.
pub fn normalize_exception_code(code: &str) -> String {
    let trimmed = code.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    // Anything that is not pure hex digits is not an exception code we can trust.
    if body.is_empty() || body.len() > 8 || !body.chars().all(|c| c.is_ascii_hexdigit()) {
        return "0x00000000".to_owned();
    }
    let digits = body.to_ascii_uppercase();
    format!("0x{digits:0>8}")
}

/// Keeps a lifecycle stage to a short plain word.
pub fn sanitize_stage(stage: &str) -> String {
    stage
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect()
}

/// Drops control characters, collapses whitespace, redacts anything that looks like a
/// credential and clips the result.
pub fn sanitize(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len().min(MAX_MESSAGE_CHARS));
    for ch in text.chars().take(MAX_MESSAGE_CHARS) {
        cleaned.push(if ch.is_control() || ch == '\u{feff}' {
            ' '
        } else {
            ch
        });
    }
    let collapsed = redact(&collapse_whitespace(&cleaned));
    collapsed.chars().take(MAX_MESSAGE_CHARS).collect()
}

/// Keeps `file.rs:12` shaped locations, and nothing else.
pub fn sanitize_location(location: &str) -> String {
    let trimmed = location.trim();
    let kept: String = trimmed
        .chars()
        .filter(|c| !c.is_control())
        .take(160)
        .collect();
    kept.rsplit(['/', '\\'])
        .next()
        .unwrap_or(&kept)
        .trim()
        .to_owned()
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

/// Field names whose value is a credential, matched case-insensitively before `=` or `:`.
const CREDENTIAL_KEYS: [&str; 9] = [
    "token",
    "password",
    "passwd",
    "secret",
    "api_key",
    "apikey",
    "authorization",
    "auth",
    "bearer",
];

/// Replaces `token=...`, `Authorization: Bearer ...` and bare `Bearer ...` values with
/// `<redacted>`.
fn mask_credential_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let mut matched = false;
        for key in CREDENTIAL_KEYS {
            if !lower[index..].starts_with(key) {
                continue;
            }
            // The key must stand alone: not the tail of a longer word.
            if index > 0 {
                let prev = bytes[index - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' {
                    continue;
                }
            }
            let mut cursor = index + key.len();
            while cursor < bytes.len() && bytes[cursor] == b' ' {
                cursor += 1;
            }
            let separator = cursor < bytes.len() && matches!(bytes[cursor], b'=' | b':');
            if separator {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] == b' ' {
                    cursor += 1;
                }
            } else if key != "bearer" {
                // Only an auth scheme may appear without a separator.
                continue;
            }
            let value_start = cursor;
            let mut value_end = cursor;
            while value_end < bytes.len() && !bytes[value_end].is_ascii_whitespace() {
                value_end += 1;
            }
            if value_end == value_start {
                continue;
            }
            // `Authorization: Bearer <token>`: the scheme is not the secret, the token is.
            let mut end = value_end;
            if matches!(
                &lower[value_start..value_end],
                "bearer" | "basic" | "digest"
            ) {
                let mut next = value_end;
                while next < bytes.len() && bytes[next] == b' ' {
                    next += 1;
                }
                let next_start = next;
                while next < bytes.len() && !bytes[next].is_ascii_whitespace() {
                    next += 1;
                }
                if next > next_start {
                    end = next;
                }
            }
            out.push_str(&text[index..value_start]);
            out.push_str("<redacted>");
            index = end;
            matched = true;
            break;
        }
        if matched {
            continue;
        }
        let ch = text[index..].chars().next().unwrap_or(' ');
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Replaces email-shaped tokens. Panic text is developer text, but a support address in it is
/// still somebody's personal data.
fn mask_emails(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (position, segment) in text.split(' ').enumerate() {
        if position > 0 {
            out.push(' ');
        }
        if is_emailish(segment) {
            out.push_str("<redacted>");
        } else {
            out.push_str(segment);
        }
    }
    out
}

fn is_emailish(segment: &str) -> bool {
    let trimmed = segment.trim_matches(|c: char| {
        !(c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-' | '+'))
    });
    let Some((local, domain)) = trimmed.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.len() > 2
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

/// Replaces long hex/base64-looking runs, credential values and URL query strings.
fn redact(text: &str) -> String {
    let masked = mask_credential_values(&mask_emails(text));
    let text = masked.as_str();
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.is_empty() {
            return;
        }
        let secretish = run.len() >= 24
            && run
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-'));
        if secretish {
            out.push_str("<redacted>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-') {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
            out.push(ch);
        }
    }
    flush(&mut run, &mut out);
    out.split('?').next().unwrap_or(&out).to_owned()
}

/// Applies the rotation policy to the existing log text.
///
/// `None` means "nothing to do"; otherwise the returned text replaces the file.
pub fn rotated(existing: &str) -> Option<String> {
    if existing.len() <= EXIT_RECORD_MAX_BYTES {
        return None;
    }
    let lines: Vec<&str> = existing
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(EXIT_RECORD_KEEP);
    let mut out = String::with_capacity(EXIT_RECORD_MAX_BYTES);
    for line in &lines[start..] {
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_and_panic_records_round_trip_through_json() {
        let clean = ExitRecord::clean("0.1.0", 6_000, 1_700_000_000_000);
        let line = clean.to_line();
        assert!(line.contains("\"kind\":\"clean\""));
        assert!(line.contains("\"report_id\":\"clean-"), "{line}");
        assert_eq!(ExitRecord::parse(&line), Some(clean));

        let panic = ExitRecord::panic(
            "0.1.0",
            1_200,
            1_700_000_000_500,
            Some("index out of bounds"),
            Some("/home/valentin/projects/postbox-rs/crates/postbox-ui/src/desktop.rs:42:9"),
        );
        let line = panic.to_line();
        assert!(line.contains("\"kind\":\"panic\""));
        assert!(line.contains("\"location\":\"desktop.rs:42:9\""), "{line}");
        assert_eq!(ExitRecord::parse(&line), Some(panic));
    }

    #[test]
    fn every_record_carries_a_unique_report_id() {
        let a = ExitRecord::panic("0.1.0", 1, 2, Some("boom"), None);
        let b = ExitRecord::panic("0.1.0", 1, 2, Some("boom"), None);
        assert_ne!(a.report_id, b.report_id);
        assert!(a.report_id.starts_with("panic-"), "{}", a.report_id);
        assert!(a.report_id.len() > 10);
        assert!(
            a.report_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
    }

    #[test]
    fn records_carry_no_chat_or_message_fields() {
        let line = ExitRecord::panic("0.1.0", 1, 2, Some("boom"), None).to_line();
        for forbidden in ["chat", "message_text", "sender", "token", "phone"] {
            assert!(!line.contains(forbidden), "{line}");
        }
    }

    #[test]
    fn sanitize_drops_control_characters_and_clips_length() {
        let dirty = format!("a\u{0}b\u{7}c\nnext{}", "x".repeat(500));
        let clean = sanitize(&dirty);
        assert!(!clean.chars().any(|c| c.is_control()));
        assert!(clean.chars().count() <= MAX_MESSAGE_CHARS);
        assert!(clean.starts_with("a b c "), "{clean}");
    }

    #[test]
    fn sanitize_redacts_credentials_and_query_strings() {
        let with_token = sanitize(
            "failed to send bearer=eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxIn0.abcdef",
        );
        assert!(with_token.contains("<redacted>"), "{with_token}");

        let with_url = sanitize("GET https://hub.example/v1/wazig/chats?token=s3cret failed");
        assert!(!with_url.contains("s3cret"), "{with_url}");
        assert!(with_url.starts_with("GET https://hub.example/v1/wazig/chats"));
    }

    #[test]
    fn sanitize_strips_bearer_headers_and_credential_pairs() {
        let header = sanitize("Authorization: Bearer abc123def456 failed");
        assert!(!header.contains("abc123def456"), "{header}");
        assert!(header.contains("<redacted>"), "{header}");

        let sloppy = sanitize("POST failed api_key=shortkey and password: hunter2");
        assert!(!sloppy.contains("shortkey"), "{sloppy}");
        assert!(!sloppy.contains("hunter2"), "{sloppy}");

        // A long chat title or message body is clipped to the message bound, never shipped whole.
        let body = sanitize(&"conversation about the summer trip ".repeat(20));
        assert!(body.chars().count() <= MAX_MESSAGE_CHARS, "{body}");
    }

    #[test]
    fn rotation_keeps_the_newest_records_only() {
        assert_eq!(rotated(""), None);
        let small = "{\"kind\":\"clean\"}\n";
        assert_eq!(rotated(small), None);

        let mut big = String::new();
        for i in 0..EXIT_RECORD_KEEP * 3 {
            big.push_str(&format!("record-{i} {}\n", "y".repeat(600)));
        }
        assert!(big.len() > EXIT_RECORD_MAX_BYTES);
        let kept = rotated(&big).expect("rotation happens");
        let lines: Vec<&str> = kept.lines().collect();
        assert_eq!(lines.len(), EXIT_RECORD_KEEP);
        assert!(lines[0].starts_with("record-100 "), "{}", lines[0]);
        assert!(
            lines.last().unwrap().starts_with("record-149 "),
            "{}",
            lines.last().unwrap()
        );
    }

    #[test]
    fn location_keeps_only_the_file_and_line() {
        assert_eq!(
            sanitize_location("C:\\src\\postbox\\desktop.rs:42:9"),
            "desktop.rs:42:9"
        );
        assert_eq!(sanitize_location("  "), "");
    }

    #[test]
    fn native_exception_lines_parse_into_records() {
        let line = "native-exception ts=1700000000123 code=0xc0000005 module=postbox.exe offset=0x1a2b version=0.1.1";
        let record = ExitRecord::from_native_line(line).expect("the filter line parses");
        assert_eq!(record.kind, ExitKind::NativeException);
        assert_eq!(record.version, "0.1.1");
        assert_eq!(record.timestamp_unix_ms, 1_700_000_000_123);
        assert_eq!(record.exception_code.as_deref(), Some("0xC0000005"));
        assert_eq!(record.location.as_deref(), Some("postbox.exe+0x1a2b"));
        assert!(record.report_id.starts_with("native-"));

        assert_eq!(ExitRecord::from_native_line("clean exit"), None);
        assert_eq!(ExitRecord::from_native_line("native-exception ts=x"), None);
    }

    #[test]
    fn exception_codes_are_normalised_to_eight_hex_digits() {
        assert_eq!(normalize_exception_code("0xC0000005"), "0xC0000005");
        assert_eq!(normalize_exception_code("c0000409"), "0xC0000409");
        assert_eq!(normalize_exception_code("0X1"), "0x00000001");
        assert_eq!(normalize_exception_code("nonsense"), "0x00000000");
    }

    #[test]
    fn a_sudden_exit_record_carries_the_dead_session_facts() {
        let session = RunningSession {
            version: "0.1.1".to_owned(),
            started_at_unix_ms: 1_700_000_000_000,
            pid: 4242,
            last_stage: "ready".to_owned(),
        };
        let record = ExitRecord::sudden_exit(&session, 1_700_000_060_000);
        assert_eq!(record.kind, ExitKind::SuddenExit);
        assert_eq!(record.version, "0.1.1");
        assert_eq!(record.pid, Some(4242));
        assert_eq!(record.started_at_unix_ms, Some(1_700_000_000_000));
        assert_eq!(record.last_stage.as_deref(), Some("ready"));

        let report = record.report().expect("a sudden exit is reportable");
        assert_eq!(report.kind, CrashReportKind::SuddenExit);
        assert_eq!(report.last_stage.as_deref(), Some("ready"));
        assert!(!report.os.is_empty());
        assert!(!report.arch.is_empty());
    }

    #[test]
    fn clean_exits_are_never_reportable() {
        assert_eq!(ExitRecord::clean("0.1.0", 10, 20).report(), None);
    }

    #[test]
    fn a_report_rejects_content_and_credentials_even_when_built_by_hand() {
        let report = CrashReport {
            report_id: "panic-0123456789abcdef".to_owned(),
            kind: CrashReportKind::Panic,
            version: "0.1.1".to_owned(),
            os: "windows".to_owned(),
            arch: "x86_64".to_owned(),
            timestamp_unix_ms: 1,
            location: Some("C:\\Users\\ada\\desktop.rs:42:9".to_owned()),
            exception_code: None,
            last_stage: Some("ready".to_owned()),
            summary: "send failed Bearer abcdef123456 for Ada <ada@example.com> token=xyz987"
                .to_owned(),
        }
        .redacted();
        let line = report.to_line();
        for leaked in ["ada@example.com", "abcdef123456", "xyz987", "Users"] {
            assert!(!line.contains(leaked), "{line}");
        }
        assert_eq!(report.location.as_deref(), Some("desktop.rs:42:9"));
    }

    #[test]
    fn report_ids_are_clamped_to_the_generated_shape() {
        let dirty = CrashReport {
            report_id: "panic-../../etc/passwd Ada".to_owned(),
            kind: CrashReportKind::Panic,
            version: "0.1.1".to_owned(),
            os: "windows".to_owned(),
            arch: "x86_64".to_owned(),
            timestamp_unix_ms: 1,
            location: None,
            exception_code: None,
            last_stage: None,
            summary: String::new(),
        }
        .redacted();
        assert!(!dirty.report_id.contains('/'), "{}", dirty.report_id);
        assert!(!dirty.report_id.contains(' '), "{}", dirty.report_id);
    }

    #[test]
    fn upload_state_settles_a_report_after_success_or_the_attempt_cap() {
        let mut state = CrashReportState::default();
        assert!(!state.is_settled("panic-1"));
        state.record_attempt("panic-1");
        assert!(!state.is_settled("panic-1"));
        for _ in 1..MAX_REPORT_UPLOAD_ATTEMPTS {
            state.record_attempt("panic-1");
        }
        assert!(state.is_settled("panic-1"));

        state.record_sent("panic-2");
        assert!(state.is_settled("panic-2"));
        assert!(!state.sent.iter().any(|id| id == "panic-1"));

        let line = state.to_line();
        assert_eq!(CrashReportState::parse(&line), Some(state));
    }

    #[test]
    fn session_and_report_state_round_trip() {
        let session = RunningSession::new("0.1.1", 1_700_000_000_000, 7);
        assert_eq!(
            RunningSession::parse(&session.to_line()),
            Some(session.clone())
        );
        assert_eq!(session.last_stage, SESSION_STAGE_STARTUP);
        assert_eq!(RunningSession::parse("not json"), None);
    }
}
