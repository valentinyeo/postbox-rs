//! Reverse channel for `postboxctl`: a small line server plus the client end.
//!
//! The Windows build listens on `\\.\pipe\postbox-control-<user>` through
//! `crate::win::ipc`. Every other target, and every test, uses a portable Unix-domain
//! socket in the app data directory, so a full request/response round trip runs on Linux CI.
//! Both ends only ever move one JSON line per connection.

#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a server thread waits for one request line before giving up.
pub const READ_TIMEOUT_MS: u64 = 2_000;
/// How long `postboxctl` waits for the app to answer.
pub const CONNECT_TIMEOUT_MS: u64 = 2_000;

/// Where the control channel lives on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `\\.\pipe\<name>` (Windows).
    NamedPipe { name: String },
    /// A Unix-domain socket file (portable, used by tests on every OS).
    UnixSocket { path: PathBuf },
}

impl Transport {
    /// A pipe name that can never collide between users or test runs.
    pub fn pipe(name: &str) -> Self {
        Self::NamedPipe {
            name: name.to_owned(),
        }
    }

    pub fn socket(path: impl Into<PathBuf>) -> Self {
        Self::UnixSocket { path: path.into() }
    }

    /// Parses `--pipe`: a path when it contains a separator, a named pipe otherwise.
    pub fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.starts_with(r"\\.\pipe\") || trimmed.starts_with("\\\\.\\pipe\\") {
            return Self::pipe(trimmed.split('\\').next_back().unwrap_or(trimmed));
        }
        if trimmed.contains(['/', '\\']) || trimmed.ends_with(".sock") {
            return Self::socket(trimmed);
        }
        Self::pipe(trimmed)
    }

    /// Human-readable endpoint, for error messages and `postboxctl --help`.
    pub fn display(&self) -> String {
        match self {
            Self::NamedPipe { name } => format!(r"\\.\pipe\{name}"),
            Self::UnixSocket { path } => path.display().to_string(),
        }
    }
}

/// One line in, one line out. `Ok(None)` means the peer hung up without answering.
pub fn round_trip(
    transport: &Transport,
    request_line: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    match transport {
        Transport::NamedPipe { name } => {
            #[cfg(windows)]
            {
                crate::win::ipc::pipe_round_trip(name, request_line, timeout)
            }
            #[cfg(not(windows))]
            {
                let _ = timeout;
                Err(format!(
                    "named pipes are Windows-only; use a socket path instead of {name}"
                ))
            }
        }
        Transport::UnixSocket { path } => {
            #[cfg(unix)]
            {
                unix_round_trip(path, request_line, timeout)
            }
            #[cfg(not(unix))]
            {
                let _ = (path, request_line, timeout);
                Err("unix sockets are not available on this target".to_owned())
            }
        }
    }
}

/// Starts the control server. The returned handle keeps it alive and stops it on drop.
pub fn serve(
    transport: &Transport,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<ServerHandle, String> {
    match transport {
        Transport::NamedPipe { name } => start_named_pipe_server(name.clone(), handler),
        Transport::UnixSocket { path } => start_unix_server(path.clone(), handler),
    }
}

#[cfg(windows)]
fn start_named_pipe_server(
    name: String,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<ServerHandle, String> {
    crate::win::ipc::start_pipe_server(&name, handler)
}

#[cfg(not(windows))]
fn start_named_pipe_server(
    name: String,
    _handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<ServerHandle, String> {
    Err(format!(
        "named pipes are Windows-only; {name} needs a socket path on this host"
    ))
}

/// Keeps the serve loop alive; dropping it closes the listener.
pub struct ServerHandle {
    stop: Option<Box<dyn FnOnce() + Send>>,
}

impl ServerHandle {
    pub fn endpoint(&self) -> &'static str {
        "control"
    }

    /// Wraps a platform-specific stop action (the Windows pipe acceptor needs one).
    pub fn from_stop(stop: Box<dyn FnOnce() + Send>) -> Self {
        Self { stop: Some(stop) }
    }

    pub fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
    }
}

/// The endpoint this host's running Postbox listens on.
///
/// Windows uses the per-user named pipe; every other target uses the per-user socket in the
/// same data directory the app stores its database in.
pub fn default_control_transport() -> Transport {
    let user = postbox_core::os_integration::current_user();
    if cfg!(windows) {
        return Transport::pipe(&postbox_core::os_integration::control_pipe_name(&user));
    }
    Transport::socket(socket_path(&default_data_dir(), &user))
}

/// App data directory, matching what `postbox-app` uses for its database.
pub fn default_data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join("Postbox");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("postbox");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("postbox");
        }
    }
    std::env::temp_dir().join("postbox")
}

/// Socket path for a user's app data directory.
pub fn socket_path(data_dir: &Path, user: &str) -> PathBuf {
    data_dir.join(format!("control-{user}.sock"))
}

#[cfg(unix)]
fn unix_round_trip(
    path: &Path,
    request_line: &str,
    timeout: Duration,
) -> Result<Option<String>, String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(path)
        .map_err(|err| format!("cannot reach Postbox at {}: {err}", path.display()))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    stream
        .write_all(format!("{request_line}\n").as_bytes())
        .map_err(|err| format!("write failed: {err}"))?;
    stream
        .flush()
        .map_err(|err| format!("flush failed: {err}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line)),
        Err(err) => Err(format!("read failed: {err}")),
    }
}

#[cfg(not(unix))]
fn start_unix_server(
    path: PathBuf,
    _handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<ServerHandle, String> {
    Err(format!(
        "unix sockets are not available on this target; {} needs a pipe name",
        path.display()
    ))
}

#[cfg(unix)]
fn start_unix_server(
    path: PathBuf,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<ServerHandle, String> {
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // A leftover socket from a crashed run would block `bind`.
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)
        .map_err(|err| format!("cannot listen on {}: {err}", path.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("cannot set non-blocking: {err}"))?;

    let running = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&running);
    let handler = Arc::new(handler);
    let socket = path.clone();
    std::thread::Builder::new()
        .name("postbox-control".to_owned())
        .spawn(move || {
            while flag.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let handler = Arc::clone(&handler);
                        std::thread::spawn(move || {
                            let _ = stream
                                .set_read_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)));
                            let _ = stream
                                .set_write_timeout(Some(Duration::from_millis(READ_TIMEOUT_MS)));
                            let mut reader = BufReader::new(stream);
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let response = handler(line.trim_end());
                            let writer = reader.get_mut();
                            let _ = writer.write_all(format!("{response}\n").as_bytes());
                            let _ = writer.flush();
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = std::fs::remove_file(&socket);
        })
        .map_err(|err| format!("cannot start control thread: {err}"))?;

    Ok(ServerHandle {
        stop: Some(Box::new(move || {
            running.store(false, Ordering::Relaxed);
            let _ = std::fs::remove_file(&path);
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_parses_a_win32_pipe_name() {
        assert_eq!(
            Transport::parse(r"\\.\pipe\postbox-control-valentin"),
            Transport::pipe("postbox-control-valentin")
        );
        assert_eq!(
            Transport::parse(r"\\.\pipe\postbox-control-valentin").display(),
            r"\\.\pipe\postbox-control-valentin"
        );
    }

    #[test]
    fn transport_parses_a_socket_path_or_a_bare_name() {
        assert_eq!(
            Transport::parse("/tmp/postbox/control.sock"),
            Transport::socket("/tmp/postbox/control.sock")
        );
        assert_eq!(
            Transport::parse("relative/control.sock"),
            Transport::socket("relative/control.sock")
        );
        assert_eq!(
            Transport::parse("postbox-control-test"),
            Transport::pipe("postbox-control-test")
        );
    }

    #[test]
    fn the_default_endpoint_is_the_per_user_name() {
        let transport = default_control_transport();
        let user = postbox_core::os_integration::current_user();
        if cfg!(windows) {
            assert_eq!(
                transport.display(),
                format!(r"\\.\pipe\postbox-control-{user}")
            );
        } else {
            assert!(
                transport.display().contains(&format!("control-{user}")),
                "{}",
                transport.display()
            );
        }
    }

    #[test]
    fn socket_path_is_per_user_and_inside_the_data_dir() {
        let path = socket_path(Path::new("/tmp/postbox"), "valentin");
        assert_eq!(path, PathBuf::from("/tmp/postbox/control-valentin.sock"));
        assert_ne!(
            socket_path(Path::new("/tmp/postbox"), "a"),
            socket_path(Path::new("/tmp/postbox"), "b")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_round_trip_over_a_local_socket_returns_the_server_line() {
        let dir = std::env::temp_dir().join(format!("postbox-ctl-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transport = Transport::socket(dir.join("control.sock"));

        let server = serve(&transport, |line: &str| {
            postbox_core::control::encode_response(
                &postbox_core::control::ControlResponse::with_message(format!("echo:{line}")),
            )
        })
        .expect("server starts");

        let reply = round_trip(&transport, r#"{"cmd":"status"}"#, Duration::from_secs(2))
            .expect("no transport error");
        let reply = reply.expect("server answered");
        assert!(reply.contains(r#""ok":true"#), "{reply}");
        let decoded = postbox_core::control::decode_response(&reply).expect("valid JSON line");
        assert_eq!(decoded.message.as_deref(), Some(r#"echo:{"cmd":"status"}"#));

        // A second call works too: the listener must keep accepting.
        let again = round_trip(&transport, r#"{"cmd":"theme"}"#, Duration::from_secs(2))
            .expect("no transport error")
            .expect("server answered again");
        let decoded = postbox_core::control::decode_response(&again).expect("valid JSON line");
        assert_eq!(decoded.message.as_deref(), Some(r#"echo:{"cmd":"theme"}"#));

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_server_reports_an_error_instead_of_hanging() {
        let dir = std::env::temp_dir().join(format!("postbox-ctl-gone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transport = Transport::socket(dir.join("missing.sock"));
        let err = round_trip(
            &transport,
            r#"{"cmd":"status"}"#,
            Duration::from_millis(200),
        )
        .expect_err("no server is listening");
        assert!(err.contains("cannot reach Postbox"), "{err}");
        assert!(err.contains("missing.sock"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_named_pipe_transport_is_refused_cleanly_where_pipes_do_not_exist() {
        let transport = Transport::pipe("postbox-control-test");
        let result = round_trip(&transport, r#"{"cmd":"status"}"#, Duration::from_millis(50));
        if cfg!(windows) {
            assert!(result.is_err() || result.unwrap().is_some());
        } else {
            let err = result.expect_err("named pipes need Windows");
            assert!(err.contains("Windows-only"), "{err}");
        }
    }
}
