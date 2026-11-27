//! WinRT Toast notifications.
//!
//! The markup itself comes from `postbox_core::toast`, which is pure and unit-tested. This
//! module only loads that markup into a WinRT `XmlDocument` and shows it through
//! `ToastNotificationManager::CreateToastNotifierWithId` for the registered App User Model ID,
//! so an unpackaged Win32 process gets a toast that lands in the Action Center.
//!
//! A click runs the `postbox://` launch target through the registered protocol handler, which
//! is the same path an in-app link takes: single instance check, link pipe, chat select.

use crate::PlatformError;
use postbox_core::ChatKey;
use postbox_core::os_integration::APP_USER_MODEL_ID;
use postbox_core::toast;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

/// Builds the markup, shows it, and returns the `postbox://` target the click will open.
pub fn show_message_toast(
    chat: &ChatKey,
    title: &str,
    body: &str,
) -> Result<String, PlatformError> {
    let xml = toast::build_xml(chat, title, body);
    let launch = toast::launch_url(chat, None);
    let document = XmlDocument::new().map_err(|_| PlatformError::OsUnavailable)?;
    document
        .LoadXml(&HSTRING::from(xml.as_str()))
        .map_err(|_| PlatformError::OsUnavailable)?;

    let notification = ToastNotification::CreateToastNotification(&document)
        .map_err(|_| PlatformError::OsUnavailable)?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID))
            .map_err(|_| PlatformError::OsUnavailable)?;
    notifier
        .Show(&notification)
        .map_err(|_| PlatformError::OsUnavailable)?;
    Ok(launch)
}

/// Removes every toast this app raised, used on quit so a stale alert never opens a dead app.
/// Best effort: a shell without a notification history is not an error worth reporting.
pub fn clear_toasts() -> Result<(), PlatformError> {
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID))
            .map_err(|_| PlatformError::OsUnavailable)?;
    let history = ToastNotificationManager::History().map_err(|_| PlatformError::OsUnavailable)?;
    let _ = history.ClearWithId(&HSTRING::from(APP_USER_MODEL_ID));
    let _ = history.Clear();
    let _ = notifier;
    Ok(())
}

/// `true` when toasts can actually be raised on this host.
///
/// Returns `false` when the process runs without an interactive session, which is how the
/// headless CI check stays quiet.
pub fn toasts_available() -> bool {
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID)).is_ok()
}
