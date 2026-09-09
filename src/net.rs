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
//! **Everything here blocks.** One VM, one OS thread, one syscall at a time:
//! `accept`, `read` and `write` stop the world until the kernel answers. That
//! is correct for this milestone and wrong for a server, and the three places
//! it changes are marked "where the green-thread swap lands" in
//! `crate::stream` — nothing outside those three lines needs to know.
//!
//! **One thing the swap must do that is not visible today**, recorded here so
//! it is not rediscovered as a panic: all three of those calls currently hold a
//! `RefCell` borrow of the stream's interior *across* the blocking syscall.
//! That is harmless while blocking means the whole VM is stopped — nothing else
//! can run to observe the borrow. It stops being harmless the moment a parked
//! task can be suspended there, because a second task calling `close()` on the
//! same stream would hit `BorrowMutError` and panic the interpreter. Parking
//! therefore has to release the borrow before it yields and re-take it on
//! resume, which is a constraint on the shape of `Step::Park`, not on this
//! module.
//!
//! Not here, on purpose: UDP (not a stream, so it cannot satisfy the io
//! protocol), Unix domain sockets, TLS, and `SO_REUSEPORT` scale-out.

use std::net::{TcpListener, TcpStream, ToSocketAddrs};

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
    let ln = TcpListener::bind(&addrs[..]).map_err(|e| err_msg(&e))?;
    OroStream::listener(ln).map_err(|e| err_msg(&e))
}

/// `net.dial(addr)`: connect, and hand back a stream.
///
/// **DNS blocks the whole VM.** `ToSocketAddrs` resolves synchronously, so a
/// hostname that takes two seconds to resolve is two seconds in which this VM
/// runs nothing else. That is invisible when testing against `127.0.0.1` and
/// unacceptable under green threads, where the resolution has to move to a
/// blocking pool. It is listed in §4 as an implementation trap for exactly that
/// reason, and it is a trap this milestone walks into knowingly rather than
/// accidentally: blocking is the milestone.
pub fn dial(addr: &str) -> VResult<OroStream> {
    let addrs = resolve(addr, "dial")?;
    // `TcpStream::connect` over a slice tries each resolved address in turn and
    // reports the *last* failure. With one address — every literal `ip:port`,
    // which is the case that matters for the error mapping — that is the only
    // failure, so `ConnectionRefusedError` survives the loop.
    let sock = TcpStream::connect(&addrs[..]).map_err(|e| err_msg(&e))?;
    OroStream::socket(sock).map_err(|e| err_msg(&e))
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
        // A read or write deadline. Unix reports an expired `SO_RCVTIMEO` as
        // `EWOULDBLOCK` and Windows as `ETIMEDOUT`; both are the timeout,
        // because Oro never puts a socket in non-blocking mode. CPython's
        // message for this is the bare "timed out", with no errno.
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
