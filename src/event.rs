//! The events a [`crate::display::Display`] delivers.
//!
//! Every display turns its input into the same [`InputEvent`] stream, and
//! the engine loop blocks on `wait_event` and dispatches:
//!
//! ```text
//! loop {
//!     match display.wait_event(deadline) {
//!         Event::Input(InputEvent::Vsync) => on_frame(),
//!         Event::Input(InputEvent::Key(k)) => on_key(k),
//!         Event::Input(InputEvent::Close) => break,
//!         Event::Reply { id, body } => on_reply(id, body),
//!         Event::Timeout => on_tick(),
//!     }
//! }
//! ```
//!
//! The display decides when a `Vsync` arrives, with a timer in the
//! terminal and in the window, rAF in the browser and the view on stdio. The
//! engine derives a simulation tick from the time between two.

/// What `wait_event` returns.
#[derive(Clone, Debug)]
pub enum Event {
    /// From the user, the platform or the peer.
    Input(InputEvent),
    /// What a `Sender` of the display pushed from any thread. The engine
    /// picks `id` and `body`, and a reply never crosses the wire.
    Reply { id: u64, body: Vec<u8> },
    /// The deadline passed with nothing to deliver.
    Timeout,
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

/// A key that went down, repeats or came up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub kind: KeyKind,
    /// The W3C `KeyboardEvent.key` value: a name in [`key`] for a key that
    /// types no text, or the text the key types, such as `"a"`, `"A"` or
    /// `" "`.
    pub key: String,
    pub modifiers: Modifiers,
    /// The key is held and the system repeats it.
    pub repeat: bool,
}

/// What happened to a key. The window and a browser send `Down` and then
/// `Press` when a key goes down and each time it repeats, and `Up` when it
/// comes up. A terminal sends `Press` alone, since it does not see a key go
/// down or come up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum KeyKind {
    /// The key typed, when it goes down and each time it repeats. Every
    /// display sends it.
    Press = 0,
    /// The key went down or repeats, just before its `Press`.
    Down = 1,
    /// The key came up.
    Up = 2,
}

/// The names of the keys that type no text, as [`KeyEvent::key`] carries
/// them from the terminal and the window. Each is the W3C
/// `KeyboardEvent.key` value of its key. A browser peer may send any other
/// W3C name.
pub mod key {
    pub const ARROW_LEFT: &str = "ArrowLeft";
    pub const ARROW_RIGHT: &str = "ArrowRight";
    pub const ARROW_UP: &str = "ArrowUp";
    pub const ARROW_DOWN: &str = "ArrowDown";
    pub const PAGE_UP: &str = "PageUp";
    pub const PAGE_DOWN: &str = "PageDown";
    pub const HOME: &str = "Home";
    pub const END: &str = "End";
    pub const BACKSPACE: &str = "Backspace";
    pub const TAB: &str = "Tab";
    pub const ENTER: &str = "Enter";
    pub const ESCAPE: &str = "Escape";
    pub const DELETE: &str = "Delete";
    pub const INSERT: &str = "Insert";
    pub const F1: &str = "F1";
    pub const F2: &str = "F2";
    pub const F3: &str = "F3";
    pub const F4: &str = "F4";
    pub const F5: &str = "F5";
    pub const F6: &str = "F6";
    pub const F7: &str = "F7";
    pub const F8: &str = "F8";
    pub const F9: &str = "F9";
    pub const F10: &str = "F10";
    pub const F11: &str = "F11";
    pub const F12: &str = "F12";

    /// Every name above.
    pub const ALL: &[&str] = &[
        ARROW_LEFT,
        ARROW_RIGHT,
        ARROW_UP,
        ARROW_DOWN,
        PAGE_UP,
        PAGE_DOWN,
        HOME,
        END,
        BACKSPACE,
        TAB,
        ENTER,
        ESCAPE,
        DELETE,
        INSERT,
        F1,
        F2,
        F3,
        F4,
        F5,
        F6,
        F7,
        F8,
        F9,
        F10,
        F11,
        F12,
    ];

    /// `F1` to `F12`, in order.
    pub const FUNCTION_KEYS: [&str; 12] = [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12];
}

/// The modifier keys held during a key event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub alt: bool,
    pub ctrl: bool,
    pub shift: bool,
    /// The Windows, Command or Super key.
    pub meta: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_classifiers() {
        assert!(InputEvent::Vsync.is_vsync());
        assert!(!InputEvent::Vsync.is_close());
        assert!(InputEvent::Close.is_close());
        let k = InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: "a".into(),
            modifiers: Modifiers::default(),
            repeat: false,
        });
        assert!(!k.is_vsync());
        assert!(k.as_key().is_some());
    }
}
