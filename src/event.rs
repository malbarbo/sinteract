//! Input events delivered to the engine via [`crate::frontend::Frontend`].
//!
//! Every frontend (terminal, window, stdio, future browser) collapses native
//! input into the same [`InputEvent`] stream. The host's main loop blocks on
//! `wait_event` and dispatches:
//!
//! ```text
//! while let Some(ev) = frontend.wait_event(deadline) {
//!     match ev {
//!         InputEvent::Vsync          => on_frame(),
//!         InputEvent::Key(KeyEvent { kind: KeyKind::Press, .. }) => ...,
//!         InputEvent::Close          => break,
//!         _ => {}
//!     }
//! }
//! ```
//!
//! `Vsync` is a frame-opportunity event: the frontend (its OS swap chain in a
//! window, an internal timer in the terminal, rAF in the browser, the peer in
//! stdio multiplayer) decides when one lands. The engine does not own the
//! clock. The host loop is free to derive its own "tick" (logical simulation
//! step) on top by accumulating elapsed time between Vsyncs.

/// Modifier-key bitmask. Matches the order spython has historically used in
/// its FFI `[bool; 5]`: `[alt, ctrl, shift, meta, repeat]`.
pub const MOD_ALT: u8 = 1 << 0;
pub const MOD_CTRL: u8 = 1 << 1;
pub const MOD_SHIFT: u8 = 1 << 2;
pub const MOD_META: u8 = 1 << 3;
pub const MOD_REPEAT: u8 = 1 << 4;

/// Distinguishes press from down/up. Terminals only ever produce `Press`
/// (they do not separate down from up); native windows and the browser
/// produce all three. Hosts that want the lowest-common-denominator API can
/// match on `Press` only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyKind {
    #[default]
    Press = 0,
    Down = 1,
    Up = 2,
}

impl KeyKind {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Down,
            2 => Self::Up,
            _ => Self::Press,
        }
    }
}

/// A single keyboard event. `key` is the JS-style key name
/// (`"ArrowLeft"`, `"Enter"`, `"a"`, …) — same convention as the existing
/// host code so renames are minimal.
#[derive(Clone, Debug, Default)]
pub struct KeyEvent {
    pub kind: KeyKind,
    pub key: String,
    pub modifiers: u8,
}

impl KeyEvent {
    pub fn alt(&self) -> bool {
        self.modifiers & MOD_ALT != 0
    }
    pub fn ctrl(&self) -> bool {
        self.modifiers & MOD_CTRL != 0
    }
    pub fn shift(&self) -> bool {
        self.modifiers & MOD_SHIFT != 0
    }
    pub fn meta(&self) -> bool {
        self.modifiers & MOD_META != 0
    }
    pub fn repeat(&self) -> bool {
        self.modifiers & MOD_REPEAT != 0
    }
}

/// What landed on the input queue. `Vsync` and `Close` are not key events;
/// they are first-class so handlers do not have to invent sentinel keys.
#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    /// One frame opportunity — the surface can be repainted now. Emitted by
    /// each frontend at its native cadence (timer in terminal, swap-chain
    /// vsync in window, rAF in browser, peer in stdio).
    Vsync,
    /// Window/terminal closed, or transport shut down. The host loop should
    /// exit cleanly.
    Close,
}

impl InputEvent {
    pub fn is_vsync(&self) -> bool {
        matches!(self, InputEvent::Vsync)
    }
    pub fn is_close(&self) -> bool {
        matches!(self, InputEvent::Close)
    }
    pub fn as_key(&self) -> Option<&KeyEvent> {
        match self {
            InputEvent::Key(k) => Some(k),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_helpers_read_bits() {
        let k = KeyEvent {
            kind: KeyKind::Press,
            key: "a".into(),
            modifiers: MOD_CTRL | MOD_SHIFT,
        };
        assert!(k.ctrl());
        assert!(k.shift());
        assert!(!k.alt());
        assert!(!k.meta());
        assert!(!k.repeat());
    }

    #[test]
    fn input_event_classifiers() {
        assert!(InputEvent::Vsync.is_vsync());
        assert!(!InputEvent::Vsync.is_close());
        assert!(InputEvent::Close.is_close());
        let k = InputEvent::Key(KeyEvent::default());
        assert!(!k.is_vsync());
        assert!(k.as_key().is_some());
    }

    #[test]
    fn key_kind_from_u8_clamps() {
        assert_eq!(KeyKind::from_u8(0), KeyKind::Press);
        assert_eq!(KeyKind::from_u8(1), KeyKind::Down);
        assert_eq!(KeyKind::from_u8(2), KeyKind::Up);
        assert_eq!(KeyKind::from_u8(99), KeyKind::Press);
    }
}
