//! The queue behind every [`super::Display`]. A display owns an [`Inbox`]
//! and hands out [`Sender`]s. Its input threads, the engine and any other
//! thread push through a `Sender`, and `wait_event` pops from the `Inbox`,
//! in the order of arrival.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::mem;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::vsync_clock::VsyncClock;
use crate::event::{Event, InputEvent, Interrupt, KeyEvent, MouseEvent};

/// Pushes into the queue of a display from any thread, and wakes a
/// `wait_event` that blocks on it. Get one from
/// [`super::Display::sender`].
///
/// The queue has no bound. Once the display closes or delivers
/// [`Interrupt::Close`], every send returns [`Closed`]. A message sent after
/// a Close that has not gone out yet is lost.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::Sender<Msg>,
    wake: Option<Waker>,
}

/// Wakes a display that blocks somewhere other than the channel, as the
/// window does in its event loop.
pub(crate) type Waker = Arc<dyn Fn() + Send + Sync>;

/// The error of a [`Sender`] whose display was closed or dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed;

impl Sender {
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

    /// Queue [`Interrupt::Read`], in order with the events. A reader that
    /// sends a read error sends a close right after.
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub(crate) fn send_read_error(&self, e: io::Error) -> Result<(), Closed> {
        self.send(Entry::Read(e))
    }

    /// Ask the display to draw the last scene again, as after a resize.
    /// The request never reaches the engine. Only the terminal and the
    /// window redraw.
    pub(crate) fn request_redraw(&self) -> Result<(), Closed> {
        self.put(Msg::Redraw)
    }

    pub(crate) fn send_key(&self, key: KeyEvent) -> Result<(), Closed> {
        self.send(Entry::Input(InputEvent::Key(key)))
    }

    pub(crate) fn send_mouse(&self, mouse: MouseEvent) -> Result<(), Closed> {
        self.send(Entry::Input(InputEvent::Mouse(mouse)))
    }

    /// Queue a Resize to the scene size `width` by `height`.
    pub(crate) fn send_resize(&self, width: f32, height: f32) -> Result<(), Closed> {
        self.send(Entry::Input(InputEvent::Resize { width, height }))
    }

    fn send(&self, entry: Entry) -> Result<(), Closed> {
        self.put(Msg::Entry(entry))
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
        f.write_str("the display is closed")
    }
}

impl std::error::Error for Closed {}

/// The receiving end. It holds a sender of its own for [`Inbox::sender`].
pub(crate) struct Inbox {
    tx: mpsc::Sender<Msg>,
    wake: Option<Waker>,
    rx: mpsc::Receiver<Msg>,
    /// What left the channel and did not go out yet, oldest first.
    pending: VecDeque<Entry>,
    /// A redraw was requested and did not go out yet. Many requests make
    /// one redraw.
    redraw: bool,
    /// Set by the first Close out or by [`Inbox::close`]. Every wait
    /// returns Close from then on.
    closed: bool,
}

enum Msg {
    Entry(Entry),
    Redraw,
}

/// What waits in the queue for `wait_event`.
enum Entry {
    Input(InputEvent),
    /// Only the clock of the display queues it.
    Vsync,
    Wake,
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    Read(io::Error),
    Close,
}

/// What [`Inbox::next`] asks of the display.
pub(crate) enum Next {
    /// Return this from `wait_event`.
    Ready(Result<Event, Interrupt>),
    /// Draw the last scene again.
    Redraw,
    /// Wait for a message for at most this long, and ask again.
    Block(Duration),
}

impl Inbox {
    /// A queue whose [`Sender`]s also call `wake` after they push, for a
    /// display that blocks somewhere other than the channel.
    pub(crate) fn new(wake: Option<Waker>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            wake,
            rx,
            pending: VecDeque::new(),
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

    /// A [`Sender`] without the waker, for the callbacks of the loop that
    /// the waker wakes. A wake from inside the loop only adds a turn with
    /// nothing to do.
    #[cfg(feature = "window")]
    pub(crate) fn sender_without_waker(&self) -> Sender {
        Sender {
            tx: self.tx.clone(),
            wake: None,
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

    /// The next step of `wait_event`, which calls it in a loop until it
    /// gets [`Next::Ready`]. The oldest event goes out first, then a
    /// redraw, then [`Interrupt::Timeout`] once `deadline` passes. A
    /// `deadline` of `None` waits for as long as it takes. So an engine
    /// that presents anyway skips the redraw.
    ///
    /// A Vsync that fell due on `clock` goes in behind what arrived so
    /// far, so the input that waited on a busy engine goes out first.
    pub(crate) fn next(&mut self, clock: &mut VsyncClock, deadline: Option<Instant>) -> Next {
        let now = Instant::now();
        if let Some(ready) = self.pop(clock, now) {
            return Next::Ready(ready);
        }
        if mem::take(&mut self.redraw) {
            return Next::Redraw;
        }
        if deadline.is_some_and(|d| now >= d) {
            return Next::Ready(Err(Interrupt::Timeout));
        }
        let wake_at = deadline.map_or(clock.due(), |d| d.min(clock.due()));
        Next::Block(wake_at.saturating_duration_since(now))
    }

    /// Take the next message of the channel, or wait until `timeout`
    /// passes. A [`Sender`] ends the wait.
    #[cfg_attr(not(feature = "terminal"), allow(dead_code))]
    pub(crate) fn wait_for_message(&mut self, timeout: Duration) {
        match self.rx.recv_timeout(timeout) {
            Ok(msg) => self.take(msg),
            Err(RecvTimeoutError::Timeout) => {}
            // Only the receiver of a closed inbox disconnects.
            Err(RecvTimeoutError::Disconnected) => self.closed = true,
        }
    }

    /// The oldest entry, after the channel drains and a Vsync goes in at
    /// the back if `clock` has one due by `now`, or `None` when nothing
    /// waits. At most one Vsync waits, so a flood of input delays the
    /// Vsync but never drops it.
    fn pop(&mut self, clock: &mut VsyncClock, now: Instant) -> Option<Result<Event, Interrupt>> {
        if self.closed {
            return Some(Err(Interrupt::Close));
        }
        while let Ok(msg) = self.rx.try_recv() {
            self.take(msg);
        }
        if !self.pending.iter().any(|e| matches!(e, Entry::Vsync)) && clock.take_due(now) {
            self.pending.push_back(Entry::Vsync);
        }
        Some(match self.pending.pop_front()? {
            Entry::Input(ev) => Ok(Event::Input(ev)),
            Entry::Vsync => Ok(Event::Input(InputEvent::Vsync)),
            Entry::Wake => Err(Interrupt::Wake),
            Entry::Read(e) => Err(Interrupt::Read(e)),
            Entry::Close => {
                self.close();
                Err(Interrupt::Close)
            }
        })
    }

    fn take(&mut self, msg: Msg) {
        match msg {
            Msg::Entry(entry) => self.push(entry),
            Msg::Redraw => self.redraw = true,
        }
    }

    /// Queue `entry`. A move of the mouse or a resize replaces one of its
    /// kind at the back of the queue, since only the latest one counts. A
    /// mouse at 1000 Hz would flood an engine that runs at 60 Hz.
    fn push(&mut self, entry: Entry) {
        if let Entry::Input(new) = &entry
            && let Some(Entry::Input(old)) = self.pending.back()
            && new.supersedes(old)
        {
            self.pending.pop_back();
        }
        self.pending.push_back(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyKind, Modifiers, MouseAction, MouseButtons};
    use std::num::NonZeroU32;
    use std::thread;

    fn key(name: &str) -> KeyEvent {
        KeyEvent {
            kind: KeyKind::Press,
            key: name.into(),
            modifiers: Modifiers::default(),
            repeat: false,
        }
    }

    fn key_name(ready: &Result<Event, Interrupt>) -> Option<&str> {
        match ready {
            Ok(Event::Input(InputEvent::Key(k))) => Some(&k.key),
            _ => None,
        }
    }

    fn move_to(x: f32) -> MouseEvent {
        MouseEvent {
            action: MouseAction::Move,
            x,
            y: 0.0,
            modifiers: Modifiers::default(),
            buttons: MouseButtons::default(),
        }
    }

    fn mouse_x(ready: &Result<Event, Interrupt>) -> f32 {
        match ready {
            Ok(Event::Input(InputEvent::Mouse(m))) => m.x,
            other => panic!("got {other:?}"),
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

    /// The loop of `wait_event` in a display that blocks on the channel,
    /// as the terminal does.
    struct TestDisplay {
        inbox: Inbox,
        clock: VsyncClock,
    }

    impl TestDisplay {
        fn new(rate: u32) -> Self {
            Self {
                inbox: Inbox::new(None),
                clock: VsyncClock::from_millihertz(NonZeroU32::new(rate).unwrap()),
            }
        }

        fn sender(&self) -> Sender {
            self.inbox.sender()
        }

        fn close(&mut self) {
            self.inbox.close();
        }

        /// [`Inbox::next`] up to a step other than a block.
        fn next(&mut self, deadline: Option<Instant>) -> Next {
            loop {
                match self.inbox.next(&mut self.clock, deadline) {
                    Next::Block(timeout) => self.inbox.wait_for_message(timeout),
                    next => return next,
                }
            }
        }

        /// The next event, with no redraw.
        fn wait(&mut self, deadline: Option<Instant>) -> Result<Event, Interrupt> {
            loop {
                if let Next::Ready(ready) = self.next(deadline) {
                    return ready;
                }
            }
        }
    }

    /// A display whose next Vsync is 1000 s away, for the tests of the
    /// other events.
    fn past_the_first_vsync() -> TestDisplay {
        let mut inbox = TestDisplay::new(1);
        assert!(is_vsync(&inbox.wait(None)));
        inbox
    }

    #[test]
    fn delivers_in_the_order_of_arrival() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_key(key("a")).unwrap();
        tx.wake().unwrap();
        tx.send_key(key("b")).unwrap();
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        assert_eq!(key_name(&inbox.wait(None)), Some("b"));
    }

    #[test]
    fn a_read_error_keeps_its_place() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_key(key("a")).unwrap();
        tx.send_read_error(io::Error::other("the tty went away"))
            .unwrap();
        tx.send_key(key("b")).unwrap();
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
    fn a_move_or_a_resize_replaces_one_of_its_kind_at_the_back() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_mouse(move_to(1.0)).unwrap();
        tx.send_mouse(move_to(2.0)).unwrap();
        tx.send_key(key("a")).unwrap();
        tx.send_mouse(move_to(3.0)).unwrap();
        tx.send_resize(1.0, 1.0).unwrap();
        tx.send_resize(2.0, 1.0).unwrap();
        assert_eq!(mouse_x(&inbox.wait(None)), 2.0);
        assert_eq!(key_name(&inbox.wait(None)), Some("a"));
        assert_eq!(mouse_x(&inbox.wait(None)), 3.0);
        assert!(matches!(
            inbox.wait(None),
            Ok(Event::Input(InputEvent::Resize { width: 2.0, .. }))
        ));
        assert!(is_timeout(&inbox.wait(soon())));
    }

    #[test]
    fn a_move_before_a_wake_stays() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_mouse(move_to(1.0)).unwrap();
        tx.wake().unwrap();
        tx.send_mouse(move_to(2.0)).unwrap();
        assert_eq!(mouse_x(&inbox.wait(None)), 1.0);
        assert!(matches!(inbox.wait(None), Err(Interrupt::Wake)));
        assert_eq!(mouse_x(&inbox.wait(None)), 2.0);
    }

    #[test]
    fn close_goes_out_in_order_and_stays() {
        let mut inbox = past_the_first_vsync();
        let tx = inbox.sender();
        tx.send_key(key("a")).unwrap();
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
        tx.send_key(key("a")).unwrap();
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
        tx.send_key(key("a")).unwrap();
        tx.request_redraw().unwrap();
        let deadline = soon();
        assert!(matches!(inbox.next(deadline), Next::Ready(r) if key_name(&r) == Some("a")));
        assert!(matches!(inbox.next(deadline), Next::Redraw));
        assert!(matches!(
            inbox.next(deadline),
            Next::Ready(Err(Interrupt::Timeout))
        ));
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
        let mut inbox = TestDisplay::new(33_000);
        let start = Instant::now();
        assert!(is_vsync(&inbox.wait(None)));
        assert!(is_timeout(&inbox.wait(Some(Instant::now()))));
        assert!(is_vsync(&inbox.wait(None)));
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    fn push_key(inbox: &mut TestDisplay, name: &str) {
        inbox.inbox.push(Entry::Input(InputEvent::Key(key(name))));
    }

    fn pop(inbox: &mut TestDisplay, now: Instant) -> Option<Result<Event, Interrupt>> {
        inbox.inbox.pop(&mut inbox.clock, now)
    }

    #[test]
    fn a_slow_engine_gets_the_input_before_the_vsync() {
        let mut inbox = TestDisplay::new(60_000);
        let t0 = Instant::now();
        assert!(is_vsync(&pop(&mut inbox, t0).unwrap()));
        // A key arrives, and the engine comes back ten periods later.
        push_key(&mut inbox, "a");
        let late = t0 + Duration::from_millis(170);
        assert_eq!(key_name(&pop(&mut inbox, late).unwrap()), Some("a"));
        assert!(is_vsync(&pop(&mut inbox, late).unwrap()));
    }

    #[test]
    fn input_that_arrives_behind_a_vsync_waits_for_it() {
        let mut inbox = TestDisplay::new(1);
        let t0 = Instant::now();
        push_key(&mut inbox, "a");
        assert_eq!(key_name(&pop(&mut inbox, t0).unwrap()), Some("a"));
        push_key(&mut inbox, "b");
        assert!(is_vsync(&pop(&mut inbox, t0).unwrap()));
        assert_eq!(key_name(&pop(&mut inbox, t0).unwrap()), Some("b"));
        assert!(pop(&mut inbox, t0).is_none());
    }
}
