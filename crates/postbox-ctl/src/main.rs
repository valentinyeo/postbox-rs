//! `postboxctl` — drives a running Postbox over its control pipe.
//!
//! Every command becomes one [`ControlRequest`] JSON line on
//! `\\.\pipe\postbox-control-<user>` (Windows) or the per-user control socket, and the answer
//! is one [`ControlResponse`] JSON line on stdout.
//!
//! `--pipe <path>` overrides the endpoint, which is what the round-trip tests use.

use postbox_core::control::{ControlRequest, ControlResponse, decode_response, encode_request};
use postbox_core::{Provider, ThemeChoice};
use postbox_platform::control_pipe::{Transport, round_trip};
use std::time::Duration;

const USAGE: &str = "\
usage: postboxctl [--pipe <path>] [--timeout <ms>] <command> [args]

commands:
  status                          live RAM, FPS, outbox and the active view
  theme <zinc|amoled|light>       switch theme
  font <60..160>                  text scale percentage
  chats [--provider P] [--archived] [--all] [--limit N]
  search <text>                   full-text search across messages
  view <id|inbox|whatsapp|slack|telegram|signal>
  select <id> [--provider P] [--account A]
  messages [id] [--limit N]
  archive <id>
  unarchive <id>
  archive-many <id...> [--unarchive]

options:
  --pipe <path>                   endpoint override (a named pipe name or a socket path)
  --timeout <ms>                  how long to wait for the app (default 2000)
  --dry-run                       print the request line and send nothing
";

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cli {
    pub pipe: Option<String>,
    pub timeout_ms: u64,
    pub request: ControlRequest,
    /// Prints the request line instead of sending it, so the parser is drivable with no app.
    pub dry_run: bool,
}

/// Parses `argv` (without the program name).
pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut pipe = None;
    let mut timeout_ms = 2_000u64;
    let mut dry_run = false;
    let mut positional: Vec<String> = Vec::new();
    let mut options = Options::default();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pipe" => {
                pipe = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--pipe needs a path".to_owned())?,
                );
            }
            "--timeout" => {
                timeout_ms = iter
                    .next()
                    .and_then(|raw| raw.parse().ok())
                    .ok_or_else(|| "--timeout needs milliseconds".to_owned())?;
            }
            "--provider" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--provider needs a value".to_owned())?;
                options.provider = Some(Provider::parse(raw).ok_or_else(|| {
                    format!("unknown provider: {raw} (whatsapp|slack|telegram|signal)")
                })?);
            }
            "--account" => {
                options.account = Some(
                    iter.next()
                        .cloned()
                        .ok_or_else(|| "--account needs a value".to_owned())?,
                );
            }
            "--limit" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| "--limit needs a number".to_owned())?;
                options.limit = Some(
                    raw.parse::<u32>()
                        .map_err(|_| format!("--limit needs a number, got {raw}"))?,
                );
            }
            "--dry-run" => dry_run = true,
            "--archived" => options.archived = true,
            "--all" => options.all = true,
            "--unarchive" => options.unarchive = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other if other.starts_with("--") => {
                return Err(format!("unknown option: {other}\n\n{USAGE}"));
            }
            other => positional.push(other.to_owned()),
        }
    }

    let request = build_request(&positional, &options)?;
    Ok(Cli {
        pipe,
        timeout_ms,
        request,
        dry_run,
    })
}

/// Switches a command line may set, already validated.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Options {
    provider: Option<Provider>,
    account: Option<String>,
    limit: Option<u32>,
    archived: bool,
    all: bool,
    unarchive: bool,
}

fn build_request(positional: &[String], options: &Options) -> Result<ControlRequest, String> {
    let mut ids: Vec<String> = positional.to_vec();
    let command = if ids.is_empty() {
        "status".to_owned()
    } else {
        ids.remove(0)
    };

    match command.as_str() {
        "status" => Ok(ControlRequest::Status),
        "theme" => {
            let raw = ids
                .first()
                .ok_or_else(|| "usage: postboxctl theme <zinc|amoled|light>".to_owned())?;
            let theme = match raw.as_str() {
                "zinc" | "dark" | "zincdark" => ThemeChoice::ZincDark,
                "amoled" => ThemeChoice::Amoled,
                "light" => ThemeChoice::Light,
                other => return Err(format!("unknown theme: {other}")),
            };
            Ok(ControlRequest::Theme { theme })
        }
        "font" => {
            let percent = ids
                .first()
                .ok_or_else(|| "usage: postboxctl font <60..160>".to_owned())?
                .parse::<u16>()
                .map_err(|_| "usage: postboxctl font <60..160>".to_owned())?;
            Ok(ControlRequest::Font { percent })
        }
        "chats" => Ok(ControlRequest::Chats {
            provider: options.provider,
            archived: options.archived,
            all: options.all,
            limit: options.limit,
        }),
        "search" => Ok(ControlRequest::Search {
            text: ids.join(" "),
        }),
        "view" => {
            let target = ids
                .first()
                .cloned()
                .ok_or_else(|| "usage: postboxctl view <id|inbox|provider>".to_owned())?;
            Ok(ControlRequest::View { target })
        }
        "select" => {
            let id = ids
                .first()
                .cloned()
                .ok_or_else(|| "usage: postboxctl select <id>".to_owned())?;
            Ok(ControlRequest::Select {
                id,
                provider: options.provider,
                account: options.account.clone(),
            })
        }
        "messages" => Ok(ControlRequest::Messages {
            id: ids.first().cloned(),
            limit: options.limit,
        }),
        "archive" => {
            let id = ids
                .first()
                .cloned()
                .ok_or_else(|| "usage: postboxctl archive <id>".to_owned())?;
            Ok(ControlRequest::Archive { id })
        }
        "unarchive" => {
            let id = ids
                .first()
                .cloned()
                .ok_or_else(|| "usage: postboxctl unarchive <id>".to_owned())?;
            Ok(ControlRequest::Unarchive { id })
        }
        "archive-many" => {
            if ids.is_empty() {
                return Err("usage: postboxctl archive-many <id...> [--unarchive]".to_owned());
            }
            Ok(ControlRequest::ArchiveMany {
                ids,
                archived: !options.unarchive,
            })
        }
        other => Err(format!("unknown command: {other}\n\n{USAGE}")),
    }
}

/// Sends one request and returns the decoded answer.
pub fn send(
    transport: &Transport,
    request: &ControlRequest,
    timeout_ms: u64,
) -> Result<ControlResponse, String> {
    let line = encode_request(request);
    let reply = round_trip(transport, &line, Duration::from_millis(timeout_ms))?
        .ok_or_else(|| "Postbox closed the connection without answering".to_owned())?;
    decode_response(&reply)
}

fn default_transport() -> Transport {
    postbox_platform::control_pipe::default_control_transport()
}

/// Pretty one-line summary for a human, plus the raw JSON when asked.
pub fn print_response(response: &ControlResponse, raw: &str, as_json: bool) {
    if as_json {
        println!("{raw}");
        return;
    }
    if let Some(status) = &response.status {
        println!(
            "Postbox {} | view {} | fps {:.1} | RAM {} MB (private {} MB) | GPU {} MB | outbox {} | frames {}",
            status.version,
            status.active_view,
            status.fps,
            status.working_set_kb / 1024,
            status.private_usage_kb / 1024,
            status.gpu_texture_bytes / (1024 * 1024),
            status.pending_outbox,
            status.frames_rendered,
        );
        if let Some(chat) = &status.selected_chat {
            println!("selected {chat}");
        }
        return;
    }
    if let Some(chats) = &response.chats {
        for chat in chats {
            println!(
                "{}\t{}\t{}\t{}",
                chat.provider.as_str(),
                chat.account,
                chat.id,
                if chat.title.is_empty() {
                    chat.preview.as_str()
                } else {
                    chat.title.as_str()
                }
            );
        }
        return;
    }
    if let Some(messages) = &response.messages {
        for message in messages {
            println!(
                "{}\t{}\t{}\t{}",
                message.chat_id, message.id, message.sender, message.body
            );
        }
        return;
    }
    if let Some(message) = &response.message {
        println!("{message}");
        return;
    }
    if let Some(error) = &response.error {
        eprintln!("{error}");
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&argv) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    if cli.dry_run {
        println!("{}", encode_request(&cli.request));
        return;
    }

    let transport = match &cli.pipe {
        Some(raw) => Transport::parse(raw),
        None => default_transport(),
    };

    match send(&transport, &cli.request, cli.timeout_ms) {
        Ok(response) => {
            let line = postbox_core::control::encode_response(&response);
            print_response(&response, &line, false);
            if !response.ok {
                std::process::exit(1);
            }
        }
        Err(message) => {
            eprintln!("{message}");
            eprintln!("endpoint: {}", transport.display());
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use postbox_core::control::{ControlResponse, StatusReport, TelemetryCounters};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn every_documented_command_parses() {
        assert_eq!(
            parse_args(&args(&["status"])).unwrap().request,
            ControlRequest::Status
        );
        assert_eq!(
            parse_args(&args(&["theme", "amoled"])).unwrap().request,
            ControlRequest::Theme {
                theme: ThemeChoice::Amoled
            }
        );
        assert_eq!(
            parse_args(&args(&["font", "130"])).unwrap().request,
            ControlRequest::Font { percent: 130 }
        );
        assert_eq!(
            parse_args(&args(&[
                "chats",
                "--provider",
                "slack",
                "--all",
                "--limit",
                "5"
            ]))
            .unwrap()
            .request,
            ControlRequest::Chats {
                provider: Some(Provider::Slack),
                archived: false,
                all: true,
                limit: Some(5),
            }
        );
        assert_eq!(
            parse_args(&args(&["chats", "--archived"])).unwrap().request,
            ControlRequest::Chats {
                provider: None,
                archived: true,
                all: false,
                limit: None,
            }
        );
        assert_eq!(
            parse_args(&args(&["search", "two", "words"]))
                .unwrap()
                .request,
            ControlRequest::Search {
                text: "two words".into()
            }
        );
        assert_eq!(
            parse_args(&args(&["view", "inbox"])).unwrap().request,
            ControlRequest::View {
                target: "inbox".into()
            }
        );
        assert_eq!(
            parse_args(&args(&[
                "select",
                "C02",
                "--provider",
                "slack",
                "--account",
                "T01"
            ]))
            .unwrap()
            .request,
            ControlRequest::Select {
                id: "C02".into(),
                provider: Some(Provider::Slack),
                account: Some("T01".into()),
            }
        );
        assert_eq!(
            parse_args(&args(&["messages", "C02", "--limit", "3"]))
                .unwrap()
                .request,
            ControlRequest::Messages {
                id: Some("C02".into()),
                limit: Some(3),
            }
        );
        assert_eq!(
            parse_args(&args(&["archive", "C02"])).unwrap().request,
            ControlRequest::Archive { id: "C02".into() }
        );
        assert_eq!(
            parse_args(&args(&["unarchive", "C02"])).unwrap().request,
            ControlRequest::Unarchive { id: "C02".into() }
        );
        assert_eq!(
            parse_args(&args(&["archive-many", "a", "b"]))
                .unwrap()
                .request,
            ControlRequest::ArchiveMany {
                ids: vec!["a".into(), "b".into()],
                archived: true,
            }
        );
        assert_eq!(
            parse_args(&args(&["archive-many", "a", "--unarchive"]))
                .unwrap()
                .request,
            ControlRequest::ArchiveMany {
                ids: vec!["a".into()],
                archived: false,
            }
        );
    }

    #[test]
    fn a_bare_invocation_asks_for_status() {
        assert_eq!(parse_args(&[]).unwrap().request, ControlRequest::Status);
    }

    #[test]
    fn pipe_and_timeout_overrides_are_picked_up() {
        let cli = parse_args(&args(&[
            "--pipe",
            r"\\.\pipe\postbox-control-valentin",
            "--timeout",
            "500",
            "status",
        ]))
        .unwrap();
        assert_eq!(
            cli.pipe.as_deref(),
            Some(r"\\.\pipe\postbox-control-valentin")
        );
        assert_eq!(cli.timeout_ms, 500);
        assert_eq!(cli.request, ControlRequest::Status);
    }

    #[test]
    fn bad_input_explains_the_usage() {
        assert!(parse_args(&args(&["theme"])).is_err());
        assert!(
            parse_args(&args(&["theme", "neon"]))
                .unwrap_err()
                .contains("unknown theme")
        );
        assert!(parse_args(&args(&["font", "loud"])).is_err());
        assert!(
            parse_args(&args(&["bogus"]))
                .unwrap_err()
                .contains("unknown command")
        );
        assert!(
            parse_args(&args(&["--pipe"]))
                .unwrap_err()
                .contains("--pipe needs a path")
        );
        assert!(
            parse_args(&args(&["chats", "--provider", "myspace"]))
                .unwrap_err()
                .contains("unknown provider")
        );
        assert!(
            parse_args(&args(&["chats", "--limit", "many"]))
                .unwrap_err()
                .contains("--limit needs a number")
        );
        assert!(parse_args(&args(&["archive-many"])).is_err());
        assert!(
            parse_args(&args(&["--help"]))
                .unwrap_err()
                .contains("usage: postboxctl")
        );
    }

    #[test]
    fn a_status_response_carries_live_telemetry() {
        let response = ControlResponse::with_status(StatusReport::from_counters(
            &TelemetryCounters {
                frames_rendered: 300,
                working_set_kb: 65 * 1024,
                private_usage_kb: 55 * 1024,
                gpu_texture_bytes: 4 * 1024 * 1024,
                pending_outbox: 0,
            },
            60.0,
            "0.1.0",
            "all",
            None,
        ));
        let line = postbox_core::control::encode_response(&response);
        let decoded = decode_response(&line).unwrap();
        assert_eq!(decoded.status.unwrap().working_set_kb, 66_560);
    }

    /// The endpoint the round-trip test binds: a named pipe on Windows, a socket elsewhere.
    fn test_transport(dir: &std::path::Path) -> Transport {
        #[cfg(windows)]
        {
            let _ = dir;
            Transport::pipe(&format!("postboxctl-test-{}", std::process::id()))
        }
        #[cfg(not(windows))]
        {
            Transport::socket(dir.join("control.sock"))
        }
    }

    #[test]
    fn a_round_trip_against_a_live_server_returns_a_decoded_response() {
        let dir = std::env::temp_dir().join(format!("postboxctl-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let transport = test_transport(&dir);

        let server = postbox_platform::control_pipe::serve(&transport, |line| {
            let request = postbox_core::control::decode_request(line).unwrap();
            let response = match request {
                ControlRequest::Status => {
                    ControlResponse::with_status(StatusReport::from_counters(
                        &TelemetryCounters {
                            frames_rendered: 12,
                            working_set_kb: 40 * 1024,
                            private_usage_kb: 30 * 1024,
                            gpu_texture_bytes: 2 * 1024 * 1024,
                            pending_outbox: 1,
                        },
                        59.9,
                        "0.1.0",
                        "all",
                        Some("slack/T01/C02"),
                    ))
                }
                _ => ControlResponse::error("unexpected"),
            };
            postbox_core::control::encode_response(&response)
        })
        .expect("server starts");

        // The Windows acceptor creates its first pipe instance on a thread, so a send that
        // arrives before then is retried rather than failing the test.
        let mut last = String::new();
        let mut response = None;
        for _ in 0..100 {
            match send(&transport, &ControlRequest::Status, 2_000) {
                Ok(reply) => {
                    response = Some(reply);
                    break;
                }
                Err(err) => {
                    last = err;
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        let response = response.unwrap_or_else(|| panic!("round trip works: {last}"));
        let status = response.status.expect("status payload");
        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.working_set_kb, 40 * 1024);
        assert!((status.fps - 59.9).abs() < f64::EPSILON);
        assert_eq!(status.selected_chat.as_deref(), Some("slack/T01/C02"));

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
