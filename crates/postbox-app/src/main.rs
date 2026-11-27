#![cfg_attr(
    all(feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod audio_service;
mod sync;
mod update_source;

use audio_service::spawn_offline_audio_worker;
use postbox_hub::Endpoint;
use postbox_keymap::Keymap;
use postbox_media::VideoThumbnailCache;
use postbox_platform::exit_records::{ExitRecordStore, install_panic_hook};
#[cfg(all(windows, feature = "desktop"))]
use postbox_platform::os_integration::startup_relaunch;
use postbox_platform::os_integration::{StartupRole, link_from_args};
use postbox_platform::{
    ApiKeyOutcome, AudioBackend, GiphyKeyOutcome, HubDiscoveryOutcome, NativePlatform, OsRegistry,
    REG_DICTATION_LANGUAGE, VideoBackend, discover_deepgram_api_key_from_system,
    discover_format_provider_from_system, discover_giphy_api_key_from_system,
    discover_hub_credentials_from_system, load_microphone_choice,
};
use postbox_store::Store;
use postbox_ui::{HubConnectionStatus, PostboxLaunchContext, create_sync_bridge};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use sync::{StoreLocation, spawn_sync_engine};

static MEM_DB_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StartupConfig {
    db_path: Option<PathBuf>,
    keymap_path: Option<PathBuf>,
    hub_url: Option<String>,
    /// `postbox://` URL handed to this launch by the protocol handler.
    link: Option<String>,
    /// Set by the updater relaunch, so a fresh build takes the lock from the old process.
    update_relaunch: bool,
    /// Window geometry the updater relaunch passed, so the new build opens maximized there.
    restore_window: Option<postbox_platform::WindowState>,
    in_memory: bool,
    allow_loopback: bool,
    /// Skips registry registration, the single-instance guard and the pipes (tests, CI).
    no_os_integration: bool,
}

impl StartupConfig {
    fn from_args<I>(args: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let mut cfg = Self::default();
        let mut iter = args.into_iter().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--in-memory" => cfg.in_memory = true,
                "--allow-loopback" => cfg.allow_loopback = true,
                "--db" => {
                    if let Some(path) = iter.next() {
                        cfg.db_path = Some(PathBuf::from(path));
                    }
                }
                "--keymap" => {
                    if let Some(path) = iter.next() {
                        cfg.keymap_path = Some(PathBuf::from(path));
                    }
                }
                "--hub-url" => {
                    if let Some(url) = iter.next() {
                        cfg.hub_url = Some(url);
                    }
                }
                "--no-os-integration" => cfg.no_os_integration = true,
                postbox_platform::os_integration::UPDATE_RELAUNCH_ARG => {
                    cfg.update_relaunch = true;
                }
                postbox_platform::window_state::WINDOW_STATE_ARG => {
                    if let Some(raw) = iter.next() {
                        cfg.restore_window = Some(postbox_platform::WindowState::decode(&raw));
                    }
                }
                other => {
                    // The protocol handler passes the URL as a bare argument, so a stray
                    // `postbox://` anywhere on the command line counts as the link.
                    if cfg.link.is_none() && postbox_core::deep_link::is_postbox_url(other) {
                        cfg.link = Some(other.to_owned());
                    }
                }
            }
        }
        cfg
    }
}

fn default_data_dir() -> PathBuf {
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

fn open_store(cfg: &StartupConfig, data_dir: &Path) -> (Store, StoreLocation) {
    if cfg.in_memory {
        let seq = MEM_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("postbox_mem_{}_{}", std::process::id(), seq);
        let loc = StoreLocation::SharedMemory(name);
        let store = loc.open().expect("shared in-memory SQLite store init");
        return (store, loc);
    }
    let db_path = cfg
        .db_path
        .clone()
        .unwrap_or_else(|| data_dir.join("postbox.db"));
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match Store::open(&db_path) {
        Ok(store) => (store, StoreLocation::File(db_path)),
        Err(_) => {
            let seq = MEM_DB_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("postbox_fallback_{}_{}", std::process::id(), seq);
            let loc = StoreLocation::SharedMemory(name);
            let store = loc.open().expect("fallback in-memory SQLite store init");
            (store, loc)
        }
    }
}

fn load_keymap(cfg: &StartupConfig, data_dir: &Path) -> Keymap {
    let mut keymap = Keymap::postbox_defaults();
    let path = cfg
        .keymap_path
        .clone()
        .unwrap_or_else(|| data_dir.join("keymap.json"));
    let _ = keymap.load_override_file(&path);
    keymap
}

fn build_launch_context(
    cfg: &StartupConfig,
    started_at: Instant,
    spawn_background_sync: bool,
) -> PostboxLaunchContext {
    let data_dir = default_data_dir();
    let (store, store_loc) = open_store(cfg, &data_dir);
    let keymap = load_keymap(cfg, &data_dir);

    // Feature #42: Auto-discover Hub URL & Bearer token on startup (HKCU\Environment -> env -> DPAPI HubToken).
    // Feature #26: Auto-discover GIPHY_API_KEY -> DPAPI GiphyApiKey.
    // Feature #29: Auto-discover DEEPGRAM_API_KEY -> DPAPI DeepgramApiKey + optional Anthropic/OpenRouter keys.
    let platform = NativePlatform::default();
    let discovery =
        discover_hub_credentials_from_system(cfg.hub_url.as_deref(), &platform, &platform);
    let giphy_api_key = match discover_giphy_api_key_from_system(&platform, &platform) {
        GiphyKeyOutcome::Ready(key) => Some(key),
        GiphyKeyOutcome::Missing | GiphyKeyOutcome::Error(_) => None,
    };
    let giphy_key_available = giphy_api_key.is_some();

    let deepgram_api_key = match discover_deepgram_api_key_from_system(&platform, &platform) {
        ApiKeyOutcome::Ready(key) => Some(key),
        ApiKeyOutcome::Missing | ApiKeyOutcome::Error(_) => None,
    };
    let deepgram_key_available = deepgram_api_key.is_some();

    let format_provider = discover_format_provider_from_system(&platform, &platform);
    let llm_format_key_available = format_provider.is_some();

    // Feature #30/#32: Sync persisted Windows HKCU\Software\Messages dictation language & microphone
    // into local SQLite Store on startup (no-op on non-Windows where NativePlatform registry is empty).
    if let Ok(Some(dword)) = platform.load_dword(REG_DICTATION_LANGUAGE) {
        let lang = postbox_core::dictation::DictationLanguage::from_registry_dword(dword);
        let _ = store.save_dictation_language(lang);
    }
    if let Some(mic) = load_microphone_choice(&platform) {
        let _ = store.save_microphone_preference(Some(&mic));
    }

    let audio_backend: Arc<dyn AudioBackend> = Arc::new(NativePlatform::default());
    // Milestone 6: Windows Media Foundation + Intel Arc DXVA (mock on non-Windows tests). Media
    // Foundation only starts on the first video request, so idle RAM is unchanged.
    let video_backend: Arc<dyn VideoBackend> = Arc::new(NativePlatform::default());
    let video_thumb_cache = VideoThumbnailCache::new(data_dir.join("hub-media"));

    let (hub_status, sync_bridge) = match discovery {
        HubDiscoveryOutcome::Configured(discovered) => {
            let allow_loopback = cfg.allow_loopback || cfg.hub_url.is_some();
            match Endpoint::parse(&discovered.url, allow_loopback) {
                Ok(endpoint) => {
                    let url_display = endpoint.base_url();
                    if spawn_background_sync {
                        let (bridge, worker) = create_sync_bridge();
                        let _ = spawn_sync_engine(
                            endpoint,
                            discovered.token,
                            store_loc,
                            data_dir.clone(),
                            giphy_api_key,
                            deepgram_api_key,
                            format_provider,
                            audio_backend,
                            worker,
                        );
                        (
                            HubConnectionStatus::Connecting { url: url_display },
                            Some(bridge),
                        )
                    } else {
                        (HubConnectionStatus::Connecting { url: url_display }, None)
                    }
                }
                Err(err) => {
                    let bridge = if spawn_background_sync {
                        let (bridge, worker) = create_sync_bridge();
                        let _ = spawn_offline_audio_worker(
                            store_loc,
                            data_dir.clone(),
                            deepgram_api_key,
                            format_provider,
                            audio_backend,
                            worker,
                        );
                        Some(bridge)
                    } else {
                        None
                    };
                    (
                        HubConnectionStatus::Error {
                            url: Some(discovered.url),
                            message: format!("Invalid Hub URL: {err}"),
                        },
                        bridge,
                    )
                }
            }
        }
        HubDiscoveryOutcome::UnconfiguredMissingUrl => {
            let bridge = if spawn_background_sync {
                let (bridge, worker) = create_sync_bridge();
                let _ = spawn_offline_audio_worker(
                    store_loc,
                    data_dir.clone(),
                    deepgram_api_key,
                    format_provider,
                    audio_backend,
                    worker,
                );
                Some(bridge)
            } else {
                None
            };
            (
                HubConnectionStatus::NotConfigured {
                    missing_url: true,
                    missing_token: true,
                    configured_url: None,
                    detail: None,
                },
                bridge,
            )
        }
        HubDiscoveryOutcome::UnconfiguredMissingToken { url } => {
            let bridge = if spawn_background_sync {
                let (bridge, worker) = create_sync_bridge();
                let _ = spawn_offline_audio_worker(
                    store_loc,
                    data_dir.clone(),
                    deepgram_api_key,
                    format_provider,
                    audio_backend,
                    worker,
                );
                Some(bridge)
            } else {
                None
            };
            (
                HubConnectionStatus::NotConfigured {
                    missing_url: false,
                    missing_token: true,
                    configured_url: Some(url),
                    detail: None,
                },
                bridge,
            )
        }
        HubDiscoveryOutcome::InvalidToken { url, reason }
        | HubDiscoveryOutcome::SecretStoreError { url, reason } => {
            let bridge = if spawn_background_sync {
                let (bridge, worker) = create_sync_bridge();
                let _ = spawn_offline_audio_worker(
                    store_loc,
                    data_dir.clone(),
                    deepgram_api_key,
                    format_provider,
                    audio_backend,
                    worker,
                );
                Some(bridge)
            } else {
                None
            };
            (
                HubConnectionStatus::Error {
                    url: Some(url),
                    message: reason,
                },
                bridge,
            )
        }
    };

    PostboxLaunchContext {
        store,
        keymap,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at,
        hub_status,
        sync_bridge,
        data_dir,
        giphy_key_available,
        deepgram_key_available,
        llm_format_key_available,
        // M5.5 RAM diet: the same OS platform object owns working-set trim and
        // `GetProcessMemoryInfo` telemetry for the shell's idle/minimize hook.
        memory_platform: Some(Arc::new(NativePlatform::default())),
        video_backend: Some(video_backend),
        audio_backend: Some(Arc::new(NativePlatform::default())),
        video_thumb_cache: Some(video_thumb_cache),
        // Filled in by `main` once the startup handshake has run.
        startup_link: None,
        last_exit_record: None,
        os_integration: false,
        restore_window: cfg.restore_window,
        release_source: update_source::GithubReleaseSource::new()
            .ok()
            .map(|source| Arc::new(source) as Arc<dyn postbox_platform::ReleaseSource>),
    }
}

/// Registers `postbox://`, hands a second launch's URL to the running instance and reports
/// whether this process should open a window.
///
/// On a host without the Windows pieces this is a no-op that always says "open a window", so
/// the Linux build and the headless CI check behave exactly as before.
#[cfg_attr(not(feature = "desktop"), allow(dead_code))]
fn resolve_startup_role(cfg: &StartupConfig, data_dir: &std::path::Path) -> StartupRole {
    if cfg.no_os_integration {
        return StartupRole::Primary {
            link: cfg
                .link
                .as_deref()
                .and_then(|raw| postbox_core::deep_link::parse(raw).ok()),
        };
    }
    let user = postbox_core::os_integration::current_user();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("postbox.exe"));
    let exe_path = exe.display().to_string();
    let link = cfg
        .link
        .clone()
        .or_else(|| link_from_args(std::env::args()));

    #[cfg(all(windows, feature = "desktop"))]
    {
        use postbox_platform::os_integration::startup;

        // The Windows path takes the data directory from the OS integration layer instead.
        let _ = data_dir;
        let registry = postbox_platform::win::os::WindowsUrlHandler::new();
        let instance = postbox_platform::win::os::WindowsSingleInstance::new();
        // The updater relaunch replaces a process that is still exiting, so it waits for the
        // mutex instead of forwarding a focus signal to a window that is about to close.
        if cfg.update_relaunch {
            return startup_relaunch(
                &exe_path,
                &user,
                &registry,
                &instance,
                postbox_platform::UPDATE_RELAUNCH_WAIT,
            )
            .unwrap_or(StartupRole::Primary { link: None });
        }
        let pipe = postbox_platform::win::os::WindowsLinkPipe::new();
        match startup(
            &exe_path,
            &user,
            link.as_deref(),
            &registry,
            &instance,
            &pipe,
        ) {
            Ok(role) => {
                // The mutex must outlive `main`; leak it so it is released by process exit.
                std::mem::forget(instance);
                role
            }
            Err(err) => {
                eprintln!("postbox: OS registration failed ({err}); starting anyway");
                StartupRole::Primary {
                    link: link
                        .as_deref()
                        .and_then(|raw| postbox_core::deep_link::parse(raw).ok()),
                }
            }
        }
    }
    #[cfg(not(all(windows, feature = "desktop")))]
    {
        // Linux and test hosts have no registry or named pipe: parse the URL and open a window.
        let _ = (&exe_path, &user, data_dir);
        StartupRole::Primary {
            link: link
                .as_deref()
                .and_then(|raw| postbox_core::deep_link::parse(raw).ok()),
        }
    }
}

/// Arms the crash hook and returns the last run's record, if any.
fn arm_exit_records(
    data_dir: &std::path::Path,
    started_at: Instant,
) -> Option<postbox_core::crash::ExitRecord> {
    let store = ExitRecordStore::new(data_dir);
    let previous = store.last().ok().flatten();
    install_panic_hook(store, env!("CARGO_PKG_VERSION"), started_at);
    previous
}

fn main() {
    let started_at = Instant::now();
    let cfg = StartupConfig::from_args(std::env::args());
    let data_dir = default_data_dir();

    #[cfg(feature = "desktop")]
    {
        let last_exit = arm_exit_records(&data_dir, started_at);
        let role = resolve_startup_role(&cfg, &data_dir);
        match role {
            // Another Postbox already owns the single-instance lock and has the URL.
            StartupRole::Forwarded { .. } => {
                println!("Postbox is already running; handed the request over.");
            }
            StartupRole::Primary { link } => {
                let mut launch_ctx = build_launch_context(&cfg, started_at, true);
                launch_ctx.startup_link = link.map(|target| postbox_ui::toast_launch_url(&target));
                launch_ctx.last_exit_record = last_exit;
                launch_ctx.os_integration = !cfg.no_os_integration;
                postbox_ui::desktop::run_desktop(launch_ctx);
            }
        }
    }

    #[cfg(not(feature = "desktop"))]
    {
        let _ = arm_exit_records(&data_dir, started_at);
        let launch_ctx = build_launch_context(&cfg, started_at, false);
        let mut vm = postbox_ui::PostboxViewModel::new_uninitialized(
            launch_ctx.store,
            launch_ctx.keymap,
            launch_ctx.version,
        )
        .with_hub_status(launch_ctx.hub_status)
        .with_data_dir(launch_ctx.data_dir)
        .with_giphy_key_available(launch_ctx.giphy_key_available)
        .with_deepgram_key_available(launch_ctx.deepgram_key_available)
        .with_llm_format_key_available(launch_ctx.llm_format_key_available);
        vm.initialize();
        let elapsed_ms = launch_ctx.started_at.elapsed().as_millis();
        println!(
            "{} (headless check) | hub={} | font={} | theme={} | scale={}% | {} bindings | {} cached chats | init={}ms",
            vm.window_title(),
            vm.hub_status().palette_label(),
            postbox_ui::FONT_SANS,
            vm.theme().label(),
            vm.font_scale().percent(),
            vm.keymap().bindings().len(),
            vm.chats().len(),
            elapsed_ms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_config_parses_cli_flags() {
        let cfg = StartupConfig::from_args(vec![
            "postbox".into(),
            "--in-memory".into(),
            "--allow-loopback".into(),
            "--db".into(),
            "custom.db".into(),
            "--keymap".into(),
            "keys.json".into(),
            "--hub-url".into(),
            "http://127.0.0.1:8080".into(),
        ]);
        assert!(cfg.in_memory);
        assert!(cfg.allow_loopback);
        assert_eq!(cfg.db_path, Some(PathBuf::from("custom.db")));
        assert_eq!(cfg.keymap_path, Some(PathBuf::from("keys.json")));
        assert_eq!(cfg.hub_url.as_deref(), Some("http://127.0.0.1:8080"));
    }

    #[test]
    fn launch_context_bootstraps_in_memory_and_builds_view_model() {
        let cfg = StartupConfig {
            in_memory: true,
            hub_url: Some("https://hub.example.invalid".into()),
            ..StartupConfig::default()
        };
        let ctx = build_launch_context(&cfg, Instant::now(), false);
        let vm = postbox_ui::PostboxViewModel::new(ctx.store, ctx.keymap, ctx.version)
            .with_hub_status(ctx.hub_status);
        assert_eq!(
            vm.window_title(),
            format!("Combined inbox - Postbox v{}", env!("CARGO_PKG_VERSION"))
        );
    }
}
