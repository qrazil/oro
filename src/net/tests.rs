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
use crate::stream::OroStream;

/// The hang guard, in seconds. Nothing here takes milliseconds; this only ever
/// fires on a bug.
const GUARD: f64 = 20.0;

/// A connected pair on the loopback, plus the listener that made it.
///
/// No thread is needed to build one: `connect` completes into the listen
/// backlog, so the dial returns before anything has accepted it, and the
/// `accept` that follows finds a connection already waiting.
fn pair() -> (OroStream, OroStream, OroStream) {
    let ln = listen("127.0.0.1:0").expect("bind on an ephemeral port");
    let addr = ln.addr_attr("local").unwrap();
    let client = dial(&addr).expect("dial the listener we just bound");
    let server = ln.accept().expect("accept the connection we just made");
    client.set_timeout(Some(GUARD)).unwrap();
    server.set_timeout(Some(GUARD)).unwrap();
    (ln, server, client)
}

/// The error from a constructor that must fail. `unwrap_err` is not available:
/// `OroStream` holds live file descriptors and deliberately has no `Debug`.
fn failure(r: VResult<OroStream>) -> String {
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
    client.write(b"ping").unwrap();
    assert_eq!(server.read(4).unwrap(), b"ping");
    server.write(b"pong").unwrap();
    assert_eq!(client.read(4).unwrap(), b"pong");
}

#[test]
fn read_returns_at_most_n_and_at_least_one_byte() {
    let (_ln, server, client) = pair();
    client.write(b"abc").unwrap();
    // `n` is a maximum, and a short read is not EOF — it is "this is what has
    // arrived". The only thing guaranteed is 1..=3, which is precisely why
    // code needing exactly n bytes calls `io.read(r, n)`.
    let got = server.read(4096).unwrap();
    assert!(!got.is_empty() && got.len() <= 3, "read(4096) returned {got:?}");
    assert!(b"abc".starts_with(&got[..]));
}

#[test]
fn read_zero_is_an_error_on_a_socket_too() {
    let (_ln, server, _client) = pair();
    // `read(0)` would return b"" and look like EOF; there is exactly one thing
    // b"" means (§2).
    assert!(server.read(0).is_err());
    assert!(server.read(-1).is_err());
}

#[test]
fn eof_is_an_empty_read_after_the_peer_closes() {
    let (_ln, server, client) = pair();
    client.write(b"last").unwrap();
    client.close().unwrap();
    assert_eq!(server.read(64).unwrap(), b"last");
    // The end of a stream is not a fault, and it stays ended.
    assert_eq!(server.read(64).unwrap(), b"");
    assert_eq!(server.read(64).unwrap(), b"");
}

#[test]
fn read_until_spans_two_packets() {
    // The case that catches buffer bugs: the delimiter arrives split across two
    // `read`s, so a scan that only looks inside one buffer-full misses it. The
    // sleep makes the split near-certain; the assertions hold either way, so an
    // unlucky coalesce weakens the test's reach without making it flaky.
    let (_ln, server, client) = pair();
    let writer = std::thread::spawn(move || {
        client.write(b"GET / HTTP/1.1\r\nHost: x\r\n\r").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        client.write(b"\nBODY").unwrap();
        client
    });
    let head = server.read_until(b"\r\n\r\n", 65536).unwrap();
    assert_eq!(head, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    // Everything after the delimiter belongs to the next read, and must still
    // be there: over-reading past the delimiter with nowhere to put the excess
    // is the other half of this bug.
    let _client = writer.join().unwrap();
    assert_eq!(server.read(64).unwrap(), b"BODY");
}

#[test]
fn read_until_raises_past_the_limit() {
    let (_ln, server, client) = pair();
    client.write(b"a header line far longer than the limit\r\n").unwrap();
    let e = server.read_until(b"\r\n", 8).unwrap_err();
    // The limit is what stops a client sending an unbounded header block. The
    // classifier answers `ValueError` to this message; 42_net.oro checks that.
    assert!(e.contains("found no delimiter"), "{e}");
}

#[test]
fn read_all_on_a_socket_stops_at_the_peers_fin() {
    // `io.read(r)` with no count reaches this for a Rust stream. A socket has
    // no size to `stat`, so this is the chunked path, and it must end at EOF
    // rather than block for ever.
    let (_ln, server, client) = pair();
    client.write(b"whole message").unwrap();
    client.shutdown_write().unwrap();
    assert_eq!(server.read_all().unwrap(), b"whole message");
}

// --- The half-close, §4 -------------------------------------------------------

#[test]
fn shutdown_write_ends_one_direction_and_keeps_the_other() {
    let (_ln, server, client) = pair();
    client.write(b"request").unwrap();
    client.shutdown_write().unwrap();
    assert_eq!(server.read(64).unwrap(), b"request");
    // The peer saw the FIN...
    assert_eq!(server.read(64).unwrap(), b"");
    // ...and the answer still gets back, which is the entire point of a
    // half-close being a different thing from `close()`.
    server.write(b"response").unwrap();
    assert_eq!(client.read(64).unwrap(), b"response");
    // Writing after the half-close is the caller's mistake, not a silent no-op.
    assert!(client.write(b"more").is_err());
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
    let server = ln.accept().unwrap();
    client.write(b"v6").unwrap();
    assert_eq!(server.read(2).unwrap(), b"v6");
}

#[test]
fn an_address_without_a_port_is_named_not_guessed() {
    for bad in ["127.0.0.1", "localhost", "", "127.0.0.1:"] {
        let e = failure(dial(bad));
        assert!(e.contains("must be 'host:port'"), "{bad}: {e}");
        assert_eq!(classify(&e), Some("ValueError"));
    }
}

// --- Error mapping, §4's table -------------------------------------------------

#[test]
fn connect_to_a_closed_port_is_connection_refused() {
    let e = failure(dial(&dead_addr()));
    assert!(e.contains("Connection refused"), "{e}");
    assert_eq!(classify(&e), Some("ConnectionRefusedError"));
}

#[test]
fn binding_a_used_port_is_an_os_error() {
    let ln = listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    let e = failure(listen(&addr));
    // `SO_REUSEADDR` does not make two listeners share a port — that is
    // `SO_REUSEPORT`, which is M6 and deliberately absent. So this must fail.
    assert!(e.contains("Address already in use"), "{e}");
    // Not a `ConnectionError`: those four classes are for connections, and
    // everything else in §4's table is a plain `OSError`, which is what the
    // general `[Errno …]` rule in the classifier already answers.
    assert_eq!(classify(&e), None);
    assert!(e.starts_with("[Errno "), "{e}");
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
        match server.write(b"x") {
            Ok(()) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    let e = err.expect("writing to a closed peer never failed");
    let kind = classify(&e).unwrap_or("");
    assert!(
        kind == "BrokenPipeError" || kind == "ConnectionResetError",
        "unexpected class {kind:?} for {e}"
    );
}

#[test]
fn a_read_deadline_is_a_timeout_error() {
    let (_ln, server, client) = pair();
    server.set_timeout(Some(0.05)).unwrap();
    let e = server.read(64).unwrap_err();
    // CPython's message for a socket deadline is the bare "timed out", with no
    // errno, and that is what the classifier already keys `TimeoutError` on.
    assert_eq!(e, "timed out");
    // Clearing it puts the socket back to blocking.
    server.set_timeout(None).unwrap();
    client.write(b"now").unwrap();
    assert_eq!(server.read(64).unwrap(), b"now");
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
    let e = ln.read(16).unwrap_err();
    assert!(e.contains("not a stream of bytes"), "{e}");
    assert!(ln.write(b"x").unwrap_err().contains("not a stream of bytes"));
    assert!(ln.read_until(b"\n", 16).is_err());
    // ...and the socket operations are the other way round.
    assert!(ln.shutdown_write().is_err());
    assert!(ln.set_nodelay(true).is_err());
    // §4: "A listener has no timeout; use a task and a shutdown flag."
    assert!(ln.set_timeout(Some(1.0)).is_err());
}

#[test]
fn accept_on_a_socket_is_a_mistake_not_a_hang() {
    let (_ln, server, _client) = pair();
    assert!(failure(server.accept()).contains("not a listener"));
}

#[test]
fn close_frees_the_port_and_the_stream() {
    let ln = listen("127.0.0.1:0").unwrap();
    let addr = ln.addr_attr("local").unwrap();
    ln.close().unwrap();
    // Every operation on a closed stream says so, rather than answering with a
    // plausible EOF.
    assert!(failure(ln.accept()).contains("on a closed TcpListener"));
    assert!(ln.read(1).unwrap_err().contains("on a closed TcpListener"));
    // The fd really went: the port is refused and bindable again, which is also
    // what makes `dead_addr()` above trustworthy.
    assert!(failure(dial(&addr)).contains("Connection refused"));
    let again = listen(&addr).expect("the port did not come back");
    assert_eq!(again.addr_attr("local").unwrap(), addr);
}

#[test]
fn a_closed_socket_refuses_reads_and_writes() {
    let (_ln, server, client) = pair();
    client.close().unwrap();
    assert!(client.read(1).unwrap_err().contains("on a closed TcpStream"));
    assert!(client.write(b"x").unwrap_err().contains("on a closed TcpStream"));
    assert!(client.shutdown_write().unwrap_err().contains("on a closed TcpStream"));
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
