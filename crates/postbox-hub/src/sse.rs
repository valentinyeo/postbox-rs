use crate::error::HubError;
use crate::types::SseEvent;

pub const MAX_SSE_FRAME_BYTES: usize = 64 * 1024;

/// Incremental SSE `text/event-stream` parser matching `wa_hub.zig:280-326`:
/// - `64 KiB` max frame buffer (clears buffer and errors `EventTooLarge` if exceeded)
/// - Strips `\r` bytes
/// - Splits frames on `\n\n`
/// - Strictly verifies `id:` header equals JSON `event.id` (`1..=32` digits);
///   NEVER advances `last_event_id` on mismatch!
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    last_event_id: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn last_event_id(&self) -> Option<&str> {
        self.last_event_id.as_deref()
    }

    pub fn last_event_id_u64(&self) -> Option<u64> {
        self.last_event_id.as_deref().and_then(|s| s.parse().ok())
    }

    pub fn clear_buffer(&mut self) {
        self.buf.clear();
    }

    /// Feeds incoming stream bytes and yields parsed & validated `SseEvent`s.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, HubError> {
        let mut events = Vec::new();
        for &byte in chunk {
            if byte == b'\r' {
                continue;
            }
            if self.buf.len() >= MAX_SSE_FRAME_BYTES {
                self.buf.clear();
                return Err(HubError::EventTooLarge);
            }
            self.buf.push(byte);

            if self.buf.ends_with(b"\n\n") {
                let frame = std::mem::take(&mut self.buf);
                if let Some(ev) = self.parse_frame(&frame)? {
                    events.push(ev);
                }
            }
        }
        Ok(events)
    }

    fn parse_frame(&mut self, frame: &[u8]) -> Result<Option<SseEvent>, HubError> {
        let text = std::str::from_utf8(frame).map_err(|_| HubError::BadResponse)?;
        let mut id: Option<&str> = None;
        let mut data = String::new();

        for line in text.split('\n') {
            if let Some(rest) = line.strip_prefix("id:") {
                id = Some(rest.trim_matches([' ', '\r']));
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }

        if data.is_empty() {
            return Ok(None);
        }

        let event: SseEvent = serde_json::from_str(&data).map_err(|_| HubError::BadResponse)?;
        let raw_id = id.ok_or(HubError::BadEventId)?;
        if raw_id.is_empty() || raw_id.len() > 32 || !raw_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HubError::BadEventId);
        }
        let parsed_id: u64 = raw_id.parse().map_err(|_| HubError::BadEventId)?;
        if parsed_id != event.id {
            return Err(HubError::BadEventId);
        }

        // Only advance `last_event_id` AFTER full validation passes.
        self.last_event_id = Some(raw_id.to_owned());
        Ok(Some(event))
    }
}
