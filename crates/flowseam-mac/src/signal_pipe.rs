//! A self-pipe lets a signal handler wake ordinary Rust code without polling.

use std::io;
use std::os::fd::RawFd;

pub(crate) struct SignalPipe {
    read: RawFd,
    write: RawFd,
    #[cfg(test)]
    waits: std::sync::atomic::AtomicUsize,
}

impl SignalPipe {
    pub(crate) fn new() -> io::Result<Self> {
        let mut fds = [-1; 2];
        // SAFETY: `fds` has room for the read and write descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // Not inherited by the processes the worker starts.
        for fd in fds {
            // SAFETY: `fd` is one of the descriptors `pipe` just returned.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        // A signal must never block when a burst has already filled the pipe.
        if unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) } == -1 {
            let e = io::Error::last_os_error();
            // SAFETY: `pipe` just returned these descriptors.
            unsafe { libc::close(fds[0]); libc::close(fds[1]); }
            return Err(e);
        }
        Ok(Self { read: fds[0], write: fds[1], #[cfg(test)] waits: std::sync::atomic::AtomicUsize::new(0) })
    }

    pub(crate) fn write_fd(&self) -> RawFd {
        self.write
    }

    /// Block until the handler's byte arrives.
    pub(crate) fn wait(&self) -> io::Result<()> {
        #[cfg(test)]
        self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut byte = 0u8;
        loop {
            // SAFETY: `read` is this pipe's valid read descriptor and `byte` is writable.
            let n = unsafe { libc::read(self.read, (&mut byte as *mut u8).cast(), 1) };
            if n == 1 {
                return Ok(());
            }
            if n == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(io::Error::last_os_error());
        }
    }

    #[cfg(test)]
    pub(crate) fn notify(&self) {
        // SAFETY: the test uses the same one-byte write as the signal handler.
        unsafe { notify_fd(self.write) };
    }

    #[cfg(test)]
    pub(crate) fn wait_count(&self) -> usize {
        self.waits.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn ready_within(&self, millis: i32) -> bool {
        let mut fd = libc::pollfd { fd: self.read, events: libc::POLLIN, revents: 0 };
        // SAFETY: `fd` is a valid poll descriptor for this pipe.
        unsafe { libc::poll(&mut fd, 1, millis) == 1 }
    }
}

impl Drop for SignalPipe {
    fn drop(&mut self) {
        // SAFETY: these descriptors are owned by this pipe.
        unsafe { libc::close(self.read); libc::close(self.write); }
    }
}

/// Async-signal-safe after the caller has published a valid descriptor.
pub(crate) unsafe fn notify_fd(fd: RawFd) {
    if fd >= 0 {
        let byte = 1u8;
        // SAFETY: `write(2)` is async-signal-safe; a full nonblocking pipe only loses a redundant wake.
        unsafe { libc::write(fd, (&byte as *const u8).cast(), 1); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn waits_without_polling_then_wakes_from_handler_byte() {
        let pipe = SignalPipe::new().unwrap();
        let (tx, rx) = mpsc::channel();
        thread::scope(|s| {
            s.spawn(|| {
                pipe.wait().unwrap();
                tx.send(()).unwrap();
            });
            assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "the watcher woke without a signal");
            assert_eq!(pipe.waits.load(std::sync::atomic::Ordering::SeqCst), 1);
            pipe.notify();
            rx.recv_timeout(Duration::from_millis(100)).expect("the handler byte wakes the watcher");
        });
    }
}
