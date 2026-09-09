//! Byte streams: the Rust side of the `read(n)` / `write(b)` protocol.
//!
//! The protocol is two methods and a naming convention, not a declared type
//! (`docs/stdlib-server-design.md` §2):
//!
//! ```text
//! read(n)   -> bytes    # 1..n bytes; b"" at EOF; may return fewer than n
//! write(b)  -> None     # writes all of b, or raises
//! ```
//!
//! Everything here is bytes in both directions; no stream in the language
//! accepts a `str`. Three design rules are load-bearing and are implemented
//! rather than merely documented:
//!
//! * **Every reader buffers**, because `read_until` needs a buffer and HTTP
//!   header parsing needs `read_until`. Buffering is not opt-in; there is no
//!   `BufReader` to forget to construct.
//! * **The buffer is allocated on first read, not at construction.** 8 KiB per
//!   stream is nothing for files and 80 MB across 10,000 idle connections. An
//!   idle stream should cost what an idle stream costs.
//! * **Writers are unbuffered, and there is no `flush()`** anywhere in the
//!   language — a `flush` you can forget is a truncated response with nothing
//!   raised. Batching is `parts.join(b"")` in the program, where it is visible.
//!
//! Oro code has no handle on the read buffer: no size to pass, no way to
//! observe whether it has been allocated. Exposing any of that would be
//! reintroducing `BufReader` under a new name.

use std::cell::{RefCell, RefMut};
use std::io::{Read, Write};

use crate::value::VResult;

/// The read-buffer size, and the largest single `read` served from the buffer.
/// One syscall per 8 KiB instead of one per byte.
const BUFSIZE: usize = 8 * 1024;

/// What a stream *is*, for `type()` and `repr()`. Immutable, and deliberately
/// outside the `RefCell` so a diagnostic can name the type while a method holds
/// the borrow.
pub enum StreamKind {
    /// A file descriptor: a path from `open()`, or one of the three standard
    /// streams (whose "path" is `<stdin>`/`<stdout>`/`<stderr>`, as CPython
    /// spells them).
    File { path: String, mode: &'static str },
    /// `io.buffer(b)` — bytes in memory, a Reader and a Writer at once. The
    /// only way to get a Reader you can feed literal bytes to, which is what
    /// lets a protocol be tested before a socket exists.
    Buffer,
}

impl StreamKind {
    pub fn type_name(&self) -> &'static str {
        match self {
            StreamKind::File { .. } => "File",
            StreamKind::Buffer => "Buffer",
        }
    }
}

/// Where a stream's octets come from and go to.
enum Backing {
    Read(std::fs::File),
    Write(std::fs::File),
    /// A `Buffer`'s bytes live in the read buffer itself: `write` appends to
    /// it, `read` consumes from the front. One `Vec`, one position.
    Mem,
    Stdin,
    Stdout,
    Stderr,
}

/// The mutable half of a stream.
struct Inner {
    back: Backing,
    /// The read buffer. Empty (and unallocated) until the first read; for a
    /// `Buffer` it is the stream's contents.
    buf: Vec<u8>,
    /// Consumed prefix of `buf`; `buf[pos..end]` is what a read serves next.
    pos: usize,
    /// Valid extent of `buf`. Not `buf.len()`: a fd-backed reader keeps `buf`
    /// sized at `BUFSIZE` so refilling never re-zeroes it.
    end: usize,
    closed: bool,
}

/// An open byte stream.
pub struct OroStream {
    pub kind: StreamKind,
    inner: RefCell<Inner>,
}

impl OroStream {
    fn new(kind: StreamKind, back: Backing) -> OroStream {
        OroStream {
            kind,
            inner: RefCell::new(Inner { back, buf: Vec::new(), pos: 0, end: 0, closed: false }),
        }
    }

    /// `open(path, "r")`.
    pub fn open_read(path: &str) -> std::io::Result<OroStream> {
        let f = std::fs::File::open(path)?;
        Ok(OroStream::new(
            StreamKind::File { path: path.to_string(), mode: "r" },
            Backing::Read(f),
        ))
    }

    /// `open(path, "w")` — truncating.
    pub fn open_write(path: &str) -> std::io::Result<OroStream> {
        let f = std::fs::File::create(path)?;
        Ok(OroStream::new(
            StreamKind::File { path: path.to_string(), mode: "w" },
            Backing::Write(f),
        ))
    }

    /// `open(path, "a")` — appending.
    pub fn open_append(path: &str) -> std::io::Result<OroStream> {
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(OroStream::new(
            StreamKind::File { path: path.to_string(), mode: "a" },
            Backing::Write(f),
        ))
    }

    /// `io.buffer(b)` — an in-memory Reader and Writer over `b`.
    pub fn buffer(b: Vec<u8>) -> OroStream {
        let end = b.len();
        OroStream {
            kind: StreamKind::Buffer,
            inner: RefCell::new(Inner { back: Backing::Mem, buf: b, pos: 0, end, closed: false }),
        }
    }

    /// One of the three standard streams. They are ordinary fd-backed `File`s;
    /// the only thing special about them is that the VM makes them for you.
    pub fn std_stream(which: &'static str) -> OroStream {
        let (back, mode) = match which {
            "<stdin>" => (Backing::Stdin, "r"),
            "<stderr>" => (Backing::Stderr, "w"),
            _ => (Backing::Stdout, "w"),
        };
        OroStream::new(StreamKind::File { path: which.to_string(), mode }, back)
    }

    /// `<File 'x.bin' mode 'r'>` / `<Buffer 12 bytes>`. CPython spells these
    /// `<_io.BufferedReader …>`; naming Oro's own type is more use than
    /// mirroring a wrapper Oro does not have.
    pub fn repr(&self) -> String {
        match &self.kind {
            StreamKind::File { path, mode } => format!("<File '{path}' mode '{mode}'>"),
            StreamKind::Buffer => {
                let inner = self.inner.borrow();
                format!("<Buffer {} bytes>", inner.end - inner.pos)
            }
        }
    }

    fn borrow_open(&self, who: &str) -> VResult<RefMut<'_, Inner>> {
        let inner = self.inner.borrow_mut();
        if inner.closed {
            return Err(format!("{who}() on a closed {}", self.kind.type_name()));
        }
        Ok(inner)
    }

    /// Like [`borrow_open`](Self::borrow_open), and refuses a stream that has
    /// no read side at all — reading a writer is a mistake, not an EOF.
    fn borrow_readable(&self, who: &str) -> VResult<RefMut<'_, Inner>> {
        let inner = self.borrow_open(who)?;
        if !inner.can_read() {
            return Err(format!("{who}() on a stream open for writing (mode 'w')"));
        }
        Ok(inner)
    }

    /// `read(n)`: between 1 and `n` bytes, or `b""` at EOF. A short read is not
    /// an error and does not mean EOF — it means "this is what has arrived".
    /// Code that needs exactly `n` bytes calls `io.read(r, n)`.
    pub fn read(&self, n: i64) -> VResult<Vec<u8>> {
        // `read(0)` would return b"" and look like EOF, so it is a ValueError
        // rather than a second thing b"" can mean.
        if n < 1 {
            return Err(format!("read() size must be at least 1, not {n}"));
        }
        let n = n as usize;
        let mut inner = self.borrow_readable("read")?;
        // A read larger than the buffer, with the buffer empty, goes straight
        // to its own allocation: buffering it would only copy it twice.
        if inner.refills() && inner.pos == inner.end && n >= BUFSIZE {
            let mut out = vec![0u8; n];
            let got = inner.read_source(&mut out)?;
            out.truncate(got);
            return Ok(out);
        }
        let avail = inner.fill()?;
        let k = avail.min(n);
        let out = inner.buf[inner.pos..inner.pos + k].to_vec();
        inner.pos += k;
        Ok(out)
    }

    /// `write(b)`: all of `b`, or an error. No count is returned — under green
    /// threads a short write is the runtime's problem, not the caller's, so
    /// there is no branch here for every call site to get wrong.
    pub fn write(&self, b: &[u8]) -> VResult<()> {
        let mut inner = self.borrow_open("write")?;
        inner.write_source(b)
    }

    /// `read_until(delim, limit)`: bytes up to and including `delim`.
    ///
    /// A method rather than a free function because it must see inside the read
    /// buffer — written over `read(n)` it would either read a byte at a time or
    /// over-read past the delimiter with nowhere to put the excess.
    ///
    /// Raises `ValueError` if `limit` bytes arrive without the delimiter, which
    /// is what stops a client sending an unbounded header block. At EOF it
    /// returns what it has, delimiter or not — the same rule `read` follows,
    /// where the end of a stream is not a fault.
    pub fn read_until(&self, delim: &[u8], limit: i64) -> VResult<Vec<u8>> {
        if delim.is_empty() {
            return Err("read_until() delimiter must not be empty".to_string());
        }
        if limit < 1 {
            return Err(format!("read_until() limit must be at least 1, not {limit}"));
        }
        let limit = limit as usize;
        let mut inner = self.borrow_readable("read_until")?;
        let mut out: Vec<u8> = Vec::new();
        loop {
            // Only the tail of what is already collected can start a delimiter
            // that the next chunk completes, so the scan never restarts.
            let scan_from = out.len().saturating_sub(delim.len() - 1);
            if inner.fill()? == 0 {
                return Ok(out);
            }
            let prev = out.len();
            out.extend_from_slice(&inner.buf[inner.pos..inner.end]);
            match find(&out[scan_from..], delim) {
                Some(i) => {
                    let take = scan_from + i + delim.len();
                    if take > limit {
                        return Err(format!(
                            "read_until() found no delimiter in the first {limit} bytes"
                        ));
                    }
                    // Leave everything after the delimiter in the buffer; it
                    // belongs to the next read.
                    inner.pos += take - prev;
                    out.truncate(take);
                    return Ok(out);
                }
                None => {
                    inner.pos = inner.end;
                    if out.len() >= limit {
                        return Err(format!(
                            "read_until() found no delimiter in the first {limit} bytes"
                        ));
                    }
                }
            }
        }
    }

    /// `bytes()` on a `Buffer`: everything written but not yet read.
    ///
    /// One rule covering both uses: a Buffer used only as a Writer returns
    /// everything written to it, and a Buffer used as a Reader returns what is
    /// left. It is a queue, which is what a stream is.
    pub fn bytes(&self) -> VResult<Vec<u8>> {
        match self.kind {
            StreamKind::Buffer => {
                let inner = self.borrow_open("bytes")?;
                Ok(inner.buf[inner.pos..inner.end].to_vec())
            }
            _ => Err(format!("'{}' object has no method 'bytes'", self.kind.type_name())),
        }
    }

    /// The whole stream, from wherever it is now to EOF, in one allocation
    /// where the size is knowable.
    ///
    /// This is `io.read(r)`'s Rust path. Growing a buffer chunk by chunk holds
    /// the old and new allocations at the moment of the last doubling — a
    /// transient peak of 1.5–2× the result, which is what actually kills the
    /// process on a large file. A `stat` up front takes the peak to 1×. That is
    /// a 2× reduction, not immunity: a 10 GB file still does not fit in 8 GB.
    /// Where the size is not knowable (a pipe, a terminal) it falls back to
    /// chunked growth and the transient comes back.
    pub fn read_all(&self) -> VResult<Vec<u8>> {
        let mut inner = self.borrow_readable("read")?;
        let buffered = inner.end - inner.pos;
        let hint = inner.size_hint().map(|n| n + buffered);
        let mut out = Vec::with_capacity(hint.unwrap_or(buffered));
        out.extend_from_slice(&inner.buf[inner.pos..inner.end]);
        inner.pos = inner.end;
        inner.read_source_to_end(&mut out)?;
        Ok(out)
    }

    /// `close()`. Not part of the protocol and not an interface: refcounting
    /// already closes a stream when its last reference drops, which is why Oro
    /// has no `with`. This is for the cases where end of scope is too late.
    pub fn close(&self) -> VResult<()> {
        let mut inner = self.inner.borrow_mut();
        // Dropping the handle closes the fd. Writers are unbuffered, so there
        // is nothing pending and nothing to lose by never calling this at all.
        inner.back = Backing::Mem;
        inner.buf = Vec::new();
        inner.pos = 0;
        inner.end = 0;
        inner.closed = true;
        Ok(())
    }
}

impl Inner {
    /// Bytes available at `buf[pos..end]`, refilling from the source first if
    /// the buffer is spent. `0` means EOF.
    fn fill(&mut self) -> VResult<usize> {
        if self.pos < self.end {
            return Ok(self.end - self.pos);
        }
        if !self.refills() {
            return Ok(0);
        }
        // First read: this is where the 8 KiB appears. An idle stream never
        // reaches here and never pays for it.
        if self.buf.len() < BUFSIZE {
            self.buf.resize(BUFSIZE, 0);
        }
        self.pos = 0;
        self.end = 0;
        let mut scratch = std::mem::take(&mut self.buf);
        let got = self.read_source(&mut scratch);
        self.buf = scratch;
        self.end = got?;
        Ok(self.end)
    }

    /// Whether this stream has a read side. A `Buffer` does, and serves it
    /// from `buf` alone.
    fn can_read(&self) -> bool {
        matches!(self.back, Backing::Read(_) | Backing::Stdin | Backing::Mem)
    }

    /// Whether an exhausted buffer can be refilled from a source.
    fn refills(&self) -> bool {
        matches!(self.back, Backing::Read(_) | Backing::Stdin)
    }

    fn read_source(&mut self, out: &mut [u8]) -> VResult<usize> {
        let r = match &mut self.back {
            Backing::Read(f) => f.read(out),
            Backing::Stdin => std::io::stdin().read(out),
            // A Buffer's bytes are all in `buf`, and a writer never reads.
            Backing::Mem | Backing::Write(_) | Backing::Stdout | Backing::Stderr => {
                return Err("internal: read from a stream with no source".to_string())
            }
        };
        r.map_err(|e| e.to_string())
    }

    fn read_source_to_end(&mut self, out: &mut Vec<u8>) -> VResult<()> {
        let r = match &mut self.back {
            Backing::Read(f) => f.read_to_end(out).map(|_| ()),
            Backing::Stdin => std::io::stdin().read_to_end(out).map(|_| ()),
            // A Buffer is already whole; `read_all` took its remainder above.
            Backing::Mem => Ok(()),
            Backing::Write(_) | Backing::Stdout | Backing::Stderr => {
                return Err("internal: read from a stream with no source".to_string())
            }
        };
        r.map_err(|e| e.to_string())
    }

    fn write_source(&mut self, b: &[u8]) -> VResult<()> {
        let r = match &mut self.back {
            Backing::Write(f) => f.write_all(b),
            // Unbuffered: the write is flushed before the call returns, which
            // is how `print` and `sys.stdout.write` stay in program order.
            Backing::Stdout => {
                let mut h = std::io::stdout().lock();
                h.write_all(b).and_then(|()| h.flush())
            }
            Backing::Stderr => {
                let mut h = std::io::stderr().lock();
                h.write_all(b).and_then(|()| h.flush())
            }
            Backing::Mem => {
                // Reclaim the consumed prefix rather than growing forever when
                // a Buffer is used as a pipe.
                if self.pos > 0 && (self.pos == self.end || self.pos >= BUFSIZE) {
                    self.buf.drain(..self.pos);
                    self.end -= self.pos;
                    self.pos = 0;
                }
                self.buf.truncate(self.end);
                self.buf.extend_from_slice(b);
                self.end = self.buf.len();
                Ok(())
            }
            Backing::Read(_) | Backing::Stdin => {
                return Err("write() on a stream open for reading (mode 'r')".to_string())
            }
        };
        r.map_err(|e| e.to_string())
    }

    /// Bytes remaining in the source, when that is knowable: a regular file's
    /// size less how far into it we have already read. A pipe, a socket or a
    /// terminal has no answer, and says so.
    fn size_hint(&mut self) -> Option<usize> {
        let f = match &mut self.back {
            Backing::Read(f) => f,
            _ => return None,
        };
        let meta = f.metadata().ok()?;
        if !meta.is_file() {
            return None;
        }
        let pos = std::io::Seek::stream_position(f).ok()?;
        usize::try_from(meta.len().saturating_sub(pos)).ok()
    }

}

/// The offset of `needle` in `hay`, or `None`.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests;
