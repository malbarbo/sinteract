//! POSIX shared memory objects, which carry the pixels of a Kitty image to
//! a terminal on the same machine (the transmission medium `t=s`). The
//! terminal reads an object without a decode and then unlinks it, so an
//! object that no terminal reads stays until [`unlink`].

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Create the object `name` with `len` bytes, and let `fill` write them.
/// Fails when `name` exists. A failure to size or map the new object
/// removes it.
pub fn create(name: &CStr, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
    let fd = unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let filled = Mapping::new(&fd, len).map(|mut mapping| fill(mapping.bytes()));
    if filled.is_err() {
        unlink(name);
    }
    filled
}

/// Remove the object `name`. The terminal removes an object after the
/// read, so an object that is already gone is no error.
pub fn unlink(name: &CStr) {
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

/// The bytes of an object, in the memory of the process until the drop.
struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Mapping {
    /// Size the object of `fd` to `len` bytes and map them.
    fn new(fd: &OwnedFd, len: usize) -> io::Result<Self> {
        let size = libc::off_t::try_from(len).map_err(|_| io::ErrorKind::InvalidInput)?;
        if unsafe { libc::ftruncate(fd.as_raw_fd(), size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr, len })
    }

    fn bytes(&mut self) -> &mut [u8] {
        // The mapping holds `len` bytes, writable, until the drop.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.cast(), self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// Linux lists the objects under `/dev/shm`, where a test reads them back.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::CString;

    fn name(test: &str) -> CString {
        CString::new(format!("/sinteract-test-{:x}-{test}", std::process::id())).unwrap()
    }

    fn read(name: &CStr) -> Vec<u8> {
        std::fs::read(format!("/dev/shm{}", name.to_str().unwrap())).unwrap()
    }

    #[test]
    fn an_object_holds_what_fill_wrote() {
        let name = name("fill");
        create(&name, 4, |bytes| bytes.copy_from_slice(&[1, 2, 3, 4])).unwrap();
        let bytes = read(&name);
        unlink(&name);
        assert_eq!(bytes, [1, 2, 3, 4]);
    }

    #[test]
    fn a_name_in_use_fails_until_the_unlink() {
        let name = name("twice");
        create(&name, 1, |_| {}).unwrap();
        let taken = create(&name, 1, |_| {}).unwrap_err();
        assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
        unlink(&name);
        create(&name, 1, |_| {}).unwrap();
        unlink(&name);
    }
}
