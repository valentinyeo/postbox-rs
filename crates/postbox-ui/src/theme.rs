use postbox_core::ThemeChoice;

/// Semantic shadcn/ui color roles (RGB hex) with crisp 1px borders and focus rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadcnPalette {
    pub background: u32,
    pub foreground: u32,
    pub sidebar: u32,
    pub card: u32,
    pub popover: u32,
    pub primary: u32,
    pub primary_foreground: u32,
    pub secondary: u32,
    pub muted: u32,
    pub muted_foreground: u32,
    pub accent: u32,
    pub border: u32,
    pub ring: u32,
    pub destructive: u32,
    pub bubble_outgoing: u32,
    pub bubble_incoming: u32,
}

impl ShadcnPalette {
    pub fn for_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::ZincDark => Self {
                background: 0x09090B,
                foreground: 0xFAFAFA,
                sidebar: 0x0C0C0F,
                card: 0x111114,
                popover: 0x111114,
                primary: 0xFAFAFA,
                primary_foreground: 0x18181B,
                secondary: 0x18181B,
                muted: 0x18181B,
                muted_foreground: 0xA1A1AA,
                accent: 0x27272A,
                border: 0x27272A,
                ring: 0xD4D4D8,
                destructive: 0xEF4444,
                bubble_outgoing: 0x27272A,
                bubble_incoming: 0x141417,
            },
            ThemeChoice::Amoled => Self {
                background: 0x000000,
                foreground: 0xFAFAFA,
                sidebar: 0x050506,
                card: 0x0A0A0C,
                popover: 0x0A0A0C,
                primary: 0xFAFAFA,
                primary_foreground: 0x000000,
                secondary: 0x121215,
                muted: 0x121215,
                muted_foreground: 0x9CA3AF,
                accent: 0x1F1F23,
                border: 0x222226,
                ring: 0xD4D4D8,
                destructive: 0xEF4444,
                bubble_outgoing: 0x1F1F24,
                bubble_incoming: 0x0D0D10,
            },
            ThemeChoice::Light => Self {
                background: 0xFFFFFF,
                foreground: 0x09090B,
                sidebar: 0xFAFAFA,
                card: 0xFFFFFF,
                popover: 0xFFFFFF,
                primary: 0x18181B,
                primary_foreground: 0xFAFAFA,
                secondary: 0xF4F4F5,
                muted: 0xF4F4F5,
                muted_foreground: 0x71717A,
                accent: 0xF4F4F5,
                border: 0xE4E4E7,
                ring: 0x18181B,
                destructive: 0xDC2626,
                bubble_outgoing: 0x18181B,
                bubble_incoming: 0xF4F4F5,
            },
        }
    }
}
