/// Exact reconnect backoff ladder (`sync_gate.zig:13-37`): `[1, 2, 4, 8, 15, 30]s`.
pub const HUB_RECONNECT_BACKOFF_MS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
pub const HUB_WAKE_GAP_MS: u64 = 60_000;
pub const HUB_WAKE_GAP_SECS: i64 = 60;
pub const HUB_INBOX_SAFETY_MS: u64 = 60_000;

pub fn hub_reconnect_wait_ms(failures: u32) -> u64 {
    let idx = (failures as usize).min(HUB_RECONNECT_BACKOFF_MS.len() - 1);
    HUB_RECONNECT_BACKOFF_MS[idx]
}

/// Checks BOTH monotonic elapsed milliseconds and wall-clock elapsed seconds so OS sleep/hibernate
/// triggers an immediate wake/safety refresh even if monotonic timers paused (`sync_gate.zig:17-32`).
pub fn should_run_safety_poll(mono_elapsed_ms: u64, wall_elapsed_secs: i64) -> bool {
    mono_elapsed_ms >= HUB_INBOX_SAFETY_MS
        || mono_elapsed_ms >= HUB_WAKE_GAP_MS
        || wall_elapsed_secs >= HUB_WAKE_GAP_SECS
}

/// Tracks SSE reconnect backoff state (`main.zig:25106-25109`):
/// - Clean EOF resets failure counter to `0` and waits `1_000 ms`.
/// - Connect/stream/parse error uses current failure count then increments.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SseReconnectState {
    failures: u32,
}

impl SseReconnectState {
    pub fn new() -> Self {
        Self { failures: 0 }
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn on_connected(&mut self) {
        self.failures = 0;
    }

    pub fn on_clean_eof(&mut self) -> u64 {
        self.failures = 0;
        hub_reconnect_wait_ms(0)
    }

    pub fn on_error(&mut self) -> u64 {
        let wait = hub_reconnect_wait_ms(self.failures);
        self.failures = self.failures.saturating_add(1);
        wait
    }
}
