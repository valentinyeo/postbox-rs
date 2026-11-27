use postbox_platform::exit_records::{clear_pending_crash_if_matches, read_pending_crash};
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_CRASH_URL: &str = "https://postbox-crashes.valentin-yeo.workers.dev/v1/crashes";
const UPLOAD_THREAD: &str = "postbox-crash-upload";
const STARTUP_UPLOAD_TIMEOUT: Duration = Duration::from_secs(2);

pub fn upload_pending_crash_blocking(data_dir: &Path) {
    upload_pending_crash_with_timeout(data_dir, STARTUP_UPLOAD_TIMEOUT);
}

fn upload_pending_crash_with_timeout(data_dir: &Path, timeout: Duration) {
    let url = std::env::var("POSTBOX_CRASH_URL").unwrap_or_else(|_| DEFAULT_CRASH_URL.to_owned());
    upload_to(data_dir, &url, timeout);
}

fn upload_to(data_dir: &Path, url: &str, timeout: Duration) {
    let Some(report) = read_pending_crash(data_dir) else {
        return;
    };
    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("postbox-crash-reporter")
        .build()
    else {
        return;
    };
    if let Ok(response) = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(report.clone())
        .send()
        && response.status().is_success()
    {
        clear_pending_crash_if_matches(data_dir, &report);
    }
}

pub fn install_panic_upload_hook(data_dir: &Path) {
    let data_dir = data_dir.to_owned();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        if std::thread::current().name() == Some(UPLOAD_THREAD) {
            return;
        }
        let dir = data_dir.clone();
        let (done, received) = std::sync::mpsc::channel();
        if std::thread::Builder::new()
            .name(UPLOAD_THREAD.to_owned())
            .spawn(move || {
                upload_pending_crash_with_timeout(&dir, Duration::from_secs(2));
                let _ = done.send(());
            })
            .is_ok()
        {
            // An aborting panic kills detached threads when the hook returns.
            let _ = received.recv_timeout(Duration::from_secs(2));
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_platform::exit_records::{PENDING_CRASH_FILE, clear_pending_crash};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    const REPORT: &str = r#"{"kind":"panic","version":"0.1.0","os":"windows-x86_64","uptime_ms":12,"message":"render failed","location":"desktop.rs:3:1","backtrace":"frame one\nframe two","timestamp_ms":123}"#;
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn pending_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "postbox-crash-upload-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(PENDING_CRASH_FILE), REPORT).unwrap();
        dir
    }

    fn server(
        status: u16,
        replacement_dir: Option<PathBuf>,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/crashes", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0; 1024];
            loop {
                let size = stream.read(&mut chunk).unwrap();
                assert_ne!(size, 0, "request ended before its JSON body");
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(end) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            if let Some(dir) = replacement_dir {
                std::fs::write(dir.join(PENDING_CRASH_FILE), r#"{"kind":"new_panic"}"#).unwrap();
            }
            write!(
                stream,
                "HTTP/1.1 {status} Result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            String::from_utf8(bytes).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn successful_upload_posts_the_json_and_clears_the_pending_report() {
        let dir = pending_dir();
        let (url, server) = server(201, None);
        upload_to(&dir, &url, Duration::from_secs(5));
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/crashes HTTP/1.1\r\n"));
        assert!(
            request
                .to_lowercase()
                .contains("content-type: application/json")
        );
        assert!(
            request
                .to_lowercase()
                .contains("user-agent: postbox-crash-reporter")
        );
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let report: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            report,
            serde_json::from_str::<serde_json::Value>(REPORT).unwrap()
        );
        assert_eq!(read_pending_crash(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejected_upload_keeps_the_report_for_next_startup() {
        let dir = pending_dir();
        let (url, server) = server(503, None);
        upload_to(&dir, &url, Duration::from_secs(5));
        server.join().unwrap();
        assert_eq!(read_pending_crash(&dir).as_deref(), Some(REPORT));
        clear_pending_crash(&dir);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn network_error_keeps_the_report() {
        let dir = pending_dir();
        upload_to(&dir, "not a URL", Duration::from_millis(100));
        assert_eq!(read_pending_crash(&dir).as_deref(), Some(REPORT));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn successful_old_upload_does_not_clear_a_new_crash() {
        let dir = pending_dir();
        let (url, server) = server(201, Some(dir.clone()));
        upload_to(&dir, &url, Duration::from_secs(5));
        server.join().unwrap();
        assert_eq!(
            read_pending_crash(&dir).as_deref(),
            Some(r#"{"kind":"new_panic"}"#)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn startup_upload_times_out_and_keeps_the_report() {
        let dir = pending_dir();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/crashes", listener.local_addr().unwrap());
        let (release, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
            drop(stream);
        });

        let started = std::time::Instant::now();
        upload_to(&dir, &url, STARTUP_UPLOAD_TIMEOUT);
        let elapsed = started.elapsed();
        let _ = release.send(());
        server.join().unwrap();

        assert!(
            elapsed >= STARTUP_UPLOAD_TIMEOUT,
            "returned early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(4),
            "timed out late: {elapsed:?}"
        );
        assert_eq!(read_pending_crash(&dir).as_deref(), Some(REPORT));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn blocking_startup_without_a_report_is_a_noop() {
        let dir = pending_dir();
        clear_pending_crash(&dir);
        upload_pending_crash_blocking(&dir);
        assert_eq!(read_pending_crash(&dir), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
