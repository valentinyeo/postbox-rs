//! Structured exit and crash records.
//!
//! One JSON object per line in `exit-records.jsonl` inside the app data directory, so a
//! bug report can say "it died at 14:03 while showing a video" without shipping any message
//! content. The record carries the version, when it happened, how long the process had been
//! up, and a sanitised panic message plus source location.

use serde::{Deserialize, Serialize};

/// Filename written inside the app data directory.
pub const EXIT_RECORD_FILE: &str = "exit-records.jsonl";
/// Rotate once the log is bigger than this.
pub const EXIT_RECORD_MAX_BYTES: usize = 64 * 1024;
/// How many of the newest records a rotation keeps.
pub const EXIT_RECORD_KEEP: usize = 50;
/// Longest sanitised message kept, in characters.
pub const MAX_MESSAGE_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExitKind {
    /// The window closed and the process exited on purpose.
    Clean,
    /// A panic unwound (or aborted) the process.
    Panic,
}

/// One line of `exit-records.jsonl`.
///
/// Deliberately carries no chat, message, contact or credential detail: `message` is run
/// through [`sanitize`] and `location` is a file/line pair from the compiler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitRecord {
    pub kind: ExitKind,
    pub version: String,
    pub timestamp_unix_ms: u64,
    pub uptime_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

impl ExitRecord {
    pub fn clean(version: &str, uptime_ms: u64, now_unix_ms: u64) -> Self {
        Self {
            kind: ExitKind::Clean,
            version: version.to_owned(),
            timestamp_unix_ms: now_unix_ms,
            uptime_ms,
            message: None,
            location: None,
        }
    }

    pub fn panic(
        version: &str,
        uptime_ms: u64,
        now_unix_ms: u64,
        message: Option<&str>,
        location: Option<&str>,
    ) -> Self {
        Self {
            kind: ExitKind::Panic,
            version: version.to_owned(),
            timestamp_unix_ms: now_unix_ms,
            uptime_ms,
            message: message.map(sanitize).filter(|m| !m.is_empty()),
            location: location.map(sanitize_location).filter(|l| !l.is_empty()),
        }
    }

    pub fn to_line(&self) -> String {
        // `ExitRecord` is plain owned data, so serialisation cannot fail here.
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
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

/// Replaces long hex/base64-looking runs, `token=`-style assignments and URL query strings.
fn redact(text: &str) -> String {
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
}
