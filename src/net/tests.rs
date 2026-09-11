//! TCP, over real sockets on the loopback interface.
//!
//! Networking cannot be oracled: `corpus/oracle.sh` runs a program under
//! CPython and diffs the output, and a socket program's output depends on the
//! kernel, the machine and the day. So these are the primary check for §4, and
//! they are written to three rules:
//!
//! * **Ephemeral ports only.** Every listener binds `127.0.0.1:0` and reads the
//!   port back from `local`. A hard-coded port is a test that fails on the one
//!   machine already running something there.
//! * **Loopback only.** Nothing here resolves a name or leaves the host.
//! * **Nothing may hang CI.** Every socket gets a timeout as a guard, so a
//!   deadlock in the code under test fails the run in seconds instead of
//!   stalling it. The guard is deliberately far longer than any of these
//!   operations needs, so it can only fire on a real fault, never on a slow
//!   machine.
//!
//! The exception *types* these messages map to are checked end to end in
//! `corpus/divergence/42_net.oro`, which is where the classifier and the
//! hierarchy are both in scope.

use super::*;
use crate::exc::{Exc, VErr};
use crate::stream::{Io, OroStream};

/// The hang guard, in seconds. Nothing here takes milliseconds; this only ever
/// fires on a bug.
const GUARD: f64 = 20.0;

/// `net.listen` as every test but the `SO_REUSEPORT` ones want it: an address,
/// and the default for the second argument.
///
/// This shadows the glob-imported [`super::listen`] deliberately. `reuseport`
/// is a bind-time option that no test about reading, writing, timeouts or
/// closing has an opinion on, and threading a `false` through forty call sites
/// would put a constant in front of the reader forty times to say nothing. The
/// tests that *are* about it call `super::listen` with both arguments, which is
/// also what makes them visible as the ones that care.
fn listen(addr: &str) -> VResult<OroStream> {
    super::listen(addr, false)
}

/// Wait for a non-blocking operation to finish, the way the scheduler would.
///
/// Every socket in Oro is non-blocking now, so `read`, `write`, `read_until`
/// and `accept` answer [`Io<T>`]: the result, or "would block". The thing that
/// *waits* is the reactor, and there is no VM here — these tests exercise
/// `crate::stream`'s semantics directly, one layer below the scheduler, which
/// is the layer they have always been about.
///
/// Spinning with a 1 ms sleep is the honest stand-in at that layer, and it
/// keeps the file's third rule: [`GUARD`] bounds it, so a bug in the code under
/// test fails the run in seconds rather than stalling CI. The *scheduler's*
/// parking is checked where it belongs, in `crate::vm::tests`, against real
/// concurrent tasks.
fn spin<T>(mut f: impl FnMut() -> VResult<Io<T>>) -> VResult<T> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(GUARD);
    loop {
        match f()? {
            Io::Ready(v) => return Ok(v),
            Io::Block(_) if std::time::Instant::now() >= until => {
                return Err(crate::exc::timeout_error("timed out"))
            }
            Io::Block(_) => std::thread::sleep(std::time::Duration::from_millis(1)),
        }
    }
}

fn rd(s: &OroStream, n: i64) -> VResult<Vec<u8>> {
    spin(|| s.read(n))
}

/// §2's `write(b)`: all of `b`, or raises. The loop over the partial-write
/// count is the scheduler's job in the real thing; here it is three lines.
fn wr(s: &OroStream, b: &[u8]) -> VResult<()> {
    let mut done = 0usize;
    spin(move || {
        while done < b.len() {
            match s.write(&b[done..])? {
                Io::Ready(k) => done += k,
                Io::Block(i) => return Ok(Io::Block(i)),
            }
        }
        Ok(Io::Ready(()))
    })
}

fn ru(s: &OroStream, delim: &[u8], limit: i64) -> VResult<Vec<u8>> {
    let mut acc = Vec::new();
    spin(move || s.read_until(delim, limit, &mut acc))
}

fn ac(s: &OroStream) -> VResult<OroStream> {
    spin(|| s.accept())
}

/// `net.dial`, at this layer: the scheduler's flow with the parking spun out.
///
/// There is no longer a `net::dial` function to call. A dial stopped being one
/// call when it stopped being able to finish on its own — it is a *plan*
/// ([`plan_dial`]), then a lookup, then a non-blocking connect, and the thing
/// that stitches those together across two parks is `crate::vm::sched`. So this
/// stitches them together with [`spin`], exactly as `rd`, `wr` and `ac` above
/// do for the operations that were already non-blocking, and for the same
/// reason: the semantics under test here are one layer below the scheduler.
///
/// The address walk is reproduced rather than skipped, because it is part of
/// what these tests check. `getaddrinfo` puts `::1` before `127.0.0.1` for
/// `localhost`, so on a host with no IPv6 the first address failing is the
/// normal path and not an error.
fn dial(addr: &str) -> VResult<OroStream> {
    let addrs = match plan_dial(addr)? {
        DialPlan::Connect(a) => a,
        // Blocking here is exactly right: what the scheduler does instead is
        // park, and there is no scheduler at this layer to park on.
        DialPlan::Lookup(a) => resolve(&a, "dial")?,
    };
    let mut last = crate::exc::os_error("no address tried");
    for target in addrs {
        let sock = match mio::net::TcpStream::connect(target) {
            Ok(s) => s,
            Err(e) => {
                last = io_error(&e);
                continue;
            }
        };
        let s = OroStream::connecting(sock, target);
        match spin(|| s.connect_check()) {
            Ok(()) => return Ok(s),
            Err(e) => {
                last = e;
                continue;
            }
        }
    }
    Err(last)
}

/// A connected pair on the loopback, plus the listener that made it.
///
/// No thread is needed to build one: `connect` completes into the listen
/// backlog, so the dial returns before anything has accepted it, and the
/// `accept` that follows finds a connection already waiting.
fn pair() -> (OroStream, OroStream, OroStream) {
    let ln = listen("127.0.0.1:0").expect("bind on an ephemeral port");
    let addr = ln.addr_attr("local").unwrap();
    let client = dial(&addr).expect("dial the listener we just bound");
    let server = ac(&ln).expect("accept the connection we just made");
    client.set_timeout(Some(GUARD)).unwrap();
    server.set_timeout(Some(GUARD)).unwrap();
    (ln, server, client)
}

/// The error from a constructor that must fail. `unwrap_err` is not available:
/// `OroStream` holds live file descriptors and deliberately has no `Debug`.
fn failure(r: VResult<OroStream>) -> VErr {
    match r {
        Ok(s) => panic!("expected a failure, got {}", s.repr()),
        Err(e) => e,
    }
}

/// An address on the loopback with nothing listening on it: bind an ephemeral
/// port, learn which one the kernel picked, and drop the listener.
fn dead_addr() -> String {
    let ln = listen("127.0.0.1:0").unwrap();
    // `ln` drops at the end of this expression, which closes the fd — by the
    // time the caller has the string, the port is unbound.
    ln.addr_attr("local").unwrap()
}

// --- The io protocol, §2, on a socket ---------------------------------------

#[test]
fn a_socket_is_a_reader_and_a_writer() {
    let (_ln, server, client) = pair();
    wr(&client, b"ping").unwrap();
    assert_eq!(rd(&server, 4).unwrap(), b"ping");
    wr(&server, b"pong").unwrap();
    assert_eq!(rd(&client, 4).unwrap(), b"pong");
}

#[test]
fn read_returns_at_most_n_and_at_least_one_byte() {
    let (_ln, server, client) = pair();
    wr(&client, b"abc").unwrap();
    // `n` is a maximum, and a short read is not EOF — it is "this is what has
    // arrived". The only thing guaranteed is 1..=3, which is precisely why
    // code needing exactly n bytes calls `io.read(r, fixed_size=n)`.
    let got = rd(&server, 4096).unwrap();
    assert!(!got.is_empty() && got.len() <= 3, "read(4096) returned {got:?}");
    assert!(b"abc".starts_with(&got[..]));
}

#[test]
fn read_zero_is_an_error_on_a_socket_too() {
    let (_ln, server, _client) = pair();
    // `read(0)` would return b"" and look like EOF; there is exactly one thing
    // b"" means (§2).
    assert!(rd(&server, 0).is_err());
    assert!(rd(&server, -1).is_err());
}

#[test]
fn eof_is_an_empty_read_after_the_peer_closes() {
    let (_ln, server, client) = pair();
    wr(&client, b"last").unwrap();
    client.close().unwrap();
    assert_eq!(rd(&server, 64).unwrap(), b"last");
    // The end of a stream is not a fault, and it stays ended.
    assert_eq!(rd(&server, 64).unwrap(), b"");
    assert_eq!(rd(&server, 64).unwrap(), b"");
}

#[test]
fn read_until_spans_two_packets() {
    // The case that catches buffer bugs: the delimiter arrives split across two
    // `read`s, so a scan that only looks inside one buffer-full misses it. The
    // sleep makes the split near-certain; the assertions hold either way, so an
    // unlucky coalesce weakens the test's reach without making it flaky.
    let (_ln, server, client) = pair();
    let writer = std::thread::spawn(move || {
        wr(&client, b"GET / HTTP/1.1\r\nHost: x\r\n\r").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        wr(&client, b"\nBODY").unwrap();
        client
    });
    let head = ru(&server, b"\r\n\r\n", 65536).unwrap();
    assert_eq!(head, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    // Everything after the delimiter belongs to the next read, and must still
    // be there: over-reading past the delimiter with nowhere to put the excess
    // is the other half of this bug.
    let _client = writer.join().unwrap();
    assert_eq!(rd(&server, 64).unwrap(), b"BODY");
}

#[test]
fn read_until_raises_past_the_limit() {
    let (_ln, server, client) = pair();
    wr(&client, b"a header line far longer than the limit\r\n").unwrap();
    let e = ru(&server, b"\r\n", 8).unwrap_err();
    // The limit is what stops a client sending an unbounded header block.
    // The fault names `ValueError` at the raise site; 42_net.oro checks that.
    assert!(e.message.contains("found no delimiter"), "{e}");
    assert_eq!(e.class, Exc::ValueError);
}

#[test]
fn read_all_refuses_a_socket_and_the_chunk_loop_covers_it() {
    // `read_all` is the `stat`-and-allocate-once path, and it used to accept a
    // socket by falling back to `read_to_end`. Under green threads it cannot:
    // "read until EOF" on a socket is an unbounded wait, and there is no park
    // point in the middle of one Rust call. So it says so...
    let (_ln, server, client) = pair();
    wr(&client, b"whole message").unwrap();
    client.shutdown_write().unwrap();
    let e = server.read_all().unwrap_err();
    assert!(e.message.contains("io.read(r) takes the chunk loop"), "{e}");

    // ...and nothing loses a capability, because `std/io.oro` has always
    // dispatched `io.read(r)` on the type: `File` and `Buffer` take the fast
    // path, and everything else — a socket, or an Oro class with a `read`
    // method — takes the `while chunk != b""` loop, where every iteration is a
    // park point. This is that loop, spelled in Rust.
    let mut all = Vec::new();
    loop {
        let chunk = rd(&server, 65536).unwrap();
        if chunk.is_empty() {
            break;
        }
        all.extend_from_slice(&chunk);
    }
    assert_eq!(all, b"whole message");
}

// --- The half-close, §4 -------------------------------------------------------

#[test]
fn shutdown_write_ends_one_direction_and_keeps_the_other() {
    let (_ln, server, client) = pair();
    wr(&client, b"request").unwrap();
    client.shutdown_write().unwrap();
    assert_eq!(rd(&server, 64).unwrap(), b"request");
    // The peer saw the FIN...
    assert_eq!(rd(&server, 64).unwrap(), b"");
    // ...and the answer still gets back, which is the entire point of a
    // half-close being a different thing from `close()`.
    wr(&server, b"response").unwrap();
    assert_eq!(rd(&client, 64).unwrap(), b"response");
    // Writing after the half-close is the caller's mistake, not a silent no-op.
    assert!(wr(&client, b"more").is_err());
}

// --- Addresses ----------------------------------------------------------------

#[test]
fn a_connected_socket_knows_both_ends() {
    let (ln, server, client) = pair();
    let bound = ln.addr_attr("local").unwrap();
    assert_eq!(client.addr_attr("peer").unwrap(), bound);
    assert_eq!(server.addr_attr("local").unwrap(), bound);
    assert_eq!(server.addr_attr("peer").unwrap(), client.addr_attr("local").unwrap());
    // A listener has one address and no peer.
    assert!(ln.has_addr_attr("local"));
    assert!(!ln.has_addr_attr("peer"));
    assert!(ln.addr_attr("peer").is_err());
}

#[test]
fn an_ephemeral_port_is_a_real_port() {
    let ln = listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    let port: u16 = addr.rsplit(':').next().unwrap().parse().unwrap();
    assert!(addr.starts_with("127.0.0.1:"));
    assert_ne!(port, 0, "the kernel's assigned port must be readable back");
}

#[test]
fn ipv6_uses_gos_bracket_form() {
    // Skipped rather than failed where the host has no IPv6 loopback: that is
    // a property of the machine, not of the code.
    let Ok(ln) = listen("[::1]:0") else { return };
    let addr = ln.addr_attr("local").unwrap();
    assert!(addr.starts_with("[::1]:"), "{addr}");
    let client = dial(&addr).unwrap();
    let server = ac(&ln).unwrap();
    wr(&client, b"v6").unwrap();
    assert_eq!(rd(&server, 2).unwrap(), b"v6");
}

#[test]
fn an_address_without_a_port_is_named_not_guessed() {
    for bad in ["127.0.0.1", "localhost", "", "127.0.0.1:"] {
        let e = failure(dial(bad));
        assert!(e.message.contains("must be 'host:port'"), "{bad}: {e}");
        assert_eq!(e.class, Exc::ValueError);
    }
}

// --- Error mapping, §4's table -------------------------------------------------

#[test]
fn connect_to_a_closed_port_is_connection_refused() {
    let e = failure(dial(&dead_addr()));
    assert!(e.message.contains("Connection refused"), "{e}");
    assert_eq!(e.class, Exc::ConnectionRefusedError);
}

#[test]
fn binding_a_used_port_is_an_os_error() {
    let ln = listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    let e = failure(listen(&addr));
    // `SO_REUSEADDR` does not make two listeners share a port — that is
    // `SO_REUSEPORT`, which is M6 and deliberately absent. So this must fail.
    assert!(e.message.contains("Address already in use"), "{e}");
    // Not a `ConnectionError`: those four classes are for connections, and
    // everything else in §4's table is a plain `OSError`, which is the class
    // `io_class` gives every other `ErrorKind`.
    assert_eq!(e.class, Exc::OSError);
    assert!(e.message.starts_with("[Errno "), "{e}");
}

#[test]
fn writing_to_a_vanished_peer_raises_a_connection_error() {
    let (_ln, server, client) = pair();
    client.close().unwrap();
    // The first write after the peer's close still succeeds: it goes into the
    // send buffer, the peer's kernel answers RST, and the *next* write fails.
    // Which write that is depends on the kernel's timing, so this loops rather
    // than asserting a fixed one.
    let mut err = None;
    for _ in 0..50 {
        match wr(&server, b"x") {
            Ok(()) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let e = err.expect("writing to a closed peer never failed");
    assert!(
        matches!(e.class, Exc::BrokenPipeError | Exc::ConnectionResetError),
        "unexpected class {:?} for {e}",
        e.class
    );
}

#[test]
fn a_deadline_is_recorded_here_and_enforced_by_the_scheduler() {
    // `set_timeout` used to be `SO_RCVTIMEO`, and an expired read came back
    // from the kernel as `EWOULDBLOCK`. It cannot be that any more: the socket
    // is non-blocking, so the kernel never waits and a kernel-side deadline has
    // nothing to expire. The number is recorded here...
    // `pair()` has already set one — the hang guard — so this starts by
    // overwriting it rather than by asserting there is none.
    let (_ln, server, client) = pair();
    server.set_timeout(Some(0.05)).unwrap();
    assert_eq!(server.timeout(), Some(std::time::Duration::from_millis(50)));
    server.set_timeout(None).unwrap();
    assert_eq!(server.timeout(), None);

    // ...and it does not change what this layer does, which is answer "would
    // block" at once, every time, deadline or no deadline.
    server.set_timeout(Some(0.05)).unwrap();
    assert!(matches!(server.read(64), Ok(Io::Block(_))));

    // Turning that deadline into the same `TimeoutError` it always raised is
    // the scheduler's timer list, which needs tasks to be meaningful and is
    // checked in `crate::vm::tests::a_read_timeout_fires_while_another_task_runs`.
    // What this layer still owes is that clearing it works.
    server.set_timeout(None).unwrap();
    wr(&client, b"now").unwrap();
    assert_eq!(rd(&server, 64).unwrap(), b"now");
}

#[test]
fn a_timeout_must_be_a_positive_number() {
    let (_ln, server, _client) = pair();
    assert!(server.set_timeout(Some(0.0)).is_err());
    assert!(server.set_timeout(Some(-1.0)).is_err());
    assert!(server.set_timeout(Some(f64::INFINITY)).is_err());
}

// --- What a listener is not ----------------------------------------------------

#[test]
fn a_listener_is_not_a_stream_of_bytes() {
    let ln = listen("127.0.0.1:0").unwrap();
    let e = rd(&ln, 16).unwrap_err();
    assert!(e.message.contains("not a stream of bytes"), "{e}");
    assert!(wr(&ln, b"x").unwrap_err().message.contains("not a stream of bytes"));
    assert!(ru(&ln, b"\n", 16).is_err());
    // ...and the socket operations are the other way round.
    assert!(ln.shutdown_write().is_err());
    assert!(ln.set_nodelay(true).is_err());
    // §4: "A listener has no timeout; use a task and a shutdown flag."
    assert!(ln.set_timeout(Some(1.0)).is_err());
}

#[test]
fn accept_on_a_socket_is_a_mistake_not_a_hang() {
    let (_ln, server, _client) = pair();
    assert!(failure(ac(&server)).message.contains("not a listener"));
}

#[test]
fn close_frees_the_port_and_the_stream() {
    let ln = listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    ln.close().unwrap();
    // Every operation on a closed stream says so, rather than answering with a
    // plausible EOF.
    assert!(failure(ac(&ln)).message.contains("on a closed TcpListener"));
    assert!(rd(&ln, 1).unwrap_err().message.contains("on a closed TcpListener"));
    // The fd really went: the port is refused and bindable again, which is also
    // what makes `dead_addr()` above trustworthy.
    assert!(failure(dial(&addr)).message.contains("Connection refused"));
    let again = listen(&addr).expect("the port did not come back");
    assert_eq!(again.addr_attr("local").unwrap(), addr);
}

#[test]
fn a_closed_socket_refuses_reads_and_writes() {
    let (_ln, server, client) = pair();
    client.close().unwrap();
    assert!(rd(&client, 1).unwrap_err().message.contains("on a closed TcpStream"));
    assert!(wr(&client, b"x").unwrap_err().message.contains("on a closed TcpStream"));
    assert!(client.shutdown_write().unwrap_err().message.contains("on a closed TcpStream"));
    let _ = server;
}

#[test]
fn nodelay_is_a_knob_that_takes() {
    let (_ln, _server, client) = pair();
    client.set_nodelay(true).unwrap();
    client.set_nodelay(false).unwrap();
}

// --- Naming --------------------------------------------------------------------

#[test]
fn the_types_name_themselves() {
    let (ln, server, _client) = pair();
    assert_eq!(ln.kind.type_name(), "TcpListener");
    assert_eq!(server.kind.type_name(), "TcpStream");
    assert!(ln.repr().starts_with("<TcpListener 127.0.0.1:"));
    assert!(server.repr().starts_with("<TcpStream 127.0.0.1:"));
    assert!(server.repr().contains(" -> "));
}

// --- DNS: which addresses need a lookup, and which do not ----------------------

/// `plan_dial` is the whole of the decision, and it makes it without touching
/// the network.
///
/// This is the seam the non-blocking dial hangs on: everything `plan_dial`
/// answers `Connect` to connects immediately, and everything it answers
/// `Lookup` to parks a task on a helper thread. Getting the line wrong in one
/// direction sends `127.0.0.1` through a thread pool for nothing; getting it
/// wrong in the other puts `getaddrinfo` back on the VM thread, which is the
/// bug being fixed.
///
/// The line is `str::parse::<SocketAddr>`, which is exactly where
/// `impl ToSocketAddrs for str` decides the same thing — so these cases are
/// checking that Oro's split and std's are one split, not two that agree today.
#[test]
fn only_a_name_needs_a_lookup() {
    for literal in ["127.0.0.1:8080", "0.0.0.0:0", "[::1]:8080", "[::]:80"] {
        match plan_dial(literal).expect("a well-formed literal") {
            DialPlan::Connect(addrs) => assert_eq!(addrs.len(), 1, "{literal}"),
            DialPlan::Lookup(_) => panic!("{literal} was sent to the resolver"),
        }
    }
    for name in ["localhost:80", "example.com:443", "sub.domain.example:8080"] {
        match plan_dial(name).expect("a well-formed name") {
            // Note what is *not* asserted: that the name resolves. `plan_dial`
            // does no lookup, which is why `example.com` can appear in a test
            // that never touches the network.
            DialPlan::Lookup(addr) => assert_eq!(addr, name),
            DialPlan::Connect(_) => panic!("{name} skipped the resolver"),
        }
    }
}

/// A malformed address is refused at the call site, before any lookup.
///
/// It has to be. A missing port is a programming error, no resolver can change
/// the answer, and parking a task in order to tell it that it made one would
/// deliver the `ValueError` from somewhere the program cannot see. The class
/// and the message are the ones this module has always produced.
#[test]
fn a_malformed_address_never_reaches_the_resolver() {
    for bad in ["127.0.0.1", "localhost", "", "example.com:"] {
        let e = match plan_dial(bad) {
            Err(e) => e,
            Ok(_) => panic!("{bad} was accepted"),
        };
        assert!(e.message.contains("must be 'host:port'"), "{bad}: {e}");
        assert_eq!(e.class, Exc::ValueError);
    }
}

// --- `SO_REUSEPORT`, §4's scale-out half ------------------------------------
//
// These tests are written against one rule that the rest of this file does not
// need: **the sockopt being accepted proves nothing.** A kernel that stored the
// flag and ignored it, or an implementation that set it on the wrong socket, or
// after `bind` where it has no effect, would all pass a `getsockopt` check and
// deliver a server that runs on one core. So the flag readback is here as a
// unit check, and the test the feature actually stands on is
// `two_listeners_on_one_port_both_receive_connections`, which counts arrivals.
//
// The file's own three rules still hold: ephemeral ports only (the first
// listener binds `:0` and the second is told the port it got), loopback only,
// and a deadline on everything.

/// How many connections the load-balancing test makes.
///
/// Sized for a claim that cannot flake rather than for a tight one. The
/// assertion is "each listener received at least one", not "they split evenly":
/// the kernel picks a socket by hashing the connection's 4-tuple, so an even
/// split is a tendency and a *non-empty* split, over this many independent
/// draws, is a certainty in every sense that matters to CI. Asserting a ratio
/// would be asserting a property of the hash, and it would eventually fail on
/// somebody's machine at three in the morning.
const SPREAD_CONNS: usize = 64;

/// Bind a `reuseport` listener on a known address, as `net.listen` would.
fn reuse_listen(addr: &str) -> VResult<OroStream> {
    super::listen(addr, true)
}

/// Accept everything queued on `ln` right now, and stop at the first `Block`.
///
/// Bounded by [`GUARD`] like everything else here, but it is not a spin: the
/// connections are all established before any accept runs, so a `Block` means
/// this listener's share has been drained, not that the work has not arrived.
fn drain(ln: &OroStream) -> Vec<OroStream> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(GUARD);
    let mut out = Vec::new();
    loop {
        match ln.accept().expect("accept on a live listener") {
            Io::Ready(s) => out.push(s),
            Io::Block(_) => return out,
        }
        assert!(std::time::Instant::now() < until, "drain() ran past the guard");
    }
}

/// **The test the feature stands on.** Two listeners, one port, N connections,
/// and both listeners get some.
///
/// This is the claim `SO_REUSEPORT` exists to make and the only one that
/// distinguishes it from an option that was set and forgotten. Two listeners in
/// one process rather than two processes is not a weakening: the kernel's
/// reuseport group is keyed on the address, the port and the socket's UID, and
/// has no notion of which process a member lives in — what it balances across
/// is *sockets*. The N-process arrangement Oro actually deploys is checked, end
/// to end and through the language, in `tests/reuseport.rs`.
#[test]
fn two_listeners_on_one_port_both_receive_connections() {
    let a = reuse_listen("127.0.0.1:0").expect("bind an ephemeral port with reuseport");
    let addr = a.addr_attr("local").unwrap();
    let b = reuse_listen(&addr).expect("a second listener on the same port");

    // Every connection completes into a listen backlog before anything
    // accepts, so by the time the drains run the kernel has already made all
    // N of its choices. Nothing here races.
    let clients: Vec<_> = (0..SPREAD_CONNS)
        .map(|i| dial(&addr).unwrap_or_else(|e| panic!("connection {i}: {e}")))
        .collect();

    let (got_a, got_b) = (drain(&a).len(), drain(&b).len());
    assert_eq!(got_a + got_b, SPREAD_CONNS, "connections went missing: {got_a} + {got_b}");
    assert!(got_a > 0 && got_b > 0, "one listener got everything: {got_a} / {got_b}");
    drop(clients);
}

/// Without `reuseport`, the second bind is refused — which is what makes the
/// test above evidence rather than a coincidence.
///
/// If the port could be shared anyway, "two listeners both received
/// connections" would say nothing about the option. This is the control.
#[test]
fn without_reuseport_the_second_bind_is_refused() {
    let a = listen("127.0.0.1:0").expect("bind an ephemeral port");
    let addr = a.addr_attr("local").unwrap();

    let e = failure(listen(&addr));
    assert!(e.message.contains("Address already in use"), "{e}");
    // `EADDRINUSE` is not one of the four `ConnectionError` subclasses, so
    // `io_class` gives it the plain `OSError` every other failed syscall gets
    // — which is the answer CPython gives it.
    assert_eq!(e.class, Exc::OSError);

    // And the asymmetry is real in the other direction too: a *reuseport*
    // listener cannot join a port that was bound without it. Both sockets must
    // have set the option before their own bind — that is the kernel's rule,
    // and it is the rule that makes the post-`bind` shortcut in
    // `super::reuseport`'s docs unusable here.
    let e = failure(reuse_listen(&addr));
    assert!(e.message.contains("Address already in use"), "{e}");
}

/// Both socket options are on the socket, and `SO_REUSEADDR` did not get lost
/// on the way to the new path.
///
/// The second half is the one worth having. `net.listen`'s documented contract
/// is that *every* listener carries `SO_REUSEADDR`; on the default path std
/// sets it, and on the `reuseport` path nothing does unless this crate does. A
/// regression there would not show up as a failing test anywhere else — it
/// would show up as a production server that cannot restart, months later.
#[test]
fn both_options_are_set_on_a_reuseport_listener_and_neither_is_on_a_plain_one() {
    use super::reuseport::getsockopt_int;

    let ln = super::reuseport::bind(&resolve("127.0.0.1:0", "listen").unwrap()).unwrap();
    assert_ne!(getsockopt_int(&ln, libc::SO_REUSEPORT).unwrap(), 0, "SO_REUSEPORT");
    assert_ne!(getsockopt_int(&ln, libc::SO_REUSEADDR).unwrap(), 0, "SO_REUSEADDR");

    // The default path: std's bind, which sets `SO_REUSEADDR` and only that.
    // This pins the asymmetry the two paths are supposed to have — if std ever
    // started setting `SO_REUSEPORT` too, `reuseport=false` would silently
    // become a shared port and this would say so.
    let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    assert_ne!(getsockopt_int(&plain, libc::SO_REUSEADDR).unwrap(), 0, "SO_REUSEADDR");
    assert_eq!(getsockopt_int(&plain, libc::SO_REUSEPORT).unwrap(), 0, "SO_REUSEPORT");
}

/// IPv6 goes down the other `bind_fd` arm, and the two arms share no code.
///
/// That is exactly why this test exists: the `sockaddr` marshalling is written
/// twice, once per family, and the v6 arm is the one no other test in this file
/// would ever reach. A wrong length or a wrong family there is the one mistake
/// in this feature that is unsound rather than merely broken.
///
/// Skipped rather than failed where the loopback has no IPv6 — the same shape
/// as `an_ipv6_listener_accepts_an_ipv6_dial` above, and for the same reason.
#[test]
fn reuseport_works_over_ipv6() {
    let Ok(a) = reuse_listen("[::1]:0") else { return };
    let addr = a.addr_attr("local").unwrap();
    let b = reuse_listen(&addr).expect("a second v6 listener on the same port");

    let clients: Vec<_> = (0..SPREAD_CONNS).map(|_| dial(&addr).unwrap()).collect();
    let (got_a, got_b) = (drain(&a).len(), drain(&b).len());
    assert_eq!(got_a + got_b, SPREAD_CONNS);
    assert!(got_a > 0 && got_b > 0, "one v6 listener got everything: {got_a} / {got_b}");
    drop(clients);
}

/// A `reuseport` listener is an ordinary listener in every other respect.
///
/// The accepted connection has to be a full Reader and Writer with §2's
/// semantics, because `serve` does not know or care which way the listener was
/// bound — that indifference is the entire reason scale-out needed no changes
/// above the listener.
#[test]
fn a_reuseport_listener_accepts_an_ordinary_connection() {
    let ln = reuse_listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    let client = dial(&addr).unwrap();
    let server = ac(&ln).unwrap();
    client.set_timeout(Some(GUARD)).unwrap();
    server.set_timeout(Some(GUARD)).unwrap();

    wr(&client, b"ping").unwrap();
    assert_eq!(rd(&server, 4).unwrap(), b"ping");
    wr(&server, b"pong").unwrap();
    assert_eq!(rd(&client, 4).unwrap(), b"pong");
}

/// The address is still checked before anything is bound, and by the same code.
///
/// `reuseport=true` must not become a second door into `listen` with its own
/// diagnostics — same message, same class, whichever way it is called.
#[test]
fn a_malformed_address_is_refused_on_the_reuseport_path_too() {
    for bad in ["127.0.0.1", "", "localhost"] {
        let plain = failure(listen(bad));
        let reuse = failure(reuse_listen(bad));
        assert_eq!(plain, reuse, "{bad}: the two paths disagree");
        assert_eq!(reuse.class, Exc::ValueError);
    }
}

/// The platform refusal is an `OSError` and says what to do instead.
///
/// The message is checked here rather than only on the platforms that raise it,
/// because those are precisely the platforms this suite does not run on.
#[test]
fn the_unsupported_platform_message_classifies_as_an_oserror() {
    let msg = super::REUSEPORT_UNSUPPORTED;
    assert_eq!(crate::exc::os_error(msg).class, Exc::OSError);
    // Actionable, not just correct: it names the platform that has the
    // feature, the option FreeBSD would need, and the way to keep working.
    assert!(msg.contains("Linux 3.9+"), "{msg}");
    assert!(msg.contains("SO_REUSEPORT_LB"), "{msg}");
    assert!(msg.contains("omit reuseport"), "{msg}");
}
