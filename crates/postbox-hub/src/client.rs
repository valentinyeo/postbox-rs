use crate::error::HubError;
use crate::normalize::{merge_archived_chats, normalize_chats, normalize_messages};
use crate::request::{
    MAX_DOWNLOAD_BYTES, MAX_SLACK_EMOJI_INDEX_BYTES, MAX_SLACK_EMOJI_MEDIA_BYTES, USER_AGENT,
    archive_batch_request, archive_request, chats_request, crash_report_request, delete_request,
    encode_send_file_multipart, events_request, generate_boundary, mark_read_request,
    media_request, messages_request, react_request, send_text_request, slack_emoji_index_request,
    slack_emoji_media_request, valid_last_event_id, valid_token, workspace_icon_request,
};
use crate::types::{
    ArchiveBatchItemResult, ArchiveOutcome, Endpoint, HubChat, HubMessage, HubRequest,
    MediaFetchDetailed, MediaFetchState, SendFileRequest, SendReply,
};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use serde::Deserialize;
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REST_TIMEOUT: Duration = Duration::from_secs(30);
/// M5.5 RAM diet: keep at most two idle keep-alive sockets per host instead of reqwest's
/// default of `usize::MAX`, so a long sync session cannot hold a pool of open TLS buffers.
pub const MAX_IDLE_CONNECTIONS_PER_HOST: usize = 2;
/// 150s stall timeout for up-to-100 MiB Hub media downloads (`media_drain.zig:179-183`).
pub const HUB_MEDIA_TIMEOUT: Duration = Duration::from_secs(150);
pub const DEFAULT_MEDIA_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const DEFAULT_MEDIA_POLL_MAX_WAIT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct ArchiveBatchEnvelope {
    results: Vec<ArchiveBatchItemResult>,
}

/// Strictly Hub-only `/v1/wazig/*` HTTP/2 + SSE client (`reqwest` rustls, zero OpenSSL, zero direct providers).
#[derive(Clone)]
pub struct HubClient {
    endpoint: Endpoint,
    auth_header: HeaderValue,
    rest: reqwest::Client,
    sse: reqwest::Client,
}

impl HubClient {
    pub fn new(endpoint: Endpoint, token: impl AsRef<str>) -> Result<Self, HubError> {
        let tok = token.as_ref();
        valid_token(tok)?;
        let auth_header =
            HeaderValue::from_str(&format!("Bearer {tok}")).map_err(|_| HubError::InvalidToken)?;

        let mut rest_builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST)
            .timeout(REST_TIMEOUT);
        let mut sse_builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_max_idle_per_host(MAX_IDLE_CONNECTIONS_PER_HOST);

        if !endpoint.secure {
            // Insecure loopback (127.0.0.1) bypasses system proxy (`winhttp.zig:343`).
            rest_builder = rest_builder.no_proxy();
            sse_builder = sse_builder.no_proxy();
        }

        let rest = rest_builder
            .build()
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let sse = sse_builder
            .build()
            .map_err(|e| HubError::Transport(e.to_string()))?;

        Ok(Self {
            endpoint,
            auth_header,
            rest,
            sse,
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    async fn read_capped_bytes(
        resp: reqwest::Response,
        max_bytes: usize,
    ) -> Result<Vec<u8>, HubError> {
        if let Some(len) = resp.content_length()
            && len > max_bytes as u64
        {
            return Err(HubError::ResponseTooLarge);
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| HubError::Transport(e.to_string()))?;
            if out.len().saturating_add(chunk.len()) > max_bytes {
                return Err(HubError::ResponseTooLarge);
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    async fn execute_rest_detailed(
        &self,
        req: &HubRequest,
        max_bytes: usize,
        timeout: Option<Duration>,
    ) -> Result<(u16, Option<String>, Vec<u8>), HubError> {
        let url = self.endpoint.url_for(&req.path);
        let method = match req.method {
            "GET" => reqwest::Method::GET,
            "POST" => reqwest::Method::POST,
            _ => return Err(HubError::InvalidSend),
        };

        let mut builder = self
            .rest
            .request(method, &url)
            .header(AUTHORIZATION, self.auth_header.clone())
            .header(ACCEPT, "*/*");

        if let Some(t) = timeout {
            builder = builder.timeout(t);
        }

        if let Some(body) = &req.body {
            builder = builder
                .header(CONTENT_TYPE, "application/json; charset=utf-8")
                .body(body.clone());
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or(s).trim().to_owned())
            .filter(|s| !s.is_empty());
        let bytes = Self::read_capped_bytes(resp, max_bytes).await?;
        Ok((status, content_type, bytes))
    }

    async fn execute_rest(
        &self,
        req: &HubRequest,
        max_bytes: usize,
    ) -> Result<(u16, Vec<u8>), HubError> {
        let (status, _ct, bytes) = self.execute_rest_detailed(req, max_bytes, None).await?;
        Ok((status, bytes))
    }

    fn parse_send_reply(
        status: u16,
        bytes: &[u8],
        expected_client_id: &str,
    ) -> Result<SendReply, HubError> {
        let body = String::from_utf8_lossy(bytes).into_owned();
        if !(200..300).contains(&status) {
            return Err(HubError::HttpStatus { status, body });
        }
        let reply: SendReply =
            serde_json::from_slice(bytes).map_err(|_| HubError::SendNotConfirmed)?;
        if reply.client_id == expected_client_id && reply.status == "sent" {
            Ok(reply)
        } else {
            Err(HubError::SendNotConfirmed)
        }
    }

    pub async fn fetch_chats(
        &self,
        archived: bool,
        limit: usize,
    ) -> Result<Vec<HubChat>, HubError> {
        let req = chats_request(archived, limit);
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        let body = String::from_utf8(bytes).map_err(|_| HubError::BadResponse)?;
        if status != 200 {
            return Err(HubError::HttpStatus { status, body });
        }
        normalize_chats(&body, "hub")
    }

    pub async fn fetch_archived_and_inbox_chats(
        &self,
        limit: usize,
    ) -> Result<Vec<HubChat>, HubError> {
        let archived_req = chats_request(true, limit);
        let inbox_req = chats_request(false, limit);
        let (arch_res, inbox_res) = tokio::join!(
            self.execute_rest(&archived_req, MAX_DOWNLOAD_BYTES),
            self.execute_rest(&inbox_req, MAX_DOWNLOAD_BYTES),
        );
        let (arch_status, arch_bytes) = arch_res?;
        let (inbox_status, inbox_bytes) = inbox_res?;
        let arch_body = String::from_utf8(arch_bytes).map_err(|_| HubError::BadResponse)?;
        let inbox_body = String::from_utf8(inbox_bytes).map_err(|_| HubError::BadResponse)?;
        if arch_status != 200 {
            return Err(HubError::HttpStatus {
                status: arch_status,
                body: arch_body,
            });
        }
        if inbox_status != 200 {
            return Err(HubError::HttpStatus {
                status: inbox_status,
                body: inbox_body,
            });
        }
        merge_archived_chats(&arch_body, &inbox_body, "hub")
    }

    pub async fn fetch_messages(
        &self,
        chat: &str,
        limit: usize,
        before: Option<&str>,
    ) -> Result<Vec<HubMessage>, HubError> {
        let req = messages_request(chat, limit, before)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        let body = String::from_utf8(bytes).map_err(|_| HubError::BadResponse)?;
        if status != 200 {
            return Err(HubError::HttpStatus { status, body });
        }
        normalize_messages(&body, chat)
    }

    pub async fn send_text(
        &self,
        chat: &str,
        text: &str,
        reply_to: Option<&str>,
        reply_broadcast: bool,
        client_id: &str,
    ) -> Result<SendReply, HubError> {
        let req = send_text_request(chat, text, reply_to, reply_broadcast, client_id)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        Self::parse_send_reply(status, &bytes, client_id)
    }

    pub async fn send_file(&self, req: SendFileRequest<'_>) -> Result<SendReply, HubError> {
        let mut last_err = HubError::InvalidBoundary;
        let mut encoded = None;
        for _ in 0..4 {
            let boundary = generate_boundary()?;
            match encode_send_file_multipart(&req, &boundary) {
                Ok(pair) => {
                    encoded = Some(pair);
                    break;
                }
                Err(HubError::InvalidBoundary) => {
                    last_err = HubError::InvalidBoundary;
                }
                Err(other) => return Err(other),
            }
        }
        let (content_type, body) = encoded.ok_or(last_err)?;
        let url = self.endpoint.url_for("/v1/wazig/send");
        let resp = self
            .rest
            .post(&url)
            .header(AUTHORIZATION, self.auth_header.clone())
            .header(ACCEPT, "*/*")
            .header(CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = Self::read_capped_bytes(resp, MAX_DOWNLOAD_BYTES).await?;
        Self::parse_send_reply(status, &bytes, req.client_id)
    }

    /// Uploads one sanitised crash report to `/v1/wazig/crash-reports`.
    ///
    /// Best effort by design: the caller ignores every error, and nothing here surfaces to the
    /// user. A non-2xx response is reported as an error so the caller can count the attempt.
    pub async fn send_crash_report(
        &self,
        report: &postbox_core::crash::CrashReport,
    ) -> Result<(), HubError> {
        let req = crash_report_request(report)?;
        let (status, bytes) = self.execute_rest(&req, 16 * 1024).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
    }

    pub async fn mark_read(&self, chat: &str, message_id: &str) -> Result<(), HubError> {
        let req = mark_read_request(chat, message_id)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
    }

    pub async fn archive(&self, chat: &str, archived: bool) -> Result<ArchiveOutcome, HubError> {
        let req = archive_request(chat, archived)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        if (200..300).contains(&status) {
            return Ok(ArchiveOutcome::Completed);
        }
        if status == 502 && (chat.starts_with("signal:") || chat.starts_with("telegram:")) {
            return Ok(ArchiveOutcome::ManagedLocally);
        }
        Err(HubError::HttpStatus {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    }

    pub async fn archive_batch(
        &self,
        chats: &[&str],
        archived: bool,
    ) -> Result<Vec<ArchiveBatchItemResult>, HubError> {
        let req = archive_batch_request(chats, archived)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        if !(200..300).contains(&status) {
            return Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        let env: ArchiveBatchEnvelope =
            serde_json::from_slice(&bytes).map_err(|_| HubError::BadResponse)?;
        if env.results.len() != chats.len() {
            return Err(HubError::BadResponse);
        }
        for (item, &expected_chat) in env.results.iter().zip(chats.iter()) {
            if item.chat != expected_chat {
                return Err(HubError::BadResponse);
            }
        }
        Ok(env.results)
    }

    pub async fn react(
        &self,
        chat: &str,
        msg_id: &str,
        emoji: &str,
        client_id: &str,
    ) -> Result<SendReply, HubError> {
        let req = react_request(chat, msg_id, emoji, client_id)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        if !(200..300).contains(&status) {
            return Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        if let Ok(reply) = Self::parse_send_reply(status, &bytes, client_id) {
            return Ok(reply);
        }
        // Accept minimal `200 OK` `{"ok": true}` or empty body responses cleanly.
        if bytes.is_empty()
            || serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("ok").and_then(serde_json::Value::as_bool))
                == Some(true)
        {
            return Ok(SendReply {
                client_id: client_id.to_owned(),
                msg_id: msg_id.to_owned(),
                ts: String::new(),
                status: "sent".to_owned(),
            });
        }
        Err(HubError::SendNotConfirmed)
    }

    pub async fn delete_message(
        &self,
        chat: &str,
        msg_id: &str,
        client_id: &str,
    ) -> Result<(), HubError> {
        let req = delete_request(chat, msg_id, client_id)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            })
        }
    }

    /// Single-attempt media download returning both state and response `Content-Type`, using
    /// the 150-second Hub media stall timeout (`200` ready, `202` pending, `206` REJECTED
    /// `PartialMedia`, `404` unavailable, `410` permanently gone).
    pub async fn fetch_media_detailed_once(
        &self,
        chat: &str,
        msg_id: &str,
    ) -> Result<MediaFetchDetailed, HubError> {
        let req = media_request(chat, Some(msg_id))?;
        let (status, content_type, bytes) = self
            .execute_rest_detailed(&req, MAX_DOWNLOAD_BYTES, Some(HUB_MEDIA_TIMEOUT))
            .await?;
        let state = match status {
            200 => MediaFetchState::Ready(bytes),
            202 => MediaFetchState::Pending,
            206 => return Err(HubError::PartialMedia),
            404 => MediaFetchState::Unavailable,
            410 => MediaFetchState::Gone,
            other => {
                return Err(HubError::HttpStatus {
                    status: other,
                    body: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
        };
        Ok(MediaFetchDetailed {
            state,
            content_type,
        })
    }

    /// Single-attempt media download (`200` ready, `202` pending, `206` REJECTED `PartialMedia`,
    /// `404` unavailable, `410` permanently gone).
    pub async fn fetch_media_once(
        &self,
        chat: &str,
        msg_id: &str,
    ) -> Result<MediaFetchState, HubError> {
        Ok(self.fetch_media_detailed_once(chat, msg_id).await?.state)
    }

    /// Polls `202 Pending` every `poll_interval` (default `1s`) up to `max_wait` (default `120s`),
    /// returning both state and `Content-Type`.
    pub async fn fetch_media_detailed_polling(
        &self,
        chat: &str,
        msg_id: &str,
        poll_interval: Duration,
        max_wait: Duration,
    ) -> Result<MediaFetchDetailed, HubError> {
        let start = Instant::now();
        loop {
            let detailed = self.fetch_media_detailed_once(chat, msg_id).await?;
            match detailed.state {
                MediaFetchState::Pending => {
                    if start.elapsed() >= max_wait {
                        return Ok(detailed);
                    }
                    let remaining = max_wait.saturating_sub(start.elapsed());
                    tokio::time::sleep(poll_interval.min(remaining)).await;
                }
                _ => return Ok(detailed),
            }
        }
    }

    /// Polls `202 Pending` every `poll_interval` (default `1s`) up to `max_wait` (default `120s`).
    pub async fn fetch_media_polling(
        &self,
        chat: &str,
        msg_id: &str,
        poll_interval: Duration,
        max_wait: Duration,
    ) -> Result<MediaFetchState, HubError> {
        Ok(self
            .fetch_media_detailed_polling(chat, msg_id, poll_interval, max_wait)
            .await?
            .state)
    }

    pub async fn fetch_avatar(&self, chat: &str) -> Result<Option<Vec<u8>>, HubError> {
        let req = media_request(chat, None)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        match status {
            200 => Ok(Some(bytes)),
            206 => Err(HubError::PartialMedia),
            404 | 410 => Ok(None),
            other => Err(HubError::HttpStatus {
                status: other,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }),
        }
    }

    pub async fn fetch_workspace_icon(&self, team: &str) -> Result<Option<Vec<u8>>, HubError> {
        let req = workspace_icon_request(team)?;
        let (status, bytes) = self.execute_rest(&req, MAX_DOWNLOAD_BYTES).await?;
        match status {
            200 => Ok(Some(bytes)),
            206 => Err(HubError::PartialMedia),
            404 | 410 => Ok(None),
            other => Err(HubError::HttpStatus {
                status: other,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }),
        }
    }

    pub async fn fetch_slack_emoji_index(&self, team: &str) -> Result<serde_json::Value, HubError> {
        let req = slack_emoji_index_request(team)?;
        let (status, bytes) = self.execute_rest(&req, MAX_SLACK_EMOJI_INDEX_BYTES).await?;
        if status != 200 {
            return Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        serde_json::from_slice(&bytes).map_err(|_| HubError::BadResponse)
    }

    pub async fn fetch_slack_emoji_media(
        &self,
        team: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, HubError> {
        let req = slack_emoji_media_request(team, name)?;
        let (status, bytes) = self.execute_rest(&req, MAX_SLACK_EMOJI_MEDIA_BYTES).await?;
        match status {
            200 => Ok(Some(bytes)),
            206 => Err(HubError::PartialMedia),
            404 | 410 => Ok(None),
            other => Err(HubError::HttpStatus {
                status: other,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            }),
        }
    }

    /// Opens the long-lived SSE `/v1/wazig/events` stream with optional `Last-Event-ID` header.
    pub async fn connect_events(
        &self,
        last_event_id: Option<&str>,
    ) -> Result<EventStream, HubError> {
        let req = events_request(last_event_id);
        let url = self.endpoint.url_for(&req.path);
        let mut builder = self
            .sse
            .get(&url)
            .header(AUTHORIZATION, self.auth_header.clone())
            .header(ACCEPT, "text/event-stream");

        if let Some(id) = req.last_event_id.as_deref().filter(|s| !s.is_empty()) {
            valid_last_event_id(id)?;
            let header_name = HeaderName::from_static("last-event-id");
            let header_val = HeaderValue::from_str(id).map_err(|_| HubError::BadEventId)?;
            builder = builder.header(header_name, header_val);
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| HubError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let bytes = Self::read_capped_bytes(resp, 16 * 1024)
                .await
                .unwrap_or_default();
            return Err(HubError::HttpStatus {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        Ok(EventStream { resp })
    }
}

/// Streaming handle for `/v1/wazig/events` SSE chunks.
pub struct EventStream {
    resp: reqwest::Response,
}

impl EventStream {
    pub async fn next_chunk(&mut self) -> Option<Result<bytes::Bytes, HubError>> {
        match self.resp.chunk().await {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => None,
            Err(err) => Some(Err(HubError::Transport(err.to_string()))),
        }
    }
}
