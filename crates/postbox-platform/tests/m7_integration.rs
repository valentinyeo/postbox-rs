//! Milestone 7 integration tests: `postbox://` registration and forwarding, the non-modal
//! "Chat not found" card, `postboxctl` round trips with live RAM/FPS telemetry, the atomic
//! self-updater and crash/exit records.
//!
//! Everything runs headless on any platform: the OS pieces are exercised through their traits
//! and the pipes through the portable socket transport.

use postbox_core::control::{
    ChatSummary, ControlRequest, ControlResponse, MessageSummary, StatusReport, TelemetryCounters,
    decode_request, decode_response, encode_request, encode_response,
};
use postbox_core::crash::{ExitKind, ExitRecord};
use postbox_core::deep_link;
use postbox_core::os_integration::{
    URL_PROTOCOL_SUBKEY, app_user_model_values, control_pipe_name, handler_command_line,
    link_pipe_name, normalize_user, single_instance_mutex_name, url_protocol_values,
};
use postbox_core::toast::{self, NotificationGate, Suppression, ToastFocus};
use postbox_core::update::{
    CheckOutcome, UpdateAsset, UpdateManifest, Version, digest_hex, evaluate,
};
use postbox_core::{ChatKey, Provider};
use postbox_platform::control_pipe::{Transport, round_trip, serve};
use postbox_platform::exit_records::ExitRecordStore;
use postbox_platform::os_integration::UrlHandler as _;
use postbox_platform::os_integration::{
    LinkOutcome, MockLinkPipe, MockSingleInstance, MockUrlHandler, StartupRole, link_from_args,
    route_line, startup,
};
use postbox_platform::{
    AtomicReplacer, MockReleaseSource, PlatformError, ReleaseSource, UpdateError,
};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const EXE: &str = r"C:\Program Files\Postbox\postbox.exe";
const WA_LINK: &str = "postbox://whatsapp/biz/491234%40s.whatsapp.net";

/// A short temp path: a Unix-domain socket path must stay under the platform's `SUN_LEN`.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pbm7{}{}",
        std::process::id(),
        &digest_hex(tag.as_bytes())[..8]
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wa_chat() -> ChatKey {
    ChatKey::new(Provider::Whatsapp, "biz", "491234@s.whatsapp.net")
}

// ---------------------------------------------------------------------------
// 1. Toast XML, suppression and click-to-focus routing
// ---------------------------------------------------------------------------

mod toasts {
    use super::*;

    #[test]
    fn a_message_toast_escapes_markup_and_carries_a_working_launch_url() {
        let xml = toast::build_xml(&wa_chat(), "Tom & Jerry <3", "say \"hi\" it's > ok");
        assert!(xml.contains("Tom &amp; Jerry &lt;3"), "{xml}");
        assert!(
            xml.contains("activationType=\"protocol\""),
            "a click must run the protocol handler: {xml}"
        );

        // Walk the whole click path: launch URL -> registry handler -> parse -> focus.
        let launch = toast::launch_url(&wa_chat(), Some("MSG-9"));
        let parsed = deep_link::parse(&launch).expect("the launch URL parses back");
        assert_eq!(parsed.chat, wa_chat());
        assert_eq!(parsed.message.as_deref(), Some("MSG-9"));
        let action = route_line(&launch, |_| true);
        assert_eq!(action, LinkOutcome::Focused { chat: wa_chat() });
    }

    #[test]
    fn control_characters_and_long_text_never_break_the_markup() {
        let xml = toast::build_xml(&wa_chat(), "a\u{0}b\u{7}c", &"y".repeat(5_000));
        assert!(xml.contains("<text>abc</text>"), "{xml}");
        assert!(xml.len() <= toast::MAX_XML_BYTES);
        assert!(xml.is_char_boundary(xml.len()));
    }

    #[test]
    fn the_suppression_policy_keeps_own_messages_and_the_open_chat_quiet() {
        let chat = wa_chat();
        let mut gate = NotificationGate::new();
        gate.prime(&chat, 100);

        assert_eq!(
            gate.check(&chat, 200, true, ToastFocus::blurred()),
            Some(Suppression::FromMe)
        );
        assert_eq!(
            gate.check(&chat, 200, false, ToastFocus::focused_on(&chat)),
            Some(Suppression::FocusedChat)
        );
        // 200 is newer than the primed mark, so it raises the one toast of this test.
        assert_eq!(gate.check(&chat, 200, false, ToastFocus::blurred()), None);
        // The same message again, and anything older, stays silent.
        assert_eq!(
            gate.check(&chat, 200, false, ToastFocus::blurred()),
            Some(Suppression::AlreadySeen)
        );
        assert_eq!(
            gate.check(&chat, 150, false, ToastFocus::blurred()),
            Some(Suppression::AlreadySeen)
        );
        assert_eq!(gate.check(&chat, 201, false, ToastFocus::blurred()), None);
    }
}

// ---------------------------------------------------------------------------
// 2. postbox:// registration, single instance and the link pipe
// ---------------------------------------------------------------------------

mod deep_links {
    use super::*;

    #[test]
    fn registration_writes_the_hkcu_values_and_is_idempotent() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::new();
        let pipe = MockLinkPipe::new();
        let role = startup(EXE, "valentin", None, &registry, &instance, &pipe).unwrap();
        assert!(matches!(role, StartupRole::Primary { link: None }));

        assert_eq!(
            registry.value(URL_PROTOCOL_SUBKEY, None).as_deref(),
            Some("URL:Postbox")
        );
        assert_eq!(
            registry.value(URL_PROTOCOL_SUBKEY, Some("URL Protocol")),
            Some(String::new())
        );
        assert_eq!(
            registry.registered_command().unwrap().as_deref(),
            Some(handler_command_line(EXE).as_str())
        );
        assert_eq!(
            registry.registered_command().unwrap().unwrap(),
            r#""C:\Program Files\Postbox\postbox.exe" "%1""#
        );
        assert!(
            app_user_model_values()
                .iter()
                .all(|value| { value.subkey.starts_with(r"Software\Classes\AppUserModelId") })
        );

        // Startup on an already-registered machine writes nothing.
        assert_eq!(registry.write_count(), 1);
        registry.register(EXE).unwrap();
        assert_eq!(registry.write_count(), 1);

        registry.unregister().unwrap();
        assert!(!registry.is_registered());
    }

    #[test]
    fn the_registration_layout_matches_the_contract() {
        let values = url_protocol_values(EXE);
        let subkeys: Vec<&str> = values.iter().map(|v| v.subkey.as_str()).collect();
        assert_eq!(
            subkeys,
            vec![
                r"Software\Classes\postbox",
                r"Software\Classes\postbox",
                r"Software\Classes\postbox\DefaultIcon",
                r"Software\Classes\postbox\shell\open\command",
            ]
        );
        assert_eq!(
            values[3].subkey,
            r"Software\Classes\postbox\shell\open\command"
        );
        assert!(values[3].value.ends_with("\"%1\""));
    }

    #[test]
    fn a_second_launch_forwards_the_url_over_the_link_pipe_and_exits_cleanly() {
        let registry = MockUrlHandler::new();
        let instance = MockSingleInstance::held_by("valentin");
        let pipe = MockLinkPipe::new();

        let role = startup(
            EXE,
            "valentin",
            Some(&format!("{WA_LINK}/MSG-1")),
            &registry,
            &instance,
            &pipe,
        )
        .unwrap();
        match role {
            StartupRole::Forwarded { link } => {
                let link = link.expect("the forwarded URL parsed");
                assert_eq!(link.chat, wa_chat());
                assert_eq!(link.message.as_deref(), Some("MSG-1"));
            }
            other => panic!("expected the forwarded role, got {other:?}"),
        }
        assert_eq!(
            pipe.last_payload(),
            Some(format!("{WA_LINK}/MSG-1")),
            "the running instance receives the exact URL"
        );
        assert_eq!(pipe.sent()[0].0, "valentin");
    }

    #[test]
    fn a_second_launch_without_a_url_only_asks_for_focus() {
        let pipe = MockLinkPipe::new();
        let role = startup(
            EXE,
            "valentin",
            None,
            &MockUrlHandler::new(),
            &MockSingleInstance::held_by("valentin"),
            &pipe,
        )
        .unwrap();
        assert_eq!(role, StartupRole::Forwarded { link: None });
        assert_eq!(
            pipe.last_payload().as_deref(),
            Some(postbox_core::os_integration::FOCUS_SIGNAL)
        );
    }

    #[test]
    fn a_missing_chat_route_yields_the_provider_specific_note() {
        for (url, note) in [
            (
                "postbox://whatsapp/a/c",
                "Could not find that WhatsApp chat.",
            ),
            ("postbox://slack/a/c", "Could not find that Slack channel."),
            (
                "postbox://telegram/a/c",
                "Could not find that Telegram chat.",
            ),
            ("postbox://signal/a/c", "Could not find that Signal chat."),
        ] {
            assert_eq!(
                route_line(url, |_| false),
                LinkOutcome::MissingChat {
                    note: note.to_owned()
                }
            );
        }
    }

    #[test]
    fn per_user_names_never_collide_and_stay_in_the_kernel_charset() {
        assert_eq!(
            link_pipe_name("valentin"),
            r"\\.\pipe\postbox-link-valentin"
        );
        assert_eq!(
            control_pipe_name("valentin"),
            r"\\.\pipe\postbox-control-valentin"
        );
        assert_ne!(control_pipe_name("a"), control_pipe_name("b"));
        assert_eq!(
            single_instance_mutex_name("valentin"),
            r"Local\postbox-single-valentin"
        );

        let odd = normalize_user("Valentin Yeo (work)");
        assert_eq!(odd, "Valentin-Yeo-work");
        let name = link_pipe_name(&odd);
        assert!(
            name.chars().all(|c| c.is_ascii_alphanumeric()
                || c == '-'
                || c == '\\'
                || c == '.'
                || c == '_'),
            "{name}"
        );
        assert_eq!(
            control_pipe_name(&normalize_user("a/b\\c")),
            r"\\.\pipe\postbox-control-a-b-c"
        );
        // A user name that is nothing but separators still yields a usable endpoint.
        assert!(control_pipe_name(&normalize_user("  ..  ")).ends_with("-default"));
    }

    #[test]
    fn the_command_line_carries_the_url_the_handler_passes() {
        let args: Vec<String> = vec![
            "postbox.exe".into(),
            "%1".into(),
            "--no-os-integration".into(),
            WA_LINK.into(),
        ];
        assert_eq!(link_from_args(args).as_deref(), Some(WA_LINK));
    }
}

// ---------------------------------------------------------------------------
// 3. postboxctl round trips with live telemetry
// ---------------------------------------------------------------------------

mod control_round_trip {
    use super::*;

    fn status() -> StatusReport {
        StatusReport::from_counters(
            &TelemetryCounters {
                frames_rendered: 12_345,
                working_set_kb: 61 * 1024,
                private_usage_kb: 54 * 1024,
                gpu_texture_bytes: 16 * 1024 * 1024,
                pending_outbox: 4,
            },
            59.9,
            "0.1.0",
            "all|Combined inbox|connected",
            Some("slack/T01/C02"),
        )
    }

    /// The same request/response logic the app serves, without a window.
    fn handle(line: &str) -> String {
        match decode_request(line) {
            Ok(ControlRequest::Status) => encode_response(&ControlResponse::with_status(status())),
            Ok(ControlRequest::Chats { .. }) => {
                encode_response(&ControlResponse::with_chats(vec![ChatSummary {
                    provider: Provider::Slack,
                    account: "T01".into(),
                    id: "C02".into(),
                    title: "Engineering".into(),
                    unread: 2,
                    archived: false,
                    last_ts: "2025-01-15T12:00:00Z".into(),
                    preview: "standup in 5".into(),
                }]))
            }
            Ok(ControlRequest::Messages { .. }) => {
                encode_response(&ControlResponse::with_messages(vec![MessageSummary {
                    id: "m1".into(),
                    provider: Provider::Slack,
                    account: "T01".into(),
                    chat_id: "C02".into(),
                    sender: "Ada".into(),
                    ts: "2025-01-15T12:00:00Z".into(),
                    from_me: false,
                    body: "standup in 5".into(),
                }]))
            }
            Ok(ControlRequest::Archive { id }) => {
                encode_response(&ControlResponse::with_message(format!("Archived {id}")))
            }
            Ok(other) => encode_response(&ControlResponse::with_message(other.command_name())),
            Err(err) => encode_response(&ControlResponse::error(err)),
        }
    }

    #[test]
    fn status_carries_live_ram_fps_and_the_active_view_over_a_real_pipe() {
        let dir = temp_dir("ctl-status");
        let transport = Transport::socket(dir.join("control.sock"));
        let server = serve(&transport, handle).expect("server starts");

        let reply = round_trip(
            &transport,
            &encode_request(&ControlRequest::Status),
            Duration::from_secs(2),
        )
        .expect("no transport error")
        .expect("server answered");
        let response = decode_response(&reply).expect("valid JSON line");
        let status = response.status.expect("status payload");

        assert_eq!(status.version, "0.1.0");
        assert_eq!(status.frames_rendered, 12_345);
        assert_eq!(status.working_set_kb, 62_464);
        assert_eq!(status.private_usage_kb, 55_296);
        assert_eq!(status.gpu_texture_bytes, 16_777_216);
        assert_eq!(status.pending_outbox, 4);
        assert!((status.fps - 59.9).abs() < f64::EPSILON);
        assert_eq!(status.selected_chat.as_deref(), Some("slack/T01/C02"));
        assert!(status.active_view.contains("Combined inbox"));

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_control_subcommand_round_trips_on_one_line() {
        let dir = temp_dir("ctl-all");
        let transport = Transport::socket(dir.join("control.sock"));
        let server = serve(&transport, handle).expect("server starts");

        let requests = vec![
            ControlRequest::Status,
            ControlRequest::Theme {
                theme: postbox_core::ThemeChoice::Amoled,
            },
            ControlRequest::Font { percent: 130 },
            ControlRequest::Chats {
                provider: Some(Provider::Slack),
                archived: false,
                all: true,
                limit: Some(5),
            },
            ControlRequest::Search {
                text: "standup".into(),
            },
            ControlRequest::View {
                target: "slack:T01".into(),
            },
            ControlRequest::Select {
                id: "C02".into(),
                provider: Some(Provider::Slack),
                account: Some("T01".into()),
            },
            ControlRequest::Messages {
                id: Some("C02".into()),
                limit: Some(5),
            },
            ControlRequest::Archive { id: "C02".into() },
            ControlRequest::Unarchive { id: "C02".into() },
            ControlRequest::ArchiveMany {
                ids: vec!["C02".into(), "C03".into()],
                archived: true,
            },
        ];

        for request in requests {
            let line = encode_request(&request);
            let reply = round_trip(&transport, &line, Duration::from_secs(2))
                .unwrap_or_else(|err| panic!("{}: {err}", request.command_name()))
                .unwrap_or_else(|| panic!("{}: no answer", request.command_name()));
            let response = decode_response(&reply)
                .unwrap_or_else(|err| panic!("{}: {err}", request.command_name()));
            assert!(response.ok, "{}: {response:?}", request.command_name());
            assert!(
                !reply.trim_end_matches(['\r', '\n']).contains('\n'),
                "{}: one line only",
                request.command_name()
            );
        }

        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_chat_is_an_error_payload_not_a_broken_pipe() {
        let dir = temp_dir("ctl-error");
        let transport = Transport::socket(dir.join("control.sock"));
        let server = serve(&transport, handle).expect("server starts");
        let reply = round_trip(&transport, "not json", Duration::from_secs(2))
            .expect("transport ok")
            .expect("server answered");
        let response = decode_response(&reply).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("invalid request"));
        server.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_pipe_endpoint_names_itself_exactly_once() {
        // The Windows CI failure this test replaces: `default_control_transport` handed the
        // already-prefixed name to `Transport::pipe`, and `display` prefixed it again.
        for user in ["valentin", "runneradmin", "Valentin-Yeo"] {
            for (full, bare) in [
                (control_pipe_name(user), format!("postbox-control-{user}")),
                (link_pipe_name(user), format!("postbox-link-{user}")),
            ] {
                assert_eq!(Transport::pipe(&full).display(), full);
                assert_eq!(Transport::pipe(&bare).display(), full);
                assert_eq!(Transport::parse(&full).display(), full);
                assert_eq!(
                    Transport::pipe(&full)
                        .display()
                        .matches(r"\\.\pipe\")
                        .count(),
                    1
                );
            }
        }
        // The env-provided default endpoint is the per-user control pipe, once.
        let default = postbox_platform::control_pipe::default_control_transport();
        if cfg!(windows) {
            let user = postbox_core::os_integration::current_user();
            assert_eq!(
                default.display(),
                format!(r"\\.\pipe\postbox-control-{user}")
            );
        }
    }

    #[test]
    fn the_pipe_override_names_the_same_endpoint_the_server_uses() {
        let dir = temp_dir("ctl-override");
        let socket = dir.join("control.sock");
        let parsed = Transport::parse(&socket.display().to_string());
        assert_eq!(parsed, Transport::socket(&socket));
        assert_eq!(
            Transport::parse(r"\\.\pipe\postbox-control-valentin").display(),
            r"\\.\pipe\postbox-control-valentin"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Atomic self-updater
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum FailMode {
    /// Deletes the live file and then refuses, so only the backup is runnable.
    AfterDeleting,
}

struct ScriptedReplacer {
    calls: Mutex<Vec<(PathBuf, PathBuf)>>,
    mode: FailMode,
}

impl AtomicReplacer for ScriptedReplacer {
    fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
        self.calls
            .lock()
            .unwrap()
            .push((staged.to_path_buf(), target.to_path_buf()));
        if self.mode == FailMode::AfterDeleting {
            let _ = std::fs::remove_file(target);
        }
        Err(PlatformError::OsUnavailable)
    }
}

mod updater {
    use super::*;

    fn manifest_for(payload: &[u8], tag: &str) -> UpdateManifest {
        let body = format!(
            r#"{{"version":"{}","tag":"{tag}","asset":"postbox-windows-x64.zip","sha256":"{}","size":{},"body":"- Toasts open the right chat","assets":[{{"name":"postbox-windows-x64.zip","browser_download_url":"https://example.invalid/postbox-windows-x64.zip","size":{},"digest":"sha256:{}"}}]}}"#,
            tag.trim_start_matches('v'),
            digest_hex(payload),
            payload.len(),
            payload.len(),
            digest_hex(payload)
        );
        UpdateManifest::parse(&body).expect("manifest parses")
    }

    #[test]
    fn the_published_manifest_shape_parses_and_compares_versions() {
        let manifest = manifest_for(b"archive bytes", "v0.2.0");
        assert_eq!(manifest.version(), Some(Version::new(0, 2, 0)));
        let asset = manifest.select_asset().expect("asset selected");
        assert_eq!(asset.sha256, digest_hex(b"archive bytes"));
        assert_eq!(asset.size, 13);
        assert!(matches!(
            evaluate(&manifest, Version::new(0, 1, 0)),
            CheckOutcome::Available { .. }
        ));
        assert!(matches!(
            evaluate(&manifest, Version::new(0, 2, 0)),
            CheckOutcome::UpToDate { .. }
        ));
        assert!(matches!(
            evaluate(&manifest, Version::new(0, 3, 0)),
            CheckOutcome::UpToDate { .. }
        ));
    }

    #[test]
    fn an_update_installs_atomically_and_keeps_the_previous_binary() {
        let dir = temp_dir("update-ok");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();

        let payload = b"new build bytes";
        let source = MockReleaseSource::new(manifest_for(payload, "v0.2.0"), payload.to_vec());
        let manifest = source.fetch_manifest().unwrap();
        let asset = manifest.select_asset().unwrap();
        let bytes = source.download(&asset).unwrap();
        assert_eq!(source.download_count(), 1);

        let report = postbox_platform::install(
            &asset,
            &bytes,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::PlainFileExtractor,
            &postbox_platform::NativeReplacer,
        )
        .expect("install succeeds");

        assert_eq!(std::fs::read(&live).unwrap(), payload);
        assert_eq!(report.verified_sha256, digest_hex(payload));
        let backup = report.backup.expect("the previous build was kept");
        assert_eq!(std::fs::read(&backup).unwrap(), b"installed build");
        assert!(!dir.join("postbox-windows-x64.zip.staged").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A release zip as the PowerShell workflow writes it: every entry deflated, sizes and
    /// CRC-32 only in the central directory, and a data descriptor after the payload.
    fn deflated_release_zip(name: &str, payload: &[u8]) -> Vec<u8> {
        let raw = miniz_oxide::deflate::compress_to_vec(payload, 6);
        let crc = crc32fast::hash(payload);
        let mut out = Vec::new();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // local header
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0808u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes()); // deflate
        out.extend_from_slice(&[0u8; 16]); // time, date, crc, sizes: streamed
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&raw);
        out.extend_from_slice(&0x0807_4b50u32.to_le_bytes()); // data descriptor
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());

        let central_offset = out.len();
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes()); // central header
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0x0808u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]); // time, date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 12]); // extra, comment, disk, attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        out.extend_from_slice(name.as_bytes());

        let central_size = out.len() - central_offset;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes()); // end of central dir
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&(central_size as u32).to_le_bytes());
        out.extend_from_slice(&(central_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn a_compressed_release_zip_installs_like_the_uncompressed_one() {
        // The Windows release workflow runs `Compress-Archive`, which deflates by default.
        // This proves that archive installs without the CI workaround of storing entries.
        let dir = temp_dir("update-deflated");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();

        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 97) as u8).collect();
        let archive = deflated_release_zip("postbox-windows-x64/postbox.exe", &payload);
        let asset = manifest_for(&archive, "v0.2.0").select_asset().unwrap();

        let report = postbox_platform::install(
            &asset,
            &archive,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::ZipExtractor,
            &postbox_platform::NativeReplacer,
        )
        .expect("a deflated release zip installs");

        assert_eq!(std::fs::read(&live).unwrap(), payload);
        assert_eq!(report.verified_sha256, digest_hex(&archive));
        assert_eq!(report.backup, Some(dir.join("postbox.exe.backup.exe")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unsupported_compression_method_never_reaches_the_live_binary() {
        let dir = temp_dir("update-unsupported");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();

        let mut archive = deflated_release_zip("postbox.exe", b"new build");
        // Method 14 is LZMA: never produced by the release workflow, so it is refused.
        let central = archive.len() - 22 - 46 - "postbox.exe".len();
        archive[central + 10] = 14;
        let asset = manifest_for(&archive, "v0.2.0").select_asset().unwrap();

        let err = postbox_platform::install(
            &asset,
            &archive,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::ZipExtractor,
            &postbox_platform::NativeReplacer,
        )
        .expect_err("an unknown method is refused");
        assert!(matches!(err, UpdateError::Io(_)), "{err:?}");
        assert_eq!(std::fs::read(&live).unwrap(), b"installed build");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_hash_leaves_the_installed_executable_untouched() {
        let dir = temp_dir("update-badhash");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();

        // The manifest describes different bytes from the ones the server returns.
        let manifest = manifest_for(b"expected bytes", "v0.2.0");
        let asset = manifest.select_asset().unwrap();
        let tampered = b"tampered bytes";

        let err = postbox_platform::install(
            &asset,
            tampered,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::PlainFileExtractor,
            &postbox_platform::NativeReplacer,
        )
        .expect_err("a bad digest is refused");
        assert!(matches!(err, UpdateError::DigestMismatch { .. }), "{err:?}");
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the live binary must be byte-identical after a refused update"
        );
        assert!(!dir.join("postbox.exe.backup.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_swap_puts_the_previous_executable_back() {
        let dir = temp_dir("update-rollback");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let payload = b"new build";
        let asset = manifest_for(payload, "v0.2.0").select_asset().unwrap();
        let replacer = ScriptedReplacer {
            calls: Mutex::new(Vec::new()),
            mode: FailMode::AfterDeleting,
        };

        // The scripted replacer always fails, so the first swap loses the live file and the
        // rollback (a second `replace`) also fails, leaving the backup as the only copy.
        let err = postbox_platform::install(
            &asset,
            payload,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::PlainFileExtractor,
            &replacer,
        )
        .expect_err("the swap fails");
        match err {
            UpdateError::SwapFailedRollbackFailed { backup, .. } => {
                let backup = backup.expect("the backup path is reported");
                assert_eq!(std::fs::read(&backup).unwrap(), b"installed build");
            }
            other => panic!("expected a failed rollback with a backup path, got {other:?}"),
        }
        assert_eq!(replacer.calls.lock().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_swap_that_never_moved_the_live_binary_reports_and_leaves_it_running() {
        let dir = temp_dir("update-untouched");
        let live = dir.join("postbox.exe");
        std::fs::write(&live, b"installed build").unwrap();
        let payload = b"new build";
        let asset = manifest_for(payload, "v0.2.0").select_asset().unwrap();

        // First call fails before touching, so the rollback call succeeds (it renames the
        // backup over the untouched live file, which is still runnable either way).
        struct FirstOnly(Mutex<usize>);
        impl AtomicReplacer for FirstOnly {
            fn replace(&self, staged: &Path, target: &Path) -> Result<(), PlatformError> {
                let mut calls = self.0.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    return Err(PlatformError::OsUnavailable);
                }
                std::fs::rename(staged, target).map_err(PlatformError::Io)
            }
        }
        let replacer = FirstOnly(Mutex::new(0));
        let err = postbox_platform::install(
            &asset,
            payload,
            &dir,
            postbox_platform::updater::EXE_ASSET_NAME,
            &postbox_platform::PlainFileExtractor,
            &replacer,
        )
        .expect_err("the failed swap is reported");
        assert!(
            matches!(err, UpdateError::SwapFailedRolledBack(_)),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read(&live).unwrap(),
            b"installed build",
            "the installed build survives a failed swap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_fails_never_reaches_the_installer() {
        let dir = temp_dir("update-offline");
        std::fs::write(dir.join("postbox.exe"), b"installed build").unwrap();
        let source =
            MockReleaseSource::new(manifest_for(b"new build", "v0.2.0"), b"new build".to_vec());
        source.set_fail_download("connection reset");
        let asset = source.fetch_manifest().unwrap().select_asset().unwrap();
        let err = source.download(&asset).expect_err("download fails");
        assert!(matches!(err, UpdateError::Io(_)), "{err:?}");
        assert_eq!(
            std::fs::read(dir.join("postbox.exe")).unwrap(),
            b"installed build"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_never_crosses_a_volume_or_touches_the_live_binary() {
        let dir = temp_dir("update-staging");
        let staged =
            postbox_platform::updater::staged_archive_path(&dir, "postbox-windows-x64.zip");
        assert_eq!(staged.parent(), Some(dir.as_path()));
        assert_eq!(
            postbox_platform::updater::pending_exe_path(&dir, "postbox.exe").parent(),
            Some(dir.as_path())
        );
        assert_eq!(
            postbox_platform::updater::backup_exe_path(&dir, "postbox.exe").parent(),
            Some(dir.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_manifest_digest_forms_are_both_accepted() {
        let bytes = b"payload";
        assert_eq!(
            postbox_platform::verify_download(
                bytes,
                &UpdateAsset {
                    tag: "v0.2.0".into(),
                    version: Version::new(0, 2, 0),
                    url: "https://example.invalid/a.zip".into(),
                    sha256: digest_hex(bytes),
                    size: bytes.len() as u64,
                    notes: String::new(),
                }
            ),
            Ok(())
        );
        assert_eq!(
            postbox_platform::verify_download(
                bytes,
                &UpdateAsset {
                    tag: "v0.2.0".into(),
                    version: Version::new(0, 2, 0),
                    url: "https://example.invalid/a.zip".into(),
                    sha256: format!("sha256:{}", digest_hex(bytes)),
                    size: bytes.len() as u64,
                    notes: String::new(),
                }
            ),
            Ok(())
        );
    }

    #[test]
    fn the_pool_limit_and_caps_match_the_ram_diet_contract() {
        use postbox_platform::update_source::{
            MAX_ARCHIVE_BYTES, MAX_MANIFEST_BYTES, POOL_MAX_IDLE_PER_HOST,
        };
        assert_eq!(POOL_MAX_IDLE_PER_HOST, 2);
        assert_eq!(MAX_MANIFEST_BYTES, 256 * 1024);
        const { assert!(MAX_ARCHIVE_BYTES >= 128 * 1024 * 1024) };
    }
}

// ---------------------------------------------------------------------------
// 5. Crash and exit records
// ---------------------------------------------------------------------------

mod exit_records {
    use super::*;

    #[test]
    fn clean_and_crash_records_round_trip_and_stay_bounded() {
        let dir = temp_dir("records");
        let store = ExitRecordStore::new(&dir);
        assert!(store.read_all().unwrap().is_empty());

        store.record_clean("0.1.0", 5_000).unwrap();
        store
            .record(&ExitRecord::panic(
                "0.1.0",
                9_000,
                postbox_platform::now_unix_ms(),
                Some("attempt to subtract with overflow"),
                Some("crates/postbox-ui/src/desktop.rs:120:9"),
            ))
            .unwrap();

        let records = store.read_all().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, ExitKind::Clean);
        assert_eq!(records[1].kind, ExitKind::Panic);
        assert_eq!(
            records[1].location.as_deref(),
            Some("desktop.rs:120:9"),
            "the location keeps only file and line"
        );

        // One JSON object per line, no message content beyond the sanitised panic text.
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            let record = ExitRecord::parse(line).expect("each line parses");
            let json = serde_json::to_string(&record).unwrap();
            for forbidden in ["chat_id", "body", "sender_id", "token"] {
                assert!(!json.contains(forbidden), "{json}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_panic_hook_writes_a_record_without_leaking_content() {
        let dir = temp_dir("hook");
        let store = ExitRecordStore::new(&dir);
        let started = Instant::now();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        postbox_platform::install_panic_hook(store.clone(), "0.1.0", started);
        let caught = std::panic::catch_unwind(|| panic!("render panic while drawing a bubble"));
        std::panic::set_hook(previous);
        assert!(caught.is_err());

        let record = store.last().unwrap().expect("a crash record was written");
        assert_eq!(record.kind, ExitKind::Panic);
        assert_eq!(record.version, "0.1.0");
        assert_eq!(
            record.message.as_deref(),
            Some("render panic while drawing a bubble")
        );
        assert!(record.location.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_credential_in_a_panic_message_is_redacted_before_it_is_stored() {
        let dir = temp_dir("records-secret");
        let store = ExitRecordStore::new(&dir);
        store
            .record(&ExitRecord::panic(
                "0.1.0",
                1,
                postbox_platform::now_unix_ms(),
                Some("send failed with token=abcdefghijklmnopqrstuvwxyz0123456789"),
                None,
            ))
            .unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            !text.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
            "{text}"
        );
        assert!(text.contains("<redacted>"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// 6. Window bounds survive the relaunch
// ---------------------------------------------------------------------------

mod window_state {
    use postbox_platform::{MonitorRect, WindowState};

    #[test]
    fn the_last_monitor_and_bounds_survive_the_relaunch() {
        let state = WindowState::new(2100, 200, 1200, 800, true, 1);
        let encoded = state.encode();
        let decoded = WindowState::decode(&encoded);
        assert_eq!(decoded, state);

        // Second monitor still attached: unchanged.
        let monitors = [
            MonitorRect::new(0, 0, 1920, 1040),
            MonitorRect::new(1920, 0, 2560, 1400),
        ];
        let restored = decoded.restored(&monitors);
        assert_eq!((restored.x, restored.y), (2100, 200));
        assert!(restored.is_maximized());

        // Second monitor unplugged: recentred on the primary, still maximised.
        let restored = decoded.restored(&monitors[..1]);
        assert_eq!(restored.monitor, 0);
        assert!(restored.is_maximized());
        assert!(restored.x >= 0 && restored.x < 1920);
    }
}

// ---------------------------------------------------------------------------
// 7. The whole link -> registry -> chat path, end to end
// ---------------------------------------------------------------------------

mod end_to_end {
    use super::*;

    #[test]
    fn a_clicked_toast_finds_the_registered_handler_and_the_running_window() {
        // 1. Windows raises a toast for an incoming message.
        let xml = toast::build_xml(&wa_chat(), "Ada", "Ping");
        let launch = extract_launch(&xml).expect("the toast carries a launch target");
        assert_eq!(launch, WA_LINK);

        // 2. The registered handler command passes the URL back to the app.
        let command = handler_command_line(EXE);
        assert!(command.contains("\"%1\""), "{command}");
        let args: Vec<String> = vec!["postbox.exe".into(), launch.clone()];
        let link = link_from_args(args).expect("the command line carries a link");

        // 3. The first instance owns the lock, so the launcher forwards and exits.
        let registry = MockUrlHandler::new();
        let pipe = MockLinkPipe::new();
        let role = startup(
            EXE,
            "valentin",
            Some(&link),
            &registry,
            &MockSingleInstance::held_by("valentin"),
            &pipe,
        )
        .unwrap();
        assert!(matches!(role, StartupRole::Forwarded { .. }));

        // 4. The running window receives the line and focuses the chat.
        let line = pipe.last_payload().expect("the line was forwarded");
        let outcome = route_line(&line, |target| target.chat == wa_chat());
        assert_eq!(outcome, LinkOutcome::Focused { chat: wa_chat() });
    }

    #[test]
    fn a_clicked_toast_for_a_deleted_chat_cards_instead_of_failing() {
        let launch = toast::launch_url(&wa_chat(), None);
        let outcome = route_line(&launch, |_| false);
        match outcome {
            LinkOutcome::MissingChat { note } => {
                assert_eq!(note, "Could not find that WhatsApp chat.")
            }
            other => panic!("expected the missing-chat note, got {other:?}"),
        }
    }

    fn extract_launch(xml: &str) -> Option<String> {
        let marker = "launch=\"";
        let start = xml.find(marker)? + marker.len();
        let rest = &xml[start..];
        let end = rest.find('"')?;
        Some(rest[..end].to_owned())
    }
}
