//! TCP: the Rust side of `net.listen` / `net.dial`, and the error mapping that
//! turns an `errno` into the exception CPython would have raised.
//!
//! The module surface is two constructors and two objects
//! (`docs/stdlib-server-design.md` §4):
//!
//! ```text
//! ln   = net.listen("0.0.0.0:8080")   -> TcpListener
//! conn = ln.accept()                  -> TcpStream
//! conn = net.dial("example.com:80")   -> TcpStream
//! ```
//!
//! A `TcpStream` **is** a Reader and a Writer with exactly the §2 semantics,
//! and it is one because it is an [`OroStream`](crate::stream::OroStream) with
//! a socket backing rather than a second stream type that reimplements the
//! protocol. `io.read`, `io.copy` and `io.buffer` therefore work on a socket
//! without one line of special-casing, which is the whole claim the io protocol
//! was making.
//!
//! **Nothing here blocks any more, with two named exceptions.** Every socket is
//! non-blocking from birth; `accept`, `read` and `write` answer
//! `stream::Io::Block` instead of waiting, and the scheduler
//! (`crate::vm::sched`) parks the task on the mio reactor. The three places
//! that changed are still marked "where the green-thread swap landed" in
//! `crate::stream`, and nothing outside them needed to know.
//!
//! The two exceptions are both in [`dial`] and both are DNS-shaped: name
//! resolution and `connect(2)`. See that function.
//!
//! **The hazard this module recorded, and what became of it.** All three of
//! those calls used to hold a `RefCell` borrow of the stream's interior *across*
//! the blocking syscall. That was harmless only while blocking meant the whole
//! VM was stopped; the moment a task can be suspended there, a second task
//! calling `close()` on the same stream hits `BorrowMutError` and **panics the
//! interpreter** — not a catchable exception, a dead process.
//!
//! It did not survive contact, and not because anyone remembered it. `Park` has
//! no lifetime parameter, so a `Ref<'_, T>` cannot be stored in one, and a
//! parking site that tried to keep its borrow **does not compile**. Every
//! method in `crate::stream` now takes its borrow, tries the syscall and drops
//! it before answering `Io::Block`; `close()` therefore always finds the
//! `RefCell` free, and the scheduler raises a plain `ValueError` in whoever was
//! parked. `close_wakes_a_parked_reader` in `crate::vm::tests` is the check,
//! and it is written even though the compiler makes the panic unreachable,
//! because "unreachable by construction" is a claim.
//!
//! Not here, on purpose: UDP (not a stream, so it cannot satisfy the io
//! protocol), Unix domain sockets, TLS, and `SO_REUSEPORT` scale-out.

use std::net::ToSocketAddrs;

use mio::net::{TcpListener, TcpStream};

use crate::stream::OroStream;
use crate::value::VResult;

/// `net.listen(addr)`: bind, listen, and hand back a listener.
///
/// `SO_REUSEADDR` is set on every listener — a server that cannot restart until
/// its old connections leave `TIME_WAIT` is a server that cannot be deployed.
/// It is set by `std::net::TcpListener::bind` itself on every non-Windows
/// platform, which is the only way to get it without either `unsafe` or a
/// socket crate, and both of those are ruled out. `SO_REUSEPORT` — several
/// processes sharing one port — is a different option, is how the N-VM
/// deployment scales out, and is deliberately not here yet.
pub fn listen(addr: &str) -> VResult<OroStream> {
    let addrs = resolve(addr, "listen")?;
    // `std`'s bind, not mio's, and then `from_std`. It is `std::net`'s bind
    // that sets `SO_REUSEADDR` on every non-Windows platform, which is the
    // guarantee this function's whole doc comment is about; taking mio's would
    // be trusting a second crate to keep making the same choice. `from_std`
    // costs nothing — it is a wrapper around the same fd.
    let ln = std::net::TcpListener::bind(&addrs[..]).map_err(|e| err_msg(&e))?;
    ln.set_nonblocking(true).map_err(|e| err_msg(&e))?;
    OroStream::listener(TcpListener::from_std(ln)).map_err(|e| err_msg(&e))
}

/// `net.dial(addr)`: connect, and hand back a stream.
///
/// **This is the one call in `net` that still stops the world, and it does so
/// twice: the DNS lookup and the TCP handshake.** M3b made that a deliberate,
/// written-down limitation rather than closing it, and §4 asked whichever
/// milestone touched it to say which answer it picked and why. This is that
/// paragraph.
///
/// *What is wrong.* `ToSocketAddrs` resolves synchronously, so a hostname that
/// takes two seconds to resolve is two seconds in which this VM runs nothing
/// else — every task, not only the caller. `connect(2)` on a blocking socket
/// then adds the handshake RTT on top, and to a host that is dropping packets
/// that is the full TCP connect timeout.
///
/// *Why it is still here.* §4 offered two answers and both cost more than they
/// buy at this milestone. A resolver written in Oro over UDP needs UDP, which
/// §4 declines to add and which would be a frozen public surface bought for one
/// internal use. A helper OS thread with a pipe the reactor already watches is
/// the right answer and is the one this will become — but it is the first OS
/// thread in a runtime whose entire pitch is "one VM per thread, nothing
/// shared", and that is a claim to spend deliberately, in the milestone that
/// has a server to justify it, rather than as a rider on the reactor. Making
/// `connect` non-blocking is easy (park on writability, then `take_error`) and
/// is deliberately not done separately: shipping a non-blocking connect behind
/// a blocking resolve would move the stall by a millisecond and let the
/// limitation read as fixed.
///
/// *What it costs today, precisely.* A server built on `net.listen` never
/// reaches this function: `accept`, `read` and `write` all park, and `listen`
/// resolves once at startup before any task exists that could be starved. It is
/// `dial` **from inside a running server** — a proxy, an outbound API call —
/// that stalls its peers, and it stalls them for the lookup plus the handshake.
/// Dialling a literal `ip:port` skips the lookup entirely and leaves only the
/// handshake, which is why every test in this tree dials `127.0.0.1` and why
/// that is exactly the thing that makes the gap easy not to notice.
pub fn dial(addr: &str) -> VResult<OroStream> {
    let addrs = resolve(addr, "dial")?;
    // `TcpStream::connect` over a slice tries each resolved address in turn and
    // reports the *last* failure. With one address — every literal `ip:port`,
    // which is the case that matters for the error mapping — that is the only
    // failure, so `ConnectionRefusedError` survives the loop.
    let sock = std::net::TcpStream::connect(&addrs[..]).map_err(|e| err_msg(&e))?;
    // Non-blocking from here on: the handshake is over, and everything the
    // stream does from now on goes through the reactor.
    sock.set_nonblocking(true).map_err(|e| err_msg(&e))?;
    OroStream::socket(TcpStream::from_std(sock)).map_err(|e| err_msg(&e))
}

/// `"host:port"` -> socket addresses, with Go's bracket form for IPv6.
///
/// The parsing is `ToSocketAddrs`', which already accepts `"127.0.0.1:8080"`,
/// `"[::1]:8080"` and `"example.com:80"`. What it does not do is explain
/// itself, so a missing port — by far the most common mistake, and one that
/// otherwise surfaces as "invalid socket address" — is caught here and named.
fn resolve(addr: &str, who: &str) -> VResult<Vec<std::net::SocketAddr>> {
    let port_sep = match addr.rfind(']') {
        Some(b) => addr[b..].find(':').map(|i| b + i),
        None => addr.rfind(':'),
    };
    match port_sep {
        Some(i) if i + 1 < addr.len() => {}
        _ => {
            return Err(format!(
                "{who}() address must be 'host:port', not '{addr}' — a port is required \
                 (use ':0' for any free port, and brackets for IPv6, as in '[::1]:8080')"
            ))
        }
    }
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| match e.kind() {
            // A name that does not resolve is not a socket error with an
            // errno; say what failed instead of inventing one.
            std::io::ErrorKind::InvalidInput => {
                format!("{who}() could not parse the address '{addr}'")
            }
            _ => format!("[Errno -2] Name or service not known: '{addr}'"),
        })?
        .collect();
    if addrs.is_empty() {
        return Err(format!("[Errno -2] Name or service not known: '{addr}'"));
    }
    Ok(addrs)
}

/// An `io::Error` from a socket, rendered the way CPython renders an `OSError`:
/// `[Errno 111] Connection refused`.
///
/// The text is Oro's own, not `strerror`'s. `io::Error`'s own `Display` is
/// `strerror_r`'s, which is locale-dependent, and [`classify`] has to recognise
/// these messages by content — a message that reads differently under
/// `LC_ALL=fr_FR` would silently downgrade `ConnectionRefusedError` to
/// `OSError` on a French server. Fixing the strings here makes the exception
/// type a property of the code rather than of the environment.
pub fn err_msg(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    let text = match e.kind() {
        ConnectionRefused => "Connection refused",
        ConnectionReset => "Connection reset by peer",
        ConnectionAborted => "Software caused connection abort",
        BrokenPipe => "Broken pipe",
        AddrInUse => "Address already in use",
        AddrNotAvailable => "Cannot assign requested address",
        NotConnected => "Transport endpoint is not connected",
        PermissionDenied => "Permission denied",
        // `set_timeout` used to be `SO_RCVTIMEO`, whose expiry Unix reports as
        // `EWOULDBLOCK`; a deadline is now the scheduler's timer list and
        // raises through a different path entirely. `WouldBlock` is kept here
        // all the same, mapped to the same CPython message — the bare "timed
        // out", with no errno — because any `EAGAIN` that reaches this function
        // has escaped `stream::would_block`, and answering it with the timeout
        // the caller asked for beats inventing an errno for it.
        TimedOut | WouldBlock => return "timed out".to_string(),
        _ => {
            // Something uncategorised. `io::Error`'s text is the only
            // description available, minus the " (os error 111)" tail that
            // duplicates the errno this message already carries.
            let s = e.to_string();
            let head = s.split(" (os error ").next().unwrap_or(&s).to_string();
            return match e.raw_os_error() {
                Some(n) => format!("[Errno {n}] {head}"),
                None => head,
            };
        }
    };
    // Always with the `[Errno n]` prefix, because that prefix is what
    // [`classify`] uses to know the message is one of this module's and not,
    // say, the tail of a child process's stderr that happens to mention a
    // refused connection.
    format!("[Errno {}] {text}", e.raw_os_error().unwrap_or(0))
}

/// The exception class for a message [`err_msg`] produced, or `None` to let the
/// general classifier answer.
///
/// `None` is the right answer for most of them: `[Errno 98] Address already in
/// use` is an `OSError`, which is what the general rule for an `[Errno …]`
/// message already says, and "timed out" is already a `TimeoutError`. Only the
/// four `ConnectionError` subclasses need naming, because nothing else in the
/// language produces them.
///
/// Matching on text rather than on `errno` is deliberate: `ECONNREFUSED` is 111
/// on Linux and 61 on macOS, so the numbers are not a portable key, while the
/// strings above are written by [`err_msg`] and are the same everywhere.
pub fn classify(msg: &str) -> Option<&'static str> {
    // A malformed address is a bad argument, not a failed syscall.
    if msg.contains("() address must be 'host:port'") || msg.contains("() could not parse the address")
    {
        return Some("ValueError");
    }
    // Asking a socket to accept, or a listener to read: the same class of
    // mistake as reading a file opened for writing, and the same answer
    // CPython gives it — a `ValueError`, not a `TypeError`, because the object
    // is the right type and the operation is wrong for its state.
    if msg.contains("which is not a stream of bytes")
        || msg.contains("which is not a socket")
        || msg.contains("which is not a listener")
        || msg.contains("set_timeout() seconds must be positive")
    {
        return Some("ValueError");
    }
    if !msg.starts_with("[Errno ") {
        return None;
    }
    if msg.contains("Connection refused") {
        Some("ConnectionRefusedError")
    } else if msg.contains("Connection reset") {
        Some("ConnectionResetError")
    } else if msg.contains("Software caused connection abort") {
        Some("ConnectionAbortedError")
    } else if msg.contains("Broken pipe") {
        Some("BrokenPipeError")
    } else {
        None
    }
}

#[cfg(test)]
mod tests;
