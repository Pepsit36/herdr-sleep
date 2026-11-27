//! Terminal handling, straight on `libc`. All the `unsafe` of the crate lives
//! here.

use std::io;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering};

const STDIN: libc::c_int = 0;
const SILENCE_MS: libc::c_int = 250;
const CAP: usize = 64 * 1024;
/// How often a wait without a deadline looks at [`STOPPED`].
const STOP_CHECK_MS: libc::c_int = 500;

/// Set by the handler of SIGTERM, SIGINT and SIGHUP. The waits notice it, so the
/// Placeholder ends through its normal path: the terminal settings come back and
/// the Sleeping agent is released.
static STOPPED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_signal: libc::c_int) {
    STOPPED.store(true, Ordering::Relaxed);
}

/// Routes the signals that end the Placeholder to [`STOPPED`], without
/// `SA_RESTART` so a blocked wait is interrupted.
pub fn catch_stop_signals() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: `action` is fully initialised and the handler only stores
        // to an atomic, which is async-signal-safe.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

fn stopped() -> bool {
    STOPPED.load(Ordering::Relaxed)
}

/// Whether standard input is a terminal.
pub fn is_tty() -> bool {
    // SAFETY: plain libc call on fd 0.
    unsafe { libc::isatty(STDIN) == 1 }
}

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
/// waits until a stop signal). A stop signal is an `Interrupted` error.
fn poll_in(timeout_ms: libc::c_int) -> io::Result<bool> {
    loop {
        if stopped() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut fd = libc::pollfd {
            fd: STDIN,
            events: libc::POLLIN,
            revents: 0,
        };
        let slice = if timeout_ms < 0 {
            STOP_CHECK_MS
        } else {
            timeout_ms
        };
        // SAFETY: `fd` is a valid pollfd and the count is 1.
        let n = unsafe { libc::poll(&mut fd, 1, slice) };
        if n > 0 || (n == 0 && timeout_ms >= 0) {
            return Ok(n > 0);
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
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
/// a stop signal. `None` when the terminal went away (end of input or an
/// error before any key) or a stop signal came: nobody asked for a Wake.
pub fn capture(is_tty: bool) -> Option<Vec<u8>> {
    if !is_tty {
        while !stopped() {
            // SAFETY: no descriptors are polled; this only sleeps until a signal or the timeout.
            unsafe { libc::poll(std::ptr::null_mut(), 0, STOP_CHECK_MS) };
        }
        return None;
    }
    let mut data = Vec::new();
    loop {
        match poll_in(-1) {
            Ok(true) => break,
            Ok(false) => {}
            Err(_) => return None,
        }
    }
    if !read_chunk(&mut data) {
        return None;
    }
    while data.len() < CAP {
        match poll_in(SILENCE_MS) {
            Ok(true) if read_chunk(&mut data) => {}
            _ => break,
        }
    }
    Some(data)
}
