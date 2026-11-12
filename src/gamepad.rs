//! Gamepad button combinations as hotkeys (XInput: Xbox and compatible pads).
//!
//! A combination is written as text, "LB+RB+L3+R3". It fires when the last of its buttons
//! goes down while the others are held, so a macro that presses them one poll apart still
//! counts. The reading thread exists only while a combination is set or one is being
//! learned; otherwise nothing polls the pads at all.

use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

/// A combination was pressed: wparam = the id it was set up with.
pub const WM_PAD: u32 = WM_APP + 10;
/// While learning: all buttons were let go; wparam = the buttons that were held together.
pub const WM_PAD_LEARNED: u32 = WM_APP + 11;
/// The number of connected pads changed (only reported while learning); wparam = count.
pub const WM_PAD_COUNT: u32 = WM_APP + 12;

/// Triggers count as buttons when pulled past half way.
const LT: u32 = 0x1_0000;
const RT: u32 = 0x2_0000;
const TRIGGER_DOWN: u8 = 128;

/// Names in the order they are written. Bits are XInput's, plus the two triggers.
const NAMES: [(u32, &str); 16] = [
    (0x0100, "LB"),
    (0x0200, "RB"),
    (LT, "LT"),
    (RT, "RT"),
    (0x0040, "L3"),
    (0x0080, "R3"),
    (0x0020, "Back"),
    (0x0010, "Start"),
    (0x1000, "A"),
    (0x2000, "B"),
    (0x4000, "X"),
    (0x8000, "Y"),
    (0x0001, "Up"),
    (0x0002, "Down"),
    (0x0004, "Left"),
    (0x0008, "Right"),
];

/// A single button would fire in normal play, so a combination needs at least two.
pub const MIN_BUTTONS: u32 = 2;

pub fn format(mask: u32) -> String {
    NAMES.iter().filter(|(bit, _)| mask & bit != 0).map(|(_, name)| *name).collect::<Vec<_>>().join("+")
}

/// None for an empty text (switched off), an unknown name or a single button.
pub fn parse(text: &str) -> Option<u32> {
    let mut mask = 0;
    for part in text.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        let (bit, _) = NAMES.iter().find(|(_, name)| name.eq_ignore_ascii_case(part))?;
        mask |= bit;
    }
    (mask.count_ones() >= MIN_BUTTONS).then_some(mask)
}

#[derive(Default)]
struct Shared {
    owner: isize,
    combos: Vec<(usize, u32)>,
    learning: bool,
    running: bool,
}

fn shared() -> &'static (Mutex<Shared>, Condvar) {
    static S: OnceLock<(Mutex<Shared>, Condvar)> = OnceLock::new();
    S.get_or_init(|| (Mutex::new(Shared::default()), Condvar::new()))
}

/// Sets the combinations to watch: (id posted with WM_PAD, buttons).
pub fn configure(owner: HWND, combos: Vec<(usize, u32)>) {
    let (lock, cv) = shared();
    let mut s = lock.lock().unwrap();
    s.owner = owner.0 as isize;
    s.combos = combos;
    wake(&mut s, cv);
}

/// Starts or stops learning: while on, combinations do not fire and every chord that is
/// pressed and let go is reported with WM_PAD_LEARNED.
pub fn learn(owner: HWND, on: bool) {
    let (lock, cv) = shared();
    let mut s = lock.lock().unwrap();
    s.owner = owner.0 as isize;
    s.learning = on;
    wake(&mut s, cv);
}

fn wake(s: &mut Shared, cv: &Condvar) {
    if !s.running && (s.learning || !s.combos.is_empty()) {
        s.running = true;
        thread::spawn(poll_thread);
    }
    cv.notify_all();
}

/// Buttons held on one pad, None when it is not connected.
fn read(slot: u32) -> Option<u32> {
    let mut state = XINPUT_STATE::default();
    if unsafe { XInputGetState(slot, &mut state) } != 0 {
        return None;
    }
    let g = state.Gamepad;
    let mut mask = g.wButtons.0 as u32 & 0xF3FF; // without the Guide and share bits
    if g.bLeftTrigger >= TRIGGER_DOWN {
        mask |= LT;
    }
    if g.bRightTrigger >= TRIGGER_DOWN {
        mask |= RT;
    }
    Some(mask)
}

fn post(owner: isize, msg: u32, value: usize) {
    unsafe {
        let _ = PostMessageW(Some(HWND(owner as _)), msg, WPARAM(value), LPARAM(0));
    }
}

/// The last button of the combination went down while the rest are held.
fn fires(before: u32, now: u32, combo: u32) -> bool {
    now & combo == combo && before & combo != combo
}

fn poll_thread() {
    const PERIOD: Duration = Duration::from_millis(16);
    // Asking an empty slot is slow, so those are looked at only every couple of seconds.
    const PROBE_EMPTY: Duration = Duration::from_secs(2);
    let mut held = [0u32; 4];
    let mut connected = [false; 4];
    let mut next_probe = [Instant::now(); 4];
    let mut chord = [0u32; 4];
    let mut was_learning = false;
    let mut count_sent: Option<usize> = None;
    let (lock, cv) = shared();
    loop {
        let (owner, combos, learning) = {
            let mut s = lock.lock().unwrap();
            if !s.learning && s.combos.is_empty() {
                // Nothing to do: wait a moment for new work, else end the thread.
                let (guard, _) = cv.wait_timeout(s, Duration::from_secs(5)).unwrap();
                s = guard;
                if !s.learning && s.combos.is_empty() {
                    s.running = false;
                    return;
                }
            }
            (s.owner, s.combos.clone(), s.learning)
        };
        if learning != was_learning {
            chord = [0; 4];
            count_sent = None;
            was_learning = learning;
        }

        let now = Instant::now();
        for slot in 0..4 {
            if !connected[slot] && now < next_probe[slot] {
                continue;
            }
            let state = read(slot as u32);
            connected[slot] = state.is_some();
            if state.is_none() {
                next_probe[slot] = now + PROBE_EMPTY;
            }
            let now_held = state.unwrap_or(0);
            let before = held[slot];
            held[slot] = now_held;

            if learning {
                if now_held != 0 {
                    chord[slot] |= now_held;
                } else if chord[slot] != 0 {
                    post(owner, WM_PAD_LEARNED, chord[slot] as usize);
                    chord[slot] = 0;
                }
                continue;
            }
            for (id, combo) in &combos {
                if fires(before, now_held, *combo) {
                    post(owner, WM_PAD, *id);
                }
            }
        }
        if learning {
            let count = connected.iter().filter(|c| **c).count();
            if count_sent != Some(count) {
                post(owner, WM_PAD_COUNT, count);
                count_sent = Some(count);
            }
        }
        thread::sleep(PERIOD);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let mask = parse("r3+lb+L3+RB").unwrap();
        assert_eq!(format(mask), "LB+RB+L3+R3");
        assert_eq!(parse(""), None);
        assert_eq!(parse("A"), None, "one button is not a combination");
        assert_eq!(parse("LB+Turbo"), None);
        assert_eq!(format(parse("LT+RT+Back").unwrap()), "LT+RT+Back");
    }

    #[test]
    fn fires_once_when_completed() {
        let combo = parse("LB+RB+L3+R3").unwrap();
        // A macro pressing the buttons a poll apart: only the poll that completes it fires.
        let polls = ["LB", "LB+RB", "LB+RB+L3", "LB+RB+L3+R3", "LB+RB+L3+R3", "A+LB+RB+L3+R3", ""];
        let masks: Vec<u32> = polls.iter().map(|p| p.split('+').filter(|s| !s.is_empty()).map(|n| parse(&format!("{n}+{n}")).unwrap_or_else(|| super::NAMES.iter().find(|(_, x)| x == &n).unwrap().0)).fold(0, |a, b| a | b)).collect();
        let fired: Vec<bool> = masks.windows(2).map(|w| fires(w[0], w[1], combo)).collect();
        assert_eq!(fired, [false, false, true, false, false, false]);
        // Extra buttons held do not stop it.
        assert!(fires(0x1000, 0x1000 | combo, combo));
    }
}
