//! The queue behind every [`super::Display`]. A display owns an [`Inbox`]
//! and hands out [`Sender`]s. Its input threads, the engine and any other
//! thread push through a `Sender`, and `wait_event` pops from the `Inbox`,
//! in the order of arrival.

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::event::{Event, InputEvent, Interrupt};

/// Pushes into the queue of a display from any thread, and wakes a
/// `wait_event` that blocks on it. Get one from
/// [`super::Display::sender`].
///
/// The queue has no bound. Once the display closes or delivers
/// [`Interrupt::Close`], every send returns [`Closed`]. A message sent after
/// a Close that has not gone out yet is lost.
pub struct Sender<E = Event> {
    tx: mpsc::Sender<Msg<E>>,
    wake: Option<Waker>,
}

/// Wakes a display that blocks somewhere other than the channel, as the
/// window does in its event loop.
pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

/// The error of a [`Sender`] whose display was closed or dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

/// An event that the queue holds. The queue makes the Vsync with its
/// clock and delivers it with [`Queued::vsync`]. It drops what a newer
/// event makes worthless.
pub(crate) trait Queued {
    /// The Vsync that the queue delivers.
    fn vsync() -> Self;
    /// Returns `true` if the event is a Vsync, `false` otherwise.
    fn is_vsync(&self) -> bool;
    /// Returns `true` if `self` makes `old` worthless, `false` otherwise.
    fn supersedes(&self, old: &Self) -> bool;
    /// Returns `true` if the order between `self` and `other` does not
    /// matter, `false` otherwise. An event that replaces an older one moves
    /// ahead of the independent events between them.
    fn independent(&self, other: &Self) -> bool;
}

impl<E> Sender<E> {
    /// Queue [`Interrupt::Close`], in order with the events. A Ctrl-C handler
    /// of the engine calls it to end a `wait_event` that blocks.
    pub fn send_close(&self) -> Result<(), Closed> {
        self.send(Entry::Close)
    }

    /// Queue [`Interrupt::Wake`], in order with the events. A thread that
    /// hands its data over a channel of its own calls it, so the loop looks
    /// at that channel.
    pub fn wake(&self) -> Result<(), Closed> {
        self.send(Entry::Wake)
    }

    pub(crate) fn send_event(&self, ev: E) -> Result<(), Closed> {
        self.send(Entry::Input(ev))
    }

    /// Queue [`Interrupt::Read`], in order with the events. A reader that
    /// sends a `ReadError::Broken` sends a close right after.
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub(crate) fn send_read_error(&self, e: crate::wire::ReadError) -> Result<(), Closed> {
        self.send(Entry::Read(e))
    }

    /// Ask the display to draw the last scene again, as after a resize.
    /// The request never reaches the engine. Only the terminal and the
    /// window redraw.
    pub(crate) fn request_redraw(&self) -> Result<(), Closed> {
        self.put(Msg::Redraw)
    }

    fn send(&self, entry: Entry<E>) -> Result<(), Closed> {
        self.put(Msg::Item(Item {
            at: Instant::now(),
            entry,
        }))
    }

    fn put(&self, msg: Msg<E>) -> Result<(), Closed> {
        self.tx.send(msg).map_err(|_| Closed)?;
        if let Some(wake) = &self.wake {
            wake();
        }
        Ok(())
    }
}

impl Sender<Event> {
    pub(crate) fn send_input(&self, ev: InputEvent) -> Result<(), Closed> {
        self.send_event(Event::Input(ev))
    }
}

// A derive would ask for `E: Clone`, and the events never get cloned.
impl<E> Clone for Sender<E> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the display is closed")
    }
}

impl std::error::Error for Closed {}

/// The receiving end. It holds a sender of its own for [`Inbox::sender`].
pub(crate) struct Inbox<E = Event> {
    tx: mpsc::Sender<Msg<E>>,
    wake: Option<Waker>,
    rx: mpsc::Receiver<Msg<E>>,
    /// What left the channel and did not go out yet, oldest first.
    pending: VecDeque<Item<E>>,
    clock: Clock,
    /// A redraw was requested and did not go out yet. Many requests make
    /// one redraw.
    redraw: bool,
    /// Set by the first Close out or by [`Inbox::close`]. Every wait
    /// returns Close from then on.
    closed: bool,
}

enum Msg<E> {
    Item(Item<E>),
    Redraw,
}

struct Item<E> {
    at: Instant,
    entry: Entry<E>,
}

/// What waits in the queue for `wait_event`.
enum Entry<E> {
    Input(E),
    Wake,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    Read(crate::wire::ReadError),
    Close,
}

/// What [`Inbox::wait_with`] hands to the display.
pub(crate) enum Next<E = Event> {
    Ready(Result<E, Interrupt>),
    /// Draw the last scene again, and wait again.
    Redraw,
}

impl<E: Queued> Inbox<E> {
    /// A queue that makes a Vsync every `vsync_period`.
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub(crate) fn new(vsync_period: Duration) -> Self {
        Self::with_waker(vsync_period, None)
    }

    /// A queue whose [`Sender`]s also call `wake` after they push.
    pub(crate) fn with_waker(vsync_period: Duration, wake: Option<Waker>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            wake,
            rx,
            pending: VecDeque::new(),
            clock: Clock {
                period: vsync_period,
                due: Instant::now(),
            },
            redraw: false,
            closed: false,
        }
    }

    pub(crate) fn sender(&self) -> Sender<E> {
        Sender {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }

    /// A [`Sender`] without the waker, for the callbacks of the loop that
    /// the waker wakes. A wake from inside the loop only adds a turn with
    /// nothing to do.
    #[cfg(feature = "window")]
    pub(crate) fn sender_in_loop(&self) -> Sender<E> {
        Sender {
            tx: self.tx.clone(),
            wake: None,
        }
    }

    /// Queue `ev` ahead of every event, the first Vsync included, for what
    /// a display tells the engine as it opens.
    pub(crate) fn send_first(&mut self, ev: E) {
        let now = Instant::now();
        // A Vsync goes out first only when it is strictly older.
        let at = self.clock.due.min(now);
        self.pending.push_front(Item {
            at,
            entry: Entry::Input(ev),
        });
    }

    /// Deliver Close from now on and drop what is queued. A new receiver
    /// replaces the channel, so every [`Sender`] fails from now on.
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.rx = mpsc::channel().1;
        self.pending.clear();
        self.redraw = false;
    }

    /// [`Inbox::wait_with`] on the channel, with no redraw.
    #[cfg(test)]
    fn wait(&mut self, deadline: Option<Instant>) -> Result<E, Interrupt> {
        loop {
            if let Next::Ready(ready) = self.wait_with(deadline, Self::receive) {
                return ready;
            }
        }
    }

    /// The oldest event, or [`Interrupt::Timeout`] once `deadline` passes. A
    /// `deadline` of `None` waits for as long as it takes. A redraw goes
    /// out when no event is ready, so an engine that presents anyway skips it.
    ///
    /// `block` waits for at most its timeout, and a [`Sender`] wakes it. A
    /// display that blocks on the channel passes [`Inbox::receive`].
    ///
    /// A Vsync counts as arrived when it falls due, so input
    /// that arrived before it goes out first.
    pub(crate) fn wait_with(
        &mut self,
        deadline: Option<Instant>,
        mut block: impl FnMut(&mut Self, Duration),
    ) -> Next<E> {
        loop {
            if let Some(ready) = self.poll() {
                return Next::Ready(ready);
            }
            if mem::take(&mut self.redraw) {
                return Next::Redraw;
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Next::Ready(Err(Interrupt::Timeout));
            }
            let timeout = self.wake_at(deadline) - now;
            block(self, timeout);
        }
    }

    /// Take the next message of the channel, or wait until `timeout`
    /// passes.
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub(crate) fn receive(&mut self, timeout: Duration) {
        match self.rx.recv_timeout(timeout) {
            Ok(msg) => self.take(msg),
            Err(RecvTimeoutError::Timeout) => {}
            // Only the receiver of a closed inbox disconnects.
            Err(RecvTimeoutError::Disconnected) => self.closed = true,
        }
    }

    /// The oldest entry that is ready, without blocking.
    fn poll(&mut self) -> Option<Result<E, Interrupt>> {
        if self.closed {
            return Some(Err(Interrupt::Close));
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.take(msg);
        }
        self.pop(Instant::now())
    }

    /// When a wait until `deadline` has to stop and look again, the earlier
    /// of `deadline` and the next Vsync.
    fn wake_at(&self, deadline: Option<Instant>) -> Instant {
        deadline.map_or(self.clock.due, |d| d.min(self.clock.due))
    }

    fn take(&mut self, msg: Msg<E>) {
        match msg {
            Msg::Item(item) => self.push(item),
            Msg::Redraw => self.redraw = true,
        }
    }

    /// Queue `item`. The clock makes every Vsync, so `push` drops a Vsync
    /// from the channel.
    ///
    /// A move of the mouse or a resize replaces one of its kind at the back
    /// of the queue, past the independent events, since only the latest one
    /// counts. A mouse at 1000 Hz would flood an engine that runs at 60 Hz.
    fn push(&mut self, item: Item<E>) {
        if matches!(&item.entry, Entry::Input(ev) if ev.is_vsync()) {
            return;
        }
        if let Entry::Input(new) = &item.entry
            && let Some(i) = self.superseded(new)
        {
            // The older time keeps its place before a Vsync.
            self.pending
                .get_mut(i)
                .expect("an index of the queue")
                .entry = item.entry;
            return;
        }
        self.pending.push_back(item);
    }

    /// The index of the event that `new` replaces, looking from the back
    /// past the events independent of `new`.
    fn superseded(&self, new: &E) -> Option<usize> {
        for (i, item) in self.pending.iter().enumerate().rev() {
            let Entry::Input(old) = &item.entry else {
                return None;
            };
            if new.supersedes(old) {
                return Some(i);
            }
            if !new.independent(old) {
                return None;
            }
        }
        None
    }

    /// The older of the front of `pending` and a Vsync due by `now`, or
    /// `None` when neither exists.
    fn pop(&mut self, now: Instant) -> Option<Result<E, Interrupt>> {
        let front = self.pending.front().map(|item| item.at);
        let due = self.clock.due;
        if due <= now && front.is_none_or(|f| due < f) {
            self.clock.deliver(now);
            return Some(Ok(E::vsync()));
        }
        Some(match self.pending.pop_front()?.entry {
            Entry::Input(ev) => Ok(ev),
            Entry::Wake => Err(Interrupt::Wake),
            Entry::Read(e) => Err(Interrupt::Read(e)),
            Entry::Close => {
                self.close();
                Err(Interrupt::Close)
            }
        })
    }
}

impl Queued for Event {
    fn vsync() -> Self {
        Event::Input(InputEvent::Vsync)
    }

    fn is_vsync(&self) -> bool {
        matches!(self, Event::Input(InputEvent::Vsync))
    }

    fn supersedes(&self, old: &Self) -> bool {
        let (Event::Input(new), Event::Input(old)) = (self, old);
        new.supersedes(old)
    }

    /// The events of one user keep their order.
    fn independent(&self, _: &Self) -> bool {
        false
    }
}

/// A software clock that makes the Vsync.
struct Clock {
    period: Duration,
    /// When the next Vsync falls due. The first one is due at once.
    due: Instant,
}

impl Clock {
    /// The Vsync went out at `now`. The clock keeps its beat, so a frame
    /// that took less than the period loses no time to the wait. An engine
    /// slower than the period gets the next Vsync at once, and the input
    /// that came before `now` still goes out ahead of it.
    fn deliver(&mut self, now: Instant) {
        self.due = (self.due + self.period).max(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyKind, Modifiers, MouseAction, MouseButtons, MouseEvent};
    use std::thread;

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn key_name(ready: &Result<Event, Interrupt>) -> Option<&str> {
        match ready {
            Ok(Event::Input(InputEvent::Key(k))) => Some(&k.key),
            _ => None,
        }
    }

    fn is_vsync(ready: &Result<Event, Interrupt>) -> bool {
        matches!(ready, Ok(Event::Input(InputEvent::Vsync)))
    }

    fn is_close(ready: &Result<Event, Interrupt>) -> bool {
        matches!(ready, Err(Interrupt::Close))
    }

    fn is_timeout(ready: &Result<Event, Interrupt>) -> bool {
        matches!(ready, Err(Interrupt::Timeout))
    }

    fn soon() -> Option<Instant> {
        Some(Instant::now() + Duration::from_millis(20))
    }

    /// A queue whose next Vsync is an hour away, for the tests of the
    /// other events.
    fn past_the_first_vsync() -> Inbox {
        let mut inbox = Inbox::new(Duration::from_secs(3600));
        assert!(is_vsync(&inbox.wait(None)));
        inbox
    }

    #[test]
    fn delivers_in_the_order_of_arrival() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_input(key("a")).unwrap();
        tx.wake().unwrap();
        tx.send_input(key("b")).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        assert_eq!(key_name(&inbox.wait(None)), Some("b"));
    }

    #[test]
    fn a_damaged_message_keeps_its_place_and_the_queue_goes_on() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_input(key("a")).unwrap();
        tx.send_read_error(crate::wire::ReadError::Payload(
            crate::wire::Error::PathLengthMismatch {
                verbs: 2,
                coords: 1,
            },
        ))
        .unwrap();
        tx.send_input(key("b")).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(matches!(inbox.wait(None), Err(Interrupt::Read(_))));
        assert_eq!(key_name(&inbox.wait(None)), Some("b"));
    }

    #[test]
    fn times_out_when_nothing_arrives() {
        let mut inbox = past_the_first_vsync();
        assert!(is_timeout(&inbox.wait(soon())));
    }

    #[test]
    fn an_event_sent_first_goes_out_before_the_first_vsync() {
        let mut inbox: Inbox = Inbox::new(Duration::from_secs(60));
        inbox.sender().send_input(key("a")).unwrap();
        inbox.send_first(Event::Input(key("first")));
        assert_eq!(key_name(&inbox.wait(None)), Some("first"));
        assert!(is_vsync(&inbox.wait(None)));
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
    }

    #[test]
    fn a_move_or_a_resize_replaces_one_of_its_kind_at_the_back() {
        let at = |x| {
            InputEvent::Mouse(MouseEvent {
                action: MouseAction::Move,
                x,
                y: 0.0,
                modifiers: Modifiers::default(),
                buttons: MouseButtons::default(),
            })
        };
        let resize = |width| InputEvent::Resize { width, height: 1.0 };
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        for ev in [
            at(1.0),
            at(2.0),
            key("a"),
            at(3.0),
            resize(1.0),
            resize(2.0),
        ] {
            tx.send_input(ev).unwrap();
        }
        let x = |e| match e {
            Ok(Event::Input(InputEvent::Mouse(m))) => m.x,
            other => panic!("got {other:?}"),
        };
        assert_eq!(x(inbox.wait(None)), 2.0);
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert_eq!(x(inbox.wait(None)), 3.0);
        assert!(matches!(
            inbox.wait(None),
            Ok(Event::Input(InputEvent::Resize { width: 2.0, .. }))
        ));
        assert!(is_timeout(&inbox.wait(soon())));
    }

    #[test]
    fn close_goes_out_in_order_and_stays() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_input(key("a")).unwrap();
        tx.send_close().unwrap();
        tx.wake().unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(is_close(&inbox.wait(None)));
        assert!(is_close(&inbox.wait(None)));
    }

    #[test]
    fn each_wake_goes_out_in_order() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.wake().unwrap();
        tx.send_input(key("a")).unwrap();
        tx.wake().unwrap();
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        assert!(is_timeout(&inbox.wait(soon())));
    }

    #[test]
    fn a_sender_fails_once_close_goes_out() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_close().unwrap();
        assert!(is_close(&inbox.wait(None)));
        assert_eq!(tx.wake(), Err(Closed));
    }

    #[test]
    fn a_sender_fails_once_the_inbox_closes() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        inbox.close();
        assert_eq!(tx.send_close(), Err(Closed));
        assert_eq!(inbox.sender().send_close(), Err(Closed));
        assert!(is_close(&inbox.wait(None)));
    }

    #[test]
    fn a_redraw_goes_out_once_after_the_events() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.request_redraw().unwrap();
        tx.send_input(key("a")).unwrap();
        tx.request_redraw().unwrap();
        let mut next = || inbox.wait_with(soon(), Inbox::receive);
        assert!(matches!(next(), Next::Ready(r) if key_name(&r) == Some("a")));
        assert!(matches!(next(), Next::Redraw));
        assert!(matches!(next(), Next::Ready(Err(Interrupt::Timeout))));
    }

    #[test]
    fn wait_skips_a_redraw() {
        let mut inbox = past_the_first_vsync();
        inbox.sender().request_redraw().unwrap();
        assert!(is_timeout(&inbox.wait(soon())));
    }

    #[test]
    fn a_sender_on_another_thread_wakes_the_wait() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.wake().unwrap();
        });
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        t.join().unwrap();
    }

    #[test]
    fn a_sender_fails_once_the_inbox_is_gone() {
        let tx = past_the_first_vsync().sender();
        assert_eq!(tx.send_close(), Err(Closed));
    }

    #[test]
    fn the_clock_fires_at_once_and_then_after_a_period() {
        let period = Duration::from_millis(30);
        let mut inbox: Inbox = Inbox::new(period);
        let start = Instant::now();
        assert!(is_vsync(&inbox.wait(None)));
        assert!(is_timeout(&inbox.wait(Some(Instant::now()))));
        assert!(is_vsync(&inbox.wait(None)));
        assert!(start.elapsed() >= period);
    }

    fn item(at: Instant, ev: InputEvent) -> Item<Event> {
        Item {
            at,
            entry: Entry::Input(Event::Input(ev)),
        }
    }

    #[test]
    fn a_slow_engine_still_gets_the_input() {
        let period = Duration::from_millis(16);
        let mut inbox: Inbox = Inbox::new(period);
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        // A key arrives, and the engine comes back ten periods later.
        inbox.push(item(t0 + period / 2, key("a")));
        let late = t0 + period * 10;
        assert_eq!(key_name(&inbox.pop(late).unwrap()), Some("a"));
        assert!(is_vsync(&inbox.pop(late).unwrap()));
    }

    #[test]
    fn the_clock_keeps_its_beat_after_a_short_frame() {
        let period = Duration::from_millis(16);
        let mut inbox: Inbox = Inbox::new(period);
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        // The engine comes back a little after the Vsync was due.
        assert!(is_vsync(&inbox.pop(t0 + period + period / 4).unwrap()));
        assert!(inbox.pop(t0 + period * 2 - period / 8).is_none());
        assert!(is_vsync(&inbox.pop(t0 + period * 2).unwrap()));
    }

    #[test]
    fn a_late_engine_gets_the_next_vsync_at_once() {
        let period = Duration::from_millis(16);
        let mut inbox: Inbox = Inbox::new(period);
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        let late = t0 + period * 3;
        assert!(is_vsync(&inbox.pop(late).unwrap()));
        // A key arrives while the frame takes two periods. The next Vsync
        // fell due when the frame began, so it goes out first, and the key
        // goes out ahead of the Vsync after it.
        inbox.push(item(late + period / 2, key("a")));
        let next = late + period * 2;
        assert!(is_vsync(&inbox.pop(next).unwrap()));
        let after = next + period * 2;
        assert_eq!(key_name(&inbox.pop(after).unwrap()), Some("a"));
        assert!(is_vsync(&inbox.pop(after).unwrap()));
    }

    #[test]
    fn the_clock_drops_a_vsync_from_the_channel() {
        let period = Duration::from_millis(16);
        let mut inbox: Inbox = Inbox::new(period);
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        inbox.push(item(t0, InputEvent::Vsync));
        assert!(inbox.pop(t0).is_none());
    }

    #[test]
    fn a_vsync_due_before_the_input_goes_first() {
        let period = Duration::from_millis(16);
        let mut inbox: Inbox = Inbox::new(period);
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        inbox.push(item(t0 + period * 2, key("a")));
        let now = t0 + period * 3;
        assert!(is_vsync(&inbox.pop(now).unwrap()));
        assert_eq!(key_name(&inbox.pop(now).unwrap()), Some("a"));
    }
}
