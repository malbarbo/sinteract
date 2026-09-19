//! The events a [`crate::display::Display`] delivers.
//!
//! Every display turns its input into the same [`InputEvent`] stream, and
//! the engine loop blocks on `wait_event` and dispatches:
//!
//! ```text
//! loop {
//!     match display.wait_event(deadline) {
//!         Ok(Event::Input(InputEvent::Vsync)) => on_frame(),
//!         Ok(Event::Input(InputEvent::Key(k))) => on_key(k),
//!         Ok(Event::Input(InputEvent::Mouse(m))) => on_mouse(m),
//!         Ok(Event::Input(InputEvent::Resize { width, height })) => on_resize(width, height),
//!         Err(NoEvent::Wake) => on_wake(),
//!         Err(NoEvent::Timeout) => on_tick(),
//!         Err(NoEvent::Close) => break,
//!     }
//! }
//! ```
//!
//! The display decides when a `Vsync` arrives, with a timer in the
//! terminal and in the window, rAF in the browser and the view on stdio. The
//! engine derives a simulation tick from the time between two.

use std::fmt;

/// What happened, as `wait_event` delivers it.
#[derive(Clone, Debug)]
pub enum Event {
    /// From the user, the platform or the peer.
    Input(InputEvent),
}

/// Why `wait_event` returned no event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoEvent {
    /// A `Sender` woke the loop, with no data.
    Wake,
    /// The deadline passed with nothing to deliver.
    Timeout,
    /// The user, the platform or the peer ended the session, or the display
    /// closed. Every wait from now on returns it.
    Close,
}

impl fmt::Display for NoEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NoEvent::Wake => "a sender woke the loop",
            NoEvent::Timeout => "the deadline passed with no event",
            NoEvent::Close => "the session ended",
        })
    }
}

impl std::error::Error for NoEvent {}

#[derive(Clone, Debug)]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// The largest scene that the display shows at scale 1 with no margin,
    /// in logical pixels. A program may ignore it and keep its size, or
    /// take this size to fill the surface.
    Resize {
        width: f32,
        height: f32,
    },
    /// The surface can be repainted now.
    Vsync,
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

/// What the primary pointer did, in the coordinates of the scene on the
/// screen. A point over the margin of a scaled window falls outside the
/// scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub x: f32,
    pub y: f32,
    pub modifiers: Modifiers,
    /// The buttons held after this event.
    pub buttons: MouseButtons,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseAction {
    Move,
    Down(MouseButton),
    Up(MouseButton),
    /// In notches of the wheel. As in W3C, `dx > 0` scrolls right and
    /// `dy > 0` scrolls down.
    Wheel {
        dx: f32,
        dy: f32,
    },
    /// The pointer left the surface, and `x` and `y` hold its last
    /// position.
    Leave,
}

/// A mouse button, numbered as the W3C `MouseEvent.button`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    Left = 0,
    Middle = 1,
    Right = 2,
    Back = 3,
    Forward = 4,
}

/// A set of mouse buttons.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MouseButtons(u8);

impl MouseButtons {
    /// Returns `true` if `button` is in the set, `false` otherwise.
    pub fn contains(self, button: MouseButton) -> bool {
        self.0 & Self::bit(button) != 0
    }

    pub fn with(self, button: MouseButton) -> Self {
        MouseButtons(self.0 | Self::bit(button))
    }

    pub fn without(self, button: MouseButton) -> Self {
        MouseButtons(self.0 & !Self::bit(button))
    }

    /// One bit per button, `1 << button`, as on the wire.
    pub fn bits(self) -> u8 {
        self.0
    }

    /// The set of the known buttons among `bits`. A bit of a button from a
    /// newer schema drops out.
    pub fn from_bits(bits: u8) -> Self {
        MouseButtons(bits & ((1 << (MouseButton::Forward as u8 + 1)) - 1))
    }

    fn bit(button: MouseButton) -> u8 {
        1 << button as u8
    }
}

/// The modifier keys held during an event.
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
    fn mouse_buttons_keep_the_known_bits() {
        let held = MouseButtons::default()
            .with(MouseButton::Left)
            .with(MouseButton::Forward)
            .without(MouseButton::Left);
        assert!(held.contains(MouseButton::Forward));
        assert!(!held.contains(MouseButton::Left));
        assert_eq!(held.bits(), 1 << 4);
        assert_eq!(MouseButtons::from_bits(0xff).bits(), 0b1_1111);
    }
}
