//! The events that a [`display`](crate::display) delivers.
//!
//! Every display turns its input into the same [`InputEvent`] stream, adds
//! a [`Event::Tick`] at each frame, and the engine loop blocks on
//! `wait_event` and dispatches:
//!
//! ```text
//! loop {
//!     match display.wait_event(deadline) {
//!         Ok(Event::Tick) => on_frame(),
//!         Ok(Event::Input(InputEvent::Key(k))) => on_key(k),
//!         Ok(Event::Input(InputEvent::Mouse(m))) => on_mouse(m),
//!         Ok(Event::Input(InputEvent::Resize { width, height })) => on_resize(width, height),
//!         Ok(Event::Input(InputEvent::Pad(p))) => on_pad(p),
//!         Err(Interrupt::Wake) => on_wake(),
//!         Err(Interrupt::Timeout) => on_tick(),
//!         Err(Interrupt::Read(e)) => report(e),
//!         Err(Interrupt::Close) => break,
//!     }
//! }
//! ```
//!
//! The display decides when a `Tick` arrives, with a timer in the
//! terminal and in the window and rAF in the browser. In a session, the
//! tick of the server arrives as a `SessionEvent::Tick`. The engine
//! derives a simulation tick from the time between two.

/// What happened, as `wait_event` delivers it.
#[derive(Clone, Debug)]
pub enum Event {
    /// From the user or the platform.
    Input(InputEvent),
    /// Time for the next frame.
    Tick,
}

/// What interrupted a `wait_event` that delivered no event. A failure comes
/// with the error that caused it, since the library writes no message of its
/// own.
#[derive(Debug)]
pub enum Interrupt {
    /// A `Sender` woke the loop, with no data.
    Wake,
    /// The deadline passed with nothing to deliver.
    Timeout,
    /// A read from the tty failed, and a `Close` comes right after.
    Read(std::io::Error),
    /// The user or the platform ended the session, or the display closed.
    /// Every wait from now on returns it.
    Close,
}

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
    /// A button of a pad, or a pad that the view found or lost.
    Pad(PadEvent),
}

impl InputEvent {
    /// Returns `true` if `self` makes `old` worthless, `false` otherwise.
    /// Only the latest move of the mouse and the latest resize count.
    pub fn supersedes(&self, old: &InputEvent) -> bool {
        let is_move = |e: &InputEvent| {
            matches!(
                e,
                InputEvent::Mouse(MouseEvent {
                    action: MouseAction::Move,
                    ..
                })
            )
        };
        let is_resize = |e: &InputEvent| matches!(e, InputEvent::Resize { .. });
        (is_move(self) && is_move(old)) || (is_resize(self) && is_resize(old))
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
/// comes up. So does a terminal that speaks the keyboard protocol of Kitty,
/// and Windows Terminal. Any other terminal sends `Press` alone, since it
/// does not see a key go down or come up.
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

impl MouseButton {
    /// Every button, in the order of the discriminants.
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Middle,
        MouseButton::Right,
        MouseButton::Back,
        MouseButton::Forward,
    ];
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
        MouseButtons(bits & ((1 << MouseButton::ALL.len()) - 1))
    }

    fn bit(button: MouseButton) -> u8 {
        1 << button as u8
    }
}

/// The input of a gamepad or of a pad on the screen of the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadEvent {
    Down(PadButton),
    Up(PadButton),
    /// The view found a pad, which sends the buttons from now on.
    Connected,
    /// The view lost its pad.
    Disconnected,
}

/// A button of a pad, named as in the standard layout of the W3C Gamepad
/// API. `A` is the bottom button of the right cluster, `B` the right one,
/// `X` the left one and `Y` the top one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PadButton {
    Up = 0,
    Down = 1,
    Left = 2,
    Right = 3,
    A = 4,
    B = 5,
    X = 6,
    Y = 7,
    LeftShoulder = 8,
    RightShoulder = 9,
    Select = 10,
    Start = 11,
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
