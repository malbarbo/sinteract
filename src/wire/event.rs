//! `InputEvent` to and from the Cap'n Proto struct.

use crate::event::{InputEvent, KeyEvent, KeyKind, Modifiers};
use crate::event_capnp::{KeyKind as WKeyKind, input_event, key_event as wire_key_event};

use super::Error;

// The bits of `modifiers` on the wire.
const ALT_BIT: u8 = 1 << 0;
const CTRL_BIT: u8 = 1 << 1;
const SHIFT_BIT: u8 = 1 << 2;
const META_BIT: u8 = 1 << 3;
const REPEAT_BIT: u8 = 1 << 4;

fn modifier_bits(k: &KeyEvent) -> u8 {
    let m = k.modifiers;
    [
        (m.alt, ALT_BIT),
        (m.ctrl, CTRL_BIT),
        (m.shift, SHIFT_BIT),
        (m.meta, META_BIT),
        (k.repeat, REPEAT_BIT),
    ]
    .into_iter()
    .filter(|&(on, _)| on)
    .fold(0, |bits, (_, bit)| bits | bit)
}

fn modifiers_from_bits(bits: u8) -> Modifiers {
    Modifiers {
        alt: bits & ALT_BIT != 0,
        ctrl: bits & CTRL_BIT != 0,
        shift: bits & SHIFT_BIT != 0,
        meta: bits & META_BIT != 0,
    }
}

fn key_kind_to_wire(k: KeyKind) -> WKeyKind {
    match k {
        KeyKind::Press => WKeyKind::Press,
        KeyKind::Down => WKeyKind::Down,
        KeyKind::Up => WKeyKind::Up,
    }
}

fn key_kind_from_wire(k: WKeyKind) -> KeyKind {
    match k {
        WKeyKind::Down => KeyKind::Down,
        WKeyKind::Up => KeyKind::Up,
        _ => KeyKind::Press,
    }
}

pub(super) fn write_input_event(mut b: input_event::Builder<'_>, ev: &InputEvent) {
    match ev {
        InputEvent::Key(k) => {
            let mut kb: wire_key_event::Builder = b.init_key();
            kb.set_kind(key_kind_to_wire(k.kind));
            kb.set_key(&*k.key);
            kb.set_modifiers(modifier_bits(k));
        }
        InputEvent::Vsync => b.set_tick(()),
        InputEvent::Close => b.set_close(()),
    }
}

pub(super) fn read_input_event(r: input_event::Reader<'_>) -> Result<InputEvent, Error> {
    use input_event::Which;
    match r.which()? {
        Which::Key(k) => {
            let k = k?;
            let bits = k.get_modifiers();
            Ok(InputEvent::Key(KeyEvent {
                kind: key_kind_from_wire(k.get_kind()?),
                key: k.get_key()?.to_str()?.to_owned(),
                modifiers: modifiers_from_bits(bits),
                repeat: bits & REPEAT_BIT != 0,
            }))
        }
        Which::Tick(()) => Ok(InputEvent::Vsync),
        Which::Close(()) => Ok(InputEvent::Close),
    }
}
