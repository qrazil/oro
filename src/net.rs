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
//! ## `SO_REUSEPORT`, and the shape of scaling out
//!
//! `net.listen(addr, reuseport=true)` asks the kernel to let *several
//! processes* hold the same listening port and to spread accepted connections
//! across them. That is the whole scale-out story, and it is this small because
//! nothing above the listener changes: Oro runs one VM per OS thread with
//! nothing shared, so more cores means more *processes*, and N processes each
//! calling `serve` on their own listener is the same function called N times.
//!
//! The option must be set between `socket(2)` and `bind(2)`, which is a window
//! `std::net::TcpListener::bind` does not expose. [`reuseport`] is the
//! consequence: five syscalls, and the only `unsafe` in this crate. Its module
//! docs carry the evidence for every claim in that sentence, including the
//! post-`bind` shortcut that appears to work and does not.
//!
//! Not here, on purpose: UDP (not a stream, so it cannot satisfy the io
//! protocol), Unix domain sockets, and TLS.

use std::net::ToSocketAddrs;

use mio::net::TcpListener;

use crate::exc::{os_error, value_error, VErr};
use crate::stream::OroStream;
use crate::value::VResult;

/// `net.listen(addr, reuseport=false)`: bind, listen, and hand back a listener.
///
/// `SO_REUSEADDR` is set on every listener, on both paths below — a server that
/// cannot restart until its old connections leave `TIME_WAIT` is a server that
/// cannot be deployed. On the default path `std::net::TcpListener::bind` sets
/// it for us on every non-Windows platform; on the `reuseport` path nobody sets
/// it unless [`reuseport::bind`] does, which is why that function sets it
/// explicitly rather than relying on the option it was actually asked for.
///
/// ## The two paths, and why the default one did not move
///
/// `reuseport=false` is **byte-for-byte the code that was here before**: std's
/// bind, then `from_std`. That is deliberate. The new path replaces socket
/// creation, and the cheapest way to be sure a feature nobody asked for costs
/// nobody anything is for the untouched case to be literally untouched — no
/// new syscall, no new branch inside the bind, and no `unsafe` reached at all.
///
/// `reuseport=true` goes to [`reuseport::bind`], which is the only `unsafe` in
/// this crate and explains itself there.
///
/// ## What happens on a platform that does not have it
///
/// **It raises.** The alternative — bind without the option and carry on — is
/// the failure this argument exists to prevent: the first process to start gets
/// the port, every other worker dies with `EADDRINUSE` or, worse, silently
/// receives no connections, and the symptom is "our eight-core box performs
/// like a one-core box" with nothing in any log to explain it.
///
/// The line is drawn at **Linux (and Android)**, not at "platforms that define
/// the constant", and that distinction is the substance of the decision.
/// `SO_REUSEPORT` on macOS and the BSDs is not a weaker version of the Linux
/// option; it is a *different feature wearing the same name*. BSD's lets
/// several sockets hold the port, but does not distribute incoming connections
/// across them the way Linux's hash does — FreeBSD later added the balancing
/// behaviour under a separate name, `SO_REUSEPORT_LB`, precisely because the
/// original could not be changed to mean it. So a macOS build that accepted
/// `reuseport=true` would bind all N workers happily and then feed nearly all
/// of the traffic to one of them: the exact undiagnosable outcome, reached by
/// the other road. Windows has no equivalent at all.
///
/// Adding FreeBSD via `SO_REUSEPORT_LB` is a small change and a plausible one.
/// It is not made here because it cannot be tested here, and an untested guess
/// at another kernel's semantics is the same bet this function just declined to
/// take. The error message names the option so that whoever has the machine
/// knows exactly what to implement.
pub fn listen(addr: &str, reuseport: bool) -> VResult<OroStream> {
    let addrs = resolve(addr, "listen")?;
    let ln = if reuseport {
        bind_reuseport(&addrs)?
    } else {
        // `std`'s bind, not mio's, and then `from_std`. It is `std::net`'s bind
        // that sets `SO_REUSEADDR` on every non-Windows platform, which is the
        // guarantee this function's whole doc comment is about; taking mio's
        // would be trusting a second crate to keep making the same choice.
        // `from_std` costs nothing — it is a wrapper around the same fd.
        std::net::TcpListener::bind(&addrs[..]).map_err(|e| io_error(&e))?
    };
    ln.set_nonblocking(true).map_err(|e| io_error(&e))?;
    OroStream::listener(TcpListener::from_std(ln)).map_err(|e| io_error(&e))
}

/// The platform gate, kept apart from [`listen`] so that the supported and
/// unsupported builds differ in one expression rather than in the shape of the
/// function.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_reuseport(addrs: &[std::net::SocketAddr]) -> VResult<std::net::TcpListener> {
    reuseport::bind(addrs).map_err(|e| io_error(&e))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_reuseport(_addrs: &[std::net::SocketAddr]) -> VResult<std::net::TcpListener> {
    Err(os_error(REUSEPORT_UNSUPPORTED))
}

/// The message for `reuseport=true` on a platform that cannot honour it.
///
/// `OSError`, because the argument was well-formed and the *platform* is what
/// refused it — the same class CPython gives a socket option the OS will not
/// take. Not an errno: no syscall failed, one was declined, and inventing an
/// errno for it is exactly what [`err_msg`] refuses to do elsewhere. See
/// [`listen`] for why this is an error and not a shrug.
#[allow(dead_code)]
pub(crate) const REUSEPORT_UNSUPPORTED: &str = concat!(
    "listen() reuseport=true is not supported on this platform — SO_REUSEPORT ",
    "load-balances accepted connections on Linux 3.9+ only. macOS and the BSDs ",
    "define an option of the same name that shares the port without balancing ",
    "across it (FreeBSD spells the balancing one SO_REUSEPORT_LB), and Windows ",
    "has no equivalent; binding without it would give one worker the port and ",
    "leave the rest silently idle. Run a single worker here — omit reuseport — ",
    "and scale out on Linux."
);

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
        // A malformed address is a bad argument, not a failed syscall.
        _ => Err(value_error(format!(
            "{who}() address must be 'host:port', not '{addr}' — a port is required \
             (use ':0' for any free port, and brackets for IPv6, as in '[::1]:8080')"
        ))),
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
/// The error values are load-bearing and are why this is one function rather
/// than two. A failed lookup raises the same exception whichever side of the
/// channel it happened on, because it is the same [`VErr`] either way:
/// `ValueError` for an address that cannot be parsed, `OSError` for a name that
/// does not resolve.
pub(crate) fn resolve(addr: &str, who: &str) -> VResult<Vec<std::net::SocketAddr>> {
    check_host_port(addr, who)?;
    let addrs: Vec<_> = addr
        .to_socket_addrs()
        .map_err(|e| match e.kind() {
            // A name that does not resolve is not a socket error with an
            // errno; say what failed instead of inventing one.
            std::io::ErrorKind::InvalidInput => {
                value_error(format!("{who}() could not parse the address '{addr}'"))
            }
            _ => os_error(format!("[Errno -2] Name or service not known: '{addr}'")),
        })?
        .collect();
    if addrs.is_empty() {
        return Err(os_error(format!("[Errno -2] Name or service not known: '{addr}'")));
    }
    Ok(addrs)
}

/// An `io::Error` from a socket, rendered the way CPython renders an `OSError`:
/// `[Errno 111] Connection refused`.
///
/// The text is Oro's own, not `strerror`'s. `io::Error`'s own `Display` is
/// `strerror_r`'s, which is locale-dependent, so a diagnostic would otherwise
/// read differently under `LC_ALL=fr_FR`. Fixing the strings here makes the
/// *message* a property of the code rather than of the environment; the
/// exception class is a property of `e.kind()`, which was never text at all
/// (see [`io_error`]).
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
    // Always with the `[Errno n]` prefix: it is the errno CPython puts there,
    // and it is what tells a reader the message came from a syscall.
    format!("[Errno {}] {text}", e.raw_os_error().unwrap_or(0))
}

/// An `io::Error` from a socket, as the fault it raises.
///
/// The class comes from `e.kind()` — the datum the operating system actually
/// reported — and never from the text of the message. That is the whole point:
/// the message is prose, it interpolates addresses and hostnames a client can
/// choose, and a table that matched on it was a table a client could steer.
///
/// `None` for most kinds means `OSError`, which is what CPython gives them and
/// what the `[Errno n]` prefix already says. Only the four `ConnectionError`
/// subclasses and the timeout need naming, because nothing else in the language
/// produces them.
pub fn io_error(e: &std::io::Error) -> VErr {
    VErr::new(crate::exc::io_class(e.kind()), err_msg(e))
}

/// The `SO_REUSEPORT` bind, and the crate's only `unsafe`. Linux-only by
/// construction — on other platforms [`listen`] raises before it could be
/// called, so the module is not compiled at all rather than compiled and
/// unreachable.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod reuseport;

#[cfg(test)]
mod tests;
