//! `proc.spawn`'s live handle: a child process whose three standard streams are
//! [`OroStream`](crate::stream::OroStream) pipes the program reads and writes as
//! it pleases, rather than buffers handed back whole.
//!
//! **Why this exists next to `proc.run`.** `proc.run` captures the child's
//! output into `bytes` — bounded at 64 MiB, because a `bytes` is one allocation
//! and an unbounded one is an out-of-memory kill. That is exactly right for the
//! common case (run a command, look at what it said) and exactly wrong for the
//! case this handle is for: a child that produces *more output than fits in
//! memory* — a `git upload-pack` streaming a multi-gigabyte pack, a `tar` piped
//! to a socket. `proc.spawn` never buffers the whole of anything: output arrives
//! in chunks over a bounded channel, so a program can `io.copy(conn, p.stdout)`
//! a stream of any size in constant memory. The rule is `docs/reference.md`'s:
//! `run` when you want the output *whole*, `spawn` when you want it to *flow*.
//!
//! **What is Rust and what is not.** The three pipe ends are ordinary streams
//! (`crate::stream`); the only thing here that must be Rust is the [`Child`]
//! itself — `wait()` reaps it and `kill()` on drop is what keeps an abandoned
//! handle from leaking a process. Every pipe read or write parks through the
//! scheduler like a socket does, on helper threads that carry bytes (never
//! `Value`s) across the boundary — the same shape the DNS pool uses, for the
//! same reason: the OS API is blocking-only, so someone waits on a thread, and
//! the choice is only whose.
//!
//! **Not user-constructible**, like [`TaskHandle`](crate::task::TaskHandle):
//! there is no `Proc(...)` class, only `proc.spawn`.

use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::process::{Child, ExitStatus};
use std::sync::mpsc::{sync_channel, Receiver};
use std::sync::Arc;

use crate::stream::OroStream;
use crate::value::Value;

/// Bytes moved per chunk, and the bound on how many chunks may be in flight on
/// one pipe before the helper thread blocks.
///
/// `CHUNK` is `crate::stream`'s `BUFSIZE`: a chunk must fit the reader's refill
/// buffer, which is that size, so no byte is ever dropped when a chunk is copied
/// into it. `CAP` chunks — 32 × 8 KiB = 256 KiB — is the whole of the streaming
/// buffer: large enough that a briefly-slow consumer does not stall the child on
/// every chunk, small enough that it is real backpressure and not a second
/// hidden 64 MiB `proc.run`-style buffer. When the channel is full the reader
/// thread blocks on `send`, the child's pipe then fills, and the child blocks on
/// `write` — so `io.copy` moves a stream of any size in 256 KiB, not in its
/// length. Both are tunable, not API (`docs/stdlib-server-design.md` §7's
/// "buffer sizes left unfrozen"), exactly like the DNS pool size.
const CHUNK: usize = 8 * 1024;
const CAP: usize = 32;

/// Drain a child's stdout or stderr on a helper thread, and hand back the read
/// end of the pipe stream the VM reads.
///
/// The thread reads the blocking child fd in `CHUNK`-sized bites and sends each
/// down a **bounded** channel, signalling `waker` after every send so the
/// scheduler's [`drain_pipes`](crate::vm) re-attempts a parked reader. The bound
/// is the backpressure: a full channel blocks the `send`, which lets the child's
/// pipe fill and the child block, so nothing buffers without limit. The final
/// `wake` after the loop is what a consumer parked for EOF needs — dropping the
/// sender is the EOF the reader sees, and it produces no send to wake on, so the
/// thread wakes once more on its way out.
pub fn drain_to_stream(
    src: impl Read + Send + 'static,
    waker: Arc<mio::Waker>,
    label: &str,
) -> OroStream {
    let (tx, rx) = sync_channel::<Vec<u8>>(CAP);
    let mut src = src;
    std::thread::spawn(move || {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match src.read(&mut buf) {
                Ok(0) => break, // EOF: dropping `tx` is the receiver's EOF.
                Ok(n) => {
                    // The consumer's `Rc`s are gone (handle dropped): stop, and
                    // let dropping `src` close our end of the pipe.
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                    let _ = waker.wake();
                }
                // A signal, not an end. Read again.
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // Wake a consumer parked for the EOF the dropped `tx` now signals.
        let _ = waker.wake();
    });
    OroStream::pipe_read(rx, label)
}

/// Feed a child's stdin from a helper thread, and hand back the write end of the
/// pipe stream the VM writes.
///
/// The mirror of [`drain_to_stream`]: the thread receives owned chunks from a
/// bounded channel and `write_all`s each to the child, signalling `waker` after
/// every one so a writer parked on a full channel is re-attempted. Dropping the
/// sender — `p.stdin.close()`, or the handle going away — ends the thread's
/// `recv`, and dropping the child's stdin then closes it, so the child reads
/// EOF. A write that fails (the child closed its stdin, or exited) ends the
/// thread; a writer parked on it is woken to find the channel disconnected and
/// raises `BrokenPipe`, the same a dead socket peer gives.
pub fn feed_from_stream(
    dst: impl Write + Send + 'static,
    waker: Arc<mio::Waker>,
    label: &str,
) -> OroStream {
    let (tx, rx) = sync_channel::<Vec<u8>>(CAP);
    let mut dst = dst;
    std::thread::spawn(move || {
        while let Ok(chunk) = rx.recv() {
            if dst.write_all(&chunk).is_err() {
                break;
            }
            let _ = waker.wake();
        }
        // Dropping `dst` here closes the child's stdin (its EOF). Wake a writer
        // parked on a now-disconnected channel so it raises rather than hangs.
        let _ = waker.wake();
    });
    OroStream::pipe_write(tx, label)
}

/// The handle `proc.spawn` returns.
///
/// Holds the three pipe streams as ordinary [`Value`]s and the live child. The
/// reaping is **off the VM thread**: `wait()` must park the calling task, not
/// freeze every other task at the join, so the child is `wait(2)`-ed on a helper
/// thread that delivers the code through `code_rx` and signals the pipe `Waker`
/// — the same shape the pipes and the DNS pool use. See `Vm::do_proc_wait`.
pub struct Proc {
    /// The command as spawned, for `repr()`. Never mutated after construction.
    pub args: Vec<String>,
    /// The child, until it is handed to a reaper — by [`ensure_waiter`] when
    /// `wait()` is first called, or by `drop` when the handle is discarded
    /// unwaited. `None` once handed off, so it is reaped exactly once.
    child: RefCell<Option<Child>>,
    /// The exit code, once known: cached the first time it is delivered so every
    /// later `wait()` (and any second waiter) reads it without a second reap.
    code: Cell<Option<i64>>,
    /// The reaper thread's delivery end, present once `ensure_waiter` has started
    /// it. Read non-blocking by [`try_code`](Self::try_code).
    code_rx: RefCell<Option<Receiver<i64>>>,
    /// Write end (`Backing::PipeWrite`), or a closed stream after
    /// `p.stdin.close()`. Feeding the child is `p.stdin.write(b)`.
    pub stdin: Value,
    /// Read ends (`Backing::PipeRead`). Draining the child is `p.stdout.read(n)`
    /// / `io.copy(dst, p.stdout)`.
    pub stdout: Value,
    pub stderr: Value,
}

impl Proc {
    pub fn new(
        args: Vec<String>,
        child: Child,
        stdin: Value,
        stdout: Value,
        stderr: Value,
    ) -> Proc {
        Proc {
            args,
            child: RefCell::new(Some(child)),
            code: Cell::new(None),
            code_rx: RefCell::new(None),
            stdin,
            stdout,
            stderr,
        }
    }

    /// The exit code if it is already known, without touching the reaper.
    pub fn cached_code(&self) -> Option<i64> {
        self.code.get()
    }

    /// Start the reaper thread, once. It owns the child, blocks in `wait(2)` off
    /// the VM thread, sends the exit code down `code_rx`, and signals `waker` so
    /// [`Vm::drain_procs`] wakes the parked task — never blocking the VM.
    /// Idempotent: a second `wait()`, or a `wait()` after the code is known, is a
    /// no-op.
    pub fn ensure_waiter(&self, waker: Arc<mio::Waker>) {
        if self.code.get().is_some() || self.code_rx.borrow().is_some() {
            return;
        }
        let Some(mut child) = self.child.borrow_mut().take() else {
            return;
        };
        let (tx, rx) = sync_channel::<i64>(1);
        *self.code_rx.borrow_mut() = Some(rx);
        std::thread::spawn(move || {
            let code = child.wait().map(|s| exit_code(&s)).unwrap_or(-1);
            let _ = tx.send(code);
            let _ = waker.wake();
        });
    }

    /// The exit code if known *now* — the cache, or a fresh delivery from the
    /// reaper (then cached). `None` means the child is still running.
    pub fn try_code(&self) -> Option<i64> {
        if let Some(c) = self.code.get() {
            return Some(c);
        }
        let got = self
            .code_rx
            .borrow()
            .as_ref()
            .and_then(|rx| rx.try_recv().ok());
        if let Some(c) = got {
            self.code.set(Some(c));
        }
        got
    }

    /// `<Proc ['git', 'upload-pack', ...]>` — the command, and whether it is
    /// still running. Enough to tell two handles apart in a REPL without
    /// promising a stable format.
    pub fn repr(&self) -> String {
        let cmd = self
            .args
            .iter()
            .map(|a| format!("'{a}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let state = match self.code.get() {
            Some(c) => format!("exited {c}"),
            None => "running".to_string(),
        };
        format!("<Proc [{cmd}] {state}>")
    }
}

/// An `ExitStatus` as one integer: the exit code, or `-signal` for a child a
/// signal killed (which has no code), matching CPython.
fn exit_code(status: &ExitStatus) -> i64 {
    status
        .code()
        .map(i64::from)
        .unwrap_or_else(|| signal_code(status))
}

/// A child killed by a signal has no exit code; report `-signal` as CPython
/// does. On platforms without `ExitStatusExt` there is nothing to report but
/// the placeholder.
#[cfg(unix)]
fn signal_code(status: &std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    status.signal().map(|s| -i64::from(s)).unwrap_or(-1)
}

#[cfg(not(unix))]
fn signal_code(_status: &std::process::ExitStatus) -> i64 {
    -1
}

impl Drop for Proc {
    /// A handle dropped without `wait()` does not leak the child: it is killed
    /// and reaped, the same deterministic cleanup refcounting gives a file (§3's
    /// reason there is no `with`). The kill and the reap run **on a detached
    /// thread**, not here, so `drop` never blocks the VM — a child wedged in an
    /// uninterruptible syscall must not freeze every other task while it is
    /// reaped. If `wait()` already handed the child to its reaper, `child` is
    /// `None` and that thread does the reaping; this does nothing.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().take() {
            std::thread::spawn(move || {
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}
