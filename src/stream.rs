//! Byte streams: the Rust side of the `read(n)` / `write(b)` protocol.
//!
//! The protocol is two methods and a naming convention, not a declared type
//! (`docs/stdlib-server-design.md` §2):
//!
//! ```text
//! read(n)   -> bytes    # 1..n bytes; b"" at EOF; may return fewer than n
//! write(b)  -> null     # writes all of b, or raises
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
//!
//! ## Sockets never block, and that changes the *signature*, not the protocol
//!
//! Every socket in Oro is in non-blocking mode from the moment it exists, so
//! the four operations that can meet `EWOULDBLOCK` — `accept`, `read`,
//! `read_until` and `write` — answer [`Io<T>`] rather than `T`: either the
//! result, or "this would block, wait for *that* readiness and call me again".
//! The waiting is not done here. It is done by the scheduler
//! (`crate::vm::sched`), which parks the task, registers the fd with the mio
//! reactor and calls back in. `File`, `Buffer`, stdin and stdout can never
//! answer [`Io::Block`], so the tri-state costs them one `match` arm and
//! nothing else.
//!
//! **Nothing in these methods holds a `RefCell` borrow when it returns
//! `Io::Block`.** Each takes its borrow, tries the syscall and drops the borrow
//! before answering, which is what lets a second task `close()` the same stream
//! while the first is parked on it. `src/net.rs` records why that matters and
//! `crate::vm::sched`'s docs record what enforces it.
//!
//! **Where a partly-finished operation keeps its progress** is the other half
//! of that rule: not in a Rust stack frame (there is none across a park) and
//! not in the stream (a second task must be able to use it), but in the
//! caller's owned accumulator — `read_until`'s `acc`, `write`'s returned count.
//! The scheduler owns those, inside the `Park`, where they are `'static`.

use std::cell::{Cell, Ref, RefCell, RefMut};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::time::Duration;

use mio::net::{TcpListener, TcpStream};

use crate::exc::{attribute_error, broken_pipe_error, runtime_error, type_error, value_error, VErr};
use crate::value::VResult;

/// A file or stdio `io::Error`, as the fault it raises. The message is std's
/// own rendering — unchanged, and the only description available for a backing
/// that is not a socket — and the class comes from `e.kind()`. Reading a
/// directory is an `OSError` because `read(2)` failed, not because `strerror`
/// happened to say "Is a directory".
fn source_error(e: &std::io::Error) -> VErr {
    VErr::new(crate::exc::io_class(e.kind()), e.to_string())
}

/// What an I/O attempt on a non-blocking stream answers.
///
/// There is no third case: an operation either finished or is waiting on one
/// readiness. Errors ride on the `VResult` around this, exactly as they did
/// when the same calls blocked.
pub enum Io<T> {
    /// It finished, and here is the result.
    Ready(T),
    /// It would block. Wait for this readiness on this stream, then call the
    /// same method again with the same accumulator.
    Block(mio::Interest),
}

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
    /// A connected TCP socket, from `net.dial` or `listener.accept`. It is a
    /// `StreamKind` rather than a type of its own precisely so that it inherits
    /// the read buffer, `read`'s short-read rule and `read_until`'s scan
    /// unchanged — the io protocol is one convention, so a socket must be one
    /// implementation of it (`docs/stdlib-server-design.md` §4).
    TcpStream { peer: String, local: String },
    /// A listening TCP socket, from `net.listen`. Not a Reader and not a
    /// Writer: it has `accept()`, `close()` and a `local` address, and reading
    /// it is a `ValueError` like reading any stream with no read side.
    TcpListener { local: String },
    /// A `proc.spawn` child's stdin, stdout or stderr. A type of its own for the
    /// same reason `TcpStream` is: its size is not knowable, so `io.read(r)`
    /// must take the chunk loop rather than the `stat`-and-allocate path a
    /// `File` gets — a pipe that reported as a `File` would send the whole of a
    /// gigabyte-scale stream at `read_all` and defeat the point of `spawn`.
    /// `which` is the `<child stdout>`-style label a `repr()` shows.
    Pipe { which: String },
}

impl StreamKind {
    pub fn type_name(&self) -> &'static str {
        self.type_tag().name()
    }

    /// The stream's type, as `type(s)` answers with.
    pub fn type_tag(&self) -> crate::value::TypeTag {
        use crate::value::TypeTag;
        match self {
            StreamKind::File { .. } => TypeTag::File,
            StreamKind::Buffer => TypeTag::Buffer,
            StreamKind::TcpStream { .. } => TypeTag::TcpStream,
            StreamKind::TcpListener { .. } => TypeTag::TcpListener,
            StreamKind::Pipe { .. } => TypeTag::Pipe,
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
    /// A connected socket, always in non-blocking mode. It is a `mio` socket
    /// rather than a `std` one for one reason: `mio::event::Source` is what
    /// `Registry::register` takes, and registering the fd is how a parked task
    /// gets woken. Everything else about it — `Read`, `Write`, `shutdown`,
    /// `set_nodelay`, `peer_addr` — is the `std` surface unchanged.
    Socket(TcpStream),
    /// A listening socket, also non-blocking. It has no read or write side at
    /// all.
    Listener(TcpListener),
    /// The read end of a `proc.spawn` child's stdout or stderr. The child's fd
    /// is *not* here and is *not* mio-registered — a pipe is not an mio source
    /// the way a socket is, which is the whole reason `proc.spawn` reads it on a
    /// helper thread (`crate::process`). That thread reads the blocking fd and
    /// hands `BUFSIZE` chunks down this channel; the far end dropping (the thread
    /// exiting at the child's EOF) is what this side reads as EOF.
    ///
    /// The channel is **bounded** at the helper-thread end, so a slow consumer
    /// applies backpressure: the reader thread blocks on a full channel, the
    /// child then blocks on a full pipe, and nothing buffers without limit. That
    /// bound is what lets `io.copy(dst, p.stdout)` move a stream larger than
    /// memory in constant space.
    PipeRead(Receiver<Vec<u8>>),
    /// The write end of a `proc.spawn` child's stdin. Symmetric to
    /// [`PipeRead`](Backing::PipeRead): `write(b)` hands `b` to a helper thread
    /// over a bounded channel, so a full channel answers `Io::Block` (the task
    /// parks) rather than blocking the VM on a full pipe. Dropping this end —
    /// `close()` or the handle going away — lets the thread drain what is queued
    /// and then close the child's stdin, so the child reads EOF.
    PipeWrite(SyncSender<Vec<u8>>),
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
    /// `set_timeout(secs)`. Not `SO_RCVTIMEO` any more: a non-blocking socket
    /// never waits in the kernel, so a deadline is something the scheduler
    /// enforces (`crate::vm::sched`'s timer list) and this is only where the
    /// number is kept between `set_timeout` and the park that uses it.
    timeout: Option<Duration>,
}

/// An open byte stream.
pub struct OroStream {
    pub kind: StreamKind,
    inner: RefCell<Inner>,
    /// The reactor token this stream's fd is registered under, or `0` for
    /// "never parked on". Deliberately *outside* the `RefCell`: `close()` holds
    /// the borrow while the scheduler needs to know which token to release.
    token: Cell<usize>,
}

impl OroStream {
    fn new(kind: StreamKind, back: Backing) -> OroStream {
        OroStream {
            kind,
            inner: RefCell::new(Inner {
                back,
                buf: Vec::new(),
                pos: 0,
                end: 0,
                closed: false,
                timeout: None,
            }),
            token: Cell::new(0),
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
            inner: RefCell::new(Inner {
                back: Backing::Mem,
                buf: b,
                pos: 0,
                end,
                closed: false,
                timeout: None,
            }),
            token: Cell::new(0),
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

    /// The read end of a child's stdout/stderr — a `proc.spawn` pipe fed by a
    /// helper thread over `rx`. `which` is the `<...>` label a `repr()` or an
    /// error shows (`<child stdout>`), the same convention `std_stream` uses.
    ///
    /// A [`Pipe`](StreamKind::Pipe) reads through the same lazy 8 KiB buffer, the
    /// same short-read rule and the same `read_until` scan as every other reader
    /// (`docs/stdlib-server-design.md` §4's "the io protocol is one
    /// convention") — the [`Backing`] is where it differs. It is a distinct
    /// *kind* only so `type(r)` tells `io.read` its size is unknowable, exactly
    /// as a `TcpStream` is a distinct kind for the same reason.
    pub fn pipe_read(rx: Receiver<Vec<u8>>, which: &str) -> OroStream {
        OroStream::new(StreamKind::Pipe { which: which.to_string() }, Backing::PipeRead(rx))
    }

    /// The write end of a child's stdin — a `proc.spawn` pipe drained by a
    /// helper thread reading `tx`.
    pub fn pipe_write(tx: SyncSender<Vec<u8>>, which: &str) -> OroStream {
        OroStream::new(StreamKind::Pipe { which: which.to_string() }, Backing::PipeWrite(tx))
    }

    /// Whether this stream is a `proc.spawn` pipe. The scheduler asks because a
    /// pipe that would block parks on the pipe helper's waker rather than on the
    /// mio reactor — a pipe fd is not registered with mio at all.
    pub fn is_pipe(&self) -> bool {
        matches!(self.inner.borrow().back, Backing::PipeRead(_) | Backing::PipeWrite(_))
    }

    /// A connected TCP socket — `net.dial`'s result, and `accept`'s.
    ///
    /// The addresses are read once, here, rather than on every attribute
    /// access: `getpeername(2)` on a socket the peer has already closed fails,
    /// and `conn.peer` going from a string to an exception halfway through a
    /// connection would be a worse answer than the address it was opened with.
    pub fn socket(sock: TcpStream) -> std::io::Result<OroStream> {
        let peer = sock.peer_addr()?.to_string();
        let local = sock.local_addr()?.to_string();
        Ok(OroStream::new(StreamKind::TcpStream { peer, local }, Backing::Socket(sock)))
    }

    /// A socket whose `connect(2)` is still in flight — `net.dial`'s result
    /// before the handshake finishes.
    ///
    /// It cannot go through [`socket`](Self::socket), which reads `peer_addr`
    /// and would fail with `ENOTCONN` on a socket that is still connecting. The
    /// peer is taken from the address being dialled instead, which is not a
    /// substitute for `getpeername(2)` but *the same answer*: a connect either
    /// reaches that address or never becomes a stream at all.
    ///
    /// Infallible, where [`socket`](Self::socket) is not. On Linux and macOS
    /// `connect(2)` binds the socket before it returns, so `local_addr` is
    /// available the moment this is called; the fallback is there because a
    /// `repr()` field is not worth failing a dial over.
    pub fn connecting(sock: TcpStream, target: std::net::SocketAddr) -> OroStream {
        let local = match sock.local_addr() {
            Ok(a) => a.to_string(),
            Err(_) => match target {
                std::net::SocketAddr::V4(_) => "0.0.0.0:0".to_string(),
                std::net::SocketAddr::V6(_) => "[::]:0".to_string(),
            },
        };
        OroStream::new(
            StreamKind::TcpStream { peer: target.to_string(), local },
            Backing::Socket(sock),
        )
    }

    /// Has the connect finished, and did it succeed?
    ///
    /// **Where the green-thread swap landed (4 of 4).** The other three are
    /// `accept`, `read` and `write`; this one arrived a milestone later, with
    /// non-blocking DNS, because a non-blocking connect behind a blocking
    /// resolve would have moved the stall by a millisecond and let the
    /// limitation read as fixed (`docs/stdlib-server-design.md` §4).
    ///
    /// Two checks, in this order, and the order is the correctness. `SO_ERROR`
    /// is where a *failed* connect is reported — the fd becomes writable either
    /// way, so "writable" alone means "the kernel has an answer", not "it
    /// worked". `take_error` also clears it, which is why it is read once per
    /// readiness and the answer acted on immediately. Only then does
    /// `peer_addr` distinguish "connected" from "still in flight": a spurious
    /// writable edge answers `ENOTCONN`, and re-parking on it is right.
    ///
    /// Like every other method here it takes its borrow, asks the kernel and
    /// drops it before answering `Io::Block` — the rule `src/net.rs`'s module
    /// docs record.
    pub fn connect_check(&self) -> VResult<Io<()>> {
        let inner = self.borrow_open("connect")?;
        let Backing::Socket(s) = &inner.back else {
            return Err(value_error(format!(
                "connect() on a '{}', which is not a socket",
                self.kind.type_name()
            )));
        };
        if let Some(e) = s.take_error().map_err(|e| crate::net::io_error(&e))? {
            return Err(crate::net::io_error(&e));
        }
        match s.peer_addr() {
            Ok(_) => Ok(Io::Ready(())),
            Err(e) if e.kind() == std::io::ErrorKind::NotConnected || would_block(&e) => {
                Ok(Io::Block(mio::Interest::WRITABLE))
            }
            Err(e) => Err(crate::net::io_error(&e)),
        }
    }

    /// A listening TCP socket — `net.listen`'s result.
    pub fn listener(ln: TcpListener) -> std::io::Result<OroStream> {
        let local = ln.local_addr()?.to_string();
        Ok(OroStream::new(StreamKind::TcpListener { local }, Backing::Listener(ln)))
    }

    /// `listener.accept()`: the next queued connection, as a stream that
    /// satisfies the io protocol exactly like a file does.
    ///
    /// **Where the green-thread swap landed (1 of 3).** It is the shape the
    /// module docs predicted: the syscall is unchanged, and `EWOULDBLOCK` is no
    /// longer an error but `Io::Block(READABLE)` — "park the task on this
    /// listener's readability and call `accept` again". The borrow is dropped
    /// on the way out of this function, so the task parks holding nothing but
    /// an `Rc`.
    pub fn accept(&self) -> VResult<Io<OroStream>> {
        let inner = self.borrow_open("accept")?;
        let ln = match &inner.back {
            Backing::Listener(ln) => ln,
            _ => {
                return Err(value_error(format!(
                    "accept() on a '{}', which is not a listener",
                    self.kind.type_name()
                )))
            }
        };
        let sock = match ln.accept() {
            Ok((sock, _)) => sock,
            Err(e) if would_block(&e) => return Ok(Io::Block(mio::Interest::READABLE)),
            Err(e) => return Err(crate::net::io_error(&e)),
        };
        // Dropped before the new stream is built, so `accept` never holds two
        // stream borrows at once.
        drop(inner);
        OroStream::socket(sock).map(Io::Ready).map_err(|e| crate::net::io_error(&e))
    }

    /// `conn.shutdown_write()`: send FIN, keep reading.
    ///
    /// The half-close is the only way to say "that is the whole request" on a
    /// connection the peer is still allowed to answer on, and it is not
    /// `close()` — closing drops the read side with it, so the reply is lost.
    pub fn shutdown_write(&self) -> VResult<()> {
        let inner = self.borrow_open("shutdown_write")?;
        match &inner.back {
            Backing::Socket(s) => s.shutdown(Shutdown::Write).map_err(|e| crate::net::io_error(&e)),
            _ => Err(value_error(format!(
                "shutdown_write() on a '{}', which is not a socket",
                self.kind.type_name()
            ))),
        }
    }

    /// `conn.set_timeout(seconds)` — one deadline for both directions, or
    /// `None` to clear it. Expiry raises `TimeoutError`.
    ///
    /// One knob rather than two: separate read and write deadlines are two
    /// numbers that servers set to the same value (§4).
    ///
    /// This used to be `SO_RCVTIMEO`/`SO_SNDTIMEO`, and it cannot be any more:
    /// the socket is non-blocking, so the kernel never waits and a kernel-side
    /// deadline has nothing to expire. The number is recorded here and turned
    /// into a scheduler deadline by the park that needs it — which is strictly
    /// better than the socket option was, because it now covers the *whole*
    /// operation (a `read_until` spanning four packets, a partial write
    /// finishing over three) rather than restarting on each syscall.
    pub fn set_timeout(&self, secs: Option<f64>) -> VResult<()> {
        let d = match secs {
            None => None,
            Some(s) if s > 0.0 && s.is_finite() => Some(Duration::from_secs_f64(s)),
            Some(s) => {
                return Err(value_error(format!("set_timeout() seconds must be positive, not {s}")));
            }
        };
        let mut inner = self.borrow_open_mut("set_timeout")?;
        match &inner.back {
            Backing::Socket(_) => {
                inner.timeout = d;
                Ok(())
            }
            // A listener has no timeout on purpose: an `accept` that gives up
            // after n seconds is a loop condition dressed as an error. Shutdown
            // is a flag and a `close()` (§3).
            _ => Err(value_error(format!(
                "set_timeout() on a '{}', which is not a socket",
                self.kind.type_name()
            ))),
        }
    }

    /// `conn.set_nodelay(true)` — disable Nagle's algorithm.
    pub fn set_nodelay(&self, on: bool) -> VResult<()> {
        let inner = self.borrow_open("set_nodelay")?;
        match &inner.back {
            Backing::Socket(s) => s.set_nodelay(on).map_err(|e| crate::net::io_error(&e)),
            _ => Err(value_error(format!(
                "set_nodelay() on a '{}', which is not a socket",
                self.kind.type_name()
            ))),
        }
    }

    /// Whether `name` is one of this stream's data attributes (`peer`,
    /// `local`). Only sockets and listeners have any; everything else falls
    /// through to method binding and then to `AttributeError`.
    pub fn has_addr_attr(&self, name: &str) -> bool {
        match self.kind {
            StreamKind::TcpStream { .. } => name == "peer" || name == "local",
            StreamKind::TcpListener { .. } => name == "local",
            _ => false,
        }
    }

    /// The value of a data attribute admitted by
    /// [`has_addr_attr`](Self::has_addr_attr).
    ///
    /// These are plain strings, not an `Address` type: `"127.0.0.1:8080"`, with
    /// Go's bracket form for IPv6 (`"[::1]:8080"`). A type would buy parsing
    /// that is rarely wanted, and `addr.find(":", reverse=true)` covers it when
    /// it is.
    pub fn addr_attr(&self, name: &str) -> VResult<String> {
        match (&self.kind, name) {
            (StreamKind::TcpStream { peer, .. }, "peer") => Ok(peer.clone()),
            (StreamKind::TcpStream { local, .. }, "local") => Ok(local.clone()),
            (StreamKind::TcpListener { local }, "local") => Ok(local.clone()),
            _ => Err(attribute_error(format!(
                "'{}' object has no attribute '{name}'",
                self.kind.type_name()
            ))),
        }
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
            StreamKind::TcpStream { peer, local } => format!("<TcpStream {local} -> {peer}>"),
            StreamKind::TcpListener { local } => format!("<TcpListener {local}>"),
            StreamKind::Pipe { which } => format!("<Pipe {which}>"),
        }
    }

    fn borrow_open(&self, who: &str) -> VResult<Ref<'_, Inner>> {
        let inner = self.inner.borrow();
        if inner.closed {
            return Err(value_error(format!("{who}() on a closed {}", self.kind.type_name())));
        }
        Ok(inner)
    }

    /// [`borrow_open`](Self::borrow_open), for the operations that mutate.
    fn borrow_open_mut(&self, who: &str) -> VResult<RefMut<'_, Inner>> {
        let inner = self.inner.borrow_mut();
        if inner.closed {
            return Err(value_error(format!("{who}() on a closed {}", self.kind.type_name())));
        }
        Ok(inner)
    }

    /// Like [`borrow_open`](Self::borrow_open), and refuses a stream that has
    /// no read side at all — reading a writer is a mistake, not an EOF.
    fn borrow_readable(&self, who: &str) -> VResult<RefMut<'_, Inner>> {
        let inner = self.borrow_open_mut(who)?;
        if !inner.can_read() {
            if let StreamKind::TcpListener { .. } = self.kind {
                return Err(value_error(format!(
                    "{who}() on a TcpListener, which is not a stream of bytes"
                )));
            }
            return Err(value_error(format!("{who}() on a stream open for writing (mode 'w')")));
        }
        Ok(inner)
    }

    /// `read(n)`: between 1 and `n` bytes, or `b""` at EOF. A short read is not
    /// an error and does not mean EOF — it means "this is what has arrived".
    /// Code that needs exactly `n` bytes calls `io.read(r, fixed_size=n)`.
    pub fn read(&self, n: i64) -> VResult<Io<Vec<u8>>> {
        // `read(0)` would return b"" and look like EOF, so it is a ValueError
        // rather than a second thing b"" can mean.
        if n < 1 {
            return Err(value_error(format!("read() size must be at least 1, not {n}")));
        }
        let n = n as usize;
        let mut inner = self.borrow_readable("read")?;
        // A read larger than the buffer, with the buffer empty, goes straight
        // to its own allocation: buffering it would only copy it twice.
        if inner.refills() && inner.pos == inner.end && n >= BUFSIZE {
            let mut out = vec![0u8; n];
            let got = match inner.read_source(&mut out)? {
                Io::Ready(k) => k,
                Io::Block(i) => return Ok(Io::Block(i)),
            };
            out.truncate(got);
            return Ok(Io::Ready(out));
        }
        // Buffered bytes are served without a syscall, so a `read` only ever
        // blocks when there is genuinely nothing to hand back — which is what
        // keeps the park rare rather than per-call.
        let avail = match inner.fill()? {
            Io::Ready(k) => k,
            Io::Block(i) => return Ok(Io::Block(i)),
        };
        let k = avail.min(n);
        let out = inner.buf[inner.pos..inner.pos + k].to_vec();
        inner.pos += k;
        Ok(Io::Ready(out))
    }

    /// One attempt at writing `b`: `Io::Ready(k)` with `1 <= k <= b.len()`, or
    /// `Io::Block(WRITABLE)` when the kernel's send buffer is full.
    ///
    /// This is the *primitive*, not the protocol. §2's `write(b)` writes all of
    /// `b` or raises, and that contract is met one level up, by the scheduler
    /// looping on `k` across as many parks as it takes. Keeping the count here
    /// rather than a `write_all` loop is what makes a partial write resumable:
    /// a Rust loop cannot survive a park, and an owned `done` counter in the
    /// `Park` can.
    ///
    /// A backing that cannot block (a file, a `Buffer`, stdout) still writes
    /// all of `b` in one call and answers `Io::Ready(b.len())`, so nothing but
    /// a socket ever sees the loop go round twice.
    pub fn write(&self, b: &[u8]) -> VResult<Io<usize>> {
        let mut inner = self.borrow_open_mut("write")?;
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
    /// `out` is the caller's accumulator and carries the partial result across
    /// a park: on `Io::Block` it holds every byte scanned so far, and the next
    /// call resumes from there. It is empty on the first call.
    pub fn read_until(&self, delim: &[u8], limit: i64, out: &mut Vec<u8>) -> VResult<Io<Vec<u8>>> {
        if delim.is_empty() {
            return Err(value_error("read_until() delimiter must not be empty"));
        }
        if limit < 1 {
            return Err(value_error(format!("read_until() limit must be at least 1, not {limit}")));
        }
        let limit = limit as usize;
        let mut inner = self.borrow_readable("read_until")?;
        let too_long =
            || value_error(format!("read_until() found no delimiter in the first {limit} bytes"));
        loop {
            match inner.fill()? {
                // EOF: the end of a stream is not a fault, so what has arrived
                // is the answer, delimiter or not.
                Io::Ready(0) => return Ok(Io::Ready(std::mem::take(out))),
                Io::Ready(_) => {}
                Io::Block(i) => return Ok(Io::Block(i)),
            }
            let chunk = &inner.buf[inner.pos..inner.end];
            // Everything already collected has been scanned, so the only
            // delimiter this pass can newly complete is one straddling the
            // boundary: at most `delim.len() - 1` bytes from each side.
            let carry = (delim.len() - 1).min(out.len());
            let split = if carry == 0 {
                None
            } else {
                let head = (delim.len() - 1).min(chunk.len());
                let mut edge = Vec::with_capacity(carry + head);
                edge.extend_from_slice(&out[out.len() - carry..]);
                edge.extend_from_slice(&chunk[..head]);
                find(&edge, delim).map(|i| i + delim.len() - carry)
            };
            // Bytes of this chunk that belong to the result, delimiter
            // included. The chunk is never copied wholesale before the scan:
            // a `Buffer` holding megabytes must not pay for all of them to
            // read one short line out of it.
            match split.or_else(|| find(chunk, delim).map(|i| i + delim.len())) {
                Some(take) => {
                    if out.len() + take > limit {
                        return Err(too_long());
                    }
                    // Everything after the delimiter stays in the buffer; it
                    // belongs to the next read.
                    out.extend_from_slice(&chunk[..take]);
                    inner.pos += take;
                    return Ok(Io::Ready(std::mem::take(out)));
                }
                None => {
                    out.extend_from_slice(chunk);
                    inner.pos = inner.end;
                    if out.len() >= limit {
                        return Err(too_long());
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
            _ => Err(attribute_error(format!(
                "'{}' object has no method 'bytes'",
                self.kind.type_name()
            ))),
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
    ///
    /// A task may be parked on this stream right now — that is the whole hazard
    /// `src/net.rs` records — and this method does not and cannot wake it: the
    /// scheduler owns the parked map. What it guarantees instead is that the
    /// borrow it takes is *not* one the parked task is holding, so this cannot
    /// panic; the scheduler reads [`token`](Self::token) afterwards and raises
    /// in whoever was waiting. Dropping the backing closes the fd, and a closed
    /// fd leaves the kernel's epoll set on its own, so there is no
    /// deregistration to forget.
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

    // --- What the reactor needs, and nothing more ---------------------------

    /// The deadline `set_timeout` asked for, if any. Read by the scheduler at
    /// the moment an operation first blocks.
    pub fn timeout(&self) -> Option<Duration> {
        self.inner.borrow().timeout
    }

    /// The reactor token this stream is registered under, or `None` if it has
    /// never had to park.
    pub fn token(&self) -> Option<usize> {
        match self.token.get() {
            0 => None,
            t => Some(t),
        }
    }

    /// Register this stream's fd with `registry` under `token`, for both
    /// readability and writability.
    ///
    /// One registration per stream, ever. mio's epoll registration is
    /// edge-triggered, so interest in a direction nobody is parked on costs at
    /// most one spurious retry (the writable edge that fires once, just after
    /// the fd is added) and never a spin — and it buys back the
    /// register/deregister pair that every park would otherwise pay for.
    pub fn register(&self, registry: &mio::Registry, token: usize) -> VResult<()> {
        let mut inner = self.borrow_open_mut("read")?;
        let interest = mio::Interest::READABLE | mio::Interest::WRITABLE;
        let tok = mio::Token(token);
        let r = match &mut inner.back {
            Backing::Socket(s) => registry.register(s, tok, interest),
            Backing::Listener(l) => registry.register(l, tok, interest),
            _ => return Err(runtime_error("internal: only a socket can be registered")),
        };
        r.map_err(|e| crate::net::io_error(&e))?;
        self.token.set(token);
        Ok(())
    }
}

impl Inner {
    /// Bytes available at `buf[pos..end]`, refilling from the source first if
    /// the buffer is spent. `0` means EOF.
    fn fill(&mut self) -> VResult<Io<usize>> {
        if self.pos < self.end {
            return Ok(Io::Ready(self.end - self.pos));
        }
        if !self.refills() {
            return Ok(Io::Ready(0));
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
        // `pos == end == 0` either way, so a blocked refill leaves the stream
        // exactly as it found it and the retry re-runs this whole function.
        self.end = match got? {
            Io::Ready(k) => k,
            Io::Block(i) => return Ok(Io::Block(i)),
        };
        Ok(Io::Ready(self.end))
    }

    /// Whether this stream has a read side. A `Buffer` does, and serves it
    /// from `buf` alone.
    fn can_read(&self) -> bool {
        matches!(
            self.back,
            Backing::Read(_)
                | Backing::Stdin
                | Backing::Mem
                | Backing::Socket(_)
                | Backing::PipeRead(_)
        )
    }

    /// Whether an exhausted buffer can be refilled from a source.
    fn refills(&self) -> bool {
        matches!(
            self.back,
            Backing::Read(_) | Backing::Stdin | Backing::Socket(_) | Backing::PipeRead(_)
        )
    }

    /// One `read(2)` into `out`, from whatever this stream is over.
    ///
    /// **Where the green-thread swap landed (2 of 3).** The socket arm is the
    /// only read in the language that can meet `EWOULDBLOCK`, and it is still
    /// one line plus the arm that names it. Everything above this function —
    /// `read`, `read_until`, the lazy 8 KiB buffer, `io.read`, `io.copy` — is
    /// unchanged, because the only thing that changed is that this call can now
    /// answer "not yet" instead of waiting.
    fn read_source(&mut self, out: &mut [u8]) -> VResult<Io<usize>> {
        match &mut self.back {
            Backing::Socket(s) => {
                return match s.read(out) {
                    Ok(k) => Ok(Io::Ready(k)),
                    Err(e) if would_block(&e) => Ok(Io::Block(mio::Interest::READABLE)),
                    Err(e) => Err(crate::net::io_error(&e)),
                }
            }
            // A child pipe's bytes arrive over the helper thread's channel, one
            // chunk at a time. Nothing available yet is `Io::Block` — the task
            // parks on the pipe waker and the scheduler retries when the thread
            // signals — and the sender gone (the thread exited at the child's
            // EOF) is a clean end-of-stream, the same `Io::Ready(0)` a file
            // reports at EOF. The chunk is never larger than `out`: the thread
            // reads into a `BUFSIZE` buffer and `out` is at least `BUFSIZE`
            // wherever this is reached (a full buffer refill, or a large direct
            // read), so no byte is ever dropped on the floor.
            Backing::PipeRead(rx) => {
                return match rx.try_recv() {
                    Ok(chunk) => {
                        let k = chunk.len().min(out.len());
                        out[..k].copy_from_slice(&chunk[..k]);
                        Ok(Io::Ready(k))
                    }
                    Err(TryRecvError::Empty) => Ok(Io::Block(mio::Interest::READABLE)),
                    Err(TryRecvError::Disconnected) => Ok(Io::Ready(0)),
                }
            }
            // A Buffer's bytes are all in `buf`, and a writer never reads.
            Backing::Mem | Backing::Write(_) | Backing::Stdout | Backing::Stderr
            | Backing::Listener(_) | Backing::PipeWrite(_) => {
                return Err(runtime_error("internal: read from a stream with no source"))
            }
            _ => {}
        }
        let r = match &mut self.back {
            Backing::Read(f) => f.read(out),
            Backing::Stdin => std::io::stdin().read(out),
            _ => unreachable!("every other backing answered above"),
        };
        r.map(Io::Ready).map_err(|e| source_error(&e))
    }

    fn read_source_to_end(&mut self, out: &mut Vec<u8>) -> VResult<()> {
        let r = match &mut self.back {
            Backing::Read(f) => f.read_to_end(out).map(|_| ()),
            Backing::Stdin => std::io::stdin().read_to_end(out).map(|_| ()),
            // A socket cannot come here, and the reason is worth spelling.
            // "Read to EOF" on a socket is an unbounded wait, and an unbounded
            // wait inside one Rust call is exactly the thing green threads make
            // impossible: there is no park point in the middle of a
            // `read_to_end`. `io.read(r)` already routes a socket to its Oro
            // chunk loop — `while chunk != b""` over `r.read(_CHUNK)` — where
            // every iteration is a park point and the loop survives a
            // suspension because it lives in Oro frames rather than Rust ones.
            // Only `File` and `Buffer` reach the `stat`-and-allocate-once path,
            // which is what `std/io.oro` has always dispatched on.
            Backing::Socket(_) => {
                return Err(type_error(
                    "internal: read_all() on a socket — io.read(r) takes the chunk \
                     loop for a stream with no knowable size",
                ))
            }
            // A child pipe has no knowable size, exactly like a socket, so
            // "read to EOF" is the unbounded wait green threads forbid inside one
            // Rust call. `io.read(r)` routes it to the Oro chunk loop instead,
            // where every `r.read(_CHUNK)` is a park point.
            Backing::PipeRead(_) => {
                return Err(type_error(
                    "internal: read_all() on a pipe — io.read(r) takes the chunk \
                     loop for a stream with no knowable size",
                ))
            }
            // A Buffer is already whole; `read_all` took its remainder above.
            Backing::Mem => Ok(()),
            Backing::Write(_) | Backing::Stdout | Backing::Stderr | Backing::Listener(_)
            | Backing::PipeWrite(_) => {
                return Err(runtime_error("internal: read from a stream with no source"))
            }
        };
        r.map_err(|e| source_error(&e))
    }

    fn write_source(&mut self, b: &[u8]) -> VResult<Io<usize>> {
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
            // **Where the green-thread swap landed (3 of 3).** This used to be
            // `write_all`, whose loop is exactly the contract §2 gives
            // `write` — and exactly the loop that cannot survive a park,
            // because its progress lives in a Rust stack frame. So the loop
            // moved up into the scheduler, where "how much has gone" is an
            // owned `usize` inside the `Park`, and what is left here is one
            // `write(2)`. The contract is unchanged and no caller learns about
            // any of it, because `write` has no return value to change.
            Backing::Socket(s) => {
                return match s.write(b) {
                    Ok(0) if !b.is_empty() => Err(broken_pipe_error("[Errno 32] Broken pipe")),
                    Ok(k) => Ok(Io::Ready(k)),
                    Err(e) if would_block(&e) => Ok(Io::Block(mio::Interest::WRITABLE)),
                    Err(e) => Err(crate::net::io_error(&e)),
                }
            }
            Backing::Listener(_) => {
                return Err(value_error("write() on a TcpListener, which is not a stream of bytes"))
            }
            // A child's stdin, over the writer thread's bounded channel. A full
            // channel is `Io::Block(WRITABLE)`: the task parks and the scheduler
            // retries when the thread drains a slot — the same backpressure a
            // full socket send buffer gives, so a program feeding a child faster
            // than it reads waits instead of buffering without bound. The `Rc`'d
            // `bytes` is copied into an owned `Vec` because it crosses to the
            // helper thread, which cannot hold a `!Send` `Rc`. The whole of `b`
            // goes in one message or none does, so there is no partial write for
            // the scheduler's loop to resume — `done` jumps straight to `len`.
            Backing::PipeWrite(tx) => {
                return match tx.try_send(b.to_vec()) {
                    Ok(()) => Ok(Io::Ready(b.len())),
                    Err(TrySendError::Full(_)) => Ok(Io::Block(mio::Interest::WRITABLE)),
                    // The writer thread is gone — the child closed its stdin, or
                    // exited. That is `write() on a closed pipe`, the same
                    // `BrokenPipe` a dead socket peer gives.
                    Err(TrySendError::Disconnected(_)) => {
                        Err(broken_pipe_error("[Errno 32] Broken pipe"))
                    }
                }
            }
            Backing::Read(_) | Backing::Stdin | Backing::PipeRead(_) => {
                return Err(value_error("write() on a stream open for reading (mode 'r')"))
            }
        };
        // Everything that is not a socket wrote all of `b` or failed; there is
        // no partial case for the caller's loop to go round twice on.
        r.map(|()| Io::Ready(b.len())).map_err(|e| source_error(&e))
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

/// Whether an `io::Error` is the kernel saying "not now".
///
/// `EINTR` is folded in here rather than treated as a failure: a signal that
/// interrupted the syscall means nothing happened, and the retry is a re-park
/// that costs one loop of the scheduler. `WouldBlock` is `EAGAIN` and
/// `EWOULDBLOCK` both — `io::ErrorKind` already unifies the two spellings.
fn would_block(e: &std::io::Error) -> bool {
    matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted)
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
