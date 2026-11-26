#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;

pub const DEFAULT_MAX_HEADER_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_BODY_BYTES: usize = 70 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WireError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("bad http request: {0}")]
    BadRequest(String),
    #[error("http body exceeds limit ({0} bytes)")]
    BodyTooLarge(usize),
    #[error("invalid percent encoding")]
    InvalidEncoding,
    #[error("invalid multipart payload: {0}")]
    InvalidMultipart(String),
    #[error("websocket protocol error: {0}")]
    WsProtocol(String),
    #[error("invalid zip archive: {0}")]
    InvalidZip(String),
    #[error("unexpected eof")]
    UnexpectedEof,
}

#[derive(Debug, Clone)]
pub struct RawHttpRequest {
    pub method: String,
    pub target: String,
    pub path: String,
    pub query_raw: String,
    pub query_params: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawHttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        let needle = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k == &needle)
            .map(|(_, v)| v.as_str())
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query_params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn query_all(&self, key: &str) -> Vec<&str> {
        self.query_params
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct RawHttpResponse {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawHttpResponse {
    pub fn reason_for_status(status: u16) -> &'static str {
        match status {
            101 => "Switching Protocols",
            200 => "OK",
            202 => "Accepted",
            204 => "No Content",
            206 => "Partial Content",
            302 => "Found",
            400 => "Bad Request",
            401 => "Unauthorized",
            402 => "Payment Required",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            410 => "Gone",
            413 => "Payload Too Large",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        Self {
            status,
            reason: Self::reason_for_status(status),
            headers: vec![(
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body,
        }
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            reason: Self::reason_for_status(status),
            headers: vec![("Content-Type".to_string(), content_type.to_string())],
            body,
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::bytes(
            status,
            "text/plain; charset=utf-8",
            body.into().into_bytes(),
        )
    }

    pub fn empty(status: u16) -> Self {
        Self {
            status,
            reason: Self::reason_for_status(status),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn write_to(&self, writer: &mut impl Write) -> io::Result<()> {
        write!(writer, "HTTP/1.1 {} {}\r\n", self.status, self.reason)?;
        let mut has_len = false;
        let mut has_conn = false;
        for (k, v) in &self.headers {
            if k.eq_ignore_ascii_case("content-length") {
                has_len = true;
            }
            if k.eq_ignore_ascii_case("connection") {
                has_conn = true;
            }
            write!(writer, "{k}: {v}\r\n")?;
        }
        if !has_len {
            write!(writer, "Content-Length: {}\r\n", self.body.len())?;
        }
        if !has_conn {
            write!(writer, "Connection: close\r\n")?;
        }
        write!(writer, "\r\n")?;
        if !self.body.is_empty() {
            writer.write_all(&self.body)?;
        }
        writer.flush()
    }
}

pub fn percent_decode(input: &str) -> Result<String, WireError> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(WireError::InvalidEncoding);
            }
            let hi = from_hex(bytes[i + 1]).ok_or(WireError::InvalidEncoding)?;
            let lo = from_hex(bytes[i + 2]).ok_or(WireError::InvalidEncoding)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| WireError::InvalidEncoding)
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn parse_query_params(query_raw: &str) -> Result<Vec<(String, String)>, WireError> {
    if query_raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut params = Vec::new();
    for pair in query_raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_k, raw_v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        params.push((percent_decode(raw_k)?, percent_decode(raw_v)?));
    }
    Ok(params)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub fn read_http_request(
    stream: &mut impl Read,
    max_body_bytes: usize,
) -> Result<RawHttpRequest, WireError> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            if pos > DEFAULT_MAX_HEADER_BYTES {
                return Err(WireError::BadRequest("headers too large".to_string()));
            }
            break pos;
        }
        if buf.len() > DEFAULT_MAX_HEADER_BYTES + 3 {
            return Err(WireError::BadRequest("headers too large".to_string()));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(WireError::UnexpectedEof);
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_bytes = &buf[..header_end];
    let leftover = buf[header_end + 4..].to_vec();
    let header_str = std::str::from_utf8(header_bytes)
        .map_err(|_| WireError::BadRequest("non-utf8 headers".to_string()))?;

    let mut lines = header_str.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| WireError::BadRequest("missing request line".to_string()))?;
    let mut req_parts = request_line.split_whitespace();
    let method = req_parts
        .next()
        .ok_or_else(|| WireError::BadRequest("missing method".to_string()))?
        .to_string();
    let target = req_parts
        .next()
        .ok_or_else(|| WireError::BadRequest("missing target".to_string()))?
        .to_string();

    let (path, query_raw) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };
    let query_params = parse_query_params(&query_raw)?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| WireError::BadRequest(format!("malformed header: {line}")))?;
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }

    let is_chunked = headers
        .iter()
        .find(|(k, _)| k == "transfer-encoding")
        .is_some_and(|(_, v)| v.to_ascii_lowercase().contains("chunked"));

    let body = if is_chunked {
        read_chunked_body(stream, leftover, max_body_bytes)?
    } else if let Some((_, len_str)) = headers.iter().find(|(k, _)| k == "content-length") {
        let expected_len: usize = len_str
            .parse()
            .map_err(|_| WireError::BadRequest("invalid content-length".to_string()))?;
        if expected_len > max_body_bytes {
            return Err(WireError::BodyTooLarge(expected_len));
        }
        let mut body = leftover;
        if body.len() > expected_len {
            body.truncate(expected_len);
        }
        while body.len() < expected_len {
            let remaining = expected_len - body.len();
            let to_read = remaining.min(chunk.len());
            let n = stream.read(&mut chunk[..to_read])?;
            if n == 0 {
                return Err(WireError::UnexpectedEof);
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body
    } else {
        Vec::new()
    };

    Ok(RawHttpRequest {
        method,
        target,
        path,
        query_raw,
        query_params,
        headers,
        body,
    })
}

pub const MAX_CHUNK_LINE_BYTES: usize = 8 * 1024;

pub fn read_chunked_body(
    stream: &mut impl Read,
    initial: Vec<u8>,
    max_body_bytes: usize,
) -> Result<Vec<u8>, WireError> {
    let mut raw = initial;
    let mut out = Vec::new();
    let mut cursor = 0usize;
    let mut tmp = [0u8; 4096];

    loop {
        let crlf_pos = loop {
            if let Some(rel) = find_subslice(&raw[cursor..], b"\r\n") {
                if rel > MAX_CHUNK_LINE_BYTES {
                    return Err(WireError::BadRequest(
                        "chunk size line too large".to_string(),
                    ));
                }
                break cursor + rel;
            }
            if raw.len().saturating_sub(cursor) > MAX_CHUNK_LINE_BYTES + 1 {
                return Err(WireError::BadRequest(
                    "chunk size line too large".to_string(),
                ));
            }
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(WireError::UnexpectedEof);
            }
            raw.extend_from_slice(&tmp[..n]);
        };

        let size_line = std::str::from_utf8(&raw[cursor..crlf_pos])
            .map_err(|_| WireError::BadRequest("invalid chunk size".to_string()))?;
        let size_hex = size_line.split(';').next().unwrap_or("").trim();
        if size_hex.is_empty() {
            return Err(WireError::BadRequest("empty chunk size".to_string()));
        }
        let chunk_size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| WireError::BadRequest("bad hex chunk size".to_string()))?;
        cursor = crlf_pos + 2;

        if chunk_size == 0 {
            // RFC 9112 §7.1: consume optional trailer lines and terminating empty CRLF.
            let mut trailer_bytes = 0usize;
            loop {
                let trailer_crlf = loop {
                    if let Some(rel) = find_subslice(&raw[cursor..], b"\r\n") {
                        break cursor + rel;
                    }
                    if trailer_bytes.saturating_add(raw.len().saturating_sub(cursor))
                        > DEFAULT_MAX_HEADER_BYTES
                    {
                        return Err(WireError::BadRequest(
                            "chunked trailers too large".to_string(),
                        ));
                    }
                    let n = stream.read(&mut tmp)?;
                    if n == 0 {
                        return Err(WireError::UnexpectedEof);
                    }
                    raw.extend_from_slice(&tmp[..n]);
                };
                let line_len = trailer_crlf - cursor;
                cursor = trailer_crlf + 2;
                if line_len == 0 {
                    break;
                }
                trailer_bytes = trailer_bytes.saturating_add(line_len + 2);
                if trailer_bytes > DEFAULT_MAX_HEADER_BYTES {
                    return Err(WireError::BadRequest(
                        "chunked trailers too large".to_string(),
                    ));
                }
            }
            break;
        }
        if out.len().saturating_add(chunk_size) > max_body_bytes {
            return Err(WireError::BodyTooLarge(
                out.len().saturating_add(chunk_size),
            ));
        }

        let needed = cursor
            .checked_add(chunk_size)
            .and_then(|v| v.checked_add(2))
            .ok_or(WireError::BodyTooLarge(usize::MAX))?;
        while raw.len() < needed {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                return Err(WireError::UnexpectedEof);
            }
            raw.extend_from_slice(&tmp[..n]);
        }
        let data_end = cursor + chunk_size;
        if &raw[data_end..data_end + 2] != b"\r\n" {
            return Err(WireError::BadRequest(
                "missing CRLF after chunk payload".to_string(),
            ));
        }
        out.extend_from_slice(&raw[cursor..data_end]);
        cursor = data_end + 2;

        if cursor >= 64 * 1024 {
            raw.drain(..cursor);
            cursor = 0;
        }
    }

    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartPart {
    pub name: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

pub fn parse_multipart(content_type: &str, body: &[u8]) -> Result<Vec<MultipartPart>, WireError> {
    let boundary = content_type
        .split(';')
        .skip(1)
        .find_map(|param| {
            let trimmed = param.trim();
            let (k, v) = trimmed.split_once('=')?;
            if k.trim().eq_ignore_ascii_case("boundary") {
                Some(v.trim().trim_matches('"').to_string())
            } else {
                None
            }
        })
        .ok_or_else(|| WireError::InvalidMultipart("missing boundary".to_string()))?;

    if boundary.is_empty() || boundary.len() > 80 {
        return Err(WireError::InvalidMultipart(
            "boundary out of range".to_string(),
        ));
    }

    let delimiter = format!("--{boundary}").into_bytes();
    let mut parts = Vec::new();

    let mut pos = find_subslice(body, &delimiter)
        .ok_or_else(|| WireError::InvalidMultipart("starting boundary not found".to_string()))?;

    loop {
        pos += delimiter.len();
        if body.get(pos..pos + 2) == Some(b"--") {
            break;
        }
        if body.get(pos..pos + 2) == Some(b"\r\n") {
            pos += 2;
        } else {
            return Err(WireError::InvalidMultipart(
                "missing CRLF after boundary".to_string(),
            ));
        }

        let header_rel = find_subslice(&body[pos..], b"\r\n\r\n")
            .ok_or_else(|| WireError::InvalidMultipart("missing part header end".to_string()))?;
        let header_str = std::str::from_utf8(&body[pos..pos + header_rel])
            .map_err(|_| WireError::InvalidMultipart("non-utf8 part headers".to_string()))?;
        let data_start = pos + header_rel + 4;

        let next_delim_rel = find_subslice(&body[data_start..], &delimiter)
            .ok_or_else(|| WireError::InvalidMultipart("missing closing boundary".to_string()))?;
        let next_delim = data_start + next_delim_rel;
        let data_end =
            if next_delim >= data_start + 2 && &body[next_delim - 2..next_delim] == b"\r\n" {
                next_delim - 2
            } else {
                next_delim
            };

        let mut name = None;
        let mut filename = None;
        let mut part_ct = None;

        for line in header_str.split("\r\n") {
            if let Some((k, v)) = line.split_once(':') {
                let k_low = k.trim().to_ascii_lowercase();
                let v_trim = v.trim();
                if k_low == "content-disposition" {
                    for seg in v_trim.split(';') {
                        let s = seg.trim();
                        if let Some(val) = s.strip_prefix("name=") {
                            name = Some(val.trim_matches('"').to_string());
                        } else if let Some(val) = s.strip_prefix("filename=") {
                            filename = Some(val.trim_matches('"').to_string());
                        }
                    }
                } else if k_low == "content-type" {
                    part_ct = Some(v_trim.to_string());
                }
            }
        }

        let part_name =
            name.ok_or_else(|| WireError::InvalidMultipart("missing part name".to_string()))?;
        parts.push(MultipartPart {
            name: part_name,
            filename,
            content_type: part_ct,
            body: body[data_start..data_end].to_vec(),
        });

        pos = next_delim;
    }

    Ok(parts)
}

pub fn build_multipart(boundary: &str, parts: &[MultipartPart]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match &part.filename {
            Some(fname) => {
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                        part.name, fname
                    )
                    .as_bytes(),
                );
            }
            None => {
                out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name)
                        .as_bytes(),
                );
            }
        }
        if let Some(ct) = &part.content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&part.body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

/// Pure-Rust RFC 3174 SHA-1 used exclusively for RFC 6455 `Sec-WebSocket-Accept`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let off = i * 4;
            *word =
                u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = h0;
        let mut b = h1;
        let mut c = h2;
        let mut d = h3;
        let mut e = h4;

        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC_u32),
                _ => (b ^ c ^ d, 0xCA62_C1D6_u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// Pure-Rust FIPS 180-4 SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for block in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let off = i * 4;
            *word =
                u32::from_be_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = sha256(data);
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

pub fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= data.len() {
        let n = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
        i += 3;
    }
    match data.len() - i {
        1 => {
            let n = u32::from(data[i]) << 16;
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8);
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Non-reflected CRC-32 with polynomial 0x04C11DB7 used by Ogg page framing.
pub fn crc32_ogg(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= u32::from(b) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04C1_1DB7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Builds a valid PKZIP 2.0 STORE (compression method 0) archive.
pub fn build_store_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central_dir = Vec::new();

    for &(name, data) in entries {
        let local_offset = out.len() as u32;
        let crc = crc32_ieee(data);
        let size = data.len() as u32;
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u16;

        // Local file header
        out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method: STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        // Central directory header
        central_dir.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // flags
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // method: STORE
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // mod date
        central_dir.extend_from_slice(&crc.to_le_bytes());
        central_dir.extend_from_slice(&size.to_le_bytes());
        central_dir.extend_from_slice(&size.to_le_bytes());
        central_dir.extend_from_slice(&name_len.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // disk start
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central_dir.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central_dir.extend_from_slice(&local_offset.to_le_bytes());
        central_dir.extend_from_slice(name_bytes);
    }

    let cd_offset = out.len() as u32;
    let cd_size = central_dir.len() as u32;
    let count = entries.len() as u16;
    out.extend_from_slice(&central_dir);

    // End of central directory record
    out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len

    out
}

/// Parses a STORE ZIP archive and returns `(name, data)` entries after validating
/// local headers (`comp_size == uncomp_size`, CRC32), Central Directory (`0x02014b50`),
/// and End of Central Directory (`0x06054b50`) signatures, offsets, and counts.
pub fn parse_store_zip(zip_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, WireError> {
    if zip_bytes.len() < 22 {
        return Err(WireError::InvalidZip("archive too short".to_string()));
    }

    let mut entries = Vec::new();
    let mut local_meta: Vec<(u32, String, u32, u32)> = Vec::new();
    let mut pos = 0usize;

    while pos + 4 <= zip_bytes.len() {
        let sig = u32::from_le_bytes([
            zip_bytes[pos],
            zip_bytes[pos + 1],
            zip_bytes[pos + 2],
            zip_bytes[pos + 3],
        ]);
        if sig == 0x0201_4b50 || sig == 0x0605_4b50 {
            break;
        }
        if sig != 0x0403_4b50 {
            return Err(WireError::InvalidZip(format!(
                "unexpected signature 0x{sig:08x} at offset {pos}"
            )));
        }
        if pos + 30 > zip_bytes.len() {
            return Err(WireError::InvalidZip("truncated local header".to_string()));
        }
        let method = u16::from_le_bytes([zip_bytes[pos + 8], zip_bytes[pos + 9]]);
        if method != 0 {
            return Err(WireError::InvalidZip(format!(
                "unsupported method {method}"
            )));
        }
        let expected_crc = u32::from_le_bytes([
            zip_bytes[pos + 14],
            zip_bytes[pos + 15],
            zip_bytes[pos + 16],
            zip_bytes[pos + 17],
        ]);
        let comp_size_u32 = u32::from_le_bytes([
            zip_bytes[pos + 18],
            zip_bytes[pos + 19],
            zip_bytes[pos + 20],
            zip_bytes[pos + 21],
        ]);
        let uncomp_size_u32 = u32::from_le_bytes([
            zip_bytes[pos + 22],
            zip_bytes[pos + 23],
            zip_bytes[pos + 24],
            zip_bytes[pos + 25],
        ]);
        if uncomp_size_u32 != comp_size_u32 {
            return Err(WireError::InvalidZip(format!(
                "STORE size mismatch at offset {pos}: comp={comp_size_u32}, uncomp={uncomp_size_u32}"
            )));
        }
        let comp_size = comp_size_u32 as usize;
        let name_len = u16::from_le_bytes([zip_bytes[pos + 26], zip_bytes[pos + 27]]) as usize;
        let extra_len = u16::from_le_bytes([zip_bytes[pos + 28], zip_bytes[pos + 29]]) as usize;
        let name_start = pos + 30;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| WireError::InvalidZip("name length overflow".to_string()))?;
        let data_start = name_end
            .checked_add(extra_len)
            .ok_or_else(|| WireError::InvalidZip("extra length overflow".to_string()))?;
        let data_end = data_start
            .checked_add(comp_size)
            .ok_or_else(|| WireError::InvalidZip("entry size overflow".to_string()))?;
        if data_end > zip_bytes.len() {
            return Err(WireError::InvalidZip("truncated entry data".to_string()));
        }
        let name = std::str::from_utf8(&zip_bytes[name_start..name_end])
            .map_err(|_| WireError::InvalidZip("non-utf8 entry name".to_string()))?
            .to_string();
        let data = zip_bytes[data_start..data_end].to_vec();
        if crc32_ieee(&data) != expected_crc {
            return Err(WireError::InvalidZip(format!("crc mismatch for {name}")));
        }
        let local_offset = u32::try_from(pos)
            .map_err(|_| WireError::InvalidZip("local header offset overflow".to_string()))?;
        local_meta.push((local_offset, name.clone(), expected_crc, comp_size_u32));
        entries.push((name, data));
        pos = data_end;
    }

    // Validate Central Directory (0x02014b50) entries against local headers.
    let cd_start = pos;
    let mut matched = vec![false; local_meta.len()];
    let mut cd_count = 0usize;
    while pos + 4 <= zip_bytes.len() {
        let sig = u32::from_le_bytes([
            zip_bytes[pos],
            zip_bytes[pos + 1],
            zip_bytes[pos + 2],
            zip_bytes[pos + 3],
        ]);
        if sig == 0x0605_4b50 {
            break;
        }
        if sig != 0x0201_4b50 {
            return Err(WireError::InvalidZip(format!(
                "expected central directory signature 0x02014b50 at offset {pos}, got 0x{sig:08x}"
            )));
        }
        if pos + 46 > zip_bytes.len() {
            return Err(WireError::InvalidZip(
                "truncated central directory header".to_string(),
            ));
        }
        let cd_method = u16::from_le_bytes([zip_bytes[pos + 10], zip_bytes[pos + 11]]);
        if cd_method != 0 {
            return Err(WireError::InvalidZip(format!(
                "unsupported central directory method {cd_method}"
            )));
        }
        let cd_crc = u32::from_le_bytes([
            zip_bytes[pos + 16],
            zip_bytes[pos + 17],
            zip_bytes[pos + 18],
            zip_bytes[pos + 19],
        ]);
        let cd_comp = u32::from_le_bytes([
            zip_bytes[pos + 20],
            zip_bytes[pos + 21],
            zip_bytes[pos + 22],
            zip_bytes[pos + 23],
        ]);
        let cd_uncomp = u32::from_le_bytes([
            zip_bytes[pos + 24],
            zip_bytes[pos + 25],
            zip_bytes[pos + 26],
            zip_bytes[pos + 27],
        ]);
        if cd_uncomp != cd_comp {
            return Err(WireError::InvalidZip(format!(
                "central directory STORE size mismatch: comp={cd_comp}, uncomp={cd_uncomp}"
            )));
        }
        let cd_name_len = u16::from_le_bytes([zip_bytes[pos + 28], zip_bytes[pos + 29]]) as usize;
        let cd_extra_len = u16::from_le_bytes([zip_bytes[pos + 30], zip_bytes[pos + 31]]) as usize;
        let cd_comment_len =
            u16::from_le_bytes([zip_bytes[pos + 32], zip_bytes[pos + 33]]) as usize;
        let cd_local_offset = u32::from_le_bytes([
            zip_bytes[pos + 42],
            zip_bytes[pos + 43],
            zip_bytes[pos + 44],
            zip_bytes[pos + 45],
        ]);
        let cd_name_start = pos + 46;
        let cd_name_end = cd_name_start
            .checked_add(cd_name_len)
            .ok_or_else(|| WireError::InvalidZip("cd name length overflow".to_string()))?;
        let cd_next = cd_name_end
            .checked_add(cd_extra_len)
            .and_then(|v| v.checked_add(cd_comment_len))
            .ok_or_else(|| WireError::InvalidZip("cd length overflow".to_string()))?;
        if cd_next > zip_bytes.len() {
            return Err(WireError::InvalidZip(
                "truncated central directory entry".to_string(),
            ));
        }
        let cd_name = std::str::from_utf8(&zip_bytes[cd_name_start..cd_name_end])
            .map_err(|_| WireError::InvalidZip("non-utf8 central directory name".to_string()))?;

        let Some(idx) = local_meta
            .iter()
            .enumerate()
            .position(|(i, (off, _, _, _))| !matched[i] && *off == cd_local_offset)
        else {
            return Err(WireError::InvalidZip(format!(
                "central directory entry {cd_name} has unmatched local offset {cd_local_offset}"
            )));
        };
        let (_, ref local_name, local_crc, local_size) = local_meta[idx];
        if local_name != cd_name || local_crc != cd_crc || local_size != cd_comp {
            return Err(WireError::InvalidZip(format!(
                "central directory metadata mismatch for {cd_name}"
            )));
        }
        matched[idx] = true;
        cd_count += 1;
        pos = cd_next;
    }

    if cd_count != entries.len() {
        return Err(WireError::InvalidZip(format!(
            "central directory entry count mismatch: local={}, cd={cd_count}",
            entries.len()
        )));
    }
    let actual_cd_size = pos - cd_start;

    // Validate End of Central Directory (0x06054b50) record.
    if pos + 22 > zip_bytes.len() {
        return Err(WireError::InvalidZip(
            "missing or truncated EOCD record".to_string(),
        ));
    }
    let eocd_sig = u32::from_le_bytes([
        zip_bytes[pos],
        zip_bytes[pos + 1],
        zip_bytes[pos + 2],
        zip_bytes[pos + 3],
    ]);
    if eocd_sig != 0x0605_4b50 {
        return Err(WireError::InvalidZip(format!(
            "missing EOCD signature 0x06054b50 at offset {pos}, got 0x{eocd_sig:08x}"
        )));
    }
    let entries_on_disk = u16::from_le_bytes([zip_bytes[pos + 8], zip_bytes[pos + 9]]) as usize;
    let entries_total = u16::from_le_bytes([zip_bytes[pos + 10], zip_bytes[pos + 11]]) as usize;
    let eocd_cd_size = u32::from_le_bytes([
        zip_bytes[pos + 12],
        zip_bytes[pos + 13],
        zip_bytes[pos + 14],
        zip_bytes[pos + 15],
    ]) as usize;
    let eocd_cd_offset = u32::from_le_bytes([
        zip_bytes[pos + 16],
        zip_bytes[pos + 17],
        zip_bytes[pos + 18],
        zip_bytes[pos + 19],
    ]) as usize;
    let eocd_comment_len = u16::from_le_bytes([zip_bytes[pos + 20], zip_bytes[pos + 21]]) as usize;

    if entries_on_disk != entries.len() || entries_total != entries.len() {
        return Err(WireError::InvalidZip(format!(
            "EOCD entry count mismatch: expected {}, got disk={entries_on_disk} total={entries_total}",
            entries.len()
        )));
    }
    if eocd_cd_size != actual_cd_size || eocd_cd_offset != cd_start {
        return Err(WireError::InvalidZip(format!(
            "EOCD central directory bounds mismatch: expected size={actual_cd_size} offset={cd_start}, got size={eocd_cd_size} offset={eocd_cd_offset}"
        )));
    }
    if pos + 22 + eocd_comment_len != zip_bytes.len() {
        return Err(WireError::InvalidZip(
            "EOCD comment length mismatch or trailing bytes".to_string(),
        ));
    }

    Ok(entries)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsOpcode {
    Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    Close = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

impl WsOpcode {
    pub fn from_u8(val: u8) -> Result<Self, WireError> {
        match val {
            0x0 => Ok(Self::Continuation),
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x8 => Ok(Self::Close),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            other => Err(WireError::WsProtocol(format!("unknown opcode {other:#x}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsFrame {
    pub fin: bool,
    pub opcode: WsOpcode,
    pub masked: bool,
    pub payload: Vec<u8>,
}

pub fn ws_accept_key(client_key: &str) -> String {
    const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut combined = String::with_capacity(client_key.len() + GUID.len());
    combined.push_str(client_key.trim());
    combined.push_str(GUID);
    let digest = sha1(combined.as_bytes());
    base64_encode(&digest)
}

pub const MAX_WS_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// Non-destructive RFC 6455 WebSocket frame decoder over a persistent byte buffer.
/// Returns `Ok(None)` without mutating `buf` when fewer bytes than a complete frame are buffered.
/// Validates opcode, mask expectation, and the 16 MiB frame limit as soon as the header is present.
pub fn try_decode_ws_frame(
    buf: &mut Vec<u8>,
    expect_masked: bool,
) -> Result<Option<WsFrame>, WireError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let fin = (buf[0] & 0x80) != 0;
    let opcode = WsOpcode::from_u8(buf[0] & 0x0F)?;
    let masked = (buf[1] & 0x80) != 0;
    if masked != expect_masked {
        return Err(WireError::WsProtocol(format!(
            "mask mismatch: expected {expect_masked}, got {masked}"
        )));
    }
    let len_tag = buf[1] & 0x7F;
    let (payload_len, mut offset): (usize, usize) = match len_tag {
        0..=125 => (usize::from(len_tag), 2),
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let raw_len = u64::from_be_bytes([
                buf[2], buf[3], buf[4], buf[5], buf[6], buf[7], buf[8], buf[9],
            ]);
            let len = usize::try_from(raw_len)
                .map_err(|_| WireError::WsProtocol("frame length overflow".to_string()))?;
            (len, 10)
        }
        _ => unreachable!(),
    };
    if payload_len > MAX_WS_FRAME_BYTES {
        return Err(WireError::WsProtocol("frame exceeds 16 MiB".to_string()));
    }
    let mask_key = if masked {
        if buf.len() < offset + 4 {
            return Ok(None);
        }
        let mk = [
            buf[offset],
            buf[offset + 1],
            buf[offset + 2],
            buf[offset + 3],
        ];
        offset += 4;
        Some(mk)
    } else {
        None
    };
    let total_len = offset
        .checked_add(payload_len)
        .ok_or_else(|| WireError::WsProtocol("frame length overflow".to_string()))?;
    if buf.len() < total_len {
        return Ok(None);
    }
    let mut payload = buf[offset..total_len].to_vec();
    buf.drain(..total_len);
    if let Some(mk) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mk[i % 4];
        }
    }
    Ok(Some(WsFrame {
        fin,
        opcode,
        masked,
        payload,
    }))
}

pub fn read_ws_frame(reader: &mut impl Read, expect_masked: bool) -> Result<WsFrame, WireError> {
    let mut hdr = [0u8; 2];
    reader.read_exact(&mut hdr)?;
    let fin = (hdr[0] & 0x80) != 0;
    let opcode = WsOpcode::from_u8(hdr[0] & 0x0F)?;
    let masked = (hdr[1] & 0x80) != 0;
    if masked != expect_masked {
        return Err(WireError::WsProtocol(format!(
            "mask mismatch: expected {expect_masked}, got {masked}"
        )));
    }
    let len_tag = hdr[1] & 0x7F;
    let payload_len: usize = match len_tag {
        0..=125 => usize::from(len_tag),
        126 => {
            let mut ext = [0u8; 2];
            reader.read_exact(&mut ext)?;
            usize::from(u16::from_be_bytes(ext))
        }
        127 => {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            let raw_len = u64::from_be_bytes(ext);
            usize::try_from(raw_len)
                .map_err(|_| WireError::WsProtocol("frame length overflow".to_string()))?
        }
        _ => unreachable!(),
    };
    if payload_len > MAX_WS_FRAME_BYTES {
        return Err(WireError::WsProtocol("frame exceeds 16 MiB".to_string()));
    }
    let mask_key = if masked {
        let mut mk = [0u8; 4];
        reader.read_exact(&mut mk)?;
        Some(mk)
    } else {
        None
    };
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload)?;
    }
    if let Some(mk) = mask_key {
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte ^= mk[i % 4];
        }
    }
    Ok(WsFrame {
        fin,
        opcode,
        masked,
        payload,
    })
}

pub fn write_ws_frame(
    writer: &mut impl Write,
    frame: &WsFrame,
    mask_key: Option<[u8; 4]>,
) -> io::Result<()> {
    let first = (if frame.fin { 0x80 } else { 0 }) | (frame.opcode as u8);
    let mask_bit = if mask_key.is_some() { 0x80 } else { 0 };
    let len = frame.payload.len();

    let mut header = Vec::with_capacity(14);
    header.push(first);
    if len <= 125 {
        header.push(mask_bit | (len as u8));
    } else if len <= 65_535 {
        header.push(mask_bit | 126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(mask_bit | 127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    if let Some(mk) = mask_key {
        header.extend_from_slice(&mk);
    }
    writer.write_all(&header)?;
    if let Some(mk) = mask_key {
        let masked_payload: Vec<u8> = frame
            .payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mk[i % 4])
            .collect();
        writer.write_all(&masked_payload)?;
    } else if !frame.payload.is_empty() {
        writer.write_all(&frame.payload)?;
    }
    writer.flush()
}

pub struct LoopbackServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    notify_shutdown: Arc<dyn Fn() + Send + Sync>,
    accept_thread: Option<JoinHandle<()>>,
    worker_threads: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl LoopbackServer {
    pub fn start<F>(
        name: &str,
        notify_shutdown: Arc<dyn Fn() + Send + Sync>,
        handler: F,
    ) -> io::Result<Self>
    where
        F: Fn(TcpStream, Arc<AtomicBool>) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let workers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);

        let thread_shutdown = Arc::clone(&shutdown);
        let thread_workers = Arc::clone(&workers);
        let thread_name = format!("{name}-{}", addr.port());
        let accept_thread = thread::Builder::new().name(thread_name).spawn(move || {
            for incoming in listener.incoming() {
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = incoming else {
                    if thread_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    continue;
                };
                if thread_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let _ = stream.set_nodelay(true);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

                let conn_handler = Arc::clone(&handler);
                let conn_shutdown = Arc::clone(&thread_shutdown);
                let handle = thread::spawn(move || {
                    conn_handler(stream, conn_shutdown);
                });

                if let Ok(mut list) = thread_workers.lock() {
                    list.retain(|h| !h.is_finished());
                    list.push(handle);
                }
            }
        })?;

        Ok(Self {
            addr,
            shutdown,
            notify_shutdown,
            accept_thread: Some(accept_thread),
            worker_threads: workers,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn shutdown_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.shutdown)
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        (self.notify_shutdown)();
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(handle) = self.accept_thread.take() {
            let _ = handle.join();
        }
        let handles: Vec<JoinHandle<()>> = match self.worker_threads.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for handle in handles {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn rfc6455_sha1_and_ws_accept_vector() {
        assert_eq!(
            ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn nist_sha256_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn base64_and_crc32_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(crc32_ieee(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn store_zip_roundtrip_and_zip_slip_entry() {
        let valid_zip = build_store_zip(&[
            ("postbox.exe", b"MZ-valid-binary"),
            ("IBMPlexSans-Regular.ttf", b"\x00\x01\x00\x00font"),
        ]);
        let parsed = parse_store_zip(&valid_zip).expect("valid store zip parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "postbox.exe");
        assert_eq!(parsed[0].1, b"MZ-valid-binary");
        assert_eq!(parsed[1].0, "IBMPlexSans-Regular.ttf");

        let slip_zip = build_store_zip(&[("../evil.dll", b"malicious")]);
        let parsed_slip = parse_store_zip(&slip_zip).expect("zip slip archive parses raw entries");
        assert_eq!(parsed_slip[0].0, "../evil.dll");
    }

    #[test]
    fn ws_frame_masked_and_extended_length_roundtrip() {
        let payload = vec![0x42u8; 300];
        let frame = WsFrame {
            fin: true,
            opcode: WsOpcode::Binary,
            masked: true,
            payload: payload.clone(),
        };
        let mut buf = Vec::new();
        write_ws_frame(&mut buf, &frame, Some([0x12, 0x34, 0x56, 0x78])).expect("write frame");
        let mut cursor = Cursor::new(buf);
        let decoded = read_ws_frame(&mut cursor, true).expect("read masked frame");
        assert_eq!(decoded.opcode, WsOpcode::Binary);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn multipart_ordered_roundtrip() {
        let parts = vec![
            MultipartPart {
                name: "account".into(),
                filename: None,
                content_type: None,
                body: b"hub".to_vec(),
            },
            MultipartPart {
                name: "ptt".into(),
                filename: None,
                content_type: None,
                body: b"true".to_vec(),
            },
            MultipartPart {
                name: "file".into(),
                filename: Some("voice.ogg".into()),
                content_type: Some("audio/ogg".into()),
                body: b"OggS-payload".to_vec(),
            },
        ];
        let boundary = "PostboxBoundary1234567890";
        let raw = build_multipart(boundary, &parts);
        let parsed = parse_multipart(&format!("multipart/form-data; boundary={boundary}"), &raw)
            .expect("parse multipart");
        assert_eq!(parsed, parts);
    }

    struct SplitReader {
        data: Vec<u8>,
        pos: usize,
        split_at: usize,
    }

    impl Read for SplitReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let limit = if self.pos < self.split_at {
                self.split_at - self.pos
            } else {
                self.data.len() - self.pos
            };
            let n = limit.min(buf.len());
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn read_http_request_enforces_strict_64kib_header_cap() {
        let prefix = b"GET / HTTP/1.1\r\nX-Pad: ";
        let exact_pad = DEFAULT_MAX_HEADER_BYTES - prefix.len();
        let mut exact_req = Vec::with_capacity(DEFAULT_MAX_HEADER_BYTES + 4);
        exact_req.extend_from_slice(prefix);
        exact_req.resize(DEFAULT_MAX_HEADER_BYTES, b'a');
        exact_req.extend_from_slice(b"\r\n\r\n");

        // Exact 65,536 B header in a single stream succeeds
        let parsed = read_http_request(&mut Cursor::new(exact_req.clone()), 1024)
            .expect("exact 64 KiB header succeeds");
        assert_eq!(parsed.header("x-pad").unwrap().len(), exact_pad);

        // Exact 65,536 B header split right after 65,536 + 3 bytes (`\r\n\r` then `\n`) succeeds
        let mut split = SplitReader {
            data: exact_req,
            pos: 0,
            split_at: DEFAULT_MAX_HEADER_BYTES + 3,
        };
        let parsed_split =
            read_http_request(&mut split, 1024).expect("split delimiter at exact 64 KiB succeeds");
        assert_eq!(parsed_split.header("x-pad").unwrap().len(), exact_pad);

        // 65,537 B and 65,636 B (Probe 4A) headers must both fail with BadRequest
        for over in [DEFAULT_MAX_HEADER_BYTES + 1, DEFAULT_MAX_HEADER_BYTES + 100] {
            let mut big_req = Vec::with_capacity(over + 4);
            big_req.extend_from_slice(prefix);
            big_req.resize(over, b'b');
            big_req.extend_from_slice(b"\r\n\r\n");
            let err = read_http_request(&mut Cursor::new(big_req), 1024)
                .expect_err("oversized header rejected");
            assert!(
                matches!(err, WireError::BadRequest(_)),
                "expected BadRequest for header size {over}, got {err:?}"
            );
        }
    }

    #[test]
    fn read_chunked_body_verifies_crlf_and_bounds_chunk_line() {
        let mut empty_stream = Cursor::new(Vec::<u8>::new());
        let decoded = read_chunked_body(
            &mut empty_stream,
            b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n".to_vec(),
            1024,
        )
        .expect("valid chunked body");
        assert_eq!(decoded, b"hello world");

        let with_ext_trailer = read_chunked_body(
            &mut Cursor::new(Vec::<u8>::new()),
            b"5;ext=1\r\nhello\r\n0\r\nX-T: 1\r\n\r\n".to_vec(),
            1024,
        )
        .expect("chunk extension + trailer");
        assert_eq!(with_ext_trailer, b"hello");

        let bad_crlf = read_chunked_body(
            &mut Cursor::new(Vec::<u8>::new()),
            b"5\r\nhelloXX0\r\n\r\n".to_vec(),
            1024,
        )
        .expect_err("missing chunk CRLF rejected");
        assert!(matches!(bad_crlf, WireError::BadRequest(_)));

        let missing_final = read_chunked_body(
            &mut Cursor::new(Vec::<u8>::new()),
            b"5\r\nhello\r\n0\r\n".to_vec(),
            1024,
        )
        .expect_err("missing terminating CRLF rejected");
        assert!(matches!(missing_final, WireError::UnexpectedEof));

        let mut huge_line = vec![b'0'; MAX_CHUNK_LINE_BYTES + 2];
        huge_line.extend_from_slice(b"\r\n\r\n");
        let over_line = read_chunked_body(&mut Cursor::new(Vec::<u8>::new()), huge_line, 1024)
            .expect_err("oversized chunk line rejected");
        assert!(matches!(over_line, WireError::BadRequest(_)));
    }

    #[test]
    fn parse_store_zip_validates_central_directory_eocd_and_sizes() {
        assert!(matches!(
            parse_store_zip(&[]),
            Err(WireError::InvalidZip(_))
        ));
        assert!(matches!(
            parse_store_zip(b"abc"),
            Err(WireError::InvalidZip(_))
        ));

        // Valid empty zip (22-byte EOCD) still parses cleanly
        let empty_zip = build_store_zip(&[]);
        assert_eq!(empty_zip.len(), 22);
        assert!(parse_store_zip(&empty_zip).expect("empty zip").is_empty());

        let valid = build_store_zip(&[("app.bin", b"hello-world")]);

        // Strip Central Directory + EOCD
        let cd_pos = find_subslice(&valid, &0x0201_4b50u32.to_le_bytes()).expect("cd offset");
        assert!(matches!(
            parse_store_zip(&valid[..cd_pos]),
            Err(WireError::InvalidZip(_))
        ));

        // Mutate local header uncomp_size != comp_size
        let mut bad_uncomp = valid.clone();
        bad_uncomp[22..26].copy_from_slice(&999u32.to_le_bytes());
        assert!(matches!(
            parse_store_zip(&bad_uncomp),
            Err(WireError::InvalidZip(_))
        ));

        // Mutate EOCD entries_total
        let mut bad_eocd = valid.clone();
        let eocd_pos = valid.len() - 22;
        bad_eocd[eocd_pos + 10..eocd_pos + 12].copy_from_slice(&2u16.to_le_bytes());
        assert!(matches!(
            parse_store_zip(&bad_eocd),
            Err(WireError::InvalidZip(_))
        ));
    }

    #[test]
    fn try_decode_ws_frame_handles_byte_by_byte_fragmentation_and_coalescing() {
        let f1 = WsFrame {
            fin: true,
            opcode: WsOpcode::Binary,
            masked: true,
            payload: vec![0xABu8; 100],
        };
        let f2 = WsFrame {
            fin: true,
            opcode: WsOpcode::Text,
            masked: true,
            payload: b"hello-coalesced-msg!".to_vec(),
        };
        let mut wire = Vec::new();
        write_ws_frame(&mut wire, &f1, Some([1, 2, 3, 4])).unwrap();
        let f1_len = wire.len();
        write_ws_frame(&mut wire, &f2, Some([5, 6, 7, 8])).unwrap();

        let mut buf = Vec::new();
        let mut decoded = Vec::new();
        for (idx, &byte) in wire.iter().enumerate() {
            buf.push(byte);
            let res = try_decode_ws_frame(&mut buf, true).expect("decode step");
            if idx + 1 == f1_len || idx + 1 == wire.len() {
                decoded.push(res.expect("frame completes on exact boundary"));
            } else {
                assert!(res.is_none(), "unexpected early decode at byte {idx}");
            }
        }
        assert!(buf.is_empty());
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0], f1);
        assert_eq!(decoded[1], f2);

        // Coalesced decode from a single buffer
        let mut coalesced = wire;
        assert_eq!(try_decode_ws_frame(&mut coalesced, true).unwrap(), Some(f1));
        assert_eq!(try_decode_ws_frame(&mut coalesced, true).unwrap(), Some(f2));
        assert!(coalesced.is_empty());

        // Mask mismatch fails immediately on 2 bytes
        let mut unmasked = vec![0x81, 0x05];
        assert!(matches!(
            try_decode_ws_frame(&mut unmasked, true),
            Err(WireError::WsProtocol(_))
        ));
    }
}
