//! Keep a container up until it is told to stop.

use std::io;
use std::mem::MaybeUninit;

/// Wait for `SIGTERM` (or `SIGINT`) and return.
///
/// The signals are blocked and waited for, not handled, so this works as a
/// container's PID 1, which the kernel gives no default action for them.
pub fn run() -> io::Result<()> {
    // SAFETY: `set` is initialised by `sigemptyset` before anything reads it,
    // and every call below is given pointers to live locals.
    unsafe {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
        libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
        let set = set.assume_init();
        if libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, std::ptr::null_mut()) != 0 {
            return Err(io::Error::other("cannot block signals"));
        }
        loop {
            let mut got = 0;
            match libc::sigwait(&raw const set, &raw mut got) {
                0 => return Ok(()),
                libc::EINTR => {}
                code => return Err(io::Error::from_raw_os_error(code)),
            }
        }
    }
}
