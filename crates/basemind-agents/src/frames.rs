//! Newline-delimited frames read from a harness with a size cap, so a child that never ends a
//! line cannot make the launcher buffer without bound.

use std::io::{self, Read};

/// The largest frame accepted from a harness. Tool and history snapshots can be large, but
/// nothing a harness legitimately sends in one line comes near this.
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

const CHUNK_BYTES: usize = 64 * 1024;

/// The most JSON values a frame may hold, counted as the `[`, `{` and `,` outside strings.
/// Parsed, each value takes tens of bytes (more with `preserve_order`), so a 16 MiB line of
/// `[0,0,0,...]` would grow to hundreds of MiB; this keeps a parsed frame to tens of MiB.
/// Real frames hold a few thousand at most.
pub(crate) const MAX_FRAME_VALUES: usize = 256 * 1024;

/// Whether `frame` holds more than [`MAX_FRAME_VALUES`] JSON values, found without parsing.
fn too_many_values(frame: &[u8]) -> bool {
    let (mut values, mut in_string, mut escaped) = (0_usize, false, false);
    for &byte in frame {
        if in_string {
            match (escaped, byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
        } else {
            match byte {
                b'"' => in_string = true,
                b'[' | b'{' | b',' => values += 1,
                _ => {}
            }
            if values > MAX_FRAME_VALUES {
                return true;
            }
        }
    }
    false
}

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
                return Self::bounded(frame).map(Some);
            }
            self.scanned = self.buf.len();
            self.check(self.buf.len())?;
            if self.eof {
                self.scanned = 0;
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return Self::bounded(std::mem::take(&mut self.buf)).map(Some);
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

    /// `frame`, or an [`io::ErrorKind::InvalidData`] error when it holds too many values.
    fn bounded(frame: Vec<u8>) -> io::Result<Vec<u8>> {
        if too_many_values(&frame) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("the harness sent a frame of more than {MAX_FRAME_VALUES} JSON values"),
            ));
        }
        Ok(frame)
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
    fn a_frame_of_too_many_values_is_refused_before_it_is_parsed() {
        let at_cap = format!("[{}0]\n", "0,".repeat(MAX_FRAME_VALUES - 1));
        assert!(frames(at_cap.as_bytes(), MAX_FRAME_BYTES)[0].is_ok());
        let over = format!("[{}0]\n", "0,".repeat(MAX_FRAME_VALUES));
        let error = frames(over.as_bytes(), MAX_FRAME_BYTES)[0].as_ref().unwrap_err().kind();
        assert_eq!(error, io::ErrorKind::InvalidData);
        // Commas, brackets and escaped quotes inside a string are text, not values.
        let text = format!("{{\"text\":\"\\\"{}\"}}\n", ",[{".repeat(MAX_FRAME_VALUES));
        assert!(frames(text.as_bytes(), MAX_FRAME_BYTES)[0].is_ok());
        // Without its newline, at the end of the stream, too.
        let error = frames(over.trim_end().as_bytes(), MAX_FRAME_BYTES)[0]
            .as_ref()
            .unwrap_err()
            .kind();
        assert_eq!(error, io::ErrorKind::InvalidData);
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
