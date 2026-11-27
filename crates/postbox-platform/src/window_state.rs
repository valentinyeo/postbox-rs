//! Remembered window geometry: the last monitor, the remembered bounds and whether the window
//! was maximised.
//!
//! The updater relaunches the app, and a relaunch that loses the user's window position is a
//! visible regression, so the bounds survive in the registry as one compact string:
//!
//! ```text
//! monitor=<index>;x=<i32>;y=<i32>;w=<u32>;h=<u32>;max=<0|1>
//! ```
//!
//! Restoring clamps the remembered rectangle onto a monitor that still exists, so unplugging
//! the second display cannot open the window off screen.

/// Registry value name under `HKCU\Software\Messages`.
pub const WINDOW_STATE_VALUE: &str = "WindowState";
/// Default window size when nothing was remembered.
pub const DEFAULT_WIDTH: u32 = 1240;
pub const DEFAULT_HEIGHT: u32 = 820;
/// Smallest window the layout supports.
pub const MIN_WIDTH: u32 = 720;
pub const MIN_HEIGHT: u32 = 480;

/// One monitor's work area, in virtual-screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl MonitorRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    /// `true` when at least `min_visible` pixels of `other` sit inside this monitor.
    pub fn overlaps(&self, other: &Self, min_visible: u32) -> bool {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        let width = (right - left).max(0) as u32;
        let height = (bottom - top).max(0) as u32;
        width >= min_visible && height >= min_visible
    }
}

/// Remembered geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    pub monitor: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            monitor: 0,
            x: 0,
            y: 0,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            maximized: false,
        }
    }
}

impl WindowState {
    pub fn new(x: i32, y: i32, width: u32, height: u32, maximized: bool, monitor: usize) -> Self {
        Self {
            monitor,
            x,
            y,
            width,
            height,
            maximized,
        }
    }

    /// Compact registry string.
    pub fn encode(&self) -> String {
        format!(
            "monitor={};x={};y={};w={};h={};max={}",
            self.monitor,
            self.x,
            self.y,
            self.width,
            self.height,
            u8::from(self.maximized)
        )
    }

    /// Parses a stored string. A truncated or garbled value falls back to the defaults rather
    /// than failing startup.
    pub fn decode(raw: &str) -> Self {
        let mut state = Self::default();
        let mut seen = 0u8;
        for part in raw.trim().split(';') {
            let Some((key, value)) = part.split_once('=') else {
                continue;
            };
            match key.trim() {
                "monitor" => {
                    state.monitor = value.trim().parse().unwrap_or(0);
                    seen |= 1;
                }
                "x" => {
                    state.x = value.trim().parse().unwrap_or(0);
                    seen |= 2;
                }
                "y" => {
                    state.y = value.trim().parse().unwrap_or(0);
                    seen |= 4;
                }
                "w" => {
                    state.width = value.trim().parse().unwrap_or(DEFAULT_WIDTH);
                    seen |= 8;
                }
                "h" => {
                    state.height = value.trim().parse().unwrap_or(DEFAULT_HEIGHT);
                    seen |= 16;
                }
                "max" => {
                    state.maximized = matches!(value.trim(), "1" | "true");
                    seen |= 32;
                }
                _ => {}
            }
        }
        if seen != 0b11_1111 {
            // A partial record is untrustworthy: use the default size and position.
            state.x = 0;
            state.y = 0;
        }
        state.sanitized()
    }

    /// Clamps the size to the supported range.
    pub fn sanitized(mut self) -> Self {
        self.width = self.width.clamp(MIN_WIDTH, 16_384);
        self.height = self.height.clamp(MIN_HEIGHT, 16_384);
        self
    }

    /// Fits the remembered rectangle onto the available monitors.
    ///
    /// Keeps the position when a monitor still shows the window, otherwise centres it on the
    /// remembered monitor (or the primary one).
    pub fn restored(self, monitors: &[MonitorRect]) -> Self {
        let state = self.sanitized();
        if monitors.is_empty() {
            return state;
        }
        let as_rect = MonitorRect::new(state.x, state.y, state.width, state.height);
        if monitors.iter().any(|m| m.overlaps(&as_rect, 120)) {
            // Still on a monitor, but the size may exceed it: shrink rather than move.
            let host = monitors
                .iter()
                .find(|m| m.overlaps(&as_rect, 120))
                .copied()
                .unwrap_or(monitors[0]);
            let mut fitted = state;
            fitted.width = fitted.width.min(host.width);
            fitted.height = fitted.height.min(host.height);
            return fitted;
        }
        let host = monitors.get(state.monitor).copied().unwrap_or(monitors[0]);
        Self {
            monitor: monitors.iter().position(|m| *m == host).unwrap_or(0),
            x: host.x + ((host.width.saturating_sub(state.width)) / 2) as i32,
            y: host.y + ((host.height.saturating_sub(state.height)) / 2) as i32,
            width: state.width.min(host.width),
            height: state.height.min(host.height),
            maximized: state.maximized,
        }
    }

    /// Whether the remembered geometry asks for a maximised window.
    pub fn is_maximized(&self) -> bool {
        self.maximized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> MonitorRect {
        MonitorRect::new(0, 0, 1920, 1040)
    }

    fn secondary() -> MonitorRect {
        MonitorRect::new(1920, 0, 2560, 1400)
    }

    #[test]
    fn a_remembered_position_round_trips_through_the_registry_string() {
        let state = WindowState::new(120, 80, 1400, 900, true, 1);
        let raw = state.encode();
        assert_eq!(raw, "monitor=1;x=120;y=80;w=1400;h=900;max=1");
        assert_eq!(WindowState::decode(&raw), state);
    }

    #[test]
    fn a_garbled_record_falls_back_to_the_default_size() {
        assert_eq!(WindowState::decode(""), WindowState::default());
        assert_eq!(WindowState::decode("junk"), WindowState::default());
        let partial = WindowState::decode("w=1400;h=900");
        assert_eq!(partial.width, 1400);
        assert_eq!(partial.height, 900);
        assert_eq!(
            (partial.x, partial.y),
            (0, 0),
            "an unpositioned record is centred later"
        );
    }

    #[test]
    fn the_size_is_clamped_to_what_the_layout_supports() {
        let tiny = WindowState::decode("monitor=0;x=0;y=0;w=10;h=10;max=0");
        assert_eq!(tiny.width, MIN_WIDTH);
        assert_eq!(tiny.height, MIN_HEIGHT);
    }

    #[test]
    fn a_remembered_window_on_an_unplugged_monitor_is_recentred() {
        let on_secondary = WindowState::new(2100, 200, 1200, 800, false, 1);
        // The second monitor was unplugged; only the primary is left.
        let restored = on_secondary.restored(&[primary()]);
        assert_eq!(restored.monitor, 0);
        assert_eq!(restored.width, 1200);
        assert_eq!(restored.x, (1920 - 1200) / 2);
        assert_eq!(restored.y, (1040 - 800) / 2);
        assert!(restored.x >= primary().x && restored.x < primary().right());
    }

    #[test]
    fn a_window_still_on_its_monitor_keeps_its_position() {
        let state = WindowState::new(100, 60, 1200, 800, false, 0);
        let restored = state.restored(&[primary(), secondary()]);
        assert_eq!((restored.x, restored.y), (100, 60));
        assert_eq!((restored.width, restored.height), (1200, 800));
    }

    #[test]
    fn a_window_bigger_than_its_monitor_is_shrunk_not_moved() {
        let state = WindowState::new(0, 0, 4000, 3000, false, 0);
        let restored = state.restored(&[primary()]);
        assert_eq!(restored.width, 1920);
        assert_eq!(restored.height, 1040);
        assert_eq!((restored.x, restored.y), (0, 0));
    }

    #[test]
    fn a_maximised_window_stays_maximised_after_the_relaunch() {
        let state = WindowState::new(2100, 200, 1200, 800, true, 1);
        let restored = state.restored(&[primary(), secondary()]);
        assert!(restored.is_maximized());
        assert_eq!(restored.monitor, 1);
        assert_eq!((restored.x, restored.y), (2100, 200));
    }

    #[test]
    fn no_monitors_reports_the_sanitized_state_instead_of_panicking() {
        let state = WindowState::new(-50, -50, 800, 600, false, 3);
        let restored = state.restored(&[]);
        assert_eq!((restored.x, restored.y), (-50, -50));
        assert_eq!(restored.width, 800);
    }
}
