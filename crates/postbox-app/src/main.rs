#![cfg_attr(
    all(feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use postbox_hub::Endpoint;
use postbox_keymap::Keymap;
use postbox_platform::{MemoryPlatform, SecretStore};
use postbox_store::Store;
use postbox_ui::PostboxLaunchContext;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct StartupConfig {
    db_path: Option<PathBuf>,
    keymap_path: Option<PathBuf>,
    hub_url: Option<String>,
    in_memory: bool,
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

fn open_store(cfg: &StartupConfig, data_dir: &Path) -> Store {
    if cfg.in_memory {
        return Store::open_in_memory().expect("in-memory SQLite store init");
    }
    let db_path = cfg
        .db_path
        .clone()
        .unwrap_or_else(|| data_dir.join("postbox.db"));
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match Store::open(&db_path) {
        Ok(store) => store,
        Err(_) => Store::open_in_memory().expect("fallback in-memory SQLite store init"),
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

fn build_launch_context(cfg: &StartupConfig, started_at: Instant) -> PostboxLaunchContext {
    let data_dir = default_data_dir();
    let store = open_store(cfg, &data_dir);
    let keymap = load_keymap(cfg, &data_dir);

    // Validate configured Hub endpoint and initialize platform boundary cleanly.
    let platform = MemoryPlatform::default();
    if let Some(url) = &cfg.hub_url
        && Endpoint::parse(url, true).is_ok()
    {
        let _ = platform.save_secret("WAZIG_HUB_URL", url);
    }

    PostboxLaunchContext {
        store,
        keymap,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at,
    }
}

fn main() {
    let started_at = Instant::now();
    let cfg = StartupConfig::from_args(std::env::args());
    let launch_ctx = build_launch_context(&cfg, started_at);

    #[cfg(feature = "desktop")]
    {
        postbox_ui::desktop::run_desktop(launch_ctx);
    }

    #[cfg(not(feature = "desktop"))]
    {
        let vm = postbox_ui::PostboxViewModel::new(
            launch_ctx.store,
            launch_ctx.keymap,
            launch_ctx.version,
        );
        let elapsed_ms = launch_ctx.started_at.elapsed().as_millis();
        println!(
            "{} (headless check) | font={} | theme={} | scale={}% | {} bindings | {} cached chats | init={}ms",
            vm.window_title(),
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
            "--db".into(),
            "custom.db".into(),
            "--keymap".into(),
            "keys.json".into(),
            "--hub-url".into(),
            "http://127.0.0.1:8080".into(),
        ]);
        assert!(cfg.in_memory);
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
        let ctx = build_launch_context(&cfg, Instant::now());
        let vm = postbox_ui::PostboxViewModel::new(ctx.store, ctx.keymap, ctx.version);
        assert_eq!(
            vm.window_title(),
            format!("Combined inbox - Postbox v{}", env!("CARGO_PKG_VERSION"))
        );
    }
}
