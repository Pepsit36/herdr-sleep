//! Terminal handling, straight on `libc`. All the `unsafe` of the crate lives
//! here.

use std::io;
use std::mem::MaybeUninit;

const STDIN: libc::c_int = 0;
const SILENCE_MS: libc::c_int = 250;
const CAP: usize = 64 * 1024;

/// Raw mode on standard input; the previous settings come back on drop.
pub struct RawMode {
    saved: libc::termios,
}

impl RawMode {
    /// `None` when standard input is not a terminal.
    pub fn enter() -> Option<RawMode> {
        // SAFETY: plain libc calls on fd 0; `termios` is fully written by a
        // successful `tcgetattr` before it is read.
        unsafe {
            if libc::isatty(STDIN) != 1 {
                return None;
            }
            let mut term = MaybeUninit::<libc::termios>::uninit();
            if libc::tcgetattr(STDIN, term.as_mut_ptr()) != 0 {
                return None;
            }
            let saved = term.assume_init();
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            if libc::tcsetattr(STDIN, libc::TCSANOW, &raw) != 0 {
                return None;
            }
            Some(RawMode { saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restores settings read by `tcgetattr` earlier.
        unsafe {
            libc::tcsetattr(STDIN, libc::TCSANOW, &self.saved);
        }
    }
}

/// Waits until standard input is readable (or `timeout_ms` passes; negative
/// waits forever).
fn poll_in(timeout_ms: libc::c_int) -> io::Result<bool> {
    loop {
        let mut fd = libc::pollfd {
            fd: STDIN,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `fd` is a valid pollfd and the count is 1.
        let n = unsafe { libc::poll(&mut fd, 1, timeout_ms) };
        if n >= 0 {
            return Ok(n > 0);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Appends one chunk to `data`; `false` on end of input or error.
fn read_chunk(data: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: `buf` is valid for `buf.len()` bytes.
        let n = unsafe { libc::read(STDIN, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            data.extend_from_slice(&buf[..n as usize]);
            return true;
        }
        if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Blocks until any byte arrives, then keeps reading until the input has been
/// silent for 250 ms (or 64 KiB were read) and returns everything read.
///
/// Without a terminal there is nothing to wait for: the process sleeps until
/// a signal ends it.
pub fn capture(is_tty: bool) -> Vec<u8> {
    if !is_tty {
        loop {
            // SAFETY: `pause` has no preconditions.
            unsafe { libc::pause() };
        }
    }
    let mut data = Vec::new();
    loop {
        match poll_in(-1) {
            Ok(true) => break,
            Ok(false) => {}
            Err(_) => return data,
        }
    }
    if !read_chunk(&mut data) {
        return data;
    }
    while data.len() < CAP {
        match poll_in(SILENCE_MS) {
            Ok(true) if read_chunk(&mut data) => {}
            _ => break,
        }
    }
    data
}
