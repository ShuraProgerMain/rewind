//! Hotkeys written as text: "Ctrl+Alt+F10", "Win+Shift+R", "PrintScreen".

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: HOT_KEY_MODIFIERS,
    pub vk: u32,
}

const NAMED: &[(&str, u32)] = &[
    ("printscreen", 0x2C),
    ("prtsc", 0x2C),
    ("scrolllock", 0x91),
    ("pause", 0x13),
    ("insert", 0x2D),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("space", 0x20),
    ("tab", 0x09),
    ("backspace", 0x08),
    ("up", 0x26),
    ("down", 0x28),
    ("left", 0x25),
    ("right", 0x27),
    ("`", 0xC0),
    ("-", 0xBD),
    ("=", 0xBB),
    ("[", 0xDB),
    ("]", 0xDD),
    ("\\", 0xDC),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("/", 0xBF),
];

fn key_code(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    if let Some(num) = n.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        return (1..=24).contains(&num).then(|| 0x70 + num - 1);
    }
    if let Some(num) = n.strip_prefix("num").and_then(|d| d.parse::<u32>().ok()) {
        return (num <= 9).then(|| 0x60 + num);
    }
    if n.len() == 1 {
        let c = n.as_bytes()[0];
        if c.is_ascii_lowercase() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    NAMED.iter().find(|(k, _)| *k == n).map(|(_, v)| *v)
}

/// None for an empty string (hotkey disabled) or one that cannot be understood.
pub fn parse(text: &str) -> Option<Hotkey> {
    let mut mods = MOD_NOREPEAT;
    let mut vk = None;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "windows" | "super" => mods |= MOD_WIN,
            _ => {
                if vk.is_some() {
                    return None; // two main keys
                }
                vk = Some(key_code(part)?);
            }
        }
    }
    vk.map(|vk| Hotkey { mods, vk })
}
