//! `InputEvent` to and from the Cap'n Proto struct.

use crate::event::{InputEvent, KeyEvent, KeyKind};
use crate::event_capnp::{KeyKind as WKeyKind, input_event, key_event as wire_key_event};

use super::Error;

/// The bit of `modifiers` on the wire that carries `repeat`.
const REPEAT_BIT: u8 = 1 << 4;

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
            kb.set_modifiers(k.modifiers | if k.repeat { REPEAT_BIT } else { 0 });
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
                modifiers: bits & !REPEAT_BIT,
                repeat: bits & REPEAT_BIT != 0,
            }))
        }
        Which::Tick(()) => Ok(InputEvent::Vsync),
        Which::Close(()) => Ok(InputEvent::Close),
    }
}
