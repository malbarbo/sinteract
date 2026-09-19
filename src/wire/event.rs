//! `InputEvent` to and from the Cap'n Proto struct.
//!
//! [`InputEvent::Close`] is not an event on the wire. The view sends it as
//! the close of `ViewMessage`, so [`super::to_engine`] writes and reads it.

use crate::event::{InputEvent, KeyEvent, KeyKind, Modifiers};
use crate::event_capnp::{
    KeyKind as WKeyKind, input_event, key_event as wire_key_event, modifiers as wire_modifiers,
};

use super::{Error, ValueError, skip_unusable};

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

/// Write the key into `b`. [`InputEvent`] has no writer of its own,
/// because its close is not an event on the wire but a message of the
/// view.
pub(super) fn write_key_event(mut b: wire_key_event::Builder<'_>, k: &KeyEvent) {
    b.set_kind(key_kind_to_wire(k.kind));
    b.set_key(&*k.key);
    write_modifiers(b.reborrow().init_modifiers(), k.modifiers);
    b.set_repeat(k.repeat);
}

fn write_modifiers(mut b: wire_modifiers::Builder<'_>, m: Modifiers) {
    b.set_alt(m.alt);
    b.set_ctrl(m.ctrl);
    b.set_shift(m.shift);
    b.set_meta(m.meta);
}

/// `None` for an event of an arm from a newer schema, or for one that holds
/// a value from a newer schema, such as a key kind, which the reader skips.
pub(super) fn read_input_event(r: input_event::Reader<'_>) -> Result<Option<InputEvent>, Error> {
    let Ok(which) = r.which() else {
        return Ok(None);
    };
    skip_unusable(read_known_input_event(which))
}

fn read_known_input_event(which: input_event::WhichReader<'_>) -> Result<InputEvent, ValueError> {
    use input_event::Which;
    Ok(match which {
        Which::Key(k) => {
            let k = k?;
            InputEvent::Key(KeyEvent {
                kind: key_kind_from_wire(k.get_kind()?),
                key: k.get_key()?.to_str()?.to_owned(),
                modifiers: read_modifiers(k.get_modifiers()?),
                repeat: k.get_repeat(),
            })
        }
        Which::Tick(_) => InputEvent::Vsync,
    })
}

fn read_modifiers(r: wire_modifiers::Reader<'_>) -> Modifiers {
    Modifiers {
        alt: r.get_alt(),
        ctrl: r.get_ctrl(),
        shift: r.get_shift(),
        meta: r.get_meta(),
    }
}
