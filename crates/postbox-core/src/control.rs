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
    pub gpu_texture_bytes: u64,
    pub pending_outbox: u32,
}
