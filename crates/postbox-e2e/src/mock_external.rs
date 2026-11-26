#![forbid(unsafe_code)]

use crate::fixtures::{minimal_gif89a, minimal_jpeg, minimal_png_1x1};
use crate::wire::{
    DEFAULT_MAX_BODY_BYTES, LoopbackServer, RawHttpRequest, RawHttpResponse, build_store_zip,
    read_http_request, sha256_hex,
};
use serde_json::json;
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct RecordedExternalRequest {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub enum GiphyFault {
    Unauthorized401,
    RateLimit429 { retry_after: String },
    DuplicateGifId,
    OversizedResponse257KiB,
    BadMetaStatus500,
}

#[derive(Debug, Clone)]
pub enum GiphyCdnFault {
    CorruptMagic,
    Redirect302(String),
    SlowMs(u64),
    HttpStatus(u16),
}

#[derive(Debug, Clone)]
pub enum UpdateScenario {
    ValidRelease {
        tag: String,
        zip_entries: Vec<(String, Vec<u8>)>,
    },
    ZipSlipArchive,
    Sha256Mismatch,
    DuplicateMatchingAssets,
    DraftOrPrerelease {
        draft: bool,
        prerelease: bool,
    },
    HttpsToHttpRedirect302,
}

struct MockExternalState {
    giphy_api_key: String,
    giphy_fault: Option<GiphyFault>,
    giphy_cdn_faults: HashMap<String, GiphyCdnFault>,
    custom_pages: HashMap<String, String>,
    update_scenario: UpdateScenario,
    recorded: Vec<RecordedExternalRequest>,
}

impl MockExternalState {
    fn new(giphy_api_key: String) -> Self {
        Self {
            giphy_api_key,
            giphy_fault: None,
            giphy_cdn_faults: HashMap::new(),
            custom_pages: HashMap::new(),
            update_scenario: UpdateScenario::ValidRelease {
                tag: "v0.2.0".into(),
                zip_entries: vec![
                    ("postbox.exe".into(), b"MZ-POSTBOX-V020-BINARY".to_vec()),
                    (
                        "postboxctl.exe".into(),
                        b"MZ-POSTBOXCTL-V020-BINARY".to_vec(),
                    ),
                    (
                        "IBMPlexSans-Regular.ttf".into(),
                        b"\x00\x01\x00\x00-font".to_vec(),
                    ),
                    (
                        "IBM-Plex-LICENSE.txt".into(),
                        b"SIL OPEN FONT LICENSE Version 1.1".to_vec(),
                    ),
                ],
            },
            recorded: Vec::new(),
        }
    }
}

pub struct MockExternalServer {
    server: LoopbackServer,
    state: Arc<Mutex<MockExternalState>>,
}

impl MockExternalServer {
    pub fn start(giphy_api_key: impl Into<String>) -> Self {
        let state = Arc::new(Mutex::new(MockExternalState::new(giphy_api_key.into())));
        let handler_state = Arc::clone(&state);

        let server = LoopbackServer::start("mock-external", Arc::new(|| {}), move |stream, _| {
            let port = stream.local_addr().map(|a| a.port()).unwrap_or(0);
            handle_external_connection(stream, port, &handler_state);
        })
        .expect("bind MockExternalServer on 127.0.0.1:0");

        Self { server, state }
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    pub fn port(&self) -> u16 {
        self.server.port()
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port())
    }

    pub fn set_giphy_fault(&self, fault: Option<GiphyFault>) {
        self.state.lock().unwrap().giphy_fault = fault;
    }

    pub fn set_giphy_cdn_fault(&self, gif_id: impl Into<String>, fault: GiphyCdnFault) {
        self.state
            .lock()
            .unwrap()
            .giphy_cdn_faults
            .insert(gif_id.into(), fault);
    }

    pub fn register_page(&self, slug: impl Into<String>, html: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .custom_pages
            .insert(slug.into(), html.into());
    }

    pub fn set_update_scenario(&self, scenario: UpdateScenario) {
        self.state.lock().unwrap().update_scenario = scenario;
    }

    pub fn recorded_requests(&self) -> Vec<RecordedExternalRequest> {
        self.state.lock().unwrap().recorded.clone()
    }
}

fn handle_external_connection(
    mut stream: TcpStream,
    port: u16,
    state: &Arc<Mutex<MockExternalState>>,
) {
    let Ok(req) = read_http_request(&mut stream, DEFAULT_MAX_BODY_BYTES) else {
        return;
    };
    state
        .lock()
        .unwrap()
        .recorded
        .push(RecordedExternalRequest {
            method: req.method.clone(),
            path: req.path.clone(),
            query: req.query_params.clone(),
        });

    let base = format!("http://127.0.0.1:{port}");
    let resp = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/v1/gifs/trending") | ("GET", "/v1/gifs/search") => {
            handle_giphy_api(&req, &base, state)
        }
        ("GET", p) if p.starts_with("/media/") => handle_giphy_cdn(p, state),
        ("GET", "/oembed") => handle_oembed(&base),
        ("GET", p) if p.starts_with("/vi/") && p.ends_with(".jpg") => {
            RawHttpResponse::bytes(200, "image/jpeg", minimal_jpeg())
        }
        ("GET", p) if p.starts_with("/image/") => {
            if p.ends_with(".gif") {
                RawHttpResponse::bytes(200, "image/gif", minimal_gif89a())
            } else if p.ends_with(".jpg") || p.ends_with(".jpeg") {
                RawHttpResponse::bytes(200, "image/jpeg", minimal_jpeg())
            } else {
                RawHttpResponse::bytes(200, "image/png", minimal_png_1x1())
            }
        }
        ("GET", p) if p.starts_with("/page/") => handle_page(p, &base, state),
        ("GET", "/update.json")
        | ("GET", "/valentinyeo/postbox/releases/latest/download/update.json") => {
            handle_update_json(&base, state)
        }
        ("GET", p) if p.starts_with("/releases/") => handle_release_download(p, &base, state),
        _ => RawHttpResponse::json(404, &json!({"error": "not found"})),
    };

    let _ = resp.write_to(&mut stream);
}

fn handle_giphy_api(
    req: &RawHttpRequest,
    base: &str,
    state: &Arc<Mutex<MockExternalState>>,
) -> RawHttpResponse {
    let st = state.lock().unwrap();
    if let Some(fault) = &st.giphy_fault {
        match fault {
            GiphyFault::Unauthorized401 => {
                return RawHttpResponse::json(401, &json!({"meta": {"status": 401}}));
            }
            GiphyFault::RateLimit429 { retry_after } => {
                return RawHttpResponse::json(429, &json!({"meta": {"status": 429}}))
                    .with_header("Retry-After", retry_after.clone());
            }
            GiphyFault::DuplicateGifId => {
                let item = giphy_item("dup123", "Duplicate Cat", base);
                return RawHttpResponse::json(
                    200,
                    &json!({"data": [item.clone(), item], "meta": {"status": 200}}),
                );
            }
            GiphyFault::OversizedResponse257KiB => {
                let pad = "a".repeat(257 * 1024);
                return RawHttpResponse::json(
                    200,
                    &json!({"data": [], "pad": pad, "meta": {"status": 200}}),
                );
            }
            GiphyFault::BadMetaStatus500 => {
                return RawHttpResponse::json(200, &json!({"data": [], "meta": {"status": 500}}));
            }
        }
    }

    if req.query("api_key") != Some(st.giphy_api_key.as_str()) {
        return RawHttpResponse::json(401, &json!({"meta": {"status": 401}}));
    }

    let title = req
        .query("q")
        .map(|q| format!("Search {q}"))
        .unwrap_or_else(|| "Trending Cat".into());
    RawHttpResponse::json(
        200,
        &json!({
            "data": [
                giphy_item("cat001", &title, base),
                giphy_item("dog002", "Happy Dog", base)
            ],
            "meta": {"status": 200, "msg": "OK"}
        }),
    )
}

fn giphy_item(id: &str, title: &str, base: &str) -> serde_json::Value {
    json!({
        "id": id,
        "title": title,
        "images": {
            "fixed_width": {
                "url": format!("{base}/media/{id}/giphy.gif"),
                "width": "200",
                "height": "150",
                "size": "43"
            },
            "downsized_medium": {
                "url": format!("{base}/media/{id}/giphy.gif"),
                "width": "320",
                "height": "240",
                "size": "43"
            }
        }
    })
}

fn handle_giphy_cdn(path: &str, state: &Arc<Mutex<MockExternalState>>) -> RawHttpResponse {
    // Expects /media/{id}/giphy.gif or /media/v1.{token}/{id}/{file}.gif
    let segments: Vec<&str> = path.trim_start_matches("/media/").split('/').collect();
    let gif_id = if segments.first().is_some_and(|s| s.starts_with("v1.")) {
        segments.get(1).copied().unwrap_or("")
    } else {
        segments.first().copied().unwrap_or("")
    };

    let fault = state.lock().unwrap().giphy_cdn_faults.get(gif_id).cloned();
    if let Some(f) = fault {
        match f {
            GiphyCdnFault::CorruptMagic => {
                return RawHttpResponse::bytes(200, "image/gif", b"<html>not gif".to_vec());
            }
            GiphyCdnFault::Redirect302(loc) => {
                return RawHttpResponse::empty(302).with_header("Location", loc);
            }
            GiphyCdnFault::SlowMs(ms) => {
                thread::sleep(Duration::from_millis(ms));
            }
            GiphyCdnFault::HttpStatus(code) => {
                return RawHttpResponse::empty(code);
            }
        }
    }

    RawHttpResponse::bytes(200, "image/gif", minimal_gif89a())
}

fn handle_oembed(base: &str) -> RawHttpResponse {
    RawHttpResponse::json(
        200,
        &json!({
            "title": "Intel Arc DXVA Demo",
            "author_name": "Postbox",
            "thumbnail_url": format!("{base}/vi/dQw4w9WgXcQ/hqdefault.jpg"),
            "provider_name": "YouTube"
        }),
    )
}

fn handle_page(path: &str, base: &str, state: &Arc<Mutex<MockExternalState>>) -> RawHttpResponse {
    let slug = path.trim_start_matches("/page/");
    if let Some(custom) = state.lock().unwrap().custom_pages.get(slug).cloned() {
        return RawHttpResponse::bytes(200, "text/html; charset=utf-8", custom.into_bytes());
    }
    let html = match slug {
        "standard" => format!(
            "<!doctype html><html><head>\
             <meta property=\"og:title\" content=\"Postbox Rust Guide\">\
             <meta property=\"og:description\" content=\"Zero-toolbar messaging client\">\
             <meta property=\"og:image\" content=\"{base}/image/card.png\">\
             </head><body>ok</body></html>"
        ),
        "relative-img" => "<!doctype html><html><head>\
             <meta property=\"og:title\" content=\"Relative Image Card\">\
             <meta property=\"og:image\" content=\"/image/card.png\">\
             </head><body>ok</body></html>"
            .to_string(),
        "protocol-relative" => format!(
            "<!doctype html><html><head>\
             <meta property=\"og:title\" content=\"Proto Relative\">\
             <meta property=\"og:image\" content=\"//{}/image/card.png\">\
             </head><body>ok</body></html>",
            base.trim_start_matches("http://")
        ),
        "js-xss" => "<!doctype html><html><head>\
             <meta property=\"og:title\" content=\"XSS Attempt\">\
             <meta property=\"og:image\" content=\"javascript:alert(1)\">\
             </head><body>ok</body></html>"
            .to_string(),
        "truncated" => "<html<head><meta property=\"og:video\" content=\"https://".to_string(),
        "video-cdn" => "<!doctype html><html><head>\
             <meta property=\"og:title\" content=\"Video Card\">\
             <meta property=\"og:video\" content=\"https://evil.cdninstagram.com.attacker.invalid/v.mp4\">\
             </head><body>ok</body></html>"
            .to_string(),
        _ => return RawHttpResponse::text(404, "page not found"),
    };
    RawHttpResponse::bytes(200, "text/html; charset=utf-8", html.into_bytes())
}

fn build_scenario_zip(scenario: &UpdateScenario) -> (String, String, Vec<u8>, String) {
    match scenario {
        UpdateScenario::ValidRelease { tag, zip_entries } => {
            let refs: Vec<(&str, &[u8])> = zip_entries
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            let zip = build_store_zip(&refs);
            let digest = sha256_hex(&zip);
            let version = tag.trim_start_matches('v');
            (tag.clone(), format!("Postbox-{version}.zip"), zip, digest)
        }
        UpdateScenario::ZipSlipArchive => {
            let zip = build_store_zip(&[("../evil.dll", b"malicious"), ("postbox.exe", b"MZ-OK")]);
            let digest = sha256_hex(&zip);
            ("v0.2.1".into(), "Postbox-0.2.1.zip".into(), zip, digest)
        }
        UpdateScenario::Sha256Mismatch => {
            let zip = build_store_zip(&[("postbox.exe", b"MZ-TAMPERED")]);
            // Advertise a wrong SHA-256 digest (0000... != actual)
            let wrong_digest = "0".repeat(64);
            (
                "v0.2.2".into(),
                "Postbox-0.2.2.zip".into(),
                zip,
                wrong_digest,
            )
        }
        UpdateScenario::DuplicateMatchingAssets
        | UpdateScenario::DraftOrPrerelease { .. }
        | UpdateScenario::HttpsToHttpRedirect302 => {
            let zip = build_store_zip(&[("postbox.exe", b"MZ-OK")]);
            let digest = sha256_hex(&zip);
            ("v0.2.3".into(), "Postbox-0.2.3.zip".into(), zip, digest)
        }
    }
}

fn handle_update_json(base: &str, state: &Arc<Mutex<MockExternalState>>) -> RawHttpResponse {
    let scenario = state.lock().unwrap().update_scenario.clone();
    let (tag, filename, zip, digest) = build_scenario_zip(&scenario);
    let size = zip.len();

    let (draft, prerelease) = match scenario {
        UpdateScenario::DraftOrPrerelease { draft, prerelease } => (draft, prerelease),
        _ => (false, false),
    };

    let mut assets = vec![json!({
        "name": filename,
        "browser_download_url": format!("{base}/releases/{filename}"),
        "size": size,
        "digest": format!("sha256:{digest}")
    })];

    if matches!(scenario, UpdateScenario::DuplicateMatchingAssets) {
        assets.push(json!({
            "name": "Postbox-0.2.3-duplicate.zip",
            "browser_download_url": format!("{base}/releases/Postbox-0.2.3-duplicate.zip"),
            "size": size,
            "digest": format!("sha256:{digest}")
        }));
    }

    let body_notes = "- IGSH-1683: Faster startup on Windows\n\
                      - Fix main.zig (#42)\n\
                      - See https://example.com\n\
                      - Voice notes resume where you left off";

    RawHttpResponse::json(
        200,
        &json!({
            "tag_name": tag,
            "draft": draft,
            "prerelease": prerelease,
            "body": body_notes,
            "history": [],
            "assets": assets
        }),
    )
}

fn handle_release_download(
    _path: &str,
    base: &str,
    state: &Arc<Mutex<MockExternalState>>,
) -> RawHttpResponse {
    let scenario = state.lock().unwrap().update_scenario.clone();
    if matches!(scenario, UpdateScenario::HttpsToHttpRedirect302) {
        return RawHttpResponse::empty(302)
            .with_header("Location", format!("{base}/releases/downgraded.zip"));
    }
    let (_, _, zip, _) = build_scenario_zip(&scenario);
    RawHttpResponse::bytes(200, "application/zip", zip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::LoopbackHttpClient;

    #[test]
    fn external_server_derives_port_from_stream_local_addr() {
        let server = MockExternalServer::start("test-key");
        assert_ne!(server.port(), 0);
        let client = LoopbackHttpClient::default();
        let resp = client
            .execute_raw(server.addr(), "GET", "/update.json", &[], None)
            .expect("GET /update.json");
        assert_eq!(resp.status, 200);
        let v: serde_json::Value = resp.json().expect("parse update.json");
        let url = v["assets"][0]["browser_download_url"]
            .as_str()
            .expect("download url");
        assert!(
            url.starts_with(&format!("http://127.0.0.1:{}/releases/", server.port())),
            "unexpected download url: {url}"
        );
    }
}
