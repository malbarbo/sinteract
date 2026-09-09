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
//! It waits at most 150 ms. A local terminal answers in a few milliseconds
//! and ssh in well under 100 ms, and a terminal that does not answer in time
//! counts as unsupported. Under a multiplexer without passthrough the
//! queries never reach the outer terminal, and the probe reports no support.
//! The result is cached, so the probe runs once per process.

use std::sync::OnceLock;
use std::time::Duration;

const QUERY_TIMEOUT: Duration = Duration::from_millis(150);
const KITTY_QUERY_ID: &str = "31";

fn build_query() -> String {
    format!("\x1b_Gi={KITTY_QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b[6n")
}

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
// Parsers, shared by both platforms.
// -----------------------------------------------------------------------------

/// Returns `true` if `buf` holds a Cursor Position Report, ESC `[` digits
/// `;` digits `R`, `false` otherwise.
fn has_cpr_response(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' {
            let mut j = i + 2;
            while j < buf.len() {
                let b = buf[j];
                if b == b'R' {
                    return true;
                }
                // A final byte (0x40..=0x7E) other than R ends another sequence.
                if !(b.is_ascii_digit() || b == b';' || b == b':') && (0x40..=0x7E).contains(&b) {
                    break;
                }
                j += 1;
            }
        }
        i += 1;
    }
    false
}

/// Returns `true` if `buf` holds `\x1b_Gi=<id>;OK`, `false` otherwise.
fn parse_kitty_ok(buf: &[u8]) -> bool {
    let needle = format!("\x1b_Gi={KITTY_QUERY_ID};OK");
    buf.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Parse the `CSI 16 t` reply, `\x1b[6;<height>;<width>t`, into
/// `(width, height)` pixels per cell.
fn parse_cell_pixels(buf: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'6' && buf[i + 3] == b';' {
            let mut j = i + 4;
            while j < buf.len() && buf[j] != b't' {
                j += 1;
            }
            if j < buf.len() {
                let body = &buf[i + 4..j];
                let parts: Vec<&[u8]> = body.split(|&b| b == b';').collect();
                if parts.len() == 2
                    && let (Ok(hs), Ok(ws)) =
                        (std::str::from_utf8(parts[0]), std::str::from_utf8(parts[1]))
                    && let (Ok(h), Ok(w)) = (hs.trim().parse::<u32>(), ws.trim().parse::<u32>())
                    && w > 0
                    && h > 0
                {
                    return Some((w, h));
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Returns `true` if the DA1 reply `\x1b[?<list>c` lists `4`, the Sixel
/// attribute, `false` otherwise.
fn parse_da1_has_sixel(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'?' {
            let mut j = i + 3;
            while j < buf.len() && buf[j] != b'c' {
                j += 1;
            }
            if j < buf.len() {
                let params = &buf[i + 3..j];
                if params
                    .split(|&b| b == b';')
                    .any(|tok| std::str::from_utf8(tok).is_ok_and(|s| s.trim() == "4"))
                {
                    return true;
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    false
}

// -----------------------------------------------------------------------------
// Unix implementation (libc).
// -----------------------------------------------------------------------------

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::io::Write;
    use std::os::fd::{AsRawFd, RawFd};
    use std::time::Instant;

    pub fn probe() -> GraphicsCaps {
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() {
            return GraphicsCaps::default();
        }
        let tty = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
        {
            Ok(f) => f,
            Err(_) => return GraphicsCaps::default(),
        };
        let fd = tty.as_raw_fd();

        let saved = match get_termios(fd) {
            Some(t) => t,
            None => return GraphicsCaps::default(),
        };
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if !set_termios(fd, &raw) {
            return GraphicsCaps::default();
        }

        let result = probe_with_raw(&tty, fd);

        // Nothing to do if the restore fails.
        set_termios(fd, &saved);

        result
    }

    fn probe_with_raw(tty: &std::fs::File, fd: RawFd) -> GraphicsCaps {
        let query = build_query();
        let mut writer = match tty.try_clone() {
            Ok(w) => w,
            Err(_) => return GraphicsCaps::default(),
        };
        if writer.write_all(query.as_bytes()).is_err() || writer.flush().is_err() {
            return GraphicsCaps::default();
        }

        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let deadline = Instant::now() + QUERY_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining_ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
            if !poll_readable(fd, remaining_ms) {
                break;
            }
            let mut chunk = [0u8; 256];
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if has_cpr_response(&buf) || buf.len() > 4096 {
                break;
            }
        }

        GraphicsCaps {
            kitty: parse_kitty_ok(&buf),
            sixel: parse_da1_has_sixel(&buf),
            cell_px: parse_cell_pixels(&buf),
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

    fn get_termios(fd: RawFd) -> Option<libc::termios> {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::tcgetattr(fd, &mut t) };
        (r == 0).then_some(t)
    }

    fn set_termios(fd: RawFd, t: &libc::termios) -> bool {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) == 0 }
    }
}

// -----------------------------------------------------------------------------
// Windows implementation (windows-sys).
// -----------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use std::time::Instant;
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

        let query = build_query();
        let bytes = query.as_bytes();
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

        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let deadline = Instant::now() + QUERY_TIMEOUT;
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining_ms = (deadline - now).as_millis().min(u32::MAX as u128) as u32;
            let wait = unsafe { WaitForSingleObject(conin.0, remaining_ms) };
            if wait != WAIT_OBJECT_0 {
                break;
            }
            let mut chunk = [0u8; 256];
            let mut read: u32 = 0;
            let r = unsafe {
                ReadFile(
                    conin.0,
                    chunk.as_mut_ptr() as *mut _,
                    chunk.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if r == 0 || read == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..read as usize]);
            if has_cpr_response(&buf) || buf.len() > 4096 {
                break;
            }
        }

        GraphicsCaps {
            kitty: parse_kitty_ok(&buf),
            sixel: parse_da1_has_sixel(&buf),
            cell_px: parse_cell_pixels(&buf),
        }
    }
}

// -----------------------------------------------------------------------------
// Tests cover the parsers. The I/O needs a real tty.
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_ok_in_buffer() {
        let buf = b"junk\x1b_Gi=31;OK\x1b\\more";
        assert!(parse_kitty_ok(buf));
    }

    #[test]
    fn kitty_enotsupported_is_not_ok() {
        let buf = b"\x1b_Gi=31;ENOTSUPPORTED:no graphics\x1b\\";
        assert!(!parse_kitty_ok(buf));
    }

    #[test]
    fn kitty_wrong_id_is_not_ok() {
        let buf = b"\x1b_Gi=99;OK\x1b\\";
        assert!(!parse_kitty_ok(buf));
    }

    #[test]
    fn da1_with_sixel_attr_detected() {
        let buf = b"\x1b[?62;1;4;6;9;15;22c";
        assert!(parse_da1_has_sixel(buf));
    }

    #[test]
    fn da1_without_sixel_attr() {
        let buf = b"\x1b[?62;1;6;9;15c";
        assert!(!parse_da1_has_sixel(buf));
    }

    #[test]
    fn da1_must_match_token_not_substring() {
        // Attribute 14 (NRCS) must not match 4.
        let buf = b"\x1b[?62;14;22c";
        assert!(!parse_da1_has_sixel(buf));
    }

    #[test]
    fn cpr_response_recognized() {
        assert!(has_cpr_response(b"\x1b[12;34R"));
    }

    #[test]
    fn cpr_not_in_other_csi() {
        assert!(!has_cpr_response(b"\x1b[?62;1;4c"));
    }

    #[test]
    fn cell_pixels_parses_csi16t_response() {
        // Ghostty and xterm reply ESC [ 6 ; height ; width t.
        let buf = b"junk\x1b[6;28;14tmore";
        assert_eq!(parse_cell_pixels(buf), Some((14, 28)));
    }

    #[test]
    fn cell_pixels_returns_none_when_absent() {
        let buf = b"\x1b[?62;1;4c\x1b[12;34R";
        assert_eq!(parse_cell_pixels(buf), None);
    }

    #[test]
    fn cell_pixels_skips_zero_dimensions() {
        // A terminal that does not know the cell size replies 0.
        let buf = b"\x1b[6;0;0t";
        assert_eq!(parse_cell_pixels(buf), None);
    }
}
