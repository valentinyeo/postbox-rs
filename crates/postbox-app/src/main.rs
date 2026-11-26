#![cfg_attr(
    all(feature = "desktop", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use postbox_core::ViewTab;
use postbox_keymap::Keymap;
use postbox_store::Store;

fn main() {
    #[cfg(feature = "desktop")]
    {
        postbox_ui::desktop::run_desktop();
    }

    #[cfg(not(feature = "desktop"))]
    {
        let store = Store::open_in_memory().expect("in-memory SQLite store init");
        let chats = store.list_inbox_chats(false, 50).expect("query inbox");
        let keymap = Keymap::postbox_defaults();
        let title = ViewTab::CombinedInbox.window_title(env!("CARGO_PKG_VERSION"));
        println!(
            "{title} (headless check) | font={} | {} default bindings | {} cached inbox chats",
            postbox_ui::FONT_SANS,
            keymap.bindings().len(),
            chats.len()
        );
    }
}
