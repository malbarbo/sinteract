//! Asks the terminal which graphics it supports, on Unix and Windows.
//!
//! The environment variables that name a terminal do not survive ssh and
//! multiplexers, so the probe writes four queries to the controlling tty
//! and reads the replies:
//!
//! - The Kitty graphics query, a 1×1 transparent image with id `N`. A
//!   terminal that supports the protocol answers `\x1b_Gi=N;OK`.
//! - DA1, `\x1b[c`. The reply lists the attributes, and `4` means Sixel.
//! - `\x1b[16t`, the pixel size of a character cell.
//! - CPR, `\x1b[6n`. Every VT terminal answers it, and it goes last, so its
//!   reply means the terminal has finished answering the others.
//!
//! The probe opens the tty itself, so it works with stdin and stdout
//! redirected, and puts the tty in raw mode so the replies arrive as bytes.
//! It stops at the CPR reply, which a local terminal sends in a few
//! milliseconds, and waits at most a second for it. A reply that came after
//! a shorter wait would reach the program as keys, as over a slow ssh link.
//! A terminal that does not answer in time counts as unsupported. Under a
//! multiplexer without passthrough the queries never reach the outer
//! terminal, and the probe reports no support. The result is cached, so the
//! probe runs once per process.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// The Kitty query, DA1, `CSI 16 t` and CPR, in the order of the module doc.
const QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b[6n";

/// The reply of a terminal that speaks Kitty to the query of id 31.
const KITTY_OK: &[u8] = b"\x1b_Gi=31;OK";

#[derive(Default, Copy, Clone)]
pub struct GraphicsCaps {
    pub kitty: bool,
    pub sixel: bool,
    /// Pixel size of one terminal cell, from `CSI 16 t`. `None` when the
    /// terminal did not answer.
    pub cell_px: Option<(u32, u32)>,
}

static CACHED: OnceLock<GraphicsCaps> = OnceLock::new();

/// Probe the terminal on the first call and return the cached result after.
pub fn graphics_caps() -> GraphicsCaps {
    *CACHED.get_or_init(probe)
}

#[cfg(unix)]
fn probe() -> GraphicsCaps {
    unix_impl::probe()
}

#[cfg(windows)]
fn probe() -> GraphicsCaps {
    windows_impl::probe()
}

#[cfg(not(any(unix, windows)))]
fn probe() -> GraphicsCaps {
    GraphicsCaps::default()
}

// -----------------------------------------------------------------------------
// The replies, shared by both platforms.
// -----------------------------------------------------------------------------

/// What the terminal answered so far.
#[derive(Default)]
struct Replies {
    parser: vte::Parser,
    /// Every byte, for the Kitty reply. vte drops an APC string unseen.
    bytes: Vec<u8>,
    found: Found,
}

impl Replies {
    /// Take `chunk`. Returns `true` if the terminal is done answering, at
    /// the CPR reply or once too many bytes came, `false` otherwise.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        self.bytes.extend_from_slice(chunk);
        self.parser.advance(&mut self.found, chunk);
        self.found.cpr || self.bytes.len() > 4096
    }

    fn caps(&self) -> GraphicsCaps {
        GraphicsCaps {
            kitty: self.bytes.windows(KITTY_OK.len()).any(|w| w == KITTY_OK),
            sixel: self.found.sixel,
            cell_px: self.found.cell_px,
        }
    }
}

/// Read the replies until the terminal is done or [`QUERY_TIMEOUT`] passes.
/// `read` waits at most the given time for input and reads it into the
/// buffer. It returns `None` when nothing came in time or the read failed.
fn read_replies(mut read: impl FnMut(Duration, &mut [u8]) -> Option<usize>) -> GraphicsCaps {
    let mut replies = Replies::default();
    let deadline = Instant::now() + QUERY_TIMEOUT;
    let mut chunk = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(bytes) = read(left, &mut chunk)
            .filter(|&n| n > 0)
            .map(|n| chunk.get(..n).expect("a read fills at most its buffer"))
        else {
            break;
        };
        if replies.feed(bytes) {
            break;
        }
    }
    replies.caps()
}

/// The CSI replies.
#[derive(Default)]
struct Found {
    sixel: bool,
    cell_px: Option<(u32, u32)>,
    cpr: bool,
}

impl vte::Perform for Found {
    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], _: bool, action: char) {
        let params: Vec<u16> = params
            .iter()
            .map(|p| *p.first().expect("vte gives every parameter a value"))
            .collect();
        match (intermediates, action, params.as_slice()) {
            // CPR, `CSI row ; col R`.
            ([], 'R', [_, _]) => self.cpr = true,
            // DA1, `CSI ? attr ; ... c`, where 4 means Sixel.
            ([b'?'], 'c', attrs) => self.sixel |= attrs.contains(&4),
            // `CSI 6 ; height ; width t`. A terminal that does not know
            // the size of a cell answers 0.
            ([], 't', &[6, h, w]) if h > 0 && w > 0 => {
                self.cell_px = Some((u32::from(w), u32::from(h)));
            }
            _ => {}
        }
    }
}

// -----------------------------------------------------------------------------
// Unix implementation (libc).
// -----------------------------------------------------------------------------

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::io::{IsTerminal, Write};
    use std::os::fd::{AsRawFd, RawFd};

    pub fn probe() -> GraphicsCaps {
        if !std::io::stdout().is_terminal() {
            return GraphicsCaps::default();
        }
        let Ok(mut tty) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        else {
            return GraphicsCaps::default();
        };
        let fd = tty.as_raw_fd();
        let Some(_raw) = TermiosGuard::raw(fd) else {
            return GraphicsCaps::default();
        };
        if tty
            .write_all(QUERY.as_bytes())
            .and_then(|()| tty.flush())
            .is_err()
        {
            return GraphicsCaps::default();
        }
        read_replies(|wait, buf| {
            let timeout_ms = wait.as_millis().min(i32::MAX as u128) as i32;
            if !poll_readable(fd, timeout_ms) {
                return None;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            usize::try_from(n).ok()
        })
    }

    /// Puts the tty in raw mode, so the replies arrive as bytes, and
    /// restores the saved mode on drop.
    struct TermiosGuard {
        fd: RawFd,
        saved: libc::termios,
    }

    impl TermiosGuard {
        fn raw(fd: RawFd) -> Option<Self> {
            let mut saved: libc::termios = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
                return None;
            }
            let mut raw = saved;
            unsafe { libc::cfmakeraw(&mut raw) };
            (unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } == 0).then_some(Self { fd, saved })
        }
    }

    impl Drop for TermiosGuard {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
        }
    }

    fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        r > 0 && (pfd.revents & libc::POLLIN) != 0
    }
}

// -----------------------------------------------------------------------------
// Windows implementation (windows-sys).
// -----------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode,
        SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// Restores the console mode on drop.
    struct ModeGuard {
        handle: HANDLE,
        saved: u32,
    }

    impl ModeGuard {
        fn new(handle: HANDLE) -> Option<(Self, u32)> {
            let mut mode: u32 = 0;
            let ok = unsafe { GetConsoleMode(handle, &mut mode) != 0 };
            ok.then_some((
                ModeGuard {
                    handle,
                    saved: mode,
                },
                mode,
            ))
        }
    }

    impl Drop for ModeGuard {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.handle, self.saved);
            }
        }
    }

    /// Closes the handle on drop.
    struct HandleGuard(HANDLE);
    impl Drop for HandleGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    fn open_console(name: &str, access: u32) -> Option<HandleGuard> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            None
        } else {
            Some(HandleGuard(h))
        }
    }

    pub fn probe() -> GraphicsCaps {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return GraphicsCaps::default();
        }

        let conin = match open_console("CONIN$", GENERIC_READ | GENERIC_WRITE) {
            Some(h) => h,
            None => return GraphicsCaps::default(),
        };
        let conout = match open_console("CONOUT$", GENERIC_READ | GENERIC_WRITE) {
            Some(h) => h,
            None => return GraphicsCaps::default(),
        };

        let (_in_guard, in_mode) = match ModeGuard::new(conin.0) {
            Some(g) => g,
            None => return GraphicsCaps::default(),
        };
        let (_out_guard, out_mode) = match ModeGuard::new(conout.0) {
            Some(g) => g,
            None => return GraphicsCaps::default(),
        };

        // The replies arrive as raw bytes only with VT input on and line,
        // echo and processing off. VT processing on the output forwards the
        // queries as written.
        let new_in = (in_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if unsafe { SetConsoleMode(conin.0, new_in) } == 0 {
            return GraphicsCaps::default();
        }
        let new_out = out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        if unsafe { SetConsoleMode(conout.0, new_out) } == 0 {
            return GraphicsCaps::default();
        }

        let bytes = QUERY.as_bytes();
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                conout.0,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return GraphicsCaps::default();
        }

        read_replies(|wait, buf| {
            let timeout_ms = wait.as_millis().min(u32::MAX as u128) as u32;
            if unsafe { WaitForSingleObject(conin.0, timeout_ms) } != WAIT_OBJECT_0 {
                return None;
            }
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    conin.0,
                    buf.as_mut_ptr().cast(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            (ok != 0).then_some(read as usize)
        })
    }
}

// -----------------------------------------------------------------------------
// Tests cover the replies. The I/O needs a real tty.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn replies(bytes: &[u8]) -> Replies {
        let mut r = Replies::default();
        r.feed(bytes);
        r
    }

    #[test]
    fn kitty_ok_in_buffer() {
        assert!(replies(b"junk\x1b_Gi=31;OK\x1b\\more").caps().kitty);
    }

    #[test]
    fn kitty_enotsupported_is_not_ok() {
        let r = replies(b"\x1b_Gi=31;ENOTSUPPORTED:no graphics\x1b\\");
        assert!(!r.caps().kitty);
    }

    #[test]
    fn kitty_wrong_id_is_not_ok() {
        assert!(!replies(b"\x1b_Gi=99;OK\x1b\\").caps().kitty);
    }

    #[test]
    fn da1_with_sixel_attr_detected() {
        assert!(replies(b"\x1b[?62;1;4;6;9;15;22c").caps().sixel);
    }

    #[test]
    fn da1_without_sixel_attr() {
        assert!(!replies(b"\x1b[?62;1;6;9;15c").caps().sixel);
    }

    #[test]
    fn da1_must_match_token_not_substring() {
        // Attribute 14 (NRCS) must not match 4.
        assert!(!replies(b"\x1b[?62;14;22c").caps().sixel);
    }

    #[test]
    fn cpr_ends_the_replies() {
        assert!(Replies::default().feed(b"\x1b[12;34R"));
    }

    #[test]
    fn cpr_not_in_other_csi() {
        assert!(!Replies::default().feed(b"\x1b[?62;1;4c"));
    }

    #[test]
    fn a_reply_split_across_chunks_parses() {
        let mut r = Replies::default();
        assert!(!r.feed(b"\x1b[6;2"));
        assert!(!r.feed(b"8;14t\x1b[12;"));
        assert!(r.feed(b"34R"));
        assert_eq!(r.caps().cell_px, Some((14, 28)));
    }

    #[test]
    fn cell_pixels_parses_csi16t_response() {
        // Ghostty and xterm reply ESC [ 6 ; height ; width t.
        let r = replies(b"junk\x1b[6;28;14tmore");
        assert_eq!(r.caps().cell_px, Some((14, 28)));
    }

    #[test]
    fn cell_pixels_returns_none_when_absent() {
        let r = replies(b"\x1b[?62;1;4c\x1b[12;34R");
        assert_eq!(r.caps().cell_px, None);
    }

    #[test]
    fn a_cpr_on_row_six_is_not_a_cell_size() {
        let r = replies(b"\x1b[6;12R");
        assert!(r.found.cpr);
        assert_eq!(r.caps().cell_px, None);
    }

    #[test]
    fn cell_pixels_skips_zero_dimensions() {
        // A terminal that does not know the cell size replies 0.
        assert_eq!(replies(b"\x1b[6;0;0t").caps().cell_px, None);
    }
}
