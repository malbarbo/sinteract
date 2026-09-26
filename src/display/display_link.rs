//! The frames of the screen on macOS, from the display link of the view of
//! a window.
//!
//! A timer of the loop wakes late by 5 to 20 ms on macOS, since the system
//! coalesces the timers of an app, so a tick at the refresh rate of the
//! monitor still drifts against the frames of the screen. AppKit calls the
//! display link on the main run loop at each frame, and the call wakes the
//! loop of the window, so the tick goes out at the frame.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AllocAnyThread, DefinedClass, class, define_class, msg_send, sel};
use winit::event_loop::EventLoopProxy;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window as WinitWindow;

/// Stops at drop. A view that AppKit does not show gets no call, so the
/// clock of the window paces the ticks then.
pub(super) struct DisplayLink {
    /// `None` before macOS 14, which has no display link on a view.
    link: Option<Retained<AnyObject>>,
    frame: Rc<Cell<Option<Instant>>>,
}

impl DisplayLink {
    /// Start the display link of the view of `window`. Each frame wakes
    /// the loop through `proxy`.
    pub(super) fn start(window: &WinitWindow, proxy: EventLoopProxy<()>) -> Self {
        let frame = Rc::new(Cell::new(None));
        let link = match window.window_handle().map(|h| h.as_raw()) {
            // SAFETY: the view belongs to a live window, on the main thread.
            Ok(RawWindowHandle::AppKit(h)) => unsafe {
                start_link(h.ns_view.cast().as_ref(), frame.clone(), proxy)
            },
            _ => None,
        };
        Self { link, frame }
    }

    /// The time of the last frame, if one came after the last call.
    pub(super) fn take(&mut self) -> Option<Instant> {
        self.frame.take()
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        if let Some(link) = &self.link {
            // SAFETY: invalidate takes the link off the run loop and
            // releases the target.
            let _: () = unsafe { msg_send![&**link, invalidate] };
        }
    }
}

struct Ivars {
    frame: Rc<Cell<Option<Instant>>>,
    proxy: EventLoopProxy<()>,
}

define_class!(
    // SAFETY: NSObject has no rule for a subclass, and Target has no Drop.
    #[unsafe(super(NSObject))]
    #[ivars = Ivars]
    struct Target;

    impl Target {
        #[unsafe(method(step:))]
        fn step(&self, _link: &AnyObject) {
            self.ivars().frame.set(Some(Instant::now()));
            let _ = self.ivars().proxy.send_event(());
        }
    }
);

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSRunLoopCommonModes: &'static AnyObject;
}

/// Add a display link of `view` to the main run loop. Returns `None` before
/// macOS 14.
///
/// # Safety
///
/// `view` is an NSView, and the caller is on the main thread.
unsafe fn start_link(
    view: &AnyObject,
    frame: Rc<Cell<Option<Instant>>>,
    proxy: EventLoopProxy<()>,
) -> Option<Retained<AnyObject>> {
    let selector = sel!(displayLinkWithTarget:selector:);
    // SAFETY: plain AppKit calls, with the promise of the caller.
    unsafe {
        let known: bool = msg_send![view, respondsToSelector: selector];
        if !known {
            return None;
        }
        let target = Target::alloc().set_ivars(Ivars { frame, proxy });
        let target: Retained<Target> = msg_send![super(target), init];
        let link: Retained<AnyObject> =
            msg_send![view, displayLinkWithTarget: &*target, selector: sel!(step:)];
        let run_loop: Retained<AnyObject> = msg_send![class!(NSRunLoop), mainRunLoop];
        // The common modes keep the link on while the user drags the
        // window.
        let _: () = msg_send![&*link, addToRunLoop: &*run_loop, forMode: NSRunLoopCommonModes];
        Some(link)
    }
}
