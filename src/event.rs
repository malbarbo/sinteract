//! The input events a [`crate::frontend::Frontend`] delivers.
//!
//! Every frontend turns its input into the same [`InputEvent`] stream, and
//! the host loop blocks on `wait_event` and dispatches:
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
//! The frontend decides when a `Vsync` arrives, with a timer in the
//! terminal, the swap chain in the window, rAF in the browser and the peer
//! on stdio. The host derives a simulation tick from the time between two.

/// The modifier bits, in the order of the spython FFI.
pub const MOD_ALT: u8 = 1 << 0;
pub const MOD_CTRL: u8 = 1 << 1;
pub const MOD_SHIFT: u8 = 1 << 2;
pub const MOD_META: u8 = 1 << 3;
pub const MOD_REPEAT: u8 = 1 << 4;

/// A terminal produces only `Press`, since it does not tell down from up.
/// The window and the browser produce all three.
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

/// `key` is the JS key name, such as `"ArrowLeft"`, `"Enter"` or `"a"`.
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

#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    /// The surface can be repainted now.
    Vsync,
    /// The window or the terminal closed, or the transport shut down.
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
