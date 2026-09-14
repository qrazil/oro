//! The reactor, end to end: real sockets, real green threads, real clocks.
//!
//! `docs/stdlib-server-design.md` §3 is the specification and M3b is the
//! milestone. What is checked here is the thing that milestone exists to make
//! true — **a task that waits on I/O does not stop the others** — and the
//! handful of ways that can be true in a demo and false under load.
//!
//! Networking and timing cannot be oracled against CPython: `corpus/oracle.sh`
//! diffs a program's output against CPython's, and a socket program's output
//! depends on the kernel, the machine and the day. So these are Rust
//! integration tests over the real binary, written to four rules:
//!
//! * **Ephemeral ports only.** Every listener binds `127.0.0.1:0` and reads the
//!   port back from `.local`. A hard-coded port is a test that fails on the one
//!   machine already running something there.
//! * **Loopback only**, and no name is ever resolved.
//! * **Nothing may hang CI.** Every program runs under [`GUARD`], and a program
//!   that outlives it is killed and fails rather than stalling the run.
//! * **Programs print claims, not dumps.** `fast finished while slow was still
//!   waiting` is a sentence that fails loudly when it stops being true; a
//!   transcript of bytes is a thing nobody re-reads.
//!
//! The timing margins are deliberately wide — tens of milliseconds asserted
//! against sleeps of hundreds — because the property under test is "these
//! overlapped" and not "these took exactly n ms". A test that fails on a loaded
//! CI box is a test that gets deleted.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The hang guard. Nothing here should take a second; this only ever fires on
/// a bug, and when it does it says so instead of stalling the suite.
const GUARD: Duration = Duration::from_secs(20);

/// Run an Oro program and return its stdout, or fail the test saying why.
///
/// The program is killed if it outlives [`GUARD`], which is what makes a
/// deadlock in the scheduler a failing test rather than a hung run.
fn run(name: &str, src: &str) -> String {
    let (out, err, ok) = run_raw(name, src);
    assert!(ok, "{name} exited with failure\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
    out
}

/// The program's stdout, stderr and whether it succeeded.
fn run_raw(name: &str, src: &str) -> (String, String, bool) {
    let dir = std::env::temp_dir().join("oro_reactor_tests");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let path = dir.join(format!("{name}.oro"));
    let mut f = std::fs::File::create(&path).expect("write the program");
    f.write_all(src.as_bytes()).expect("write the program");
    drop(f);

    let mut child = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_oro")))
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch the oro binary");

    let deadline = Instant::now() + GUARD;
    loop {
        match child.try_wait().expect("wait on the child") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{name} did not finish within {GUARD:?} — the scheduler is stuck");
            }
            None => std::thread::sleep(Duration::from_millis(2)),
        }
    }
    let out = child.wait_with_output().expect("collect the child's output");
    let _ = std::fs::remove_file(&path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// [`run`], for a program that is *supposed* to fail. Returns its stdout and
/// stderr.
fn run_failing(name: &str, src: &str) -> (String, String) {
    let (out, err, ok) = run_raw(name, src);
    assert!(!ok, "{name} was expected to fail and did not\n{out}");
    (out, err)
}

/// [`run`], timed. For the properties whose whole content is "these overlapped".
fn run_timed(name: &str, src: &str) -> (String, Duration) {
    let t0 = Instant::now();
    let out = run(name, src);
    (out, t0.elapsed())
}

/// Two tasks, two sockets, and the *second* one delivers first.
///
/// This is the test that distinguishes a scheduler from a queue. A runtime that
/// serialises — running each task to completion in spawn order — passes every
/// echo-server demo ever written and fails this, because the first task is
/// still parked with nothing to read while the second one's bytes are already
/// in the kernel. The output order is delivery order or the multiplexing is not
/// real.
#[test]
fn two_tasks_read_two_sockets_and_delivery_order_wins() {
    let out = run(
        "two_sockets",
        r#"
import net

ln = net.listen("127.0.0.1:0")
a = net.dial(ln.local)
sa = ln.accept()
b = net.dial(ln.local)
sb = ln.accept()

log = []

def reader(name, s):
    log.append(name + " got " + s.read(64).to_str())

first = spawn(reader, "first", sa)
second = spawn(reader, "second", sb)

# Only the second connection has anything on it. If the scheduler serialises,
# `first` blocks the VM here and this program hangs.
b.write(b"B")
second.join()
print("second finished while first was still parked:", len(log) == 1)

a.write(b"A")
first.join()
print(log)
"#,
    );
    assert_eq!(
        out,
        "second finished while first was still parked: true\n\
         ['second got B', 'first got A']\n"
    );
}

/// A slow client does not stall a fast one. The whole point of the milestone.
///
/// The server is the shape every server is — one task per connection, blocking
/// reads and writes as far as the Oro code can tell — and the slow client holds
/// its connection open for 300 ms before saying anything. The fast client must
/// complete inside that window, not after it.
#[test]
fn a_slow_client_does_not_stall_a_fast_one() {
    let (out, took) = run_timed(
        "slow_and_fast",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")
addr = ln.local
order = []

def serve(conn):
    while true:
        b = conn.read(4096)
        if b == b"":
            break
        conn.write(b)
    conn.close()

def acceptor(n):
    conns = []
    for _, _ in range(n):
        conns.append(spawn(serve, ln.accept()))
    for _, t in conns:
        t.join()

def slow():
    c = net.dial(addr)
    time.sleep(0.3)
    c.write(b"slow")
    c.read(64)
    order.append("slow")
    c.close()

def fast():
    c = net.dial(addr)
    c.write(b"fast")
    c.read(64)
    order.append("fast")
    c.close()

acc = spawn(acceptor, 2)
s = spawn(slow)
f = spawn(fast)
f.join()
print("fast finished while slow was still waiting:", order == ["fast"])
s.join()
acc.join()
ln.close()
print("both finished:", order == ["fast", "slow"])
"#,
    );
    assert_eq!(
        out,
        "fast finished while slow was still waiting: true\nboth finished: true\n"
    );
    // The slow client sleeps 300 ms. Serialised, the fast one would have waited
    // behind it and the whole program would still take that long — so the time
    // is not the assertion, the ordering above is. This only catches a program
    // that somehow took *much* longer than the one sleep in it.
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

/// The hazard, directly: one task parked in a read while another closes the
/// same stream.
///
/// `src/net.rs` recorded this as the thing that turns into a `BorrowMutError`
/// **panic** — a dead process, not a catchable exception — the moment a task can
/// be suspended inside a socket call while holding the stream's `RefCell`
/// borrow. It is unreachable by construction now: `Park` has no lifetime
/// parameter, so a parking site cannot carry the borrow and would not compile.
///
/// "Unreachable by construction" is a claim, and this is how it is checked. The
/// exception must be an ordinary Oro `ValueError`, catchable, with the same
/// message the read would have produced had it been one instruction later.
#[test]
fn closing_a_stream_a_task_is_parked_on_raises_and_never_panics() {
    let out = run(
        "close_under_a_reader",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")
client = net.dial(ln.local)
conn = ln.accept()

def reader():
    try:
        conn.read(64)
        print("BUG: the read returned")
    except ValueError as e:
        print("reader:", e)

def closer():
    time.sleep(0.05)
    conn.close()
    print("closer: closed")

r = spawn(reader)
k = spawn(closer)
r.join()
k.join()
client.close()
ln.close()
print("the VM is still alive")
"#,
    );
    assert_eq!(
        out,
        "closer: closed\nreader: read() on a closed TcpStream\nthe VM is still alive\n"
    );
}

/// A write too big for the kernel's send buffer parks, and finishes.
///
/// §2 gives `write(b)` an all-or-raises contract and no return value, so a
/// partial write is the runtime's problem and no caller can be told about it.
/// The proof that it *is* partial is the peer: nothing reads for 150 ms, so the
/// send buffer fills long before 4 MB is out, and the third task's output has
/// to appear between the write starting and finishing.
#[test]
fn a_partial_write_parks_and_completes() {
    let out = run(
        "partial_write",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")
client = net.dial(ln.local)
conn = ln.accept()

payload = (b"x" * 1024) * 4096
progress = []

def sender():
    conn.write(payload)
    progress.append("write returned")
    conn.shutdown_write()

def meanwhile():
    for _, _ in range(3):
        progress.append("ran while the write was parked")
        yield_now()

def receiver():
    got = 0
    while true:
        b = client.read(65536)
        if b == b"":
            break
        got = got + len(b)
    progress.append("received " + got.to_str())

s = spawn(sender)
m = spawn(meanwhile)
time.sleep(0.15)
r = spawn(receiver)
s.join()
r.join()
m.join()
client.close()
conn.close()
ln.close()

print("the write parked and other tasks ran:", progress[0] != "write returned")
print("every byte arrived:", progress[len(progress) - 1] == "received 4194304")
"#,
    );
    assert_eq!(
        out,
        "the write parked and other tasks ran: true\nevery byte arrived: true\n"
    );
}

/// `time.sleep` parks the task, not the VM.
///
/// It was `std::thread::sleep`, which stopped every task in the process: one
/// task sleeping meant ten thousand connections sleeping. Three tasks sleeping
/// 300 ms each must therefore take about 300 ms in total, not 900.
#[test]
fn sleep_parks_the_task_and_not_the_vm() {
    let (out, took) = run_timed(
        "sleep_overlaps",
        r#"
import time

def nap(name):
    time.sleep(0.3)
    return name

a = spawn(nap, "a")
b = spawn(nap, "b")
c = spawn(nap, "c")
print(a.join(), b.join(), c.join())
"#,
    );
    assert_eq!(out, "a b c\n");
    // Serialised, this is 900 ms plus start-up. The margin is wide on both
    // sides on purpose: the claim is "they overlapped", and 600 ms is nowhere
    // near three sequential naps however loaded the machine is.
    assert!(
        took < Duration::from_millis(600),
        "three overlapping 300 ms sleeps took {took:?} — they were serialised"
    );
}

/// `p.wait()` parks the calling task, not the whole VM.
///
/// The regression: `wait()` used to reap on the VM thread, so a join on a slow
/// child froze every other task until it exited — defeating the streaming spawn
/// it completes. The claim is ordering, not just completion: a fast peer must
/// finish *while* the wait is still outstanding. With a blocking wait it cannot
/// run at all until the 400 ms child is gone; with a parking one it sends first.
#[test]
fn wait_parks_the_task_and_not_the_vm() {
    let (out, took) = run_timed(
        "wait_overlaps",
        r#"
import proc
import time

order = chan(cap=2)

def waiter():
    p = proc.spawn(["sleep", "0.4"])
    p.wait()
    order.send("waiter")

def fast():
    time.sleep(0.05)
    order.send("fast")

spawn(waiter)
spawn(fast)
print(order.recv(), order.recv())
"#,
    );
    assert_eq!(out, "fast waiter\n", "the fast task did not run while wait() was parked");
    // The child is 400 ms; overlapped, the whole thing is ~that. Blocked, it is
    // still ~400 ms but `fast` could not have gone first. Timing is the weak
    // check, the order above is the real one.
    assert!(took < Duration::from_millis(900), "took {took:?}");
}

/// A pending timer must not defeat the pipe/proc wake backstop.
///
/// The regression (BUG 2): `wait_for_external` applied the 20 ms backstop only
/// when *no* timer existed, so with one sleeping task a missed edge on the
/// edge-triggered pipe `Waker` waited for the timer instead — round-trips seen
/// at 4-60 s against a 22 ms worst case. The fix waits the *earlier* of the
/// timer and the backstop. Here a task sleeps a whole second (a timer pending
/// throughout) while another runs 120 spawn round-trips; none may stall.
#[test]
fn a_pending_timer_does_not_stall_pipe_roundtrips() {
    let out = run(
        "backstop_under_timer",
        r#"
import proc
import io
import time

def sleeper():
    time.sleep(1.0)          # a timer pending for the whole run

spawn(sleeper)
worst = 0.0
for i, _ in range(120):
    t0 = time.monotonic()
    p = proc.spawn(["cat"])
    p.stdin.write(b"ping")
    p.stdin.close()
    io.read(p.stdout)
    p.wait()
    dt = time.monotonic() - t0
    worst = dt > worst ? dt : worst
print(worst < 0.5 ? "roundtrips stayed fast" : f"STALLED at {worst}s")
"#,
    );
    assert_eq!(
        out, "roundtrips stayed fast\n",
        "a pipe round-trip stalled behind a pending timer"
    );
}

/// A tight loop of ready I/O operations yields so peers are not starved.
///
/// The regression (BUG 3): a ready operation never suspends, so an `io.copy`
/// over an always-ready source ran to EOF holding the VM, and another
/// connection waited behind the whole transfer (orogit hand-rolled a per-chunk
/// `yield_now` to cut a 49 ms index-page stall to 2 ms). The runtime now yields
/// every N ready ops. Here a 4 MiB in-memory copy — all ready, never blocking —
/// races a peer that only sends: with yielding the peer runs first, without it
/// the copy finishes before the peer is ever scheduled.
#[test]
fn a_fast_copy_loop_yields_to_its_peers() {
    let out = run(
        "copy_yields",
        r#"
import io

src = io.buffer(b"x" * (4 * 1024 * 1024))   # reads never block
sink = open("/dev/null", mode="w")

order = chan(cap=2)

def copier():
    io.copy(sink, src)
    order.send("copier")

def peer():
    order.send("peer")

spawn(copier)
spawn(peer)
print(order.recv())
"#,
    );
    assert_eq!(out, "peer\n", "the copy loop ran to EOF without yielding — a peer was starved");
}

/// The handler sees the client's address on `req.peer`.
///
/// The gap: brute-force damping needs the peer, and `serve`/`serve_conn` used to
/// hand the handler only the request, so a program grew its own accept loop just
/// to reach `conn.peer`. Now `serve_conn` stamps it on every request. Checked
/// end to end over a real loopback socket: the handler echoes its view of the
/// caller, which must be a `127.0.0.1` address.
#[test]
fn the_handler_sees_the_client_peer_address() {
    let out = run(
        "peer_addr",
        r#"
import http
import net

ready = chan(cap=1)

def server():
    ln = ready.recv()
    http.serve_conn(ln.accept(), req => http.Response(200, body=req.peer.to_bytes()))

ln = net.listen("127.0.0.1:0")
spawn(server)
ready.send(ln)
r = http.fetch("GET", f"http://{ln.local}/")
print(r.text().startswith("127.0.0.1:"))
"#,
    );
    assert_eq!(out, "true\n", "the handler did not see the client's peer address");
}

/// `static_files` serves a tree, and a directory without a trailing slash is a
/// 301 to the slashed form. End to end over a real socket.
#[test]
fn static_files_serves_and_redirects_a_directory() {
    let out = run(
        "static_files",
        r#"
import http
import net
import os
import proc

base = "/tmp/oro_static_test"
proc.run(["rm", "-rf", base], quiet=true, check=false)
os.mkdir(base)
os.mkdir(base + "/docs")
open(base + "/docs/index.html", mode="w").write(b"<h1>docs</h1>")
open(base + "/style.css", mode="w").write(b"body{}")

ready = chan(cap=1)

def server():
    ln = ready.recv()
    for i, _ in range(3):
        http.serve_conn(ln.accept(), http.static_files(base))

ln = net.listen("127.0.0.1:0")
spawn(server)
ready.send(ln)
u = f"http://{ln.local}"
css = http.fetch("GET", u + "/style.css")
dir_slash = http.fetch("GET", u + "/docs/")
dir_bare = http.fetch("GET", u + "/docs")
print(css.status, css.header("content-type"))
print(dir_slash.status, dir_slash.text())
print(dir_bare.status, dir_bare.header("location"))
proc.run(["rm", "-rf", base], quiet=true, check=false)
"#,
    );
    assert_eq!(
        out,
        "200 text/css; charset=utf-8\n200 <h1>docs</h1>\n301 /docs/\n",
        "static serving or the /docs -> /docs/ redirect is wrong"
    );
}

/// A task keeps running while another one's `set_timeout` expires.
///
/// Two things are being checked at once, and both were broken before M3b. The
/// deadline still raises `TimeoutError` with CPython's bare "timed out"
/// message, which is what `net::classify` has always keyed on; and the timer
/// list is what enforces it, so the ticker's output has to be interleaved with
/// the wait rather than queued behind it.
#[test]
fn a_read_timeout_fires_while_another_task_keeps_running() {
    let out = run(
        "read_timeout",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")
client = net.dial(ln.local)
conn = ln.accept()
conn.set_timeout(0.2)

ticks = []

def reader():
    try:
        conn.read(64)
        print("BUG: the read returned")
    except TimeoutError as e:
        print("reader:", e, "after", len(ticks), "ticks elapsed")

def ticker():
    for _, i in range(4):
        ticks.append(i)
        time.sleep(0.05)

r = spawn(reader)
t = spawn(ticker)
r.join()
t.join()
client.close()
conn.close()
ln.close()
"#,
    );
    assert_eq!(out, "reader: timed out after 4 ticks elapsed\n");
}

/// A deadline covers the whole operation, not each syscall inside it.
///
/// `SO_RCVTIMEO` restarted on every `recv`, so a client dribbling one byte
/// every 100 ms could hold a socket with a 200 ms timeout open indefinitely —
/// the classic slowloris. The deadline is the scheduler's now, fixed when the
/// operation first blocks and held across every re-park, so it expires.
#[test]
fn a_deadline_bounds_the_whole_operation_not_each_packet() {
    let (out, took) = run_timed(
        "slowloris",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")
client = net.dial(ln.local)
conn = ln.accept()
conn.set_timeout(0.25)

stop = []

def dribble():
    i = 0
    while i < 10 and len(stop) == 0:
        client.write(b"a")
        time.sleep(0.1)
        i = i + 1

def victim():
    try:
        conn.read_until(b"\n", 4096)
        print("BUG: the header arrived")
    except TimeoutError as e:
        stop.append(true)
        print("victim:", e)

d = spawn(dribble)
v = spawn(victim)
v.join()
d.join()
print("gave up rather than being held open")
client.close()
conn.close()
ln.close()
"#,
    );
    assert_eq!(out, "victim: timed out\ngave up rather than being held open\n");
    // Ten dribbles at 100 ms is a second; the deadline is 250 ms and has to
    // win. The spawned dribbler is abandoned when main returns.
    assert!(took < Duration::from_millis(900), "took {took:?}");
}

/// Two tasks reading one socket is named, not raced.
///
/// One registration per fd is one waiter per direction, and the alternative to
/// saying so is a queue over a resource where "whose bytes are whose" has no
/// answer. A reader *and* a writer on the same socket is a different thing —
/// that is `io.copy` in both directions, and it works.
#[test]
fn two_readers_on_one_socket_are_a_named_error_and_a_reader_plus_writer_is_not() {
    let out = run(
        "one_waiter_per_direction",
        r#"
import net

ln = net.listen("127.0.0.1:0")
client = net.dial(ln.local)
conn = ln.accept()

def read_it():
    conn.read(64)

def read_it_too():
    try:
        conn.read(64)
        print("BUG: two readers were allowed")
    except Exception as e:
        print("second reader:", e.args[0].split(sep=" (task")[0])

a = spawn(read_it)
b = spawn(read_it_too)
b.join()
client.write(b"x")
a.join()
print("the first reader still got its bytes")
client.close()
conn.close()
ln.close()
"#,
    );
    assert_eq!(
        out,
        "second reader: two tasks cannot read the same TcpStream at once\n\
         the first reader still got its bytes\n"
    );
}

/// The deadlock diagnostic learned the difference between "nobody can proceed"
/// and "everybody is waiting for the kernel".
///
/// §3's condition became "nothing ready **and** nothing registered" when the
/// reactor landed, and both halves are load-bearing in opposite directions.
/// Keeping only the old half turns every server into a spurious deadlock
/// report — a listener parked in `accept` with no other task runnable is the
/// *normal* state of an idle server, and before M3b that exact shape would have
/// been reported as one. Dropping the check turns every real deadlock into a
/// silent hang.
#[test]
fn the_deadlock_diagnostic_learned_the_difference() {
    // Half one: everything parked, but on the reactor. Main waits in `accept`
    // and the only other task is asleep, so the ready queue is empty and every
    // task is blocked — the old condition exactly. It must wait, and be woken.
    let out = run(
        "not_a_deadlock",
        r#"
import net
import time

ln = net.listen("127.0.0.1:0")

def dialer():
    time.sleep(0.05)
    c = net.dial(ln.local)
    c.write(b"hello")
    c.close()

d = spawn(dialer)
conn = ln.accept()
print("accepted:", conn.read(16).to_str())
d.join()
conn.close()
ln.close()
"#,
    );
    assert_eq!(out, "accepted: hello\n");

    // Half two: nothing registered, so nobody outside the VM can wake anyone.
    // Still a deadlock, still named, still says which wait each task is in.
    let (_, err) = run_failing(
        "still_a_deadlock",
        r#"
ch = chan()
ch.recv()
"#,
    );
    assert!(err.contains("deadlock: every task is blocked"), "{err}");
    assert!(err.contains("recv (channel cap 0, 0 buffered)"), "{err}");
}

// --- DNS: the last thing in `net` that stopped the world -----------------------
//
// The rules at the top of this file apply here with one addition that matters
// more than all of them: **no test may resolve a public hostname.** A DNS test
// that needs the network is a test that fails on a plane, and a test that fails
// on a plane is worse than no test. Everything below resolves either
// `localhost` — which `/etc/hosts` answers without a packet — or a name that
// cannot be put on the wire at all.

/// A name that fails to resolve *without leaving the machine*.
///
/// A DNS label is at most 63 bytes on the wire, so a 300-byte label cannot be
/// encoded into a query at all: the resolver rejects it while building the
/// packet and answers `EAI_NONAME` having sent nothing. That is what makes it
/// usable here — a name like `nonesuch.invalid` would *also* fail, but only
/// after a round trip to whatever nameserver the machine is configured with,
/// which on a disconnected laptop is a five-second timeout and on a plane is a
/// failing test.
const UNRESOLVABLE: &str = "\"a\" * 300";

/// The load-bearing test: a `dial` on a hostname **parks**, and the VM keeps
/// scheduling while the lookup is out.
///
/// This is the property the whole change exists for, and the one every previous
/// test in this tree was structurally unable to see: they all dial
/// `127.0.0.1`, which does no lookup, so a resolver that froze the VM for five
/// seconds passed all of them.
///
/// It is checked by **order, not by time**, and that is deliberate. `localhost`
/// comes out of `/etc/hosts` in microseconds, so no wall-clock margin could
/// separate "parked" from "blocked" — but the scheduler's behaviour separates
/// them completely. A parked lookup means the dialling task is off the ready
/// queue, so the peer runs to completion before the answer is ever drained; a
/// blocking one means `lookup finished` lands second, before the peer has had a
/// single turn. Nothing about that depends on how fast the machine is, which
/// makes it a stronger check than the timing tests above rather than a weaker
/// one.
#[test]
fn a_hostname_dial_parks_and_the_vm_keeps_scheduling() {
    let out = run(
        "dns_parks",
        r#"
import net

ln = net.listen("127.0.0.1:0")
host = "localhost:" + ln.local.split(sep=":")[1]

log = []

def dialer():
    log.append("lookup started")
    c = net.dial(host)
    log.append("lookup finished")
    c.close()

def worker():
    # Four turns. Every one of them is a scheduling decision the VM could only
    # have made with the dialling task suspended.
    for _, i in range(4):
        log.append("worker " + i.to_str())
        yield_now()

d = spawn(dialer)
w = spawn(worker)
d.join()
w.join()
ln.close()

for _, line in log:
    print(line)
"#,
    );
    assert_eq!(
        out,
        "lookup started\nworker 0\nworker 1\nworker 2\nworker 3\nlookup finished\n",
        "the lookup did not park — `lookup finished` should come after every turn the \
         peer took, and with a blocking resolve it comes second"
    );
}

/// A failed lookup raises in the task that asked for it, with the exception it
/// has always raised, and does not disturb anyone else.
///
/// Two things are being checked at once. The class and the message must be
/// unchanged now that the error is produced on a helper thread and delivered
/// through a channel — it is `OSError` with `[Errno -2]`, the same string
/// `crate::net::resolve` has always returned, because it is literally the same
/// `String` moved across. And the failure must be *local to one task*: a
/// resolver error arriving from outside the VM is exactly the shape that, done
/// carelessly, takes down the peer that was in the middle of its own dial.
#[test]
fn a_failed_lookup_raises_in_its_own_task_and_leaves_the_others_alone() {
    let out = run(
        "dns_failure_is_local",
        &format!(
            r#"
import net

ln = net.listen("127.0.0.1:0")
host = "localhost:" + ln.local.split(sep=":")[1]
bad = ({UNRESOLVABLE}) + ":80"

def doomed():
    try:
        net.dial(bad)
        return "resolved, which cannot happen"
    except OSError as e:
        return "raised " + e.to_str().split(sep=":")[0]

def fine():
    c = net.dial(host)
    peer = c.peer
    c.close()
    return "dialled " + peer.split(sep=":")[0]

a = spawn(doomed)
b = spawn(fine)
print(a.join())
print(b.join())
ln.close()
"#
        ),
    );
    assert_eq!(
        out,
        "raised [Errno -2] Name or service not known\ndialled 127.0.0.1\n",
        "a failed lookup must raise OSError in its own task and nowhere else"
    );
}

/// Several lookups at once, more than the pool has threads.
///
/// Twelve against a cap of eight, so the last four queue behind the first eight
/// rather than each getting a thread — which is the behaviour under test as
/// much as the parallelism is. A queued lookup still blocks nothing but itself:
/// every one of the twelve tasks completes, and the program finishes.
#[test]
fn many_lookups_run_at_once_and_the_rest_queue() {
    let out = run(
        "dns_concurrent",
        r#"
import net

ln = net.listen("127.0.0.1:0")
host = "localhost:" + ln.local.split(sep=":")[1]

def dial_one(n):
    c = net.dial(host)
    c.close()
    return n

tasks = []
for _, i in range(12):
    tasks.append(spawn(dial_one, i))

total = 0
for _, t in tasks:
    total = total + t.join()
ln.close()
print("resolved", len(tasks), "sum", total)
"#,
    );
    assert_eq!(out, "resolved 12 sum 66\n");
}

/// A lookup in flight when the program ends, both ways it can end.
///
/// The resolver threads are never joined — a VM shutting down must not wait out
/// a five-second resolver timeout, which is the very stall this mechanism
/// exists to remove — so "an answer arriving for a VM that has gone" is a state
/// that has to be safe rather than avoided. It is: the worker finds the answer
/// channel's receiver dropped and returns without waking anything.
///
/// The other direction is the one a program can observe. Main returning with a
/// task still resolving is §3's implicit join-all, and it has to wait for the
/// lookup like any other park — a lookup is not an excuse to abandon a task.
#[test]
fn a_lookup_in_flight_at_shutdown_neither_hangs_nor_is_abandoned() {
    // `sys.exit` from under a lookup: the documented way to abandon in-flight
    // work on purpose. It must exit promptly and cleanly.
    let (out, took) = run_timed(
        "dns_exit_in_flight",
        r#"
import net
import sys

def dialer():
    net.dial("localhost:9")
    print("unreachable: the program exits first")

spawn(dialer)
# Hand over, so the dialler reaches its park before main runs again.
yield_now()
print("exiting with a lookup still out")
sys.exit(0)
"#,
    );
    assert_eq!(out, "exiting with a lookup still out\n");
    assert!(
        took < Duration::from_secs(5),
        "shutting down took {took:?} — a VM must not wait for a resolver thread"
    );

    // Main returning, with a task still resolving. The implicit join-all owes
    // that task the same wait it owes any other park.
    let out = run(
        "dns_join_all_waits",
        r#"
import net

ln = net.listen("127.0.0.1:0")
host = "localhost:" + ln.local.split(sep=":")[1]

def dialer():
    c = net.dial(host)
    print("the join-all waited for the lookup")
    c.close()
    ln.close()

spawn(dialer)
"#,
    );
    assert_eq!(out, "the join-all waited for the lookup\n");
}

/// Dialling a literal `ip:port` takes no lookup path at all.
///
/// The observable end of that claim: a literal dial behaves exactly as it did
/// before any of this, and in particular still finishes inside `connect(2)` on
/// loopback rather than going near the reactor. That it starts no resolver
/// thread — the part a program cannot see — is asserted directly against the
/// reactor in `crate::vm::tests`, and the decision itself is checked in
/// `crate::net::tests`.
#[test]
fn a_literal_ip_dials_exactly_as_it_always_did() {
    let out = run(
        "dns_literal_unchanged",
        r#"
import net

ln = net.listen("127.0.0.1:0")
c = net.dial(ln.local)
s = ln.accept()
c.write(b"no lookup here")
print(s.read(32).to_str())
c.close()
s.close()
ln.close()
"#,
    );
    assert_eq!(out, "no lookup here\n");
}

/// The connect walks the resolved address list instead of trying only the first.
///
/// `getaddrinfo("localhost")` answers `::1` before `127.0.0.1` on a dual-stack
/// host, and the listener here is on `127.0.0.1` — so reaching it means the
/// first address was tried, refused, and fallen through from. The blocking
/// `TcpStream::connect(&addrs[..])` did that walk for free; doing it across
/// parks is code, and this is the check that the code is there.
///
/// On a host with no IPv6 the list has one entry and this asserts the same
/// thing about a shorter walk, which is why the assertion is on where the
/// connection *landed* rather than on how many addresses were tried.
#[test]
fn a_dial_falls_through_to_the_next_address() {
    let out = run(
        "dns_address_fallback",
        r#"
import net

ln = net.listen("127.0.0.1:0")
host = "localhost:" + ln.local.split(sep=":")[1]

c = net.dial(host)
s = ln.accept()
c.write(b"fell through")
print(c.peer.split(sep=":")[0], s.read(32).to_str())
c.close()
s.close()
ln.close()
"#,
    );
    assert_eq!(out, "127.0.0.1 fell through\n");
}
