//! The queue behind every [`super::Frontend`]. A frontend owns an [`Inbox`]
//! and hands out [`Sender`]s. Its input threads, the host and any other
//! thread push through a `Sender`, and `wait_event` pops from the `Inbox`,
//! in the order of arrival.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::event::{Event, InputEvent};

/// Pushes into the queue of a frontend from any thread, and wakes a
/// `wait_event` that blocks on it. Get one from
/// [`super::Frontend::sender`].
///
/// The queue has no bound. After the frontend delivers a
/// [`InputEvent::Close`], a message that arrives is lost.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::Sender<Item>,
    wake: Option<Waker>,
}

/// Wakes a frontend that blocks somewhere other than the channel, as the
/// window does in its event loop.
pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

/// The error of a [`Sender`] whose frontend no longer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl Sender {
    /// Queue an [`Event::Reply`]. `id` and `body` mean nothing to sinteract.
    pub fn send_reply(&self, id: u64, body: Vec<u8>) -> Result<(), Closed> {
        self.send(Event::Reply { id, body })
    }

    /// Queue an [`InputEvent::Close`]. A Ctrl-C handler of the host calls
    /// it to end a `wait_event` that blocks.
    pub fn send_close(&self) -> Result<(), Closed> {
        self.send_input(InputEvent::Close)
    }

    pub(crate) fn send_input(&self, ev: InputEvent) -> Result<(), Closed> {
        self.send(Event::Input(ev))
    }

    fn send(&self, event: Event) -> Result<(), Closed> {
        let item = Item {
            at: Instant::now(),
            event,
        };
        self.tx.send(item).map_err(|_| Closed)?;
        if let Some(wake) = &self.wake {
            wake();
        }
        Ok(())
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the frontend no longer exists")
    }
}

impl std::error::Error for Closed {}

/// The receiving end. It holds a sender of its own for
/// [`Inbox::sender`], so the channel never disconnects.
pub(crate) struct Inbox {
    tx: mpsc::Sender<Item>,
    wake: Option<Waker>,
    rx: mpsc::Receiver<Item>,
    /// What left the channel and did not go out yet, oldest first.
    pending: VecDeque<Item>,
    /// `pending` holds a Vsync.
    vsync_pending: bool,
    /// `None` when the Vsync events come through the channel, as on stdio.
    clock: Option<VsyncClock>,
    /// Set by the first Close out or by [`Inbox::close`]. Every wait
    /// returns Close from then on.
    closed: bool,
}

struct Item {
    at: Instant,
    event: Event,
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
            vsync_pending: false,
            clock: vsync_period.map(VsyncClock::new),
            closed: false,
        }
    }

    pub(crate) fn sender(&self) -> Sender {
        Sender {
            tx: self.tx.clone(),
            wake: self.wake.clone(),
        }
    }

    /// Deliver Close from now on and drop what is queued.
    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.pending.clear();
        self.vsync_pending = false;
    }

    /// The oldest event, or [`Event::Timeout`] once `deadline` passes. A
    /// `deadline` of `None` waits for as long as it takes.
    ///
    /// A Vsync of the clock counts as arrived when it falls due, so input
    /// that arrived before it goes out first. The clock counts the next
    /// period from the delivery, so a host slower than the period gets one
    /// Vsync per call and the input still goes out.
    pub(crate) fn wait(&mut self, deadline: Option<Instant>) -> Event {
        loop {
            if let Some(event) = self.poll() {
                return event;
            }
            let now = Instant::now();
            if deadline.is_some_and(|d| now >= d) {
                return Event::Timeout;
            }
            let received = match self.wake_at(deadline) {
                Some(t) => match self.rx.recv_timeout(t - now) {
                    Ok(item) => Some(item),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => None,
                },
                None => self.rx.recv().ok(),
            };
            match received {
                Some(item) => self.push(item),
                // The inbox holds a sender, so this does not happen.
                None => self.closed = true,
            }
        }
    }

    /// The oldest event that is ready, without blocking.
    pub(crate) fn poll(&mut self) -> Option<Event> {
        if self.closed {
            return Some(Event::Input(InputEvent::Close));
        }
        while let Ok(item) = self.rx.try_recv() {
            self.push(item);
        }
        self.pop(Instant::now())
    }

    /// When a wait until `deadline` has to stop and look again, the earlier
    /// of `deadline` and the next Vsync of the clock. `None` waits for as
    /// long as it takes.
    pub(crate) fn wake_at(&self, deadline: Option<Instant>) -> Option<Instant> {
        match (deadline, self.clock.as_ref().map(|c| c.due)) {
            (Some(d), Some(v)) => Some(d.min(v)),
            (d, v) => d.or(v),
        }
    }

    /// Queue `item`, unless it is a Vsync and one already waits. A host
    /// that falls behind gets one Vsync, not a burst.
    fn push(&mut self, item: Item) {
        if matches!(item.event, Event::Input(InputEvent::Vsync)) {
            if self.vsync_pending {
                return;
            }
            self.vsync_pending = true;
        }
        self.pending.push_back(item);
    }

    /// The oldest of the front of `pending` and a Vsync of the clock due by
    /// `now`, or `None` when neither exists.
    fn pop(&mut self, now: Instant) -> Option<Event> {
        let vsync_due = self.clock.as_ref().map(|c| c.due).filter(|&due| due <= now);
        let front_at = self.pending.front().map(|item| item.at);
        match (front_at, vsync_due) {
            (Some(at), Some(due)) if due < at => Some(self.fire(now)),
            (Some(_), _) => {
                let item = self.pending.pop_front()?;
                Some(self.deliver(item.event))
            }
            (None, Some(_)) => Some(self.fire(now)),
            (None, None) => None,
        }
    }

    fn fire(&mut self, now: Instant) -> Event {
        if let Some(clock) = self.clock.as_mut() {
            clock.due = now + clock.period;
        }
        Event::Input(InputEvent::Vsync)
    }

    fn deliver(&mut self, event: Event) -> Event {
        match &event {
            Event::Input(InputEvent::Vsync) => self.vsync_pending = false,
            Event::Input(InputEvent::Close) => self.close(),
            _ => {}
        }
        event
    }
}

/// A software Vsync, for a frontend without a platform one.
struct VsyncClock {
    period: Duration,
    /// When the next Vsync falls due. The first one is due at once.
    due: Instant,
}

impl VsyncClock {
    fn new(period: Duration) -> Self {
        Self {
            period,
            due: Instant::now(),
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
    fn a_slow_host_still_gets_the_input() {
        let period = Duration::from_millis(16);
        let mut inbox = Inbox::new(Some(period));
        let t0 = Instant::now();
        assert!(is_vsync(&inbox.pop(t0).unwrap()));
        // A key arrives, and the host comes back ten periods later.
        inbox.push(item(t0 + period / 2, key("a")));
        let late = t0 + period * 10;
        assert_eq!(key_name(&inbox.pop(late).unwrap()), Some("a"));
        assert!(is_vsync(&inbox.pop(late).unwrap()));
        assert!(inbox.pop(late).is_none());
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
