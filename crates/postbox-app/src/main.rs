#![cfg_attr(
    all(feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod sync;

use postbox_hub::Endpoint;
use postbox_keymap::Keymap;
use postbox_platform::{
    GiphyKeyOutcome, HubDiscoveryOutcome, NativePlatform, discover_giphy_api_key_from_system,
    discover_hub_credentials_from_system,
};
use postbox_store::Store;
use postbox_ui::{HubConnectionStatus, PostboxLaunchContext, create_sync_bridge};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use sync::{StoreLocation, spawn_sync_engine};

static MEM_DB_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StartupConfig {
    db_path: Option<PathBuf>,
    keymap_path: Option<PathBuf>,
    hub_url: Option<String>,
    in_memory: bool,
    allow_loopback: bool,
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
                _ => {}
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
    let platform = NativePlatform::default();
    let discovery =
        discover_hub_credentials_from_system(cfg.hub_url.as_deref(), &platform, &platform);
    let giphy_api_key = match discover_giphy_api_key_from_system(&platform, &platform) {
        GiphyKeyOutcome::Ready(key) => Some(key),
        GiphyKeyOutcome::Missing | GiphyKeyOutcome::Error(_) => None,
    };
    let giphy_key_available = giphy_api_key.is_some();

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
                Err(err) => (
                    HubConnectionStatus::Error {
                        url: Some(discovered.url),
                        message: format!("Invalid Hub URL: {err}"),
                    },
                    None,
                ),
            }
        }
        HubDiscoveryOutcome::UnconfiguredMissingUrl => (
            HubConnectionStatus::NotConfigured {
                missing_url: true,
                missing_token: true,
                configured_url: None,
                detail: None,
            },
            None,
        ),
        HubDiscoveryOutcome::UnconfiguredMissingToken { url } => (
            HubConnectionStatus::NotConfigured {
                missing_url: false,
                missing_token: true,
                configured_url: Some(url),
                detail: None,
            },
            None,
        ),
        HubDiscoveryOutcome::InvalidToken { url, reason }
        | HubDiscoveryOutcome::SecretStoreError { url, reason } => (
            HubConnectionStatus::Error {
                url: Some(url),
                message: reason,
            },
            None,
        ),
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
    }
}

fn main() {
    let started_at = Instant::now();
    let cfg = StartupConfig::from_args(std::env::args());

    #[cfg(feature = "desktop")]
    {
        let launch_ctx = build_launch_context(&cfg, started_at, true);
        postbox_ui::desktop::run_desktop(launch_ctx);
    }

    #[cfg(not(feature = "desktop"))]
    {
        let launch_ctx = build_launch_context(&cfg, started_at, false);
        let mut vm = postbox_ui::PostboxViewModel::new_uninitialized(
            launch_ctx.store,
            launch_ctx.keymap,
            launch_ctx.version,
        )
        .with_hub_status(launch_ctx.hub_status)
        .with_data_dir(launch_ctx.data_dir)
        .with_giphy_key_available(launch_ctx.giphy_key_available);
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
