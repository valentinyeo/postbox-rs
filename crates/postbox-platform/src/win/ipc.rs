//! Named pipe transport for the link and control channels.
//!
//! `CreateNamedPipeW` / `ConnectNamedPipe` are the only unsafe calls: once connected the
//! handle is handed to `std::fs::File`, which does the reading and writing.
//!
//! The client end uses `std::fs::OpenOptions` directly on the `\\.\pipe\...` path, which is
//! the documented way to open a named pipe as a file.

use crate::PlatformError;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::PCWSTR;

const BUFFER_BYTES: u32 = 64 * 1024;
/// How long the acceptor waits before retrying a pipe it cannot create.
const RETRY_MS: u64 = 250;

/// `\\.\pipe\<name>` as a path `std::fs` can open.
pub fn pipe_path(name: &str) -> PathBuf {
    if name.starts_with(r"\\.\pipe\") {
        PathBuf::from(name)
    } else {
        PathBuf::from(format!(r"\\.\pipe\{name}"))
    }
}

fn wide(value: &str) -> Vec<u16> {
    OsString::from(value).encode_wide().chain([0]).collect()
}

/// Opens the client end of `name`.
pub fn connect(name: &str) -> Result<File, PlatformError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_path(name))
        .map_err(PlatformError::Io)
}

/// Blocking one-shot request/response against a running instance.
pub fn pipe_round_trip(
    name: &str,
    request_line: &str,
    _timeout: Duration,
) -> Result<Option<String>, String> {
    let mut file = connect(name)
        .map_err(|err| format!("cannot reach {}: {err}", pipe_path(name).display()))?;
    file.write_all(format!("{request_line}\n").as_bytes())
        .map_err(|err| format!("write failed: {err}"))?;
    file.flush().map_err(|err| format!("flush failed: {err}"))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(line)),
        Err(err) => Err(format!("read failed: {err}")),
    }
}

/// Starts the blocking acceptor for `name` and a serve loop that answers one line per client.
pub fn start_pipe_server(
    name: &str,
    handler: impl Fn(&str) -> String + Send + Sync + 'static,
) -> Result<crate::control_pipe::ServerHandle, String> {
    let (sender, receiver) = mpsc::channel::<File>();
    let running = Arc::new(AtomicBool::new(true));
    let acceptor_running = Arc::clone(&running);
    let acceptor_name = name.to_owned();
    let handler = Arc::new(handler);
    let serve_handler = Arc::clone(&handler);

    std::thread::Builder::new()
        .name(format!("postbox-accept-{name}"))
        .spawn(move || accept_forever(&acceptor_name, sender, acceptor_running))
        .map_err(|err| format!("cannot start pipe acceptor: {err}"))?;

    std::thread::Builder::new()
        .name(format!("postbox-serve-{name}"))
        .spawn(move || {
            for mut file in receiver {
                let mut line = String::new();
                let response = match file.try_clone() {
                    Ok(probe) => {
                        let mut reader = BufReader::new(probe);
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => continue,
                            Ok(_) => serve_handler(line.trim_end()),
                        }
                    }
                    Err(_) => continue,
                };
                let _ = file.write_all(format!("{response}\n").as_bytes());
                let _ = file.flush();
                let _ = disconnect(&mut file);
            }
        })
        .map_err(|err| format!("cannot start pipe server: {err}"))?;

    Ok(crate::control_pipe::ServerHandle::from_stop(Box::new(
        move || running.store(false, Ordering::Relaxed),
    )))
}

fn accept_forever(name: &str, sender: Sender<File>, running: Arc<AtomicBool>) {
    let mut first = true;
    while running.load(Ordering::Relaxed) {
        match create_instance(name, first) {
            Ok(file) => {
                first = false;
                if sender.send(file).is_err() {
                    break;
                }
            }
            Err(_) => std::thread::sleep(Duration::from_millis(RETRY_MS)),
        }
    }
}

/// Creates one instance, waits for a client, and returns the connected handle as a `File`.
fn create_instance(name: &str, first: bool) -> Result<File, PlatformError> {
    let wide_name = wide(&pipe_path(name).to_string_lossy());
    let mut mode = PIPE_ACCESS_DUPLEX;
    if first {
        // Stop another app squatting on our pipe name before the first instance exists.
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    // SAFETY: `wide_name` is NUL-terminated UTF-16 and outlives the call; buffer sizes are
    // constants and no security attributes are requested.
    let handle: HANDLE = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide_name.as_ptr()),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_BYTES,
            BUFFER_BYTES,
            0,
            None,
        )
    };
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return Err(PlatformError::OsUnavailable);
    }

    // SAFETY: `handle` is a live pipe instance from `CreateNamedPipeW`. `ERROR_PIPE_CONNECTED`
    // means a client connected before we asked, which is success rather than failure.
    let connected = unsafe { ConnectNamedPipe(handle, None) };
    if connected.is_err() {
        // SAFETY: `GetLastError` reads the calling thread's last-error value.
        let last = unsafe { GetLastError() };
        if last != ERROR_PIPE_CONNECTED {
            // SAFETY: closing the instance we created and failed to connect; not used again.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(PlatformError::OsUnavailable);
        }
    }

    // SAFETY: the handle is connected and uniquely owned here; ownership moves into `File`,
    // which closes it exactly once on drop.
    Ok(unsafe { File::from_raw_handle(handle.0 as *mut _) })
}

/// Disconnects one served instance so the acceptor can reuse the slot.
pub fn disconnect(file: &mut File) -> Result<(), PlatformError> {
    use std::os::windows::io::AsRawHandle;
    let handle = HANDLE(file.as_raw_handle() as *mut _);
    // SAFETY: `handle` belongs to `file`, which is alive for the whole call.
    unsafe {
        let _ = DisconnectNamedPipe(handle);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::os_integration::{control_pipe_name, link_pipe_name};

    #[test]
    fn pipe_paths_are_absolute_win32_names() {
        assert_eq!(
            pipe_path("postbox-control-test"),
            PathBuf::from(r"\\.\pipe\postbox-control-test")
        );
        assert_eq!(
            pipe_path(r"\\.\pipe\postbox-link-test"),
            PathBuf::from(r"\\.\pipe\postbox-link-test")
        );
        assert_eq!(
            pipe_path(&control_pipe_name("valentin")),
            PathBuf::from(r"\\.\pipe\postbox-control-valentin")
        );
        assert_eq!(
            pipe_path(&link_pipe_name("valentin")),
            PathBuf::from(r"\\.\pipe\postbox-link-valentin")
        );
    }

    #[test]
    fn a_server_answers_a_round_trip_over_a_named_pipe() {
        let name = format!("postbox-ipc-test-{}", std::process::id());
        let server = start_pipe_server(&name, |line| format!("echo:{line}")).unwrap();
        // The acceptor thread needs a moment to create the first instance.
        let mut reply = None;
        for _ in 0..40 {
            match pipe_round_trip(&name, "hello", Duration::from_millis(500)) {
                Ok(Some(line)) => {
                    reply = Some(line);
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let reply = reply.expect("the pipe server answered");
        assert!(reply.contains("echo:hello"), "{reply}");
        server.shutdown();
    }

    #[test]
    fn a_missing_pipe_reports_a_reachable_error() {
        let err = pipe_round_trip(
            "postbox-ipc-absent-test",
            "hello",
            Duration::from_millis(100),
        )
        .expect_err("nothing is listening");
        assert!(err.contains("cannot reach"), "{err}");
    }
}
