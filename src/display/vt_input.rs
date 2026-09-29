//! The input that a terminal sends, read through the state machine of
//! vtparse.
//!
//! Under the keyboard protocol of Kitty, with [`KITTY_KEYBOARD_FLAGS`], every
//! key arrives as an escape that names the key without the modifiers, the
//! key with Shift, and the text that the key types. A Down records the name
//! of its key, and the Up of that key sends the same name, as the window
//! does with the physical key. So Shift+a sends Down `A` and Up `A` even
//! when Shift comes up first, and a dead key followed by `e` sends `é`.
//!
//! Under the win32-input-mode of Windows Terminal, `CSI ? 9001 h`, every key
//! arrives as `CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`, with the virtual key, the
//! scan code, the character, 1 for a key down or 0 for a key up, and the
//! state of the modifiers. The Up finds its Down by the virtual key and the
//! scan code, so it too sends the name of the Down.
//!
//! Without either protocol a key arrives as its text, a control byte or an
//! xterm escape, and nothing tells a key down from a key up, so each key is
//! a Press alone.

use vtparse::{CsiParam, VTActor, VTParser};

use crate::event::{KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, key};

/// Disambiguate the escapes (1), report the event types (2), report the
/// shifted key (4), report every key as an escape (8) and report the text
/// of a key (16). The push is `CSI > 31 u`.
/// Windows Terminal takes win32-input-mode instead.
#[cfg(unix)]
pub(super) const KITTY_KEYBOARD_FLAGS: u8 = 0b1_1111;

/// What the parser takes out of the bytes.
#[derive(Debug, PartialEq)]
pub(super) enum Input {
    Key(KeyEvent),
    Mouse(Mouse),
    /// Ctrl-C.
    Interrupt,
}

/// A mouse event at a cell, which counts from 0. The display maps the cell
/// to the scene.
#[derive(Debug, PartialEq)]
pub(super) struct Mouse {
    pub action: MouseAction,
    pub column: u16,
    pub row: u16,
    pub modifiers: Modifiers,
    /// The buttons held after this event.
    pub buttons: MouseButtons,
}

/// The bytes of the terminal in, the input out. The parser keeps an escape
/// that a read cut in two.
pub(super) struct VtInput {
    parser: VTParser,
    keys: Keys,
    /// The last byte of the last read was an Escape.
    escape_last: bool,
}

impl VtInput {
    /// The input of a terminal under the keyboard protocol of Kitty when
    /// `kitty` holds, and of a terminal without it otherwise.
    pub(super) fn new(kitty: bool) -> Self {
        Self {
            parser: VTParser::new(),
            keys: Keys {
                kitty,
                held: Vec::new(),
                ss3: false,
                buttons: MouseButtons::default(),
            },
            escape_last: false,
        }
    }

    /// Parse `bytes` after the ones of the previous call.
    ///
    /// The terminal sends no string once the probe is over, so an Escape
    /// before a byte that opens one is Alt with that key. The parser would
    /// take every key after it into the string, Ctrl-C too.
    pub(super) fn feed(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        let Some(&last) = bytes.last() else {
            return;
        };
        let mut rest = bytes;
        if self.escape_last
            && let [opener, after @ ..] = rest
            && opens_string(*opener)
        {
            self.parser = VTParser::new();
            self.keys.print(char::from(*opener), ALT, out);
            rest = after;
        }
        while let Some(i) = rest
            .windows(2)
            .position(|pair| matches!(pair, [0x1b, b] if opens_string(*b)))
        {
            let (head, tail) = rest.split_at(i);
            self.parse(head, out);
            let (&[_, opener], after) = tail
                .split_first_chunk()
                .expect("the window holds the Escape and the opener");
            self.keys.print(char::from(opener), ALT, out);
            rest = after;
        }
        self.parse(rest, out);
        self.escape_last = last == 0x1b;
    }

    fn parse(&mut self, bytes: &[u8], out: &mut Vec<Input>) {
        let mut dispatch = Dispatch {
            keys: &mut self.keys,
            out,
        };
        self.parser.parse(bytes, &mut dispatch);
    }

    /// Take a lone Escape that waits for the rest of an escape. The terminal
    /// sends an escape in one write, so a read that brought nothing after it
    /// means the byte stands alone.
    pub(super) fn flush(&mut self, out: &mut Vec<Input>) {
        if self.escape_last && !self.parser.is_ground() {
            self.parser = VTParser::new();
            self.escape_last = false;
            self.keys.typed(key::ESCAPE, Modifiers::default(), out);
        }
    }
}

/// The keys that are down, and what the input so far leaves open.
struct Keys {
    /// The terminal is under the keyboard protocol of Kitty.
    kitty: bool,
    /// The keys that are down, under the protocol.
    held: Vec<(KeyId, String)>,
    /// An `ESC O` came, and the next character names a key.
    ss3: bool,
    /// The mouse buttons that are down, since a report names only the
    /// button of its own event.
    buttons: MouseButtons,
}

impl Keys {
    /// Send the Up of every key that is down, as when the terminal loses the
    /// focus and the releases go to another window.
    fn release_all(&mut self, out: &mut Vec<Input>) {
        for (_, name) in self.held.drain(..) {
            out.push(Input::Key(key_event(
                KeyKind::Up,
                name,
                Modifiers::default(),
            )));
        }
    }

    fn csi(&mut self, csi: &Csi, out: &mut Vec<Input>) {
        let mods = csi.int(1, 0).unwrap_or(1);
        let event = csi.int(1, 1).unwrap_or(1);
        match (csi.marker, csi.final_byte) {
            (None, b'u') => {
                let Some(code) = csi.int(0, 0) else {
                    return;
                };
                let shifted = csi.int(0, 1).and_then(char::from_u32);
                let text = csi.fields(2);
                self.key(KeyId::Code(code), shifted, mods, event, &text, out);
            }
            (None, b'~') => {
                if let Some(n) = csi.int(0, 0) {
                    self.key(KeyId::Tilde(n), None, mods, event, &[], out);
                }
            }
            (None, b'O') if csi.params.is_empty() => self.release_all(out),
            (None, b'_') => self.win32(csi, out),
            // A cursor position report, which shares the letter of F3.
            (None, b'R') => {}
            // Shift+Tab without the protocol.
            (None, b'Z') => self.key(KeyId::Letter(b'Z'), None, 2, 1, &[], out),
            (Some(b'<'), b'M' | b'm') => {
                let released = csi.final_byte == b'm';
                if let Some(cb) = csi.int(0, 0) {
                    self.mouse(cb, csi, released, out);
                }
            }
            // An rxvt report, `CSI b ; x ; y M`, with the button plus 32,
            // for a terminal that does not take the SGR mode.
            (None, b'M') => {
                if let Some(cb) = csi.int(0, 0).and_then(|cb| cb.checked_sub(32)) {
                    self.mouse(cb, csi, false, out);
                }
            }
            (None, final_byte) => {
                if let Some(id) = letter_key(final_byte) {
                    self.key(id, None, mods, event, &[], out);
                }
            }
            // A reply to a query, such as the flags of the protocol.
            (Some(_), _) => {}
        }
    }

    /// Act on the key `id`, with the `mods` and the `event` of the protocol,
    /// which count from 1.
    fn key(
        &mut self,
        id: KeyId,
        shifted: Option<char>,
        mods: u32,
        event: u32,
        text: &[u32],
        out: &mut Vec<Input>,
    ) {
        let modifiers = modifiers(mods);
        if event == RELEASE {
            // A release with no press came for a key that went down before
            // the session, or before a focus loss.
            if let Some(i) = self.held.iter().position(|(held, _)| *held == id) {
                let (_, name) = self.held.remove(i);
                out.push(Input::Key(key_event(KeyKind::Up, name, modifiers)));
            }
            return;
        }
        if modifiers.ctrl && id.is_c() {
            out.push(Input::Interrupt);
            return;
        }
        let Some(name) = key_name(id, shifted, modifiers.shift, text) else {
            return;
        };
        if self.kitty || matches!(id, KeyId::Win32 { .. }) {
            if !self.held.iter().any(|(held, _)| *held == id) {
                self.held.push((id, name.clone()));
            }
            out.push(Input::Key(key_event(
                KeyKind::Down,
                name.clone(),
                modifiers,
            )));
        }
        out.push(Input::Key(key_event(KeyKind::Press, name, modifiers)));
    }

    /// Push the mouse event of the button bits `cb` at the cells of `csi`.
    /// `released` turns a press into a release, as the `m` of an SGR report
    /// does.
    fn mouse(&mut self, cb: u32, csi: &Csi, released: bool, out: &mut Vec<Input>) {
        let (Some(column), Some(row)) = (cell(csi, 1), cell(csi, 2)) else {
            return;
        };
        let code = (cb & 0b11) | ((cb & 0b1100_0000) >> 4);
        let dragging = cb & 0b10_0000 != 0;
        let button = match code {
            0 => Some(MouseButton::Left),
            1 => Some(MouseButton::Middle),
            2 => Some(MouseButton::Right),
            _ => None,
        };
        let action = match (button, code, dragging) {
            (Some(b), _, false) if released => MouseAction::Up(b),
            (Some(b), _, false) => MouseAction::Down(b),
            (Some(b), _, true) => {
                self.buttons = self.buttons.with(b);
                MouseAction::Move
            }
            // A release that does not name its button.
            (None, 3, false) => MouseAction::Up(MouseButton::Left),
            (None, 3..=5, true) => MouseAction::Move,
            (None, 4, false) => MouseAction::Wheel { dx: 0.0, dy: -1.0 },
            (None, 5, false) => MouseAction::Wheel { dx: 0.0, dy: 1.0 },
            (None, 6, false) => MouseAction::Wheel { dx: -1.0, dy: 0.0 },
            (None, 7, false) => MouseAction::Wheel { dx: 1.0, dy: 0.0 },
            (None, _, _) => return,
        };
        match action {
            MouseAction::Down(b) => self.buttons = self.buttons.with(b),
            MouseAction::Up(b) => self.buttons = self.buttons.without(b),
            MouseAction::Move | MouseAction::Wheel { .. } | MouseAction::Leave => {}
        }
        out.push(Input::Mouse(Mouse {
            action,
            column,
            row,
            modifiers: Modifiers {
                shift: cb & 0b100 != 0,
                alt: cb & 0b1000 != 0,
                ctrl: cb & 0b1_0000 != 0,
                meta: false,
            },
            buttons: self.buttons,
        }));
    }

    /// A key of win32-input-mode. AltGr holds Ctrl and Alt, which the key that types text
    /// with it does not report.
    fn win32(&mut self, csi: &Csi, out: &mut Vec<Input>) {
        let field = |i| csi.int(i, 0).unwrap_or(0);
        let id = KeyId::Win32 {
            vk: field(0),
            scan: field(1),
        };
        let text = field(2);
        let state = field(4);
        let event = if field(3) == 0 { RELEASE } else { PRESS };
        let types = char::from_u32(text).is_some_and(|c| !c.is_control());
        let altgr = types && state & (RIGHT_ALT | LEFT_CTRL) == RIGHT_ALT | LEFT_CTRL;
        let mut bits = 0;
        if state & SHIFT != 0 {
            bits |= 1;
        }
        if !altgr && state & (LEFT_ALT | RIGHT_ALT) != 0 {
            bits |= 2;
        }
        if !altgr && state & (LEFT_CTRL | RIGHT_CTRL) != 0 {
            bits |= 4;
        }
        self.key(id, None, bits + 1, event, &[text], out);
    }

    /// A key with no escape of the protocol. Under the protocol it goes
    /// down and up at once, since no release follows it, and without the
    /// protocol it is a Press alone.
    fn typed(&self, name: &str, modifiers: Modifiers, out: &mut Vec<Input>) {
        if modifiers.ctrl && name == "c" {
            out.push(Input::Interrupt);
            return;
        }
        let kinds: &[KeyKind] = if self.kitty {
            &[KeyKind::Down, KeyKind::Press, KeyKind::Up]
        } else {
            &[KeyKind::Press]
        };
        for &kind in kinds {
            out.push(Input::Key(key_event(kind, name.into(), modifiers)));
        }
    }

    /// A character outside an escape, which is a key without the protocol
    /// or text in a paste, or the key of an `ESC O`. An uppercase letter
    /// holds Shift.
    fn print(&mut self, c: char, mut modifiers: Modifiers, out: &mut Vec<Input>) {
        if std::mem::take(&mut self.ss3) {
            if let Some(id) = u8::try_from(c).ok().and_then(letter_key) {
                self.key(id, None, 1, 1, &[], out);
            }
            return;
        }
        modifiers.shift |= c.is_uppercase();
        match c {
            '\x7f' => self.typed(key::BACKSPACE, modifiers, out),
            c if c.is_control() => {}
            c => self.typed(c.encode_utf8(&mut [0; 4]), modifiers, out),
        }
    }

    /// A control byte, which a terminal without the protocol sends for
    /// Enter, Tab and Ctrl with a letter or a digit.
    fn control(&mut self, byte: u8, out: &mut Vec<Input>) {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        match byte {
            b'\r' => self.typed(key::ENTER, Modifiers::default(), out),
            b'\t' => self.typed(key::TAB, Modifiers::default(), out),
            0x00 => self.typed(" ", ctrl, out),
            0x01..=0x1a => self.typed(
                char::from(byte - 1 + b'a').encode_utf8(&mut [0; 4]),
                ctrl,
                out,
            ),
            0x1c..=0x1f => self.typed(
                char::from(byte - 0x1c + b'4').encode_utf8(&mut [0; 4]),
                ctrl,
                out,
            ),
            _ => {}
        }
    }
}

/// What the parser calls on each input, with the keys and the output.
struct Dispatch<'a> {
    keys: &'a mut Keys,
    out: &'a mut Vec<Input>,
}

impl VTActor for Dispatch<'_> {
    fn print(&mut self, c: char) {
        self.keys.print(c, Modifiers::default(), self.out);
    }

    fn execute_c0_or_c1(&mut self, control: u8) {
        self.keys.control(control, self.out);
    }

    fn esc_dispatch(&mut self, _: &[i64], intermediates: &[u8], _: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'O') => self.keys.ss3 = true,
            // No escape starts with this, so the Escape holds Alt for the
            // key after it.
            ([], byte) => self.keys.print(char::from(byte), ALT, self.out),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &[CsiParam], _: bool, byte: u8) {
        self.keys.csi(&Csi::new(params, byte), self.out);
    }

    fn dcs_hook(&mut self, _: u8, _: &[i64], _: &[u8], _: bool) {}

    fn dcs_put(&mut self, _: u8) {}

    fn dcs_unhook(&mut self) {}

    fn osc_dispatch(&mut self, _: &[&[u8]]) {}

    fn apc_dispatch(&mut self, _: Vec<u8>) {}
}

const ALT: Modifiers = Modifiers {
    alt: true,
    ctrl: false,
    shift: false,
    meta: false,
};

/// Returns `true` if `byte` after an Escape opens a string, `false`
/// otherwise. The strings are OSC, DCS, APC, SOS and PM.
fn opens_string(byte: u8) -> bool {
    matches!(byte, b']' | b'P' | b'_' | b'X' | b'^')
}

/// The event types of the protocol. A repeat, 2, goes down as a press.
const PRESS: u32 = 1;
const RELEASE: u32 = 3;

/// The bits of the modifiers in the state of a key of win32-input-mode.
const RIGHT_ALT: u32 = 0x01;
const LEFT_ALT: u32 = 0x02;
const RIGHT_CTRL: u32 = 0x04;
const LEFT_CTRL: u32 = 0x08;
const SHIFT: u32 = 0x10;

/// Which key an event names, so an Up finds its Down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyId {
    /// The key of `CSI code u`, which is the codepoint of the key without
    /// the modifiers, or a number of the protocol for a key that types no
    /// text.
    Code(u32),
    /// The key of `CSI n ~`.
    Tilde(u32),
    /// The key of `CSI 1 letter`, such as an arrow.
    Letter(u8),
    /// The key of win32-input-mode, by its virtual key and its scan code.
    Win32 { vk: u32, scan: u32 },
}

impl KeyId {
    /// Returns `true` if the key is C, `false` otherwise.
    fn is_c(self) -> bool {
        // The codepoint of c, and the virtual key of C.
        matches!(self, KeyId::Code(0x63) | KeyId::Win32 { vk: 0x43, .. })
    }
}

/// The keys that the protocol sends with a number and no text, and that
/// have a name in [`key`]. The keys of the keypad after Enter take the name
/// of the key they stand for, as in the window.
const CODE_KEYS: &[(u32, &str)] = &[
    (27, key::ESCAPE),
    (13, key::ENTER),
    (9, key::TAB),
    (127, key::BACKSPACE),
    (57414, key::ENTER),
    (57417, key::ARROW_LEFT),
    (57418, key::ARROW_RIGHT),
    (57419, key::ARROW_UP),
    (57420, key::ARROW_DOWN),
    (57421, key::PAGE_UP),
    (57422, key::PAGE_DOWN),
    (57423, key::HOME),
    (57424, key::END),
    (57425, key::INSERT),
    (57426, key::DELETE),
];

const TILDE_KEYS: &[(u32, &str)] = &[
    (1, key::HOME),
    (2, key::INSERT),
    (3, key::DELETE),
    (4, key::END),
    (5, key::PAGE_UP),
    (6, key::PAGE_DOWN),
    (7, key::HOME),
    (8, key::END),
    (11, key::F1),
    (12, key::F2),
    (13, key::F3),
    (14, key::F4),
    (15, key::F5),
    (17, key::F6),
    (18, key::F7),
    (19, key::F8),
    (20, key::F9),
    (21, key::F10),
    (23, key::F11),
    (24, key::F12),
];

const LETTER_KEYS: &[(u8, &str)] = &[
    (b'A', key::ARROW_UP),
    (b'B', key::ARROW_DOWN),
    (b'C', key::ARROW_RIGHT),
    (b'D', key::ARROW_LEFT),
    (b'H', key::HOME),
    (b'F', key::END),
    (b'P', key::F1),
    (b'Q', key::F2),
    (b'R', key::F3),
    (b'S', key::F4),
    (b'Z', key::TAB),
];

/// The virtual keys of Windows that have a name in [`key`].
const VIRTUAL_KEYS: &[(u32, &str)] = &[
    (0x08, key::BACKSPACE),
    (0x09, key::TAB),
    (0x0d, key::ENTER),
    (0x1b, key::ESCAPE),
    (0x21, key::PAGE_UP),
    (0x22, key::PAGE_DOWN),
    (0x23, key::END),
    (0x24, key::HOME),
    (0x25, key::ARROW_LEFT),
    (0x26, key::ARROW_UP),
    (0x27, key::ARROW_RIGHT),
    (0x28, key::ARROW_DOWN),
    (0x2d, key::INSERT),
    (0x2e, key::DELETE),
    (0x70, key::F1),
    (0x71, key::F2),
    (0x72, key::F3),
    (0x73, key::F4),
    (0x74, key::F5),
    (0x75, key::F6),
    (0x76, key::F7),
    (0x77, key::F8),
    (0x78, key::F9),
    (0x79, key::F10),
    (0x7a, key::F11),
    (0x7b, key::F12),
];

/// The first number of the private use area, where the protocol puts the
/// keys that type no text.
const FIRST_PRIVATE: u32 = 57344;

/// The key of a letter escape, or `None` for a letter of no key.
fn letter_key(final_byte: u8) -> Option<KeyId> {
    LETTER_KEYS
        .iter()
        .any(|(b, _)| *b == final_byte)
        .then_some(KeyId::Letter(final_byte))
}

/// The W3C name of a key. The text that the key types comes first, then the
/// name of a key that types no text, then the shifted key, and last the key
/// itself. A modifier key, a key above F12 and a media key have no name, as
/// in the window.
fn key_name(id: KeyId, shifted: Option<char>, shift: bool, text: &[u32]) -> Option<String> {
    let typed: String = text.iter().filter_map(|&c| char::from_u32(c)).collect();
    if !typed.is_empty() && !typed.chars().any(char::is_control) {
        return Some(typed);
    }
    let named = match id {
        KeyId::Code(code) => lookup(CODE_KEYS, code),
        KeyId::Tilde(n) => lookup(TILDE_KEYS, n),
        KeyId::Letter(b) => lookup(LETTER_KEYS, b),
        KeyId::Win32 { vk, .. } => lookup(VIRTUAL_KEYS, vk),
    };
    if let Some(name) = named {
        return Some(name.into());
    }
    let c = match id {
        KeyId::Code(code) if code < FIRST_PRIVATE => match shifted {
            Some(s) if shift => s,
            _ => char::from_u32(code)?,
        },
        // A key with Ctrl types a control character, so the name comes
        // from the virtual key, which is the uppercase letter, the digit
        // or the space.
        KeyId::Win32 { vk, .. } => {
            let c = char::from_u32(vk)
                .filter(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == ' ')?;
            if shift { c } else { c.to_ascii_lowercase() }
        }
        KeyId::Code(_) | KeyId::Tilde(_) | KeyId::Letter(_) => return None,
    };
    (!c.is_control()).then(|| c.to_string())
}

fn lookup<K: PartialEq>(table: &[(K, &'static str)], k: K) -> Option<&'static str> {
    table
        .iter()
        .find(|(key, _)| *key == k)
        .map(|(_, name)| *name)
}

/// The modifiers of the protocol, which sends one more than the bits.
fn modifiers(mods: u32) -> Modifiers {
    let bits = mods.saturating_sub(1);
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
        meta: bits & 8 != 0,
    }
}

fn key_event(kind: KeyKind, key: String, modifiers: Modifiers) -> KeyEvent {
    KeyEvent {
        kind,
        key,
        modifiers,
    }
}

/// A control sequence, with the parameters that vtparse split at `;` and
/// `:`.
struct Csi<'a> {
    /// A private marker, such as the `<` of a mouse report.
    marker: Option<u8>,
    params: &'a [CsiParam],
    final_byte: u8,
}

impl<'a> Csi<'a> {
    fn new(params: &'a [CsiParam], final_byte: u8) -> Self {
        match params {
            [CsiParam::P(b @ b'<'..=b'?'), rest @ ..] => Csi {
                marker: Some(*b),
                params: rest,
                final_byte,
            },
            _ => Csi {
                marker: None,
                params,
                final_byte,
            },
        }
    }

    /// The numbers in the fields of parameter `i`, empty when it is absent.
    fn fields(&self, i: usize) -> Vec<u32> {
        self.param(i)
            .map(|p| p.split(is_colon).filter_map(number).collect())
            .unwrap_or_default()
    }

    /// Field `j` of parameter `i`, or `None` when it is absent or empty.
    fn int(&self, i: usize, j: usize) -> Option<u32> {
        number(self.param(i)?.split(is_colon).nth(j)?)
    }

    fn param(&self, i: usize) -> Option<&'a [CsiParam]> {
        self.params.split(|p| *p == CsiParam::P(b';')).nth(i)
    }
}

fn is_colon(p: &CsiParam) -> bool {
    *p == CsiParam::P(b':')
}

/// The number of a field, or `None` when it is empty or too large.
fn number(field: &[CsiParam]) -> Option<u32> {
    match field {
        [CsiParam::Integer(n)] => u32::try_from(*n).ok(),
        _ => None,
    }
}

/// The cell of field `i` of `csi`, which counts from 1, counted from 0.
fn cell(csi: &Csi, i: usize) -> Option<u16> {
    u16::try_from(csi.int(i, 0)?.checked_sub(1)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kind and the name of each key in `bytes`, fed in one read.
    fn keys(bytes: &[u8]) -> Vec<(KeyKind, String)> {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(bytes, &mut out);
        names(out)
    }

    fn names(out: Vec<Input>) -> Vec<(KeyKind, String)> {
        out.into_iter()
            .filter_map(|input| match input {
                Input::Key(k) => Some((k.kind, k.key)),
                Input::Mouse(_) | Input::Interrupt => None,
            })
            .collect()
    }

    /// The modifiers and the name of each key that a terminal without the
    /// protocol sends in `bytes`, all of them a Press.
    fn legacy_keys(bytes: &[u8]) -> Vec<(Modifiers, String)> {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(bytes, &mut out);
        parser.flush(&mut out);
        out.into_iter()
            .filter_map(|input| match input {
                Input::Key(k) => {
                    assert_eq!(k.kind, KeyKind::Press);
                    Some((k.modifiers, k.key))
                }
                Input::Mouse(_) | Input::Interrupt => None,
            })
            .collect()
    }

    fn plain(name: &str) -> (Modifiers, String) {
        (Modifiers::default(), name.to_owned())
    }

    fn down_press_up(name: &str) -> Vec<(KeyKind, String)> {
        [KeyKind::Down, KeyKind::Press, KeyKind::Up]
            .into_iter()
            .map(|kind| (kind, name.to_owned()))
            .collect()
    }

    #[test]
    fn a_key_goes_down_and_comes_up_with_its_text() {
        assert_eq!(keys(b"\x1b[97;1;97u\x1b[97;1:3u"), down_press_up("a"));
    }

    #[test]
    fn a_shifted_key_comes_up_with_the_name_it_went_down_with() {
        // Shift comes up before the a.
        let bytes = b"\x1b[57441;2u\x1b[97:65;2;65u\x1b[57441;1:3u\x1b[97;1:3u";
        assert_eq!(keys(bytes), down_press_up("A"));
    }

    #[test]
    fn shift_and_a_digit_type_the_shifted_character() {
        assert_eq!(keys(b"\x1b[49:33;2;33u\x1b[49:33;2:3u"), down_press_up("!"));
    }

    #[test]
    fn a_key_without_text_takes_the_shifted_key() {
        // Ctrl+Shift+a types no text.
        assert_eq!(keys(b"\x1b[97:65;6u\x1b[97:65;6:3u"), down_press_up("A"));
    }

    #[test]
    fn a_dead_key_and_a_letter_type_the_composed_text() {
        // The dead key types nothing, and the e types é.
        let bytes = b"\x1b[39;1u\x1b[39;1:3u\x1b[101;1;233u\x1b[101;1:3u";
        let mut expected = down_press_up("'");
        expected.extend(down_press_up("\u{e9}"));
        // The dead key has no text, so its name is the key itself.
        assert_eq!(keys(bytes), expected);
    }

    #[test]
    fn enter_tab_and_backspace_come_up() {
        let bytes = b"\x1b[13u\x1b[13;1:3u\x1b[9u\x1b[9;1:3u\x1b[127u\x1b[127;1:3u";
        let mut expected = down_press_up(key::ENTER);
        expected.extend(down_press_up(key::TAB));
        expected.extend(down_press_up(key::BACKSPACE));
        assert_eq!(keys(bytes), expected);
    }

    #[test]
    fn arrows_and_function_keys_take_their_names() {
        let bytes = b"\x1b[A\x1b[1;1:3A\x1b[15~\x1b[15;1:3~";
        let mut expected = down_press_up(key::ARROW_UP);
        expected.extend(down_press_up(key::F5));
        assert_eq!(keys(bytes), expected);
    }

    #[test]
    fn a_repeat_sends_down_and_press_again() {
        let a = |kind| (kind, "a".to_owned());
        assert_eq!(
            keys(b"\x1b[97;1;97u\x1b[97;1:2;97u\x1b[97;1:3u"),
            [
                a(KeyKind::Down),
                a(KeyKind::Press),
                a(KeyKind::Down),
                a(KeyKind::Press),
                a(KeyKind::Up)
            ]
        );
    }

    #[test]
    fn a_modifier_key_and_a_release_without_a_press_send_nothing() {
        assert_eq!(keys(b"\x1b[57441;2u\x1b[57441;1:3u\x1b[98;1:3u"), []);
    }

    #[test]
    fn an_escape_cut_in_two_reads_parses_whole() {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(b"\x1b[97;1", &mut out);
        assert!(out.is_empty());
        parser.feed(b";97u\x1b[97;1:3u", &mut out);
        assert_eq!(names(out), down_press_up("a"));
    }

    #[test]
    fn a_lone_escape_goes_out_at_the_flush() {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(b"\x1b", &mut out);
        assert!(out.is_empty());
        parser.flush(&mut out);
        assert_eq!(names(out), down_press_up(key::ESCAPE));
    }

    #[test]
    fn text_outside_an_escape_goes_down_and_up_at_once() {
        assert_eq!(keys("é".as_bytes()), down_press_up("\u{e9}"));
    }

    #[test]
    fn ctrl_c_interrupts() {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(b"\x1b[99;5u", &mut out);
        assert_eq!(out, [Input::Interrupt]);
    }

    #[test]
    fn a_focus_loss_brings_up_every_key_that_is_down() {
        assert_eq!(keys(b"\x1b[97;1;97u\x1b[O\x1b[97;1:3u"), down_press_up("a"));
    }

    /// The mouse events of `out`.
    fn mice(out: &[Input]) -> Vec<&Mouse> {
        out.iter()
            .filter_map(|input| match input {
                Input::Mouse(m) => Some(m),
                Input::Key(_) | Input::Interrupt => None,
            })
            .collect()
    }

    #[test]
    fn an_sgr_report_is_a_mouse_event() {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(b"\x1b[<0;3;2M\x1b[<0;3;2m", &mut out);
        let actions: Vec<MouseAction> = mice(&out).iter().map(|m| m.action).collect();
        assert_eq!(
            actions,
            [
                MouseAction::Down(MouseButton::Left),
                MouseAction::Up(MouseButton::Left)
            ]
        );
        assert!(matches!(out.first(), Some(Input::Mouse(m)) if (m.column, m.row) == (2, 1)));
    }

    #[test]
    fn a_drag_holds_its_button_until_the_up() {
        let mut parser = VtInput::new(true);
        let mut out = Vec::new();
        parser.feed(
            b"\x1b[<4;1;1M\x1b[<36;1;1M\x1b[<0;1;1m\x1b[<64;1;1M",
            &mut out,
        );
        let left = MouseButtons::default().with(MouseButton::Left);
        let events: Vec<(MouseAction, MouseButtons)> =
            mice(&out).iter().map(|m| (m.action, m.buttons)).collect();
        assert_eq!(
            events,
            [
                (MouseAction::Down(MouseButton::Left), left),
                (MouseAction::Move, left),
                (MouseAction::Up(MouseButton::Left), MouseButtons::default()),
                (
                    MouseAction::Wheel { dx: 0.0, dy: -1.0 },
                    MouseButtons::default()
                ),
            ]
        );
        assert!(mice(&out).first().is_some_and(|m| m.modifiers.shift));
    }

    #[test]
    fn the_text_of_a_key_takes_a_character_past_16_bits() {
        let bytes = b"\x1b[97;1;128512u\x1b[97;1:3u";
        assert_eq!(keys(bytes), down_press_up("\u{1f600}"));
    }

    #[test]
    fn an_ss3_arrow_goes_down() {
        assert_eq!(
            keys(b"\x1bOA"),
            [
                (KeyKind::Down, key::ARROW_UP.to_owned()),
                (KeyKind::Press, key::ARROW_UP.to_owned())
            ]
        );
    }

    #[test]
    fn without_the_protocol_text_and_escapes_are_presses() {
        assert_eq!(
            legacy_keys("aé\r\t\x7f\x1b[A\x1bOR\x1b[1~\x1b[5;5~".as_bytes()),
            [
                plain("a"),
                plain("\u{e9}"),
                plain(key::ENTER),
                plain(key::TAB),
                plain(key::BACKSPACE),
                plain(key::ARROW_UP),
                plain(key::F3),
                plain(key::HOME),
                (
                    Modifiers {
                        ctrl: true,
                        ..Modifiers::default()
                    },
                    key::PAGE_UP.to_owned()
                ),
            ]
        );
    }

    #[test]
    fn without_the_protocol_the_modifiers_come_from_the_bytes() {
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            legacy_keys(b"A\x01\x1bx\x1b[Z"),
            [
                (shift, "A".to_owned()),
                (ctrl, "a".to_owned()),
                (alt, "x".to_owned()),
                (shift, key::TAB.to_owned()),
            ]
        );
    }

    #[test]
    fn without_the_protocol_a_lone_escape_goes_out_at_the_flush() {
        assert_eq!(legacy_keys(b"\x1b"), [plain(key::ESCAPE)]);
    }

    #[test]
    fn without_the_protocol_ctrl_c_interrupts() {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(b"\x03", &mut out);
        assert_eq!(out, [Input::Interrupt]);
    }

    #[test]
    fn alt_with_a_key_that_opens_a_string_leaves_the_keys_after_it() {
        let alt = |shift| Modifiers {
            alt: true,
            shift,
            ..Modifiers::default()
        };
        let none = Modifiers::default();
        assert_eq!(
            legacy_keys(b"\x1b]a\x1bPb\x1b_c"),
            [
                (alt(false), "]".to_string()),
                (none, "a".to_string()),
                (alt(true), "P".to_string()),
                (none, "b".to_string()),
                (alt(false), "_".to_string()),
                (none, "c".to_string()),
            ]
        );
    }

    #[test]
    fn ctrl_c_interrupts_after_alt_with_a_key_that_opens_a_string() {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(b"\x1b", &mut out);
        parser.feed(b"]\x03", &mut out);
        assert_eq!(out.last(), Some(&Input::Interrupt));
        assert_eq!(names(out).len(), 1);
    }

    #[test]
    fn a_cursor_position_report_is_not_f3() {
        assert_eq!(legacy_keys(b"\x1b[12;1R"), []);
    }

    #[test]
    fn an_rxvt_report_is_a_mouse_event() {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(b"\x1b[32;3;2M\x1b[35;3;2M", &mut out);
        let actions: Vec<MouseAction> = mice(&out).iter().map(|m| m.action).collect();
        assert_eq!(
            actions,
            [
                MouseAction::Down(MouseButton::Left),
                MouseAction::Up(MouseButton::Left)
            ]
        );
    }

    /// The kind, the modifiers and the name of each key of win32-input-mode
    /// in `bytes`.
    fn win32_keys(bytes: &[u8]) -> Vec<(KeyKind, Modifiers, String)> {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(bytes, &mut out);
        out.into_iter()
            .filter_map(|input| match input {
                Input::Key(k) => Some((k.kind, k.modifiers, k.key)),
                Input::Mouse(_) | Input::Interrupt => None,
            })
            .collect()
    }

    fn with(
        modifiers: Modifiers,
        keys: Vec<(KeyKind, String)>,
    ) -> Vec<(KeyKind, Modifiers, String)> {
        keys.into_iter()
            .map(|(kind, name)| (kind, modifiers, name))
            .collect()
    }

    #[test]
    fn a_win32_key_goes_down_and_comes_up_with_its_text() {
        assert_eq!(
            win32_keys(b"\x1b[65;30;97;1;0;1_\x1b[65;30;97;0;0;1_"),
            with(Modifiers::default(), down_press_up("a"))
        );
    }

    #[test]
    fn a_shifted_win32_key_comes_up_with_the_name_it_went_down_with() {
        // Shift comes up before the a, which then types a.
        let bytes =
            b"\x1b[16;42;0;1;16;1_\x1b[65;30;65;1;16;1_\x1b[16;42;0;0;0;1_\x1b[65;30;97;0;0;1_";
        let shift = Modifiers {
            shift: true,
            ..Modifiers::default()
        };
        let mut expected = with(shift, down_press_up("A"));
        if let Some(up) = expected.last_mut() {
            up.1 = Modifiers::default();
        }
        assert_eq!(win32_keys(bytes), expected);
    }

    #[test]
    fn a_win32_key_with_ctrl_takes_the_name_of_its_virtual_key() {
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(
            win32_keys(b"\x1b[65;30;1;1;8;1_\x1b[65;30;1;0;8;1_"),
            with(ctrl, down_press_up("a"))
        );
    }

    #[test]
    fn altgr_types_its_text_without_ctrl_and_alt() {
        assert_eq!(
            win32_keys(b"\x1b[81;16;64;1;9;1_\x1b[81;16;64;0;9;1_"),
            with(Modifiers::default(), down_press_up("@"))
        );
    }

    #[test]
    fn a_win32_arrow_and_a_modifier_key_take_their_names() {
        // Ctrl goes down and up around the arrow, and has no name.
        let bytes =
            b"\x1b[17;29;0;1;8;1_\x1b[37;75;0;1;264;1_\x1b[37;75;0;0;264;1_\x1b[17;29;0;0;0;1_";
        let ctrl = Modifiers {
            ctrl: true,
            ..Modifiers::default()
        };
        assert_eq!(
            win32_keys(bytes),
            with(ctrl, down_press_up(key::ARROW_LEFT))
        );
    }

    #[test]
    fn a_second_win32_down_sends_down_and_press_again() {
        let bytes = b"\x1b[65;30;97;1;0;1_\x1b[65;30;97;1;0;1_\x1b[65;30;97;0;0;1_";
        let a = |kind| (kind, Modifiers::default(), "a".to_owned());
        assert_eq!(
            win32_keys(bytes),
            [
                a(KeyKind::Down),
                a(KeyKind::Press),
                a(KeyKind::Down),
                a(KeyKind::Press),
                a(KeyKind::Up)
            ]
        );
    }

    #[test]
    fn win32_ctrl_c_interrupts() {
        let mut parser = VtInput::new(false);
        let mut out = Vec::new();
        parser.feed(b"\x1b[67;46;3;1;8;1_", &mut out);
        assert_eq!(out, [Input::Interrupt]);
    }

    #[test]
    fn a_focus_loss_brings_up_every_win32_key_that_is_down() {
        assert_eq!(
            win32_keys(b"\x1b[65;30;97;1;0;1_\x1b[O\x1b[65;30;97;0;0;1_"),
            with(Modifiers::default(), down_press_up("a"))
        );
    }

    #[test]
    fn a_reply_to_a_query_sends_nothing() {
        assert_eq!(keys(b"\x1b[?31u\x1b[12;1R"), []);
    }
}
