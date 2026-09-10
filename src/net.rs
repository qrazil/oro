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
//! **Nothing here blocks any more, and there are no longer any exceptions.**
//! Every socket is non-blocking from birth; `accept`, `read`, `write` and now
//! `connect` answer `stream::Io::Block` instead of waiting, and the scheduler
//! (`crate::vm::sched`) parks the task on the mio reactor. The four places that
//! changed are marked "where the green-thread swap landed" in `crate::stream`,
//! and nothing outside them needed to know.
//!
//! The two that held out longest were both in [`dial`] and both DNS-shaped:
//! name resolution and `connect(2)`. They are closed together, on purpose —
//! see [`plan_dial`].
//!
//! ## A correction: "one VM per thread, nothing shared"
//!
//! **What this module used to claim.** [`dial`]'s doc comment called a helper
//! OS thread "the first OS thread in a runtime whose entire pitch is *one VM
//! per thread, nothing shared*", and treated spending that claim as the reason
//! to leave DNS blocking.
//!
//! **Why that was overstated.** The unqualified sentence is now false: there
//! are helper threads in this process, started by [`plan_dial`]'s lookup path
//! and living in `crate::vm::sched`. But the claim was never doing the work its
//! wording suggested. What it was actually asserting — everywhere it was load
//! bearing — is that **`Value` is `!Send`, so the VM must own its own
//! scheduling**: an `Rc`-shaped ready queue cannot live in a work-stealing
//! runtime, which is why Oro has a scheduler at all rather than importing one.
//! "No threads exist" is a stronger statement that happened to be true, and it
//! got written down as though it were the premise.
//!
//! **What is true, and is the invariant to hold.** *No Oro value ever crosses a
//! thread.* The resolver threads exchange a `String` for a
//! `Vec<std::net::SocketAddr>` over an `std::sync::mpsc` channel and touch
//! nothing else; `Value` is still `Rc`-based and still `!Send`; the VM still
//! owns its ready queue, its parked map and every decision about who runs next.
//! Nothing in the architecture rests on the count of OS threads in the process,
//! and stating it that way is what makes it checkable.
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
//! parked. `closing_a_stream_a_task_is_parked_on_raises_and_never_panics` in
//! `tests/reactor.rs` is the check, and it is written even though the compiler
//! makes the panic unreachable, because "unreachable by construction" is a
//! claim and this is how a claim gets checked.
//!
//! Not here, on purpose: UDP (not a stream, so it cannot satisfy the io
//! protocol), Unix domain sockets, TLS, and `SO_REUSEPORT` scale-out.

use std::net::ToSocketAddrs;

use mio::net::TcpListener;

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

/// What a `net.dial(addr)` has to do before it can connect: a name to look up,
/// or an address to connect to right now.
///
/// **This is where DNS stopped stopping the world.** `dial` used to resolve
/// through `ToSocketAddrs` on the calling thread, which freezes the *entire
/// VM* — every task, not the caller — for the length of the lookup. Cached,
/// that is microseconds; uncached, 1–50 ms; against a resolver that is timing
/// out and retrying, 5 seconds or more, during which a thousand live
/// connections make no progress. `net.listen` is the easy half and always was:
/// it resolves once, at startup, before any task exists that could be starved.
/// A literal `ip:port` does no lookup at all, which is exactly why every test
/// in this tree dialled `127.0.0.1` and why the gap was so easy not to notice.
///
/// **The decision: a helper OS thread, not a resolver written in Oro.**
/// `docs/stdlib-server-design.md` §4 left both candidates open and asked the
/// milestone that closed it to say which and why. The reasoning, short form:
///
/// * `getaddrinfo` is blocking-only — POSIX has no async form — so *someone*
///   must wait on a thread. The only question is whose.
/// * Using the OS resolver inherits `/etc/hosts`, `resolv.conf`, search
///   domains, VPN split-DNS, IPv6/IPv4 preference, system caching and NSS
///   plugins: decades of accumulated correctness about networks nobody testing
///   this will ever see.
/// * A pure-Oro resolver would need **UDP added to the frozen surface for one
///   internal caller**, plus re-implementations of `resolv.conf` parsing,
///   search domains, `/etc/hosts`, retries, truncation-to-TCP fallback and
///   CNAME chains — and every case missed becomes "works on my machine" inside
///   a container or behind a VPN.
/// * The deciding evidence: **Go ships both** a pure-Go resolver and a cgo one
///   calling `getaddrinfo`, and switches to the OS one whenever the system
///   configuration looks non-trivial. The pure route was tried by people with
///   more resources and still needs the OS as a fallback. Node and tokio use a
///   thread pool and do not attempt it.
///
/// The thread pool, its size, its shutdown behaviour and the parking are in
/// `crate::vm::sched`; the claim that had to be corrected to make room for it
/// is in this module's docs, above.
///
/// **The connect is fixed in the same change, and that is not incidental.** §4
/// declined to make `connect(2)` non-blocking on its own, on the grounds that
/// "shipping a non-blocking connect behind a blocking resolve would move the
/// stall by a millisecond and let the limitation read as fixed". With the
/// lookup parking, that argument runs the other way: a `dial` now parks from
/// start to finish, and [`DialPlan::Connect`] is handed straight to the
/// scheduler's non-blocking connect rather than to `std::net::TcpStream`.
pub enum DialPlan {
    /// A literal `ip:port`. There is no name here, so there is nothing to look
    /// up and nothing to park on — the scheduler connects immediately.
    Connect(Vec<std::net::SocketAddr>),
    /// A hostname. The scheduler hands this whole `"host:port"` string to a
    /// resolver thread and parks the task until it comes back.
    Lookup(String),
}

/// `net.dial(addr)`, up to the point where it would touch the network.
///
/// Everything this does is string work: the `host:port` shape check, then one
/// `SocketAddr` parse to sort a literal from a name. Both are pure and both are
/// therefore still synchronous, which is what keeps a malformed address a
/// `ValueError` raised *at the call site* rather than an exception delivered
/// from a resolver a millisecond later.
///
/// The literal test is `str::parse::<SocketAddr>` rather than a hand-written
/// one on purpose. `impl ToSocketAddrs for str` starts with exactly that parse
/// and only calls `getaddrinfo` when it fails, so this function splits the
/// input on precisely the line std does — there is no address that std would
/// resolve without a lookup and this sends to the resolver, and none the other
/// way.
pub fn plan_dial(addr: &str) -> VResult<DialPlan> {
    check_host_port(addr, "dial")?;
    match addr.parse::<std::net::SocketAddr>() {
        Ok(sa) => Ok(DialPlan::Connect(vec![sa])),
        Err(_) => Ok(DialPlan::Lookup(addr.to_string())),
    }
}

/// The `"host:port"` shape check, and the diagnostic for getting it wrong.
///
/// `ToSocketAddrs`' own parsing already accepts `"127.0.0.1:8080"`,
/// `"[::1]:8080"` and `"example.com:80"`. What it does not do is explain
/// itself, so a missing port — by far the most common mistake, and one that
/// otherwise surfaces as "invalid socket address" — is caught here and named.
///
/// Separated from [`resolve`] so that it can run on the VM thread while the
/// lookup runs on a helper: a bad address is a programming error and belongs at
/// the call site, and a task should not be parked to be told it made one.
fn check_host_port(addr: &str, who: &str) -> VResult<()> {
    let port_sep = match addr.rfind(']') {
        Some(b) => addr[b..].find(':').map(|i| b + i),
        None => addr.rfind(':'),
    };
    match port_sep {
        Some(i) if i + 1 < addr.len() => Ok(()),
        _ => Err(format!(
            "{who}() address must be 'host:port', not '{addr}' — a port is required \
             (use ':0' for any free port, and brackets for IPv6, as in '[::1]:8080')"
        )),
    }
}

/// `"host:port"` -> socket addresses, with Go's bracket form for IPv6.
///
/// **This blocks, and that is now the point rather than a defect.** It is the
/// system resolver, called in the one place that is allowed to wait for it: on
/// a `net.listen` at startup, before any task exists that could be starved, and
/// on the resolver threads in `crate::vm::sched`, where waiting is the job.
/// Nothing else may call it, and nothing else does.
///
/// The error strings are load-bearing and are why this is one function rather
/// than two. A failed lookup raises the same exception whichever side of the
/// channel it happened on, because it is the same `String` either way:
/// `ValueError` for an address that cannot be parsed, `OSError` for a name that
/// does not resolve (`[Errno -2]`, through the general `[Errno …]` rule in
/// `classify_error`). See [`classify`].
pub(crate) fn resolve(addr: &str, who: &str) -> VResult<Vec<std::net::SocketAddr>> {
    check_host_port(addr, who)?;
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
