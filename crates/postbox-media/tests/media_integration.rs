use postbox_core::{ChatKey, MediaKind, Provider};
use postbox_hub::{Endpoint, HubClient};
use postbox_media::MediaError;
use postbox_media::drain::{DownloadOutcome, MediaDownloader};
use postbox_media::giphy::{GiphyClient, parse_retry_after_secs};
use postbox_media::unfurl::{UnfurlProvider, UnfurlService, classify_url};
use postbox_platform::MemoryPlatform;
use postbox_store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

struct MockRoute {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay_ms: u64,
}

async fn spawn_mock_http_server(
    handler: impl Fn(&str, usize) -> MockRoute + Send + Sync + 'static,
) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));

    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let h = handler.clone();
            let hits = hits.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let Ok(n) = sock.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                let req_str = String::from_utf8_lossy(&buf[..n]);
                let first_line = req_str.lines().next().unwrap_or("");
                let path = first_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                let count = {
                    let mut map = hits.lock().unwrap();
                    let entry = map.entry(path.clone()).or_insert(0);
                    *entry += 1;
                    *entry
                };
                let route = h(&path, count);
                if route.delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(route.delay_ms)).await;
                }
                let reason = match route.status {
                    200 => "OK",
                    202 => "Accepted",
                    206 => "Partial Content",
                    302 => "Found",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    410 => "Gone",
                    429 => "Too Many Requests",
                    503 => "Service Unavailable",
                    _ => "Error",
                };
                let mut resp = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    route.status,
                    reason,
                    route.body.len()
                );
                for (k, v) in route.headers {
                    resp.push_str(&format!("{k}: {v}\r\n"));
                }
                resp.push_str("\r\n");
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&route.body).await;
            });
        }
    });
    (port, handle)
}

fn temp_dir(prefix: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}_{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[tokio::test]
async fn loopback_media_download_200_202_206_410_and_429() {
    let (port, server) = spawn_mock_http_server(|path, hit| {
        if path.ends_with("/m_ok") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: b"\x89PNG\r\n\x1a\nPAYLOAD".to_vec(),
                delay_ms: 0,
            }
        } else if path.ends_with("/m_poll") {
            if hit < 2 {
                MockRoute {
                    status: 202,
                    headers: vec![],
                    body: Vec::new(),
                    delay_ms: 0,
                }
            } else {
                MockRoute {
                    status: 200,
                    headers: vec![("Content-Type".into(), "image/jpeg".into())],
                    body: b"\xFF\xD8\xFFJPG".to_vec(),
                    delay_ms: 0,
                }
            }
        } else if path.ends_with("/m_206") {
            MockRoute {
                status: 206,
                headers: vec![],
                body: b"PARTIAL".to_vec(),
                delay_ms: 0,
            }
        } else if path.ends_with("/m_gone") {
            MockRoute {
                status: 410,
                headers: vec![],
                body: b"gone".to_vec(),
                delay_ms: 0,
            }
        } else if path.ends_with("/m_429") {
            MockRoute {
                status: 429,
                headers: vec![],
                body: b"slow down".to_vec(),
                delay_ms: 0,
            }
        } else {
            MockRoute {
                status: 404,
                headers: vec![],
                body: b"missing".to_vec(),
                delay_ms: 0,
            }
        }
    })
    .await;

    let endpoint = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
    let client = HubClient::new(endpoint, "test-token").unwrap();
    let store = Store::open_in_memory().unwrap();
    let plat = MemoryPlatform::default();
    let cache_dir = temp_dir("postbox_dl_int");
    let downloader = MediaDownloader::new(cache_dir.clone()).with_timeouts(
        Duration::from_millis(15),
        Duration::from_secs(2),
        Duration::from_secs(5),
    );
    let chat = ChatKey::new(Provider::Whatsapp, "main", "1@s.whatsapp.net");

    // 1. 200 OK -> atomic write + .png alias + ZoneMarker called + SQLite media_cache row
    let ok = downloader
        .download_one(
            &client,
            &store,
            Some(&plat),
            &chat,
            "m_ok",
            None,
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    match ok {
        DownloadOutcome::Ready {
            path,
            mime_type,
            sha256,
        } => {
            assert_eq!(mime_type, "image/png");
            assert_eq!(sha256.len(), 64);
            assert!(path.to_string_lossy().ends_with(".png"));
            assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\nPAYLOAD");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
    assert!(!plat.zone_marked_paths().is_empty());

    // 2. 202 Pending -> polls -> 200 OK
    let polled = downloader
        .download_one(
            &client,
            &store,
            Some(&plat),
            &chat,
            "m_poll",
            None,
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    assert!(matches!(polled, DownloadOutcome::Ready { .. }));

    // 3. 206 Partial Content rejected
    let partial = downloader
        .download_one(
            &client,
            &store,
            Some(&plat),
            &chat,
            "m_206",
            None,
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    assert_eq!(partial, DownloadOutcome::OtherFailure { http_status: 206 });

    // 4. 410 Gone permanently persisted in SQLite gone_media
    let gone = downloader
        .download_one(
            &client,
            &store,
            Some(&plat),
            &chat,
            "m_gone",
            None,
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    assert_eq!(gone, DownloadOutcome::Gone);
    assert!(store.is_media_gone(&chat, "m_gone").unwrap());

    // 5. 429 classified as TransientFailure
    let limited = downloader
        .download_one(
            &client,
            &store,
            Some(&plat),
            &chat,
            "m_429",
            None,
            None,
            Some(MediaKind::Document),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        limited,
        DownloadOutcome::TransientFailure { http_status: 429 }
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn loopback_giphy_search_download_429_redirect_and_cancel() {
    let (port, server) = spawn_mock_http_server(|path, _hit| {
        if path.starts_with("/v1/gifs/search") && path.contains("q=rate") {
            MockRoute {
                status: 429,
                headers: vec![("Retry-After".into(), "42".into())],
                body: b"{}".to_vec(),
                delay_ms: 0,
            }
        } else if path.starts_with("/v1/gifs/search") && path.contains("q=slow") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: b"{}".to_vec(),
                delay_ms: 2_000,
            }
        } else if path.starts_with("/v1/gifs/trending") || path.starts_with("/v1/gifs/search") {
            let body = serde_json::json!({
                "meta": { "status": 200 },
                "pagination": { "total_count": 1, "count": 1, "offset": 0 },
                "data": [{
                    "type": "gif",
                    "id": "CatGif1",
                    "title": "Happy Cat",
                    "images": {
                        "fixed_width": {
                            "url": "https://media1.giphy.com/media/CatGif1/200w.gif",
                            "width": "200",
                            "height": "150",
                            "size": "1200"
                        },
                        "original": {
                            "url": "https://media2.giphy.com/media/CatGif1/giphy.gif",
                            "width": "400",
                            "height": "300",
                            "size": "4096"
                        }
                    }
                }]
            });
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: serde_json::to_vec(&body).unwrap(),
                delay_ms: 0,
            }
        } else if path == "/media/CatGif1/giphy.gif" {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "image/gif".into())],
                body: b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec(),
                delay_ms: 0,
            }
        } else if path == "/media/CatGif1/redir.gif" {
            MockRoute {
                status: 302,
                headers: vec![("Location".into(), "http://127.0.0.1/evil".into())],
                body: Vec::new(),
                delay_ms: 0,
            }
        } else {
            MockRoute {
                status: 404,
                headers: vec![],
                body: Vec::new(),
                delay_ms: 0,
            }
        }
    })
    .await;

    let client = GiphyClient::with_loopback_port(Some(port)).unwrap();
    let (_tx, mut rx) = watch::channel(false);

    // Trending search + valid GIF download
    let page = client
        .search_or_trending("demo-key", "", 25, 0, &mut rx)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    let gif_bytes = client
        .download_gif("CatGif1", &page.items[0].original.url, false, &mut rx)
        .await
        .unwrap();
    assert!(gif_bytes.starts_with(b"GIF89a"));

    // 429 with Retry-After header
    let err_429 = client
        .search_or_trending("demo-key", "rate", 25, 0, &mut rx)
        .await
        .unwrap_err();
    assert!(matches!(
        err_429,
        MediaError::RateLimited {
            retry_after_secs: Some(42)
        }
    ));
    assert_eq!(parse_retry_after_secs("42"), Some(42));

    // 302 redirect MUST NOT be followed!
    let err_redir = client
        .download_gif(
            "CatGif1",
            "https://media1.giphy.com/media/CatGif1/redir.gif",
            false,
            &mut rx,
        )
        .await
        .unwrap_err();
    assert!(matches!(err_redir, MediaError::HttpStatus(302)));

    // Cancellable in-flight request
    let (cancel_tx, mut cancel_rx) = watch::channel(false);
    let c2 = client.clone();
    let task = tokio::spawn(async move {
        c2.search_or_trending("demo-key", "slow", 25, 0, &mut cancel_rx)
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = cancel_tx.send(true);
    let cancelled = task.await.unwrap().unwrap_err();
    assert!(matches!(cancelled, MediaError::Cancelled));

    server.abort();
}

#[tokio::test]
async fn loopback_unfurl_og_page_youtube_disk_cache_and_ssrf_block() {
    let (port, server) = spawn_mock_http_server(|path, _hit| {
        if path == "/article" {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "text/html".into())],
                body: br#"<html><head>
                    <meta property="og:title" content="Rust &amp; GPUI Guide">
                    <meta property="og:description" content="Fast native desktop apps">
                    <meta property="og:image" content="/thumb.png">
                </head></html>"#
                    .to_vec(),
                delay_ms: 0,
            }
        } else if path == "/thumb.png" || path.ends_with("/hqdefault.jpg") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: b"\x89PNG\r\n\x1a\nTHUMB".to_vec(),
                delay_ms: 0,
            }
        } else if path.starts_with("/oembed") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: br#"{"title":"Demo Talk","author_name":"Systems Lab"}"#.to_vec(),
                delay_ms: 0,
            }
        } else if path.starts_with("/watch?v=") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "text/html".into())],
                body: br#"var x = "https://www.googlevideo.com/videoplayback?id=1\u0026itag=18";"#
                    .to_vec(),
                delay_ms: 0,
            }
        } else {
            MockRoute {
                status: 404,
                headers: vec![],
                body: Vec::new(),
                delay_ms: 0,
            }
        }
    })
    .await;

    let cache_dir = temp_dir("postbox_unfurl_int");
    let page_url = format!("http://127.0.0.1:{port}/article");
    let classified_page = classify_url(&page_url).unwrap();
    assert_eq!(classified_page.provider, UnfurlProvider::Page);

    // Production (non-loopback) UnfurlService MUST block 127.0.0.1 via SSRF guard!
    let prod_svc = UnfurlService::new(cache_dir.clone()).unwrap();
    let ssrf_err = prod_svc.fetch_or_load(&classified_page).await.unwrap_err();
    assert!(matches!(ssrf_err, MediaError::SsrfBlocked));

    // Loopback test service fetches OG page + relative image and writes atomic disk cache.
    let test_svc = UnfurlService::new_for_loopback_test(
        cache_dir.clone(),
        Some(format!("http://127.0.0.1:{port}")),
    )
    .unwrap();
    let card = test_svc.fetch_or_load(&classified_page).await.unwrap();
    assert_eq!(card.title, "Rust & GPUI Guide");
    assert_eq!(card.description, "Fast native desktop apps");
    assert!(card.thumb_path.as_ref().is_some_and(|p| p.is_file()));

    // Disk cache hit works cleanly even after server stops!
    let yt = classify_url("https://youtu.be/dQw4w9WgXcQ").unwrap();
    let yt_card = test_svc.fetch_or_load(&yt).await.unwrap();
    assert_eq!(yt_card.title, "Demo Talk");
    assert_eq!(yt_card.author, "Systems Lab");
    assert_eq!(
        yt_card.play_url.as_deref(),
        Some("https://www.googlevideo.com/videoplayback?id=1&itag=18")
    );

    server.abort();
    let cached_again = test_svc.load_from_disk(&classified_page).unwrap();
    assert_eq!(cached_again.title, "Rust & GPUI Guide");

    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn loopback_youtube_all_failed_returns_err_and_does_not_write_empty_disk_cache() {
    let (port, server) = spawn_mock_http_server(|_path, _hit| MockRoute {
        status: 404,
        headers: vec![],
        body: Vec::new(),
        delay_ms: 0,
    })
    .await;

    let cache_dir = temp_dir("postbox_unfurl_yt_404");
    let test_svc = UnfurlService::new_for_loopback_test(
        cache_dir.clone(),
        Some(format!("http://127.0.0.1:{port}")),
    )
    .unwrap();
    let yt = classify_url("https://youtu.be/dQw4w9WgXcQ").unwrap();
    let err = test_svc
        .fetch_or_load(&yt)
        .await
        .expect_err("all-404 YouTube unfurl must return Err");
    assert!(matches!(err, MediaError::Transport(_)));
    assert!(
        test_svc.load_from_disk(&yt).is_none(),
        "empty JSON cache must not be written when all YouTube requests fail"
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn loopback_media_redownload_replaces_stale_extension_alias_and_preserves_cached_mime() {
    let (port, server) = spawn_mock_http_server(|path, _hit| {
        if path.contains("/m_alias") {
            MockRoute {
                status: 200,
                headers: vec![("Content-Type".into(), "image/png".into())],
                body: b"NEW-BYTES-V2".to_vec(),
                delay_ms: 0,
            }
        } else {
            MockRoute {
                status: 404,
                headers: vec![],
                body: Vec::new(),
                delay_ms: 0,
            }
        }
    })
    .await;

    let cache_dir = temp_dir("postbox_alias_refresh");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let chat = ChatKey::new(Provider::Whatsapp, "main", "alias@s.whatsapp.net");
    let key = postbox_media::drain::media_cache_key(&chat, "m_alias");
    let stale_alias = cache_dir.join(format!("{key}.bin.png"));
    std::fs::write(&stale_alias, b"STALE-OLD-BYTES").unwrap();

    let store = Store::open_in_memory().unwrap();
    let ep = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
    let client = HubClient::new(ep, "tok").unwrap();
    let downloader = MediaDownloader::new(cache_dir.clone());

    let out1 = downloader
        .download_one(
            &client,
            &store,
            None,
            &chat,
            "m_alias",
            Some("pic.png"),
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    let DownloadOutcome::Ready {
        path,
        mime_type,
        sha256,
    } = out1
    else {
        panic!("expected Ready, got {out1:?}");
    };
    assert_eq!(mime_type, "image/png");
    assert_eq!(sha256.len(), 64);
    assert_eq!(std::fs::read(&path).unwrap(), b"NEW-BYTES-V2");

    // Second call with mime_type: None must preserve persisted "image/png" and sha256 from SQLite!
    let out2 = downloader
        .download_one(
            &client,
            &store,
            None,
            &chat,
            "m_alias",
            None,
            None,
            Some(MediaKind::Image),
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        out2,
        DownloadOutcome::Ready {
            path,
            mime_type: "image/png".to_owned(),
            sha256,
        }
    );

    server.abort();
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn loopback_download_to_cache_owned_runs_3_concurrent_on_tokio_spawn() {
    use postbox_media::drain::{CachedDownloadResult, MediaDownloadSpec};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let current = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let cur_srv = current.clone();
    let peak_srv = peak.clone();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let cur = cur_srv.clone();
            let pk = peak_srv.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
                pk.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(60)).await;
                cur.fetch_sub(1, Ordering::SeqCst);
                let resp = "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\nConnection: close\r\n\r\nDATA";
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cache_dir = temp_dir("postbox_3_concurrent");
    let downloader = Arc::new(MediaDownloader::new(cache_dir.clone()));
    let ep = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
    let client = Arc::new(HubClient::new(ep, "tok").unwrap());
    let chat = ChatKey::new(Provider::Whatsapp, "main", "conc@s.whatsapp.net");
    let store = Store::open_in_memory().unwrap();

    let mut handles = Vec::new();
    for i in 0..6 {
        let spec = MediaDownloadSpec {
            chat: chat.clone(),
            msg_id: format!("m_{i}"),
            filename: Some(format!("f_{i}.png")),
            mime_type: Some("image/png".into()),
            media_kind: Some(MediaKind::Image),
        };
        let dl = downloader.clone();
        let cl = client.clone();
        handles.push(tokio::spawn(async move {
            (i, dl.download_to_cache_owned(cl, None, spec).await)
        }));
    }

    for h in handles {
        let (i, res) = h.await.unwrap();
        let cached = res.unwrap();
        assert!(matches!(cached, CachedDownloadResult::Ready { .. }));
        let committed =
            MediaDownloader::commit_download_result(&store, &chat, &format!("m_{i}"), cached)
                .unwrap();
        assert!(matches!(committed, DownloadOutcome::Ready { .. }));
    }
    assert_eq!(peak.load(Ordering::SeqCst), 3);

    server.abort();
    let _ = std::fs::remove_dir_all(&cache_dir);
}

#[tokio::test]
async fn loopback_transport_error_classifies_as_other_failure_zero() {
    use postbox_media::drain::{CachedDownloadResult, MediaDownloadSpec};

    // Bind and immediately drop a TCP listener to guarantee a closed loopback port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let cache_dir = temp_dir("postbox_transport_err");
    let downloader = Arc::new(MediaDownloader::new(cache_dir.clone()));
    let ep = Endpoint::parse(&format!("http://127.0.0.1:{port}"), true).unwrap();
    let client = Arc::new(HubClient::new(ep, "tok").unwrap());
    let spec = MediaDownloadSpec {
        chat: ChatKey::new(Provider::Whatsapp, "main", "closed@s.whatsapp.net"),
        msg_id: "m_closed".into(),
        filename: None,
        mime_type: None,
        media_kind: Some(MediaKind::Image),
    };
    let res = downloader
        .download_to_cache_owned(client, None, spec)
        .await
        .unwrap();
    assert_eq!(res, CachedDownloadResult::OtherFailure { http_status: 0 });
    let _ = std::fs::remove_dir_all(&cache_dir);
}
