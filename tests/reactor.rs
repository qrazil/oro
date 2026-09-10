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
    i = 0
    while i < n:
        conns.append(spawn(serve, ln.accept()))
        i = i + 1
    for t in conns:
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
    i = 0
    while i < 3:
        progress.append("ran while the write was parked")
        yield_now()
        i = i + 1

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
    i = 0
    while i < 4:
        ticks.append(i)
        time.sleep(0.05)
        i = i + 1

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
        print("second reader:", e.args[0].split(" (task")[0])

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
