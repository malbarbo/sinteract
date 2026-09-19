//! `InputEvent` to and from the Cap'n Proto struct.

use crate::event::{
    InputEvent, KeyEvent, KeyKind, Modifiers, MouseAction, MouseButton, MouseButtons, MouseEvent,
};
use crate::event_capnp::{
    KeyKind as WKeyKind, MouseButton as WMouseButton, input_event, key_event as wire_key_event,
    modifiers as wire_modifiers, mouse_event as wire_mouse_event,
    resize_event as wire_resize_event,
};

use super::scene::finite;
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

/// Write the key into `b`.
pub(super) fn write_key_event(mut b: wire_key_event::Builder<'_>, k: &KeyEvent) {
    b.set_kind(key_kind_to_wire(k.kind));
    b.set_key(&*k.key);
    write_modifiers(b.reborrow().init_modifiers(), k.modifiers);
    b.set_repeat(k.repeat);
}

pub(super) fn write_mouse_event(mut b: wire_mouse_event::Builder<'_>, m: &MouseEvent) {
    b.set_x(m.x);
    b.set_y(m.y);
    write_modifiers(b.reborrow().init_modifiers(), m.modifiers);
    b.set_buttons(m.buttons.bits());
    match m.action {
        MouseAction::Move => b.set_move(()),
        MouseAction::Down(button) => b.set_down(mouse_button_to_wire(button)),
        MouseAction::Up(button) => b.set_up(mouse_button_to_wire(button)),
        MouseAction::Wheel { dx, dy } => {
            let mut wheel = b.init_wheel();
            wheel.set_dx(dx);
            wheel.set_dy(dy);
        }
        MouseAction::Leave => b.set_leave(()),
    }
}

pub(super) fn write_resize_event(mut b: wire_resize_event::Builder<'_>, width: f32, height: f32) {
    b.set_width(width);
    b.set_height(height);
}

fn mouse_button_to_wire(b: MouseButton) -> WMouseButton {
    match b {
        MouseButton::Left => WMouseButton::Left,
        MouseButton::Middle => WMouseButton::Middle,
        MouseButton::Right => WMouseButton::Right,
        MouseButton::Back => WMouseButton::Back,
        MouseButton::Forward => WMouseButton::Forward,
    }
}

fn write_modifiers(mut b: wire_modifiers::Builder<'_>, m: Modifiers) {
    b.set_alt(m.alt);
    b.set_ctrl(m.ctrl);
    b.set_shift(m.shift);
    b.set_meta(m.meta);
}

/// `None` for an event of an arm from a newer schema, for one that holds a
/// value from a newer schema, such as a key kind, and for one that holds a
/// float that is not finite. The reader skips all three.
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
        Which::Mouse(m) => InputEvent::Mouse(read_mouse_event(m?)?),
        Which::Resize(r) => {
            let r = r?;
            let (width, height) = (r.get_width(), r.get_height());
            finite(width.is_finite() && height.is_finite())?;
            InputEvent::Resize { width, height }
        }
    })
}

fn read_mouse_event(r: wire_mouse_event::Reader<'_>) -> Result<MouseEvent, ValueError> {
    use wire_mouse_event::Which;
    let action = match r.which()? {
        Which::Move(()) => MouseAction::Move,
        Which::Down(b) => MouseAction::Down(mouse_button_from_wire(b?)),
        Which::Up(b) => MouseAction::Up(mouse_button_from_wire(b?)),
        Which::Wheel(w) => {
            let (dx, dy) = (w.get_dx(), w.get_dy());
            finite(dx.is_finite() && dy.is_finite())?;
            MouseAction::Wheel { dx, dy }
        }
        Which::Leave(()) => MouseAction::Leave,
    };
    let (x, y) = (r.get_x(), r.get_y());
    finite(x.is_finite() && y.is_finite())?;
    Ok(MouseEvent {
        action,
        x,
        y,
        modifiers: read_modifiers(r.get_modifiers()?),
        buttons: MouseButtons::from_bits(r.get_buttons()),
    })
}

fn mouse_button_from_wire(b: WMouseButton) -> MouseButton {
    match b {
        WMouseButton::Left => MouseButton::Left,
        WMouseButton::Middle => MouseButton::Middle,
        WMouseButton::Right => MouseButton::Right,
        WMouseButton::Back => MouseButton::Back,
        WMouseButton::Forward => MouseButton::Forward,
    }
}

fn read_modifiers(r: wire_modifiers::Reader<'_>) -> Modifiers {
    Modifiers {
        alt: r.get_alt(),
        ctrl: r.get_ctrl(),
        shift: r.get_shift(),
        meta: r.get_meta(),
    }
}
