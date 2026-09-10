//! `SO_REUSEPORT`: the four syscalls std will not let us reach, and the only
//! `unsafe` in this crate.
//!
//! ## Why this file exists at all
//!
//! `SO_REUSEPORT` has to be set **on the socket, before `bind(2)`**. That is
//! not a style preference, it is the kernel's rule, and it is what rules out
//! every approach that does not create the socket by hand:
//!
//! * `std::net::TcpListener::bind` creates the socket, sets `SO_REUSEADDR`,
//!   binds and listens inside one function and returns the finished listener.
//!   There is no hook in the middle. This was checked against the std source
//!   rather than assumed — `library/std/src/sys/net/connection/socket/mod.rs`,
//!   `TcpListener::bind`, where the `setsockopt` is a `#[cfg(not(windows))]`
//!   block std performs on its own behalf.
//! * The public `TcpListener` surface has no socket-option setters that would
//!   help: `set_ttl`, `set_only_v6`, `set_nonblocking`, `take_error`,
//!   `try_clone`. There is no `set_reuse_port` and no `set_reuse_address`.
//! * mio does not fill the gap either. mio 1.x has no `TcpSocket` (it was
//!   removed after 0.7), and `mio::net::TcpListener` exposes `bind`,
//!   `from_std`, `accept`, `local_addr`, `set_ttl`, `ttl` and `take_error` —
//!   the same dead end.
//!
//! ## The workaround that looks like it works, and does not
//!
//! There is a tempting shortcut: let std bind the listener, then `setsockopt`
//! the option on afterwards. That needs one `unsafe` call instead of five and
//! no `sockaddr` marshalling at all, so it is worth saying plainly why it is
//! not what this file does.
//!
//! Measured on Linux 6.14, a socket that sets `SO_REUSEPORT` *after* `bind`
//! will let a **second** socket that set it *before* `bind` share the port, and
//! connections then appear to spread across both. That is the observation that
//! makes the shortcut look viable, and it is a trap for two reasons.
//!
//! The first is that it does not survive Oro's own deployment model. Scale-out
//! here is N *identical processes* — every worker runs the same
//! `net.listen(addr, reuseport=true)`, so either all of them set the option
//! after `bind` or none do. All-after-bind was measured too: **the second
//! process fails with `EADDRINUSE`**, because at *its* `bind` the kernel reads
//! the new socket's `SO_REUSEPORT`, which is still 0. The asymmetry that made
//! the shortcut appear to work is one this API cannot express.
//!
//! The second is that it was never promised. `socket(7)` states the
//! requirement — all sockets must set the option before binding — and the
//! partial success above is an artefact of where the kernel happens to check
//! `sk_reuseport`, not a contract. A feature whose scale-out silently stops
//! balancing on a kernel upgrade is worse than one that was never shipped.
//!
//! ## What makes the `unsafe` sound
//!
//! Five calls, each with its invariant named at the call site. Two structural
//! choices do most of the work:
//!
//! * **The fd is owned from the first instruction after it exists.**
//!   [`OwnedFd`] adopts the descriptor immediately, so every `?` between here
//!   and the end of [`bind_one`] closes it on unwind. There is no error path
//!   that leaks a descriptor, because there is no window in which the
//!   descriptor is not owned by a value with a `Drop`.
//! * **The handoff back to safe code costs no `unsafe`.** `impl From<OwnedFd>
//!   for std::net::TcpListener` is a *safe* conversion, so the last step is an
//!   ordinary `.into()`. `TcpListener::from_raw_fd` — the obvious route, and
//!   an `unsafe` one — is not needed and is not used.
//!
//! That leaves exactly one delicate operation: `bind(2)` takes a pointer and a
//! length, and a length larger than the object is a real out-of-bounds read of
//! this process's stack. It is written so the two cannot drift — the struct and
//! its `size_of` are named in the same expression, in the same match arm, and
//! neither arm can see the other's type.
//!
//! ## Linux only, on purpose
//!
//! This module is compiled only on Linux and Android, and [`super::listen`]
//! raises on every other platform rather than binding without the option. See
//! that function for the argument; the short form is that `SO_REUSEPORT` on
//! macOS and the BSDs is a *different feature with the same name* (it shares
//! the port without balancing across it; FreeBSD spells the balancing one
//! `SO_REUSEPORT_LB`), and shipping an untested guess at those semantics would
//! reproduce, one platform over, exactly the silent breakage the error exists
//! to prevent.
#![allow(unsafe_code)]

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// std's own listen backlog, and the reason to match it: a listener that
/// differs from `std::net::TcpListener::bind` in queue depth would make
/// `reuseport=true` change a second thing the caller did not ask about.
const BACKLOG: libc::c_int = 128;

/// Bind a `SO_REUSEPORT` listener, trying each resolved address in turn.
///
/// The loop is `std::net::TcpListener::bind`'s own behaviour — `each_addr`
/// walks the list and returns the first success — kept here so that
/// `reuseport=true` does not quietly change which address a dual-stack name
/// lands on. The last error is what surfaces, for the same reason.
pub(super) fn bind(addrs: &[SocketAddr]) -> io::Result<std::net::TcpListener> {
    let mut last = None;
    for addr in addrs {
        match bind_one(addr) {
            Ok(ln) => return Ok(ln),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "no addresses to bind")
    }))
}

/// One address: socket, both options, bind, listen.
///
/// The order is the whole point of the file. `SO_REUSEPORT` is set between
/// `socket` and `bind`, which is the window std does not expose.
fn bind_one(addr: &SocketAddr) -> io::Result<std::net::TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };

    // SAFETY: `socket(2)` with a constant domain, a constant type and protocol
    // 0. It reads no memory through pointers — there are none — so there is no
    // invariant to uphold beyond passing constants the header defines. It
    // returns a new descriptor or -1, and -1 is checked on the next line before
    // anything treats the value as a descriptor.
    //
    // `SOCK_CLOEXEC` is not decoration: std sets `O_CLOEXEC` on every socket it
    // creates, and omitting it here would leak the listener into every child
    // `proc.run` spawns — a difference between `reuseport=true` and
    // `reuseport=false` that has nothing to do with ports.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: `from_raw_fd` requires that `fd` is open, valid, and not owned by
    // anything else. All three hold by construction: `socket(2)` returned it on
    // the line above, it was checked for -1, and nothing has been given a copy
    // of it — this is the first and only use of the raw integer.
    //
    // This line is why the rest of the function has no `unsafe` cleanup in it.
    // From here on the descriptor is owned, so every `?` below closes it.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };

    // `SO_REUSEADDR` first, and not as an afterthought. `net.listen`'s contract
    // is that *every* listener has it — a server that cannot restart until
    // `TIME_WAIT` drains is a server that cannot be deployed — and on the std
    // path std sets it. On this path nobody sets it unless we do, so the
    // `reuseport=true` listener would otherwise be the one listener in the
    // language missing the guarantee the docs give for all of them.
    setsockopt_int(&sock, libc::SO_REUSEADDR, 1)?;
    setsockopt_int(&sock, libc::SO_REUSEPORT, 1)?;

    bind_fd(&sock, addr)?;

    // SAFETY: `listen(2)` takes a descriptor and an integer backlog and
    // dereferences nothing. `sock` is a live, bound socket, and `BACKLOG` is a
    // positive constant.
    if unsafe { libc::listen(sock.as_raw_fd(), BACKLOG) } < 0 {
        return Err(io::Error::last_os_error());
    }

    // Safe: `From<OwnedFd> for TcpListener` is a safe conversion (std,
    // `os/fd/owned.rs`) and moves the ownership rather than duplicating it.
    Ok(std::net::TcpListener::from(sock))
}

/// `setsockopt(2)` for the one shape this file needs: a `SOL_SOCKET` option
/// whose value is a single `int`.
///
/// Narrow on purpose. A general helper would take a pointer and a length from
/// its caller and move the only interesting invariant out of the only place
/// that can check it; this one owns both.
fn setsockopt_int(sock: &OwnedFd, option: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: the kernel reads `size_of::<c_int>()` bytes from the pointer.
    // The pointer is to `value`, a live `c_int` local that outlives the call,
    // and the length is that same type's `size_of` — so the read is exactly the
    // object and cannot run past it. `sock` is a live descriptor for as long as
    // the borrow lasts, which covers the call.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw const value).cast::<libc::c_void>(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `bind(2)`, with the `SocketAddr` marshalled into the C struct for its family.
///
/// **This is the one operation in the crate where a mistake is unsound rather
/// than merely wrong**, because the kernel copies `len` bytes from the pointer
/// and a `len` larger than the object is an out-of-bounds read of this stack.
///
/// It is written so the two cannot disagree. Each arm builds one struct and
/// names that same struct's type in its own `size_of`; the arms share no
/// variable, so there is no way to hand v6's length to v4's pointer. Every
/// field is written explicitly — no `zeroed()`, no `transmute`, no padding left
/// to chance — which is also why `sin_zero` appears: it is part of the ABI and
/// the kernel expects it clear.
///
/// The byte-order idiom is std's. `port().to_be()` puts the port in network
/// order, and `from_ne_bytes(octets())` reinterprets the four address bytes —
/// which `octets()` already yields in network order — without reordering them,
/// so `s_addr` ends up holding exactly the bytes that went in.
fn bind_fd(sock: &OwnedFd, addr: &SocketAddr) -> io::Result<()> {
    let rc = match addr {
        SocketAddr::V4(a) => {
            let sa = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                sin_zero: [0; 8],
            };
            // SAFETY: the kernel reads `size_of::<sockaddr_in>()` bytes from
            // the pointer. The pointer is to `sa`, a fully-initialised
            // `sockaddr_in` local that outlives the call, and the length is
            // that struct's own `size_of` — named here, in the arm that built
            // it, so the two cannot drift apart.
            unsafe {
                libc::bind(
                    sock.as_raw_fd(),
                    (&raw const sa).cast::<libc::sockaddr>(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(a) => {
            let sa = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: as above, with `sockaddr_in6` throughout — the struct
            // built here and the `size_of` named here are the same type.
            unsafe {
                libc::bind(
                    sock.as_raw_fd(),
                    (&raw const sa).cast::<libc::sockaddr>(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read a `SOL_SOCKET` `int` option back off a socket. Test-only.
///
/// It exists because "the sockopt was accepted" is the claim that is easy to
/// check and proves the least. The tests use it to check the two options are
/// *present*, and then go on to check the thing that actually matters — that
/// two listeners on one port both receive connections — because a kernel that
/// stored the flag and ignored it would pass the first check and fail the job.
#[cfg(test)]
pub(super) fn getsockopt_int(sock: &impl AsRawFd, option: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = -1;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes to the value pointer and
    // stores the count written in the len pointer. Both point at live locals,
    // and `len` starts as exactly `size_of::<c_int>()` — the size of the object
    // the first pointer refers to — so the write cannot exceed the object.
    let rc = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&raw mut value).cast::<libc::c_void>(),
            &raw mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}
