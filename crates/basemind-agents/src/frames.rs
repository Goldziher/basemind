//! Newline-delimited frames read from a harness with a size cap, so a child that never ends a
//! line cannot make the launcher buffer without bound.

use std::io::{self, Read};

/// The largest frame accepted from a harness. Tool and history snapshots can be large, but
/// nothing a harness legitimately sends in one line comes near this.
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const CHUNK_BYTES: usize = 64 * 1024;

/// Splits a byte stream into frames at `\n`, refusing any frame longer than its limit.
pub(crate) struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
    /// How much of `buf` is known to hold no newline.
    scanned: usize,
    limit: usize,
    eof: bool,
}

impl<R: Read> FrameReader<R> {
    /// A reader with the [`MAX_FRAME_BYTES`] limit.
    pub fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_FRAME_BYTES)
    }

    /// A reader that refuses frames longer than `limit` bytes.
    pub fn with_limit(inner: R, limit: usize) -> Self {
        Self {
            inner,
            buf: Vec::new(),
            scanned: 0,
            limit,
            eof: false,
        }
    }

    /// The next frame without its newline, `None` at the end of the stream. A last frame
    /// without a newline is returned as it is. A frame over the limit is an
    /// [`io::ErrorKind::InvalidData`] error, and the reader is not usable after it.
    pub fn next_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(at) = self.buf[self.scanned..].iter().position(|byte| *byte == b'\n') {
                let end = self.scanned + at;
                self.check(end)?;
                let mut frame: Vec<u8> = self.buf.drain(..=end).collect();
                frame.pop();
                self.scanned = 0;
                return Ok(Some(frame));
            }
            self.scanned = self.buf.len();
            self.check(self.buf.len())?;
            if self.eof {
                self.scanned = 0;
                return Ok((!self.buf.is_empty()).then(|| std::mem::take(&mut self.buf)));
            }
            let start = self.buf.len();
            self.buf.resize(start + CHUNK_BYTES, 0);
            let read = loop {
                match self.inner.read(&mut self.buf[start..]) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    other => break other,
                }
            };
            let read = match read {
                Ok(read) => read,
                Err(error) => {
                    self.buf.truncate(start);
                    return Err(error);
                }
            };
            self.buf.truncate(start + read);
            self.eof = read == 0;
        }
    }

    fn check(&self, len: usize) -> io::Result<()> {
        if len > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the harness sent a frame longer than {} bytes", self.limit),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(input: &[u8], limit: usize) -> Vec<io::Result<Option<Vec<u8>>>> {
        let mut reader = FrameReader::with_limit(input, limit);
        let mut out = Vec::new();
        loop {
            let next = reader.next_frame();
            let stop = !matches!(next, Ok(Some(_)));
            out.push(next);
            if stop {
                return out;
            }
        }
    }

    #[test]
    fn frames_split_at_newlines_and_a_last_partial_frame_is_kept() {
        let got: Vec<_> = frames(b"one\n\ntwo\nthree", 16)
            .into_iter()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            got,
            [
                Some(b"one".to_vec()),
                Some(Vec::new()),
                Some(b"two".to_vec()),
                Some(b"three".to_vec()),
                None
            ]
        );
    }

    #[test]
    fn a_frame_over_the_limit_is_an_error_with_or_without_its_newline() {
        for input in [&b"ok\n12345678901\nlater\n"[..], b"ok\n12345678901"] {
            let got = frames(input, 10);
            assert_eq!(got[0].as_ref().unwrap().as_deref(), Some(&b"ok"[..]));
            let error = got[1].as_ref().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{error}");
        }
        // Exactly at the limit is fine.
        assert_eq!(
            frames(b"1234567890\n", 10)[0].as_ref().unwrap().as_deref(),
            Some(&b"1234567890"[..])
        );
    }

    #[test]
    fn an_endless_line_stops_once_it_passes_the_limit() {
        // A reader that never ends a line: the frame reader must give up, not grow forever.
        let endless = std::io::repeat(b'a');
        let mut reader = FrameReader::with_limit(endless, 1024 * 1024);
        let error = reader.next_frame().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(reader.buf.len() <= 1024 * 1024 + CHUNK_BYTES);
    }
}
