//! `InputEvent` to and from the Cap'n Proto struct.

use crate::event::{InputEvent, KeyEvent, KeyKind, Modifiers};
use crate::event_capnp::{KeyKind as WKeyKind, input_event, key_event as wire_key_event};

use super::{Error, ReadError, skip_unknown};

fn key_kind_to_wire(k: KeyKind) -> WKeyKind {
    match k {
        KeyKind::Press => WKeyKind::Press,
        KeyKind::Down => WKeyKind::Down,
        KeyKind::Up => WKeyKind::Up,
    }
}

fn key_kind_from_wire(k: WKeyKind) -> KeyKind {
    match k {
        WKeyKind::Press => KeyKind::Press,
        WKeyKind::Down => KeyKind::Down,
        WKeyKind::Up => KeyKind::Up,
    }
}

pub(super) fn write_input_event(mut b: input_event::Builder<'_>, ev: &InputEvent) {
    match ev {
        InputEvent::Key(k) => {
            let mut kb: wire_key_event::Builder = b.init_key();
            kb.set_kind(key_kind_to_wire(k.kind));
            kb.set_key(&*k.key);
            kb.set_alt(k.modifiers.alt);
            kb.set_ctrl(k.modifiers.ctrl);
            kb.set_shift(k.modifiers.shift);
            kb.set_meta(k.modifiers.meta);
            kb.set_repeat(k.repeat);
        }
        InputEvent::Vsync => b.set_tick(()),
        InputEvent::Close => b.set_close(()),
    }
}

/// `None` for an event of an arm from a newer schema, or for one that holds
/// a value from a newer schema, such as a key kind, which the reader skips.
pub(super) fn read_input_event(r: input_event::Reader<'_>) -> Result<Option<InputEvent>, Error> {
    let Ok(which) = r.which() else {
        return Ok(None);
    };
    skip_unknown(read_known_input_event(which))
}

fn read_known_input_event(which: input_event::WhichReader<'_>) -> Result<InputEvent, ReadError> {
    use input_event::Which;
    Ok(match which {
        Which::Key(k) => {
            let k = k?;
            InputEvent::Key(KeyEvent {
                kind: key_kind_from_wire(k.get_kind()?),
                key: k.get_key()?.to_str()?.to_owned(),
                modifiers: Modifiers {
                    alt: k.get_alt(),
                    ctrl: k.get_ctrl(),
                    shift: k.get_shift(),
                    meta: k.get_meta(),
                },
                repeat: k.get_repeat(),
            })
        }
        Which::Tick(()) => InputEvent::Vsync,
        Which::Close(()) => InputEvent::Close,
    })
}
