//! Pipe I/O a session can walk away from.
//!
//! A descendant that left the harness's process group can keep the harness's pipes open, so
//! a read never sees end-of-file and a write into a full pipe never returns. On Unix every
//! read and write here waits through `poll` in short slices and gives up once the session's
//! abandon flag is set, so no thread stays blocked on a pipe after the session is over. On
//! Windows the calls block as usual.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// How long one wait for a pipe lasts before the abandon flag is looked at again.
#[cfg(unix)]
const SLICE_MS: libc::c_int = 50;

fn abandoned() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "the session abandoned the pipe")
}

/// Waits until `fd` is ready for `events`, or the flag is set (`false`).
#[cfg(unix)]
fn ready(fd: std::os::fd::RawFd, events: libc::c_short, abandon: &AtomicBool) -> io::Result<bool> {
    loop {
        if abandon.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut poll = libc::pollfd { fd, events, revents: 0 };
        // SAFETY: `poll` is a valid pollfd for the duration of the call, and the count is 1.
        let status = unsafe { libc::poll(&mut poll, 1, SLICE_MS) };
        match status {
            0 => {}
            status if status > 0 => return Ok(true),
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

/// A pipe end read from the harness, given up when the flag is set.
pub(crate) struct Reader<R> {
    inner: R,
    abandon: Arc<AtomicBool>,
}

impl<R> Reader<R> {
    pub fn new(inner: R, abandon: Arc<AtomicBool>) -> Self {
        Self { inner, abandon }
    }
}

#[cfg(unix)]
impl<R: Read + std::os::fd::AsRawFd> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Readable means data or end-of-file, so the read below does not block.
        if !ready(self.inner.as_raw_fd(), libc::POLLIN, &self.abandon)? {
            return Err(abandoned());
        }
        self.inner.read(buf)
    }
}

#[cfg(not(unix))]
impl<R: Read> Read for Reader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.abandon.load(Ordering::SeqCst) {
            return Err(abandoned());
        }
        self.inner.read(buf)
    }
}

/// A pipe end written to the harness, given up when the flag is set. On Unix the descriptor
/// is switched to non-blocking, so a write into a full pipe returns and is retried after a
/// wait instead of blocking.
pub(crate) struct Writer<W> {
    inner: W,
    abandon: Arc<AtomicBool>,
}

#[cfg(unix)]
impl<W: Write + std::os::fd::AsRawFd> Writer<W> {
    pub fn new(inner: W, abandon: Arc<AtomicBool>) -> io::Result<Self> {
        let fd = inner.as_raw_fd();
        // SAFETY: fcntl on a descriptor this process owns, with plain integer arguments.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { inner, abandon })
    }
}

#[cfg(not(unix))]
impl<W: Write> Writer<W> {
    pub fn new(inner: W, abandon: Arc<AtomicBool>) -> io::Result<Self> {
        Ok(Self { inner, abandon })
    }
}

#[cfg(unix)]
impl<W: Write + std::os::fd::AsRawFd> Write for Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.inner.write(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if !ready(self.inner.as_raw_fd(), libc::POLLOUT, &self.abandon)? {
                        return Err(abandoned());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(not(unix))]
impl<W: Write> Write for Writer<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.abandon.load(Ordering::SeqCst) {
            return Err(abandoned());
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs::File;
    use std::os::fd::FromRawFd;
    use std::time::{Duration, Instant};

    use super::*;

    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors were just created and are owned by nothing else.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
    }

    #[test]
    fn a_read_from_a_pipe_held_open_returns_once_abandoned() {
        let (read_end, _write_end) = pipe();
        let abandon = Arc::new(AtomicBool::new(false));
        let mut reader = Reader::new(read_end, abandon.clone());
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(reader.read(&mut [0; 16]).map_err(|error| error.kind()));
        });
        assert!(finished.recv_timeout(Duration::from_millis(200)).is_err());
        abandon.store(true, Ordering::SeqCst);
        let result = finished
            .recv_timeout(Duration::from_secs(2))
            .expect("the read never returned");
        assert_eq!(result, Err(io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn data_and_end_of_file_still_arrive() {
        let (read_end, mut write_end) = pipe();
        let mut reader = Reader::new(read_end, Arc::default());
        write_end.write_all(b"frame\n").unwrap();
        drop(write_end);
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        assert_eq!(text, "frame\n");
    }

    #[test]
    fn a_write_into_a_full_pipe_returns_once_abandoned() {
        let (_read_end, write_end) = pipe();
        let abandon = Arc::new(AtomicBool::new(false));
        let mut writer = Writer::new(write_end, abandon.clone()).unwrap();
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(
                writer
                    .write_all(&vec![b'x'; 4 * 1024 * 1024])
                    .map_err(|error| error.kind()),
            );
        });
        assert!(finished.recv_timeout(Duration::from_millis(200)).is_err());
        let started = Instant::now();
        abandon.store(true, Ordering::SeqCst);
        let result = finished
            .recv_timeout(Duration::from_secs(2))
            .expect("the write never returned");
        assert_eq!(result, Err(io::ErrorKind::BrokenPipe));
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
