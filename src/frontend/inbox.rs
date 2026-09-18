//! The queue behind every [`super::Frontend`]. A frontend owns an [`Inbox`]
//! and hands out [`Sender`]s. Its input threads, the engine and any other
//! thread push through a `Sender`, and `wait_event` pops from the `Inbox`,
//! in the order of arrival.

use std::collections::VecDeque;
use std::fmt;
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::event::{Event, InputEvent};

/// Pushes into the queue of a frontend from any thread, and wakes a
/// `wait_event` that blocks on it. Get one from
/// [`super::Frontend::sender`].
///
/// The queue has no bound. Once the frontend closes or delivers an
/// [`InputEvent::Close`], every send returns [`Closed`]. A message sent
/// after a Close that has not gone out yet is lost.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::Sender<Msg>,
    wake: Option<Waker>,
}

/// Wakes a frontend that blocks somewhere other than the channel, as the
/// window does in its event loop.
pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

/// The error of a [`Sender`] whose frontend was closed or dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl Sender {
    /// Queue an [`Event::Reply`]. `id` and `body` mean nothing to sinteract.
    pub fn send_reply(&self, id: u64, body: Vec<u8>) -> Result<(), Closed> {
        self.send(Event::Reply { id, body })
    }

    /// Queue an [`InputEvent::Close`]. A Ctrl-C handler of the engine calls
    /// it to end a `wait_event` that blocks.
    pub fn send_close(&self) -> Result<(), Closed> {
        self.send_input(InputEvent::Close)
    }

    pub(crate) fn send_input(&self, ev: InputEvent) -> Result<(), Closed> {
        self.send(Event::Input(ev))
    }

    /// Ask the frontend to draw the last scene again, as after a resize.
    /// The request never reaches the engine.
    pub(crate) fn request_redraw(&self) -> Result<(), Closed> {
        self.put(Msg::Redraw)
    }

    fn send(&self, event: Event) -> Result<(), Closed> {
        self.put(Msg::Item(Item {
            at: Instant::now(),
            event,
        }))
    }

    fn put(&self, msg: Msg) -> Result<(), Closed> {
        self.tx.send(msg).map_err(|_| Closed)?;
        if let Some(wake) = &self.wake {
            wake();
        }
        Ok(())
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the frontend is closed")
    }
}

impl std::error::Error for Closed {}

/// The receiving end. It holds a sender of its own for [`Inbox::sender`].
pub(crate) struct Inbox {
    tx: mpsc::Sender<Msg>,
    wake: Option<Waker>,
    rx: mpsc::Receiver<Msg>,
    /// What left the channel and did not go out yet, oldest first, except
    /// for a Vsync, which waits in `vsync`.
    pending: VecDeque<Item>,
    vsync: Vsync,
    /// A redraw was requested and did not go out yet. Many requests make
    /// one redraw.
    redraw: bool,
    /// Set by the first Close out or by [`Inbox::close`]. Every wait
    /// returns Close from then on.
    closed: bool,
}

enum Msg {
    Item(Item),
    Redraw,
}

struct Item {
    at: Instant,
    event: Event,
}

/// What [`Inbox::wait_with`] hands to the frontend.
pub(crate) enum Wait {
    Event(Event),
    /// Draw the last scene again, and wait again.
    Redraw,
}

impl Inbox {
    /// A queue that makes a Vsync every `vsync_period`, or that takes them
    /// from the channel when it is `None`.
    pub(crate) fn new(vsync_period: Option<Duration>) -> Self {
        Self::with_waker(vsync_period, None)
    }

    /// A queue whose [`Sender`]s also call `wake` after they push.
    pub(crate) fn with_waker(vsync_period: Option<Duration>, wake: Option<Waker>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            wake,
            rx,
            pending: VecDeque::new(),
            vsync: match vsync_period {
                Some(period) => Vsync::Clock {
                    period,
                    due: Instant::now(),
                },
                None => Vsync::Channel { arrived: None },
            },
            redraw: false,
            closed: false,
        }
    }

    pub(crate) fn sender(&self) -> Sender {
        Sender {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }

    /// Deliver Close from now on and drop what is queued. A new receiver
    /// replaces the channel, so every [`Sender`] fails from now on.
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.rx = mpsc::channel().1;
        self.pending.clear();
        self.redraw = false;
    }

    /// [`Inbox::wait_with`] on the channel, for a frontend that has nothing
    /// to redraw.
    pub(crate) fn wait(&mut self, deadline: Option<Instant>) -> Event {
        loop {
            if let Wait::Event(event) = self.wait_with(deadline, Self::receive) {
                return event;
            }
        }
    }

    /// The oldest event, or [`Event::Timeout`] once `deadline` passes. A
    /// `deadline` of `None` waits for as long as it takes. A redraw goes
    /// out when no event is ready, so an engine that presents anyway skips it.
    ///
    /// `block` waits for at most its timeout, or for as long as it takes
    /// when the timeout is `None`, and a [`Sender`] wakes it. A frontend
    /// that blocks on the channel passes [`Inbox::receive`].
    ///
    /// A Vsync of the clock counts as arrived when it falls due, so input
    /// that arrived before it goes out first.
    pub(crate) fn wait_with(
        &mut self,
        deadline: Option<Instant>,
        mut block: impl FnMut(&mut Self, Option<Duration>),
    ) -> Wait {
        loop {
            if let Some(event) = self.poll() {
                return Wait::Event(event);
            }
            if mem::take(&mut self.redraw) {
                return Wait::Redraw;
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Wait::Event(Event::Timeout);
            }
            let timeout = self.wake_at(deadline).map(|t| t - now);
            block(self, timeout);
        }
    }

    /// Take the next message of the channel, or wait until `timeout`
    /// passes.
    pub(crate) fn receive(&mut self, timeout: Option<Duration>) {
        let received = match timeout {
            Some(t) => match self.rx.recv_timeout(t) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => return,
                Err(RecvTimeoutError::Disconnected) => None,
            },
            None => self.rx.recv().ok(),
        };
        match received {
            Some(msg) => self.take(msg),
            // Only the receiver of a closed inbox disconnects.
            None => self.closed = true,
        }
    }

    /// The oldest event that is ready, without blocking.
    fn poll(&mut self) -> Option<Event> {
        if self.closed {
            return Some(Event::Input(InputEvent::Close));
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.take(msg);
        }
        self.pop(Instant::now())
    }

    /// When a wait until `deadline` has to stop and look again, the earlier
    /// of `deadline` and the next Vsync. `None` waits for as long as it
    /// takes.
    fn wake_at(&self, deadline: Option<Instant>) -> Option<Instant> {
        match (deadline, self.vsync.at()) {
            (Some(d), Some(v)) => Some(d.min(v)),
            (d, v) => d.or(v),
        }
    }

    fn take(&mut self, msg: Msg) {
        match msg {
            Msg::Item(item) => self.push(item),
            Msg::Redraw => self.redraw = true,
        }
    }

    /// Queue `item`. A Vsync from the channel waits in `vsync`, unless one
    /// already waits, so an engine that falls behind gets one Vsync, not a
    /// burst. A clock makes every Vsync, so it drops one from the channel.
    fn push(&mut self, item: Item) {
        if matches!(item.event, Event::Input(InputEvent::Vsync)) {
            if let Vsync::Channel { arrived } = &mut self.vsync {
                arrived.get_or_insert(item.at);
            }
            return;
        }
        self.pending.push_back(item);
    }

    /// The older of the front of `pending` and a Vsync that arrived by
    /// `now`, or `None` when neither exists.
    fn pop(&mut self, now: Instant) -> Option<Event> {
        let front = self.pending.front().map(|item| item.at);
        let vsync = self.vsync.at().filter(|&at| at <= now);
        if vsync.is_some_and(|v| front.is_none_or(|f| v < f)) {
            self.vsync.deliver(now);
            return Some(Event::Input(InputEvent::Vsync));
        }
        let event = self.pending.pop_front()?.event;
        if matches!(event, Event::Input(InputEvent::Close)) {
            self.close();
        }
        Some(event)
    }
}

/// Where the Vsync events come from.
enum Vsync {
    /// A software clock, for a frontend without a platform Vsync.
    Clock {
        period: Duration,
        /// When the next Vsync falls due. The first one is due at once.
        due: Instant,
    },
    /// The channel, as on stdio.
    Channel {
        /// When the Vsync that waits arrived.
        arrived: Option<Instant>,
    },
}

impl Vsync {
    /// When the next Vsync counts as arrived, or `None` when none waits.
    fn at(&self) -> Option<Instant> {
        match *self {
            Vsync::Clock { due, .. } => Some(due),
            Vsync::Channel { arrived } => arrived,
        }
    }

    /// The Vsync went out at `now`. The clock counts the next period from
    /// the delivery, so an engine slower than the period still gets its input.
    fn deliver(&mut self, now: Instant) {
        match self {
            Vsync::Clock { period, due } => *due = now + *period,
            Vsync::Channel { arrived } => *arrived = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyEvent, KeyKind, Modifiers};
    use std::thread;

    fn key(name: &str) -> InputEvent {
        InputEvent::Key(KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        })
    }

    fn key_name(event: &Event) -> Option<&str> {
        match event {
            Event::Input(InputEvent::Key(k)) => Some(&k.key),
            _ => None,
        }
    }

    fn is_vsync(event: &Event) -> bool {
        matches!(event, Event::Input(InputEvent::Vsync))
    }

    fn is_close(event: &Event) -> bool {
        matches!(event, Event::Input(InputEvent::Close))
    }

    fn soon() -> Option<Instant> {
        Some(Instant::now() + Duration::from_millis(20))
    }

    #[test]
    fn delivers_in_the_order_of_arrival() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        tx.send_input(key("a")).unwrap();
        tx.send_reply(7, b"x".to_vec()).unwrap();
        tx.send_input(key("b")).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(matches!(inbox.wait(None), Event::Reply { id: 7, .. }));
        assert_eq!(key_name(&inbox.wait(None)), Some("b"));
    }

    #[test]
    fn times_out_when_nothing_arrives() {
        let mut inbox = Inbox::new(None);
        assert!(matches!(inbox.wait(soon()), Event::Timeout));
    }

    #[test]
    fn keeps_one_vsync_from_the_channel() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        tx.send_input(InputEvent::Vsync).unwrap();
        tx.send_input(key("a")).unwrap();
        tx.send_input(InputEvent::Vsync).unwrap();
        assert!(is_vsync(&inbox.wait(None)));
        // The first Vsync went out, so the next one queues.
        tx.send_input(InputEvent::Vsync).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(is_vsync(&inbox.wait(None)));
        assert!(matches!(inbox.wait(soon()), Event::Timeout));
    }

    #[test]
    fn close_goes_out_in_order_and_stays() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        tx.send_input(key("a")).unwrap();
        tx.send_close().unwrap();
        tx.send_reply(1, Vec::new()).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(is_close(&inbox.wait(None)));
        assert!(is_close(&inbox.wait(None)));
    }

    #[test]
    fn a_sender_fails_once_close_goes_out() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        tx.send_close().unwrap();
        assert!(is_close(&inbox.wait(None)));
        assert_eq!(tx.send_reply(1, Vec::new()), Err(Closed));
    }

    #[test]
    fn a_sender_fails_once_the_inbox_closes() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        inbox.close();
        assert_eq!(tx.send_close(), Err(Closed));
        assert_eq!(inbox.sender().send_close(), Err(Closed));
        assert!(is_close(&inbox.wait(None)));
    }

    #[test]
    fn a_redraw_goes_out_once_after_the_events() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        tx.request_redraw().unwrap();
        tx.send_input(key("a")).unwrap();
        tx.request_redraw().unwrap();
        let mut next = || inbox.wait_with(soon(), Inbox::receive);
        assert!(matches!(next(), Wait::Event(e) if key_name(&e) == Some("a")));
        assert!(matches!(next(), Wait::Redraw));
        assert!(matches!(next(), Wait::Event(Event::Timeout)));
    }

    #[test]
    fn wait_skips_a_redraw() {
        let mut inbox = Inbox::new(None);
        inbox.sender().request_redraw().unwrap();
        assert!(matches!(inbox.wait(soon()), Event::Timeout));
    }

    #[test]
    fn a_sender_on_another_thread_wakes_the_wait() {
        let mut inbox = Inbox::new(None);
        let tx = inbox.sender();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            tx.send_reply(3, b"done".to_vec()).unwrap();
        });
        match inbox.wait(None) {
            Event::Reply { id, body } => assert_eq!((id, body.as_slice()), (3, &b"done"[..])),
            other => panic!("got {other:?}"),
        }
        t.join().unwrap();
    }

    #[test]
    fn a_sender_fails_once_the_inbox_is_gone() {
        let tx = Inbox::new(None).sender();
        assert_eq!(tx.send_close(), Err(Closed));
    }

    #[test]
    fn the_clock_fires_at_once_and_then_after_a_period() {
        let period = Duration::from_millis(30);
        let mut inbox = Inbox::new(Some(period));
        let start = Instant::now();
        assert!(is_vsync(&inbox.wait(None)));
        assert!(matches!(inbox.wait(Some(Instant::now())), Event::Timeout));
        assert!(is_vsync(&inbox.wait(None)));
        assert!(start.elapsed() >= period);
    }

    fn item(at: Instant, ev: InputEvent) -> Item {
        Item {
            at,
            event: Event::Input(ev),
        }
    }

    #[test]
    fn a_slow_engine_still_gets_the_input() {
        let period = Duration::from_millis(16);
        let mut inbox = Inbox::new(Some(period));
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        // A key arrives, and the engine comes back ten periods later.
        inbox.push(item(t0 + period / 2, key("a")));
        let late = t0 + period * 10;
        assert_eq!(key_name(&inbox.pop(late).unwrap()), Some("a"));
        assert!(is_vsync(&inbox.pop(late).unwrap()));
        assert!(inbox.pop(late).is_none());
    }

    #[test]
    fn the_clock_drops_a_vsync_from_the_channel() {
        let period = Duration::from_millis(16);
        let mut inbox = Inbox::new(Some(period));
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        inbox.push(item(t0, InputEvent::Vsync));
        assert!(inbox.pop(t0).is_none());
    }

    #[test]
    fn a_vsync_due_before_the_input_goes_first() {
        let period = Duration::from_millis(16);
        let mut inbox = Inbox::new(Some(period));
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        inbox.push(item(t0 + period * 2, key("a")));
        let now = t0 + period * 3;
        assert!(is_vsync(&inbox.pop(now).unwrap()));
        assert_eq!(key_name(&inbox.pop(now).unwrap()), Some("a"));
    }
}
