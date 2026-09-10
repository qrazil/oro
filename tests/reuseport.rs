//! Scale-out, end to end: N `oro` **processes** sharing one port.
//!
//! `src/net/tests.rs` checks that two listeners on one port both receive
//! connections. This file checks the thing that claim is *for*: that Oro's
//! actual deployment shape — N separate operating-system processes, each with
//! its own VM, its own ready queue and nothing shared with the others, each
//! running the same `net.listen(addr, reuseport=true)` — scales a server across
//! cores. Same kernel mechanism, but nothing is taken on faith here: these are
//! real processes started from the real binary, and what is counted is what
//! each of them accepted.
//!
//! That distinction is the reason for the file. A test with two listeners in
//! one process could pass while the feature was unusable in production for a
//! reason that only appears across processes — the option set after `bind`,
//! say, which lets a *second* differently-configured socket join and would
//! therefore never fail in a single-process test, but makes the second
//! identical worker die with `EADDRINUSE`. That mistake is caught here and
//! nowhere else.
//!
//! The rules are `tests/reactor.rs`'s, unchanged:
//!
//! * **Ephemeral ports only.** The port is one the kernel chose, and it is held
//!   by a live socket for the whole window in which it could be stolen.
//! * **Loopback only**, and no name is ever resolved.
//! * **Nothing may hang CI.** Every worker carries its own shutdown timer and
//!   every wait in this file has a deadline; a stuck worker fails the run in
//!   seconds instead of stalling it.
//! * **Programs print claims, not dumps.**

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The hang guard. Nothing here should take more than a second or two; this
/// only ever fires on a bug, and when it does it says so.
const GUARD: Duration = Duration::from_secs(30);

/// How long a worker accepts before its timer closes the listener.
///
/// Deliberately enormous relative to the work — the parent's connections are
/// sixty-odd loopback `connect(2)`s, which take single-digit milliseconds — so
/// that this is a shutdown mechanism and never a race. A margin of three orders
/// of magnitude is what keeps this test from being the one that fails on a
/// loaded CI box.
const WORKER_SECONDS: f64 = 5.0;

/// Workers, and connections made to them.
///
/// Three workers rather than two because the failure this is really watching
/// for — one worker taking everything — is not distinguishable from an uneven
/// split at N=2 as clearly as it is at N=3. The connection count is sized so
/// that "every worker got at least one" is a certainty rather than a
/// probability: the kernel picks by hashing each connection's 4-tuple, so with
/// 90 independent draws across 3 sockets, a shut-out worker is not something
/// that happens.
const WORKERS: usize = 3;
const CONNS: usize = 90;

/// The worker program. `{ADDR}` is substituted before it is written out.
///
/// It is deliberately the most ordinary server this language can express: bind,
/// accept in a loop, close each connection, print a count. The only thing in it
/// that is about scale-out at all is the one keyword argument — which is the
/// claim being made. `serve` did not have to change for this, and neither did
/// anything else above the listener.
const WORKER: &str = r#"
import net
import time

ln = net.listen("{ADDR}", reuseport=true)
count = 0

# The shutdown. A listener has no timeout by design (§4), so the way to bound
# one is a task and a flag — here, a task that closes the listener, which makes
# the parked `accept` raise. That is the documented behaviour of closing a
# stream a task is parked on, and it is what ends this program.
def stopper():
    time.sleep({SECONDS})
    ln.close()

s = spawn(stopper)
print("READY")

# `conn` is bound before the loop, and that is a workaround rather than a
# style: on this branch a name whose *first* assignment is inside a `try:`
# body is not visible after the `try` statement — `try: x = 1` then `print(x)`
# raises `NameError` where CPython prints `1`. It is not this feature's bug
# (it reproduces at the base commit, with no sockets in it at all, and it is
# reported separately), but this test would trip over it, so the name is
# pre-bound. Remove this line once that is fixed and the test should still
# pass.
conn = null

while true:
    try:
        conn = ln.accept()
    except ValueError:
        break
    count = count + 1
    conn.close()

s.join()
print("ACCEPTED", count)
"#;

/// A worker process, and the scratch file it was written to.
struct Worker {
    child: Child,
    path: PathBuf,
}

/// Write `src` to a scratch file and start it under the real `oro` binary.
fn start(name: &str, src: &str) -> Worker {
    let dir = std::env::temp_dir().join("oro_reuseport_tests");
    std::fs::create_dir_all(&dir).expect("scratch directory");
    let path = dir.join(format!("{name}.oro"));
    let mut f = std::fs::File::create(&path).expect("write the program");
    f.write_all(src.as_bytes()).expect("write the program");
    drop(f);

    let child = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_oro")))
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch the oro binary");
    Worker { child, path }
}

/// Wait for a worker to exit, and return its stdout.
///
/// Killed and failed at [`GUARD`] rather than waited on forever, so a worker
/// whose shutdown timer did not fire is a red test rather than a hung suite.
fn finish(mut w: Worker, name: &str) -> String {
    let deadline = Instant::now() + GUARD;
    loop {
        match w.child.try_wait().expect("wait on the worker") {
            Some(_) => break,
            None if Instant::now() >= deadline => {
                let _ = w.child.kill();
                let _ = w.child.wait();
                panic!("{name} did not finish within {GUARD:?}");
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let out = w.child.wait_with_output().expect("collect the worker's output");
    let _ = std::fs::remove_file(&w.path);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{name} exited with failure\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

/// The `ACCEPTED n` line a worker ends with.
fn accepted(stdout: &str, name: &str) -> usize {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("ACCEPTED "))
        .unwrap_or_else(|| panic!("{name} printed no count:\n{stdout}"));
    line.trim().parse().unwrap_or_else(|_| panic!("{name}: bad count {line:?}"))
}

/// **The test the feature exists for.** Three processes, one port, and the
/// connections spread across all three.
///
/// ## How the port is chosen without a race
///
/// The parent binds `127.0.0.1:0` with `SO_REUSEPORT` itself and reads back the
/// port the kernel picked. It then **keeps that socket open** while the workers
/// start, which is what makes the choice safe: nothing else on the machine can
/// take the port, because it is held, and the workers can still join it,
/// because it was bound with the option. Only once every worker has printed
/// `READY` — and therefore holds the port too — does the parent drop its own
/// listener and start connecting. There is no window in which the port is
/// unheld, and no sleep anywhere in the arrangement.
///
/// The parent's listener is dropped before any connection is made for a second
/// reason: while it was in the reuseport group the kernel would have given it a
/// share of the connections, and it never accepts. Leaving it open would lose
/// roughly a quarter of them to a socket with no reader.
#[test]
fn three_processes_share_one_port_and_all_three_get_connections() {
    // Held for the whole startup window — see the doc comment.
    let holder = oro_lang::net::listen("127.0.0.1:0", true).expect("bind with reuseport");
    let addr = holder.addr_attr("local").expect("read the port back");

    let src = WORKER
        .replace("{ADDR}", &addr)
        .replace("{SECONDS}", &WORKER_SECONDS.to_string());

    let mut workers = Vec::new();
    for i in 0..WORKERS {
        workers.push((format!("worker{i}"), start(&format!("worker{i}"), &src)));
    }

    // Every worker holds the port before the parent lets go of it. Waiting on
    // `READY` rather than sleeping is what makes this deterministic: a slow
    // machine makes the test slower, never flakier.
    let deadline = Instant::now() + GUARD;
    for (name, w) in &mut workers {
        wait_for_ready(w, name, deadline);
    }
    drop(holder);

    let conns: Vec<_> = (0..CONNS)
        .map(|i| {
            std::net::TcpStream::connect(&addr)
                .unwrap_or_else(|e| panic!("connection {i} to {addr}: {e}"))
        })
        .collect();
    // Closed before the workers are collected, so that a worker blocked reading
    // is not what the guard catches.
    drop(conns);

    let counts: Vec<usize> = workers
        .into_iter()
        .map(|(name, w)| accepted(&finish(w, &name), &name))
        .collect();

    let total: usize = counts.iter().sum();
    assert_eq!(total, CONNS, "connections went missing: {counts:?}");
    assert!(
        counts.iter().all(|&n| n > 0),
        "the kernel did not spread the load — one worker took everything: {counts:?}"
    );
    // Printed so a human reading a CI log sees the split rather than only that
    // an assertion held.
    println!("{CONNS} connections across {WORKERS} processes: {counts:?}");
}

/// Block until a worker prints `READY`, or fail at the deadline.
///
/// Reads the child's stdout on a helper thread would be the general solution;
/// this one is simpler and sufficient because `READY` is the first line the
/// worker prints and the pipe is never full. The worker is checked for early
/// death on each turn, so a worker that failed to bind fails this test with its
/// own error rather than with a timeout.
fn wait_for_ready(w: &mut Worker, name: &str, deadline: Instant) {
    use std::io::Read;
    let mut out = w.child.stdout.take().expect("worker stdout is piped");
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        assert!(Instant::now() < deadline, "{name} never printed READY");
        if let Some(status) = w.child.try_wait().expect("wait on the worker") {
            let mut rest = String::new();
            let _ = w.child.stderr.take().map(|mut e| e.read_to_string(&mut rest));
            panic!(
                "{name} exited ({status}) before it was ready\n--- stdout ---\n{}\n--- stderr ---\n{rest}",
                String::from_utf8_lossy(&buf)
            );
        }
        match out.read(&mut byte) {
            Ok(0) => panic!("{name} closed stdout before printing READY"),
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(b"READY\n") {
                    break;
                }
            }
            Err(e) => panic!("{name}: reading stdout: {e}"),
        }
    }
    w.child.stdout = Some(out);
}

/// The keyword is the spelling, and a wrong one is a clean error rather than a
/// silently ignored argument.
///
/// The failure mode this rules out is the quiet one: an unknown keyword that is
/// swallowed would make `net.listen(addr, reuseprot=true)` a server that binds
/// without the option and scales to one core, with nothing anywhere to say so.
#[test]
fn the_keyword_is_checked_and_a_typo_is_not_swallowed() {
    let w = start(
        "listen_kwargs",
        r#"
import net

ln = net.listen("127.0.0.1:0", reuseport=true)
print("reuseport listener:", ln.local != "")
ln.close()

plain = net.listen("127.0.0.1:0", reuseport=false)
print("explicit false is the default:", plain.local != "")
plain.close()

try:
    net.listen("127.0.0.1:0", reuseprot=true)
except TypeError as e:
    print("typo:", e)

try:
    net.listen("127.0.0.1:0", reuseport=1)
except TypeError as e:
    print("wrong type:", e)
"#,
    );
    let out = finish(w, "listen_kwargs");
    assert_eq!(
        out,
        "reuseport listener: true\n\
         explicit false is the default: true\n\
         typo: listen() got an unexpected keyword argument 'reuseprot'\n\
         wrong type: listen() reuseport argument must be bool, not 'int'\n"
    );
}
