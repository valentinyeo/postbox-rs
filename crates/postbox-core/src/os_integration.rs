//! Registry layout and pipe names for the Windows OS integration.
//!
//! These values are the contract between the installer, the app and `postboxctl`. Keeping
//! them here, as plain data, means the registration can be unit-tested on Linux without a
//! registry and the Windows code only has to write what this module describes.

use crate::control::CONTROL_PIPE_PREFIX;
use crate::deep_link::LINK_PIPE_PREFIX;

/// The scheme a registry handler and every toast launch payload share.
pub const URL_SCHEME: &str = "postbox";
/// Per-user protocol key (`HKCU`), so no elevation is ever needed.
pub const URL_PROTOCOL_SUBKEY: &str = r"Software\Classes\postbox";
pub const URL_PROTOCOL_ICON_SUBKEY: &str = r"Software\Classes\postbox\DefaultIcon";
pub const URL_PROTOCOL_COMMAND_SUBKEY: &str = r"Software\Classes\postbox\shell\open\command";
/// Display name Windows shows in "How do you want to open this?".
pub const URL_PROTOCOL_LABEL: &str = "URL:Postbox";
/// App User Model ID that makes unpackaged toasts show up in the Action Center.
pub const APP_USER_MODEL_ID: &str = "ValentinYeo.Postbox";
/// `HKCU\Software\Classes\AppUserModelId\<APP_USER_MODEL_ID>`.
pub const APP_USER_MODEL_SUBKEY: &str = r"Software\Classes\AppUserModelId\ValentinYeo.Postbox";
pub const APP_USER_MODEL_DISPLAY_NAME: &str = "Postbox";
/// Single-instance mutex, namespaced per user so two Windows accounts never collide.
pub const SINGLE_INSTANCE_MUTEX_PREFIX: &str = r"Local\postbox-single-";
/// What the `%1` of the protocol command receives.
pub const COMMAND_PLACEHOLDER: &str = "%1";
/// Line sent over the link pipe when a second launch only needs the window raised.
pub const FOCUS_SIGNAL: &str = "focus";
/// Longest user name honoured; longer names are hashed down by the caller.
pub const MAX_USER_NAME_CHARS: usize = 64;

/// One registry value to write. `name: None` is the key's default value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryValue {
    pub subkey: String,
    pub name: Option<String>,
    pub value: String,
}

impl RegistryValue {
    fn new(subkey: &str, name: Option<&str>, value: impl Into<String>) -> Self {
        Self {
            subkey: subkey.to_owned(),
            name: name.map(str::to_owned),
            value: value.into(),
        }
    }
}

/// The exact `HKCU` writes that register `postbox://` for the current user.
///
/// `exe_path` is the running executable, already resolved to an absolute path by the caller.
pub fn url_protocol_values(exe_path: &str) -> Vec<RegistryValue> {
    vec![
        RegistryValue::new(URL_PROTOCOL_SUBKEY, None, URL_PROTOCOL_LABEL),
        RegistryValue::new(URL_PROTOCOL_SUBKEY, Some("URL Protocol"), ""),
        RegistryValue::new(URL_PROTOCOL_ICON_SUBKEY, None, format!("{exe_path},0")),
        RegistryValue::new(
            URL_PROTOCOL_COMMAND_SUBKEY,
            None,
            handler_command_line(exe_path),
        ),
    ]
}

/// `"<exe>" "%1"`, the shell command a click runs.
pub fn handler_command_line(exe_path: &str) -> String {
    format!("\"{exe_path}\" \"{COMMAND_PLACEHOLDER}\"")
}

/// The App User Model ID values that let an unpackaged Win32 process raise toasts.
pub fn app_user_model_values() -> Vec<RegistryValue> {
    vec![
        RegistryValue::new(APP_USER_MODEL_SUBKEY, None, APP_USER_MODEL_DISPLAY_NAME),
        RegistryValue::new(
            APP_USER_MODEL_SUBKEY,
            Some("DisplayName"),
            APP_USER_MODEL_DISPLAY_NAME,
        ),
    ]
}

/// `true` when the command value is this executable's handler command.
pub fn command_matches(stored: &str, exe_path: &str) -> bool {
    stored
        .trim()
        .eq_ignore_ascii_case(handler_command_line(exe_path).trim())
}

/// `\\.\pipe\postbox-link-<user>`.
pub fn link_pipe_name(user: &str) -> String {
    format!("{LINK_PIPE_PREFIX}{user}")
}

/// `\\.\pipe\postbox-control-<user>`.
pub fn control_pipe_name(user: &str) -> String {
    format!("{CONTROL_PIPE_PREFIX}{user}")
}

/// `Local\postbox-single-<user>`.
pub fn single_instance_mutex_name(user: &str) -> String {
    format!("{SINGLE_INSTANCE_MUTEX_PREFIX}{user}")
}

/// Normalises a user name for use inside a kernel object name: ASCII alphanumerics, `-`,
/// `_` and `.` survive, everything else becomes `-`. An unusable name becomes `default`.
pub fn normalize_user(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_USER_NAME_CHARS));
    for ch in raw.trim().chars() {
        if out.len() == MAX_USER_NAME_CHARS {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(['-', '.']).to_owned();
    if trimmed.is_empty() {
        "default".to_owned()
    } else {
        trimmed
    }
}

/// The current OS user, normalised, falling back to `default`.
///
/// Reads `USERNAME` (Windows) then `USER` (Unix), which is also what the pipe names use.
pub fn current_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map(|raw| normalize_user(&raw))
        .unwrap_or_else(|_| "default".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_writes_the_four_values_windows_needs() {
        let values = url_protocol_values(r"C:\Program Files\Postbox\postbox.exe");
        assert_eq!(values.len(), 4);
        assert_eq!(values[0].subkey, URL_PROTOCOL_SUBKEY);
        assert_eq!(values[0].name, None);
        assert_eq!(values[0].value, "URL:Postbox");
        assert_eq!(values[1].name.as_deref(), Some("URL Protocol"));
        assert_eq!(values[1].value, "");
        assert_eq!(values[2].subkey, URL_PROTOCOL_ICON_SUBKEY);
        assert_eq!(values[2].value, r"C:\Program Files\Postbox\postbox.exe,0");
        assert_eq!(values[3].subkey, URL_PROTOCOL_COMMAND_SUBKEY);
        assert_eq!(
            values[3].value,
            r#""C:\Program Files\Postbox\postbox.exe" "%1""#
        );
    }

    #[test]
    fn command_line_quotes_the_exe_and_the_placeholder() {
        assert_eq!(
            handler_command_line(r"C:\Postbox\postbox.exe"),
            r#""C:\Postbox\postbox.exe" "%1""#
        );
        assert_eq!(
            handler_command_line(r"C:\Program Files\Postbox\postbox.exe"),
            r#""C:\Program Files\Postbox\postbox.exe" "%1""#
        );
        assert!(command_matches(
            r#" "C:\Postbox\postbox.exe" "%1" "#,
            r"C:\Postbox\postbox.exe"
        ));
        assert!(!command_matches(
            r#""C:\Old\postbox.exe" "%1""#,
            r"C:\Postbox\postbox.exe"
        ));
    }

    #[test]
    fn pipe_and_mutex_names_are_per_user() {
        assert_eq!(
            link_pipe_name("valentin"),
            r"\\.\pipe\postbox-link-valentin"
        );
        assert_eq!(
            control_pipe_name("valentin"),
            r"\\.\pipe\postbox-control-valentin"
        );
        assert_eq!(
            single_instance_mutex_name("valentin"),
            r"Local\postbox-single-valentin"
        );
        assert_ne!(control_pipe_name("a"), control_pipe_name("b"));
    }

    #[test]
    fn user_names_stay_inside_the_kernel_object_charset() {
        assert_eq!(normalize_user("Valentin Yeo"), "Valentin-Yeo");
        assert_eq!(normalize_user("  "), "default");
        assert_eq!(normalize_user(".."), "default");
        assert_eq!(normalize_user("a\\b/c"), "a-b-c");
        assert_eq!(normalize_user(&"x".repeat(200)).len(), MAX_USER_NAME_CHARS);
        assert!(
            normalize_user("Ünïcode Ø")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        );
    }

    #[test]
    fn app_user_model_values_make_unpackaged_toasts_visible() {
        let values = app_user_model_values();
        assert!(
            values
                .iter()
                .all(|v| v.subkey == r"Software\Classes\AppUserModelId\ValentinYeo.Postbox")
        );
        assert_eq!(values[0].value, "Postbox");
    }
}
