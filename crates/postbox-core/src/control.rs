use crate::{Provider, ThemeChoice};
use serde::{Deserialize, Serialize};

pub const CONTROL_PIPE_PREFIX: &str = r"\\.\pipe\postbox-control-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum ControlRequest {
    Status,
    Theme {
        theme: ThemeChoice,
    },
    Font {
        percent: u16,
    },
    Chats {
        provider: Option<Provider>,
        archived: bool,
        all: bool,
        limit: Option<u32>,
    },
    Search {
        text: String,
    },
    View {
        target: String,
    },
    Select {
        id: String,
        provider: Option<Provider>,
        account: Option<String>,
    },
    Messages {
        id: Option<String>,
        limit: Option<u32>,
    },
    Archive {
        id: String,
    },
    Unarchive {
        id: String,
    },
    ArchiveMany {
        ids: Vec<String>,
        archived: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryCounters {
    pub frames_rendered: u64,
    pub working_set_kb: u64,
    /// Private commit charge (`PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`). Defaulted so
    /// `postboxctl status` from an older build still deserializes against a newer one.
    #[serde(default)]
    pub private_usage_kb: u64,
    pub gpu_texture_bytes: u64,
    pub pending_outbox: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_counters() -> TelemetryCounters {
        TelemetryCounters {
            frames_rendered: 120,
            working_set_kb: 61 * 1024,
            private_usage_kb: 54 * 1024,
            gpu_texture_bytes: 16 * 1024 * 1024,
            pending_outbox: 3,
        }
    }

    #[test]
    fn telemetry_counters_round_trip_working_set_and_private_usage() {
        let counters = full_counters();
        let json = serde_json::to_string(&counters).unwrap();
        assert!(json.contains("\"working_set_kb\":62464"));
        assert!(json.contains("\"private_usage_kb\":55296"));
        let back: TelemetryCounters = serde_json::from_str(&json).unwrap();
        assert_eq!(back, counters);
    }

    #[test]
    fn telemetry_counters_tolerate_older_payloads_without_private_usage() {
        // A `postboxctl status` written by a pre-M5.5 build must still deserialize.
        let legacy = r#"{
            "frames_rendered": 7,
            "working_set_kb": 4096,
            "gpu_texture_bytes": 1024,
            "pending_outbox": 0
        }"#;
        let parsed: TelemetryCounters = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.private_usage_kb, 0);
        assert_eq!(parsed.working_set_kb, 4096);
    }
}
