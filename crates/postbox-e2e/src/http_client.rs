#![forbid(unsafe_code)]

use crate::wire::{
    DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_HEADER_BYTES, WireError, WsFrame, WsOpcode, base64_encode,
    read_chunked_body, read_ws_frame, write_ws_frame, ws_accept_key,
};
use postbox_hub::{Endpoint, HubRequest, SseEvent};
use serde::de::DeserializeOwned;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};
use thiserror::Error;

pub const SSE_MAX_FRAME_BYTES: usize = 65_536;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("non-loopback endpoint rejected: {0}")]
    NonLoopbackEndpoint(String),
    #[error("malformed http response: {0}")]
    BadHttpResponse(String),
    #[error("websocket handshake failed with status {0}")]
    WsHandshakeFailed(u16),
    #[error("websocket accept key mismatch")]
    WsAcceptMismatch,
    #[error("wire error: {0}")]
    Wire(#[from] WireError),
}

#[derive(Debug, Error)]
pub enum SseParseError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("sse event exceeded 64 KiB cap")]
    EventTooLarge,
    #[error("sse wire id does not match json id")]
    BadEventId,
    #[error("invalid sse json: {0}")]
    InvalidJson(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSseFrame {
    Event(SseEvent),
    Heartbeat,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &needle)
            .map(|(_, v)| v.as_str())
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct LoopbackHttpClient {
    timeout: Duration,
    user_agent: String,
}

impl Default for LoopbackHttpClient {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(5),
            user_agent: "Postbox/1.0".to_string(),
        }
    }
}

impl LoopbackHttpClient {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }

    fn loopback_addr(endpoint: &Endpoint) -> Result<SocketAddr, ClientError> {
        if endpoint.host != "127.0.0.1" {
            return Err(ClientError::NonLoopbackEndpoint(endpoint.host.clone()));
        }
        Ok(SocketAddr::from(([127, 0, 0, 1], endpoint.port)))
    }

    pub fn execute_hub(
        &self,
        endpoint: &Endpoint,
        token: &str,
        req: &HubRequest,
    ) -> Result<HttpResponse, ClientError> {
        let addr = Self::loopback_addr(endpoint)?;
        let auth = format!("Bearer {token}");
        let mut headers: Vec<(&str, &str)> = vec![
            ("User-Agent", self.user_agent.as_str()),
            ("Authorization", auth.as_str()),
            ("Accept", "*/*"),
        ];
        if let Some(ref last_id) = req.last_event_id {
            headers.push(("Last-Event-ID", last_id.as_str()));
        }
        if req.body.is_some() {
            headers.push(("Content-Type", "application/json; charset=utf-8"));
        }
        self.execute_raw(
            addr,
            req.method,
            &req.path,
            &headers,
            req.body.as_deref().map(str::as_bytes),
        )
    }

    pub fn execute_hub_multipart(
        &self,
        endpoint: &Endpoint,
        token: &str,
        path: &str,
        boundary: &str,
        multipart_body: &[u8],
    ) -> Result<HttpResponse, ClientError> {
        let addr = Self::loopback_addr(endpoint)?;
        let auth = format!("Bearer {token}");
        let ct = format!("multipart/form-data; boundary={boundary}");
        let headers = [
            ("User-Agent", self.user_agent.as_str()),
            ("Authorization", auth.as_str()),
            ("Content-Type", ct.as_str()),
            ("Accept", "*/*"),
        ];
        self.execute_raw(addr, "POST", path, &headers, Some(multipart_body))
    }

    pub fn execute_raw(
        &self,
        addr: SocketAddr,
        method: &str,
        path_and_query: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<HttpResponse, ClientError> {
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let body_bytes = body.unwrap_or(&[]);
        write!(
            stream,
            "{method} {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n",
            addr.port()
        )?;
        let mut has_conn = false;
        let mut has_len = false;
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("connection") {
                has_conn = true;
            }
            if k.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
            write!(stream, "{k}: {v}\r\n")?;
        }
        if !has_len && (!body_bytes.is_empty() || method == "POST" || method == "PUT") {
            write!(stream, "Content-Length: {}\r\n", body_bytes.len())?;
        }
        if !has_conn {
            write!(stream, "Connection: close\r\n")?;
        }
        write!(stream, "\r\n")?;
        if !body_bytes.is_empty() {
            stream.write_all(body_bytes)?;
        }
        stream.flush()?;

        read_http_response(&mut stream, method.eq_ignore_ascii_case("HEAD"))
    }

    pub fn open_sse(
        &self,
        endpoint: &Endpoint,
        token: &str,
        req: &HubRequest,
    ) -> Result<SseClientStream, ClientError> {
        let addr = Self::loopback_addr(endpoint)?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        write!(
            stream,
            "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUser-Agent: {}\r\nAuthorization: Bearer {}\r\nAccept: text/event-stream\r\n",
            req.method,
            req.path,
            addr.port(),
            self.user_agent,
            token
        )?;
        if let Some(ref last_id) = req.last_event_id {
            write!(stream, "Last-Event-ID: {last_id}\r\n")?;
        }
        write!(stream, "\r\n")?;
        stream.flush()?;

        let (status, headers, leftover) = read_response_head(&mut stream)?;
        if status != 200 {
            return Err(ClientError::BadHttpResponse(format!(
                "expected 200 for SSE, got {status}"
            )));
        }
        let is_sse = headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .is_some_and(|(_, v)| v.to_ascii_lowercase().starts_with("text/event-stream"));
        if !is_sse {
            return Err(ClientError::BadHttpResponse(
                "missing text/event-stream content-type".to_string(),
            ));
        }

        Ok(SseClientStream {
            stream,
            buffer: leftover,
        })
    }

    pub fn connect_ws(
        &self,
        addr: SocketAddr,
        path_and_query: &str,
        headers: &[(&str, &str)],
    ) -> Result<WsClientConn, ClientError> {
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout)?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;

        let client_key = base64_encode(b"postbox-e2e-key!");
        write!(
            stream,
            "GET {path_and_query} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {client_key}\r\nSec-WebSocket-Version: 13\r\n",
            addr.port()
        )?;
        for (k, v) in headers {
            write!(stream, "{k}: {v}\r\n")?;
        }
        write!(stream, "\r\n")?;
        stream.flush()?;

        let (status, resp_headers, leftover) = read_response_head(&mut stream)?;
        if status != 101 {
            return Err(ClientError::WsHandshakeFailed(status));
        }
        let expected_accept = ws_accept_key(&client_key);
        let got_accept = resp_headers
            .iter()
            .find(|(k, _)| k == "sec-websocket-accept")
            .map(|(_, v)| v.as_str());
        if got_accept != Some(expected_accept.as_str()) {
            return Err(ClientError::WsAcceptMismatch);
        }

        Ok(WsClientConn {
            stream,
            leftover,
            leftover_pos: 0,
            mask_seq: 1,
        })
    }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

type ResponseHead = (u16, Vec<(String, String)>, Vec<u8>);

fn read_response_head(stream: &mut TcpStream) -> Result<ResponseHead, ClientError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_header_end(&buf) {
            if pos > DEFAULT_MAX_HEADER_BYTES {
                return Err(ClientError::BadHttpResponse(
                    "response headers too large".to_string(),
                ));
            }
            break pos;
        }
        if buf.len() > DEFAULT_MAX_HEADER_BYTES + 3 {
            return Err(ClientError::BadHttpResponse(
                "response headers too large".to_string(),
            ));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(ClientError::BadHttpResponse(
                "unexpected eof reading headers".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_str = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| ClientError::BadHttpResponse("non-utf8 headers".to_string()))?;
    let leftover = buf[header_end + 4..].to_vec();

    let mut lines = header_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| ClientError::BadHttpResponse("empty status line".to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ClientError::BadHttpResponse(format!("bad status line: {status_line}")))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    Ok((status, headers, leftover))
}

fn read_http_response(
    stream: &mut TcpStream,
    no_body_method: bool,
) -> Result<HttpResponse, ClientError> {
    let (status, headers, mut body) = read_response_head(stream)?;
    let no_body_status = (100..=199).contains(&status) || status == 204 || status == 304;
    if no_body_method || no_body_status {
        body.clear();
        return Ok(HttpResponse {
            status,
            headers,
            body,
        });
    }

    let is_chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));

    if is_chunked {
        body = read_chunked_body(stream, body, DEFAULT_MAX_BODY_BYTES)?;
    } else if let Some((_, len_str)) = headers.iter().find(|(k, _)| k == "content-length") {
        let expected: usize = len_str
            .parse()
            .map_err(|_| ClientError::BadHttpResponse("invalid content-length".to_string()))?;
        if expected > DEFAULT_MAX_BODY_BYTES {
            return Err(ClientError::Wire(WireError::BodyTooLarge(expected)));
        }
        let mut chunk = [0u8; 4096];
        while body.len() < expected {
            let remaining = expected - body.len();
            let to_read = remaining.min(chunk.len());
            let n = stream.read(&mut chunk[..to_read])?;
            if n == 0 {
                return Err(ClientError::BadHttpResponse(format!(
                    "truncated response body: expected {expected} bytes, got {}",
                    body.len()
                )));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(expected);
    } else {
        let max_rest = DEFAULT_MAX_BODY_BYTES.saturating_sub(body.len());
        let mut rest = Vec::new();
        Read::by_ref(stream)
            .take(max_rest.saturating_add(1) as u64)
            .read_to_end(&mut rest)?;
        if rest.len() > max_rest {
            return Err(ClientError::Wire(WireError::BodyTooLarge(
                body.len().saturating_add(rest.len()),
            )));
        }
        body.extend_from_slice(&rest);
    }

    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

pub struct SseClientStream {
    stream: TcpStream,
    buffer: Vec<u8>,
}

impl SseClientStream {
    pub fn next_frame(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ParsedSseFrame>, SseParseError> {
        let deadline = Instant::now() + timeout;
        let mut tmp = [0u8; 4096];

        loop {
            if let Some((frame_end, sep_len)) = find_sse_delimiter(&self.buffer) {
                let raw_frame = self.buffer[..frame_end].to_vec();
                self.buffer.drain(..frame_end + sep_len);
                if raw_frame.len() > SSE_MAX_FRAME_BYTES {
                    return Err(SseParseError::EventTooLarge);
                }
                if let Some(parsed) = parse_sse_frame(&raw_frame)? {
                    return Ok(Some(parsed));
                }
                continue;
            }

            if self.buffer.len() > SSE_MAX_FRAME_BYTES + 4 {
                return Err(SseParseError::EventTooLarge);
            }

            let now = Instant::now();
            if now >= deadline {
                return Err(SseParseError::Io(io::Error::new(
                    ErrorKind::TimedOut,
                    "sse frame timed out",
                )));
            }
            let remaining = deadline - now;
            self.stream
                .set_read_timeout(Some(remaining.min(Duration::from_millis(100))))?;
            match self.stream.read(&mut tmp) {
                Ok(0) => {
                    if self.buffer.len() > SSE_MAX_FRAME_BYTES {
                        return Err(SseParseError::EventTooLarge);
                    }
                    return Ok(None);
                }
                Ok(n) => self.buffer.extend_from_slice(&tmp[..n]),
                Err(ref e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(e) => return Err(SseParseError::Io(e)),
            }
        }
    }
}

fn find_sse_delimiter(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, 4));
        }
        i += 1;
    }
    None
}

fn parse_sse_frame(raw: &[u8]) -> Result<Option<ParsedSseFrame>, SseParseError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| SseParseError::InvalidJson("non-utf8 sse frame".to_string()))?;
    let mut wire_id: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_comment = false;

    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            saw_comment = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("id:") {
            wire_id = Some(rest.trim_start_matches(' ').to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start_matches(' '));
        }
    }

    if data_lines.is_empty() {
        if saw_comment {
            return Ok(Some(ParsedSseFrame::Heartbeat));
        }
        return Ok(None);
    }

    let joined = data_lines.join("\n");
    let event: SseEvent =
        serde_json::from_str(&joined).map_err(|e| SseParseError::InvalidJson(e.to_string()))?;

    if let Some(id_str) = wire_id {
        if id_str.is_empty()
            || id_str.len() > 32
            || !id_str.bytes().all(|b| b.is_ascii_digit())
            || id_str.parse::<u64>().ok() != Some(event.id)
        {
            return Err(SseParseError::BadEventId);
        }
    } else {
        return Err(SseParseError::BadEventId);
    }

    Ok(Some(ParsedSseFrame::Event(event)))
}

pub struct WsClientConn {
    stream: TcpStream,
    leftover: Vec<u8>,
    leftover_pos: usize,
    mask_seq: u32,
}

impl Read for WsClientConn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.leftover_pos < self.leftover.len() {
            let avail = &self.leftover[self.leftover_pos..];
            let n = avail.len().min(buf.len());
            buf[..n].copy_from_slice(&avail[..n]);
            self.leftover_pos += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl WsClientConn {
    fn next_mask(&mut self) -> [u8; 4] {
        self.mask_seq = self
            .mask_seq
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        self.mask_seq.to_be_bytes()
    }

    pub fn send_binary(&mut self, payload: &[u8]) -> io::Result<()> {
        let mask = self.next_mask();
        let frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Binary,
            masked: true,
            payload: payload.to_vec(),
        };
        write_ws_frame(&mut self.stream, &frame, Some(mask))
    }

    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        let mask = self.next_mask();
        let frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Text,
            masked: true,
            payload: text.as_bytes().to_vec(),
        };
        write_ws_frame(&mut self.stream, &frame, Some(mask))
    }

    pub fn send_close(&mut self, code: u16) -> io::Result<()> {
        let mask = self.next_mask();
        let frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Close,
            masked: true,
            payload: code.to_be_bytes().to_vec(),
        };
        write_ws_frame(&mut self.stream, &frame, Some(mask))
    }

    pub fn recv_frame(&mut self, timeout: Duration) -> Result<WsFrame, WireError> {
        self.stream.set_read_timeout(Some(timeout))?;
        read_ws_frame(self, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_hub::events_request;
    use std::net::TcpListener;
    use std::thread;

    fn spawn_one_shot_server(responder: impl FnOnce(TcpStream) + Send + 'static) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral listener");
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut req_buf = Vec::with_capacity(512);
                let mut tmp = [0u8; 256];
                while find_header_end(&req_buf).is_none() && req_buf.len() < 8192 {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req_buf.extend_from_slice(&tmp[..n]),
                    }
                }
                responder(stream);
            }
        });
        addr
    }

    #[test]
    fn read_http_response_rejects_truncated_content_length_and_decodes_chunked() {
        let client = LoopbackHttpClient::new(Duration::from_millis(500));

        // (a) Truncated Content-Length body must return BadHttpResponse
        let addr_trunc = spawn_one_shot_server(|mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n12345");
            let _ = s.flush();
        });
        let err = client
            .execute_raw(addr_trunc, "GET", "/trunc", &[], None)
            .expect_err("truncated body rejected");
        assert!(matches!(err, ClientError::BadHttpResponse(_)));

        // (b) Valid chunked body decodes cleanly
        let addr_chunked = spawn_one_shot_server(|mut s| {
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
            );
            let _ = s.flush();
        });
        let resp = client
            .execute_raw(addr_chunked, "GET", "/chunked", &[], None)
            .expect("chunked decoded");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"hello world");

        // (c) Malformed chunked body fails with Wire(BadRequest)
        let addr_bad_chunk = spawn_one_shot_server(|mut s| {
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n",
            );
            let _ = s.flush();
        });
        let bad_err = client
            .execute_raw(addr_bad_chunk, "GET", "/bad-chunk", &[], None)
            .expect_err("malformed chunked rejected");
        assert!(matches!(
            bad_err,
            ClientError::Wire(WireError::BadRequest(_))
        ));

        // (d) Close-delimited stall past 100 ms timeout returns Io error (never Ok(empty))
        let short_client = LoopbackHttpClient::new(Duration::from_millis(100));
        let addr_stall = spawn_one_shot_server(|mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n");
            let _ = s.flush();
            thread::sleep(Duration::from_millis(350));
        });
        let stall_err = short_client
            .execute_raw(addr_stall, "GET", "/stall", &[], None)
            .expect_err("stalled close-delimited body errors");
        assert!(matches!(stall_err, ClientError::Io(_)));
    }

    fn build_exact_sse_frame(target_len: usize, id: u64) -> Vec<u8> {
        let prefix = format!("id: {id}\ndata: {{\"id\":{id},\"kind\":\"message\",\"chat\":\"");
        let suffix = "\"}";
        let pad_len = target_len
            .checked_sub(prefix.len() + suffix.len())
            .expect("target_len large enough");
        let mut frame = Vec::with_capacity(target_len);
        frame.extend_from_slice(prefix.as_bytes());
        frame.resize(prefix.len() + pad_len, b'x');
        frame.extend_from_slice(suffix.as_bytes());
        assert_eq!(frame.len(), target_len);
        frame
    }

    #[test]
    fn sse_client_stream_accepts_exact_64kib_split_delimiter_and_rejects_over_cap() {
        let client = LoopbackHttpClient::new(Duration::from_secs(2));

        // (a) Exact 65,536 B SSE event + `\n`, sleep 25 ms, then final `\n`
        let frame_a = build_exact_sse_frame(SSE_MAX_FRAME_BYTES, 1);
        let addr_a = spawn_one_shot_server(move |mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
            let _ = s.write_all(&frame_a);
            let _ = s.write_all(b"\n");
            let _ = s.flush();
            thread::sleep(Duration::from_millis(25));
            let _ = s.write_all(b"\n");
            let _ = s.flush();
        });
        let ep_a = Endpoint::parse(&format!("http://127.0.0.1:{}", addr_a.port()), true).unwrap();
        let mut sse_a = client
            .open_sse(&ep_a, "tok", &events_request(None))
            .unwrap();
        let parsed_a = sse_a
            .next_frame(Duration::from_secs(1))
            .expect("exact 64 KiB + split \\n\\n succeeds")
            .expect("event present");
        assert!(matches!(parsed_a, ParsedSseFrame::Event(ev) if ev.id == 1));

        // (b) Exact 65,536 B SSE event + `\r\n\r`, sleep 25 ms, then final `\n`
        let frame_b = build_exact_sse_frame(SSE_MAX_FRAME_BYTES, 2);
        let addr_b = spawn_one_shot_server(move |mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
            let _ = s.write_all(&frame_b);
            let _ = s.write_all(b"\r\n\r");
            let _ = s.flush();
            thread::sleep(Duration::from_millis(25));
            let _ = s.write_all(b"\n");
            let _ = s.flush();
        });
        let ep_b = Endpoint::parse(&format!("http://127.0.0.1:{}", addr_b.port()), true).unwrap();
        let mut sse_b = client
            .open_sse(&ep_b, "tok", &events_request(None))
            .unwrap();
        let parsed_b = sse_b
            .next_frame(Duration::from_secs(1))
            .expect("exact 64 KiB + split \\r\\n\\r\\n succeeds")
            .expect("event present");
        assert!(matches!(parsed_b, ParsedSseFrame::Event(ev) if ev.id == 2));

        // (c) 65,537 B SSE event + `\n\n` must fail with EventTooLarge
        let frame_c = build_exact_sse_frame(SSE_MAX_FRAME_BYTES + 1, 3);
        let addr_c = spawn_one_shot_server(move |mut s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");
            let _ = s.write_all(&frame_c);
            let _ = s.write_all(b"\n\n");
            let _ = s.flush();
        });
        let ep_c = Endpoint::parse(&format!("http://127.0.0.1:{}", addr_c.port()), true).unwrap();
        let mut sse_c = client
            .open_sse(&ep_c, "tok", &events_request(None))
            .unwrap();
        let err_c = sse_c
            .next_frame(Duration::from_secs(1))
            .expect_err("65,537 B SSE event rejected");
        assert!(matches!(err_c, SseParseError::EventTooLarge));
    }
}
