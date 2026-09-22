//! Asks the terminal which graphics and which keyboard protocol it
//! supports, on Unix and Windows.
//!
//! The environment variables that name a terminal do not survive ssh and
//! multiplexers, so the probe writes five queries to stdout and reads the
//! replies from stdin:
//!
//! - The Kitty graphics query, a 1×1 transparent image with id `N`. A
//!   terminal that supports the protocol answers `\x1b_Gi=N;OK`.
//! - DA1, `\x1b[c`. The reply lists the attributes, and `4` means Sixel.
//! - `\x1b[16t`, the pixel size of a character cell.
//! - `\x1b[?u`, the flags of the Kitty keyboard protocol. A terminal that
//!   supports the protocol answers `\x1b[?flags u`.
//! - CPR, `\x1b[6n`. Every VT terminal answers it, and it goes last, so its
//!   reply means the terminal has finished answering the others.
//!
//! The probe needs a terminal on both stdin and stdout, and with either one
//! redirected it reports no support. It puts stdin in raw mode, so the
//! replies arrive as bytes.
//! It stops at the CPR reply, which a local terminal sends in a few
//! milliseconds, and waits at most a second for it. A reply that came after
//! a shorter wait would reach the program as keys, as over a slow ssh link.
//! A terminal that does not answer in time counts as unsupported. Under a
//! multiplexer without passthrough the queries never reach the outer
//! terminal, and the probe reports no support. The result is cached, so the
//! probe runs once per process.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use vtparse::CsiParam;

const QUERY_TIMEOUT: Duration = Duration::from_secs(1);

/// The Kitty query, DA1, `CSI 16 t`, `CSI ? u` and CPR, in the order of the
/// module doc.
const QUERY: &str = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b[?u\x1b[6n";

/// The APC string of a terminal that speaks Kitty, in reply to the query of
/// id 31.
const KITTY_OK: &[u8] = b"Gi=31;OK";

#[derive(Default, Copy, Clone)]
pub struct GraphicsCaps {
    pub kitty: bool,
    pub sixel: bool,
    /// Pixel size of one terminal cell, from `CSI 16 t`. `None` when the
    /// terminal did not answer.
    pub cell_px: Option<(u32, u32)>,
    /// The terminal speaks the keyboard protocol of Kitty, which reports
    /// when a key goes down and comes up.
    pub kitty_keyboard: bool,
}

static CACHED: OnceLock<GraphicsCaps> = OnceLock::new();

/// The bytes that the user types in the terminal, which crossterm does not
/// read. The caller opens it only when stdin is a terminal, and
/// [`discard_input`] drops what nobody read when the display leaves.
#[cfg(unix)]
pub(super) use unix_impl::{TtyInput, discard_input};
#[cfg(windows)]
pub(super) use windows_impl::{TtyInput, discard_input, set_vt_input};

/// Turn the Ctrl-C of the terminal on stdout back into SIGINT, which raw
/// mode turns off. The display calls it when stdin is not a terminal, where
/// nobody reads the keys. Leaving raw mode puts back the whole saved mode.
#[cfg(unix)]
pub(super) use unix_impl::signal_on_ctrl_c;

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
struct Replies {
    parser: vtparse::VTParser,
    /// How many bytes came, to stop a terminal that never sends the CPR.
    len: usize,
    found: Found,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            parser: vtparse::VTParser::new(),
            len: 0,
            found: Found::default(),
        }
    }
}

impl Replies {
    /// Take `chunk`. Returns `true` if the terminal is done answering, at
    /// the CPR reply or once too many bytes came, `false` otherwise.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        self.len += chunk.len();
        self.parser.parse(chunk, &mut self.found);
        self.found.cpr || self.len > 4096
    }

    fn caps(&self) -> GraphicsCaps {
        GraphicsCaps {
            kitty: self.found.kitty,
            sixel: self.found.sixel,
            cell_px: self.found.cell_px,
            kitty_keyboard: self.found.kitty_keyboard,
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

/// The replies that came.
#[derive(Default)]
struct Found {
    kitty: bool,
    sixel: bool,
    cell_px: Option<(u32, u32)>,
    kitty_keyboard: bool,
    cpr: bool,
}

impl vtparse::VTActor for Found {
    fn csi_dispatch(&mut self, params: &[CsiParam], _: bool, byte: u8) {
        let (marker, params) = match params {
            [CsiParam::P(b'?'), rest @ ..] => (Some(b'?'), rest),
            _ => (None, params),
        };
        // The first field of each parameter. An empty one is 0.
        let params: Vec<i64> = params
            .split(|p| *p == CsiParam::P(b';'))
            .map(|p| match p {
                [CsiParam::Integer(n), ..] => *n,
                _ => 0,
            })
            .collect();
        match (marker, byte, params.as_slice()) {
            // CPR, `CSI row ; col R`.
            (None, b'R', [_, _]) => self.cpr = true,
            // DA1, `CSI ? attr ; ... c`, where 4 means Sixel.
            (Some(b'?'), b'c', attrs) => self.sixel |= attrs.contains(&4),
            // `CSI 6 ; height ; width t`. A terminal that does not know
            // the size of a cell answers 0.
            (None, b't', &[6, h, w]) => {
                if let (Ok(w @ 1..), Ok(h @ 1..)) = (u32::try_from(w), u32::try_from(h)) {
                    self.cell_px = Some((w, h));
                }
            }
            // The flags of the keyboard protocol, `CSI ? flags u`.
            (Some(b'?'), b'u', [_]) => self.kitty_keyboard = true,
            _ => {}
        }
    }

    fn apc_dispatch(&mut self, data: Vec<u8>) {
        self.kitty |= data == KITTY_OK;
    }

    fn print(&mut self, _: char) {}

    fn execute_c0_or_c1(&mut self, _: u8) {}

    fn dcs_hook(&mut self, _: u8, _: &[i64], _: &[u8], _: bool) {}

    fn dcs_put(&mut self, _: u8) {}

    fn dcs_unhook(&mut self) {}

    fn esc_dispatch(&mut self, _: &[i64], _: &[u8], _: bool, _: u8) {}

    fn osc_dispatch(&mut self, _: &[&[u8]]) {}
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
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        if !stdin.is_terminal() || !stdout.is_terminal() {
            return GraphicsCaps::default();
        }
        let fd = stdin.as_raw_fd();
        let Some(_raw) = TermiosGuard::raw(fd) else {
            return GraphicsCaps::default();
        };
        if stdout
            .write_all(QUERY.as_bytes())
            .and_then(|()| stdout.flush())
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

    pub fn signal_on_ctrl_c() -> std::io::Result<()> {
        let fd = std::io::stdout().as_raw_fd();
        let mut mode: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut mode) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        mode.c_lflag |= libc::ISIG;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &mode) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Drop the bytes that stdin holds and nobody read, which would go to
    /// the shell.
    pub fn discard_input() {
        unsafe { libc::tcflush(std::io::stdin().as_raw_fd(), libc::TCIFLUSH) };
    }

    /// The terminal on stdin.
    pub struct TtyInput(RawFd);

    impl TtyInput {
        pub fn open() -> std::io::Result<Self> {
            Ok(Self(std::io::stdin().as_raw_fd()))
        }

        /// Append to `out` the bytes that arrive within `timeout`. The read
        /// goes past the buffer of [`std::io::Stdin`], which `poll` does not
        /// see. Fails with [`std::io::ErrorKind::UnexpectedEof`] when the
        /// terminal hangs up.
        pub fn read(&mut self, timeout: Duration, out: &mut Vec<u8>) -> std::io::Result<()> {
            let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            if !poll_readable(self.0, timeout_ms) {
                return Ok(());
            }
            let mut buf = [0u8; 1024];
            let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
            match usize::try_from(n) {
                Ok(0) => Err(std::io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    out.extend_from_slice(buf.get(..n).expect("read fills at most the buffer"));
                    Ok(())
                }
                Err(_) => match std::io::Error::last_os_error() {
                    e if e.kind() == std::io::ErrorKind::Interrupted => Ok(()),
                    e => Err(e),
                },
            }
        }
    }

    /// Returns `true` if a read of `fd` does not block after `timeout_ms`,
    /// `false` otherwise. A hangup counts, so that the read finds the end of
    /// the input.
    fn poll_readable(fd: RawFd, timeout_ms: i32) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        r > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR)) != 0
    }
}

// -----------------------------------------------------------------------------
// Windows implementation (windows-sys).
// -----------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, FOCUS_EVENT,
        FlushConsoleInputBuffer, GetConsoleMode, GetStdHandle, INPUT_RECORD, KEY_EVENT,
        ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    /// Turn the VT input of the console on stdin on or off. Under it the
    /// console turns each key into the bytes of a terminal, which the
    /// win32-input-mode of Windows Terminal needs.
    pub fn set_vt_input(on: bool) -> std::io::Result<()> {
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mode = if on {
            mode | ENABLE_VIRTUAL_TERMINAL_INPUT
        } else {
            mode & !ENABLE_VIRTUAL_TERMINAL_INPUT
        };
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Drop the records that the console on stdin holds and nobody read,
    /// which would go to the shell.
    pub fn discard_input() {
        unsafe { FlushConsoleInputBuffer(GetStdHandle(STD_INPUT_HANDLE)) };
    }

    /// The console on stdin, under VT input.
    pub struct TtyInput {
        handle: HANDLE,
        /// The first half of a surrogate pair whose second half has not
        /// come yet.
        high: Option<u16>,
    }

    impl TtyInput {
        pub fn open() -> std::io::Result<Self> {
            let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self { handle, high: None })
        }

        /// Append to `out` the bytes that arrive within `timeout`, in UTF-8.
        /// Under VT input each character comes in the record of a key down.
        /// The records of the input, and not the text of `ReadConsoleW`,
        /// give the reader the focus, and a read of text would wait on a
        /// console that holds only other records.
        pub fn read(&mut self, timeout: Duration, out: &mut Vec<u8>) -> std::io::Result<()> {
            let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            match unsafe { WaitForSingleObject(self.handle, timeout_ms) } {
                WAIT_OBJECT_0 => {}
                WAIT_TIMEOUT => return Ok(()),
                _ => return Err(std::io::Error::last_os_error()),
            }
            let mut records = [INPUT_RECORD::default(); 64];
            let mut n: u32 = 0;
            let ok = unsafe {
                ReadConsoleInputW(
                    self.handle,
                    records.as_mut_ptr(),
                    records.len() as u32,
                    &mut n,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let records = records
                .get(..n as usize)
                .expect("a read fills at most its buffer");
            let mut units: Vec<u16> = self.high.take().into_iter().collect();
            for record in records {
                match u32::from(record.EventType) {
                    KEY_EVENT => {
                        let key = unsafe { record.Event.KeyEvent };
                        let unit = unsafe { key.uChar.UnicodeChar };
                        if key.bKeyDown != 0 && unit != 0 {
                            units.push(unit);
                        }
                    }
                    // The console reports the focus in a record whether or
                    // not the terminal sends the escape of a focus loss, so
                    // the reader writes that escape.
                    FOCUS_EVENT if unsafe { record.Event.FocusEvent.bSetFocus } == 0 => {
                        units.extend("\x1b[O".encode_utf16());
                    }
                    _ => {}
                }
            }
            if units.last().is_some_and(|u| (0xd800..0xdc00).contains(u)) {
                self.high = units.pop();
            }
            for c in char::decode_utf16(units) {
                let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
                out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            Ok(())
        }
    }

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

    pub fn probe() -> GraphicsCaps {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return GraphicsCaps::default();
        }
        let conin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let conout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };

        let (_in_guard, in_mode) = match ModeGuard::new(conin) {
            Some(g) => g,
            None => return GraphicsCaps::default(),
        };
        let (_out_guard, out_mode) = match ModeGuard::new(conout) {
            Some(g) => g,
            None => return GraphicsCaps::default(),
        };

        // The replies arrive as raw bytes only with VT input on and line,
        // echo and processing off. VT processing on the output forwards the
        // queries as written.
        let new_in = (in_mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        if unsafe { SetConsoleMode(conin, new_in) } == 0 {
            return GraphicsCaps::default();
        }
        let new_out = out_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
        if unsafe { SetConsoleMode(conout, new_out) } == 0 {
            return GraphicsCaps::default();
        }

        let bytes = QUERY.as_bytes();
        let mut written: u32 = 0;
        let ok = unsafe {
            WriteFile(
                conout,
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
            if unsafe { WaitForSingleObject(conin, timeout_ms) } != WAIT_OBJECT_0 {
                return None;
            }
            let mut read: u32 = 0;
            let ok = unsafe {
                ReadFile(
                    conin,
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
    fn a_keyboard_flags_reply_means_the_kitty_keyboard() {
        assert!(replies(b"\x1b[?0u\x1b[12;34R").caps().kitty_keyboard);
        assert!(!replies(b"\x1b[?62;1;4c\x1b[12;34R").caps().kitty_keyboard);
    }

    #[test]
    fn cell_pixels_skips_zero_dimensions() {
        // A terminal that does not know the cell size replies 0.
        assert_eq!(replies(b"\x1b[6;0;0t").caps().cell_px, None);
    }
}
