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
use std::process::Child;
use std::sync::mpsc::sync_channel;
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
pub fn drain_to_stream(src: impl Read + Send + 'static, waker: Arc<mio::Waker>, label: &str) -> OroStream {
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
pub fn feed_from_stream(dst: impl Write + Send + 'static, waker: Arc<mio::Waker>, label: &str) -> OroStream {
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
/// child is behind a `RefCell<Option<_>>` because `wait()` consumes it (a
/// process is reaped once) and drop must be able to see whether it still needs
/// reaping.
pub struct Proc {
    /// The command as spawned, for `repr()` and for the diagnostic a second
    /// `wait()` might want. Never mutated after construction.
    pub args: Vec<String>,
    /// The child, until `wait()` reaps it. `None` afterwards, so a second
    /// `wait()` returns the cached code rather than reaping a corpse.
    child: RefCell<Option<Child>>,
    /// The exit code, once known. Set by the first `wait()`; read by every
    /// later one.
    code: Cell<Option<i64>>,
    /// Write end (`Backing::PipeWrite`), or a closed stream after
    /// `p.stdin.close()`. Feeding the child is `p.stdin.write(b)`.
    pub stdin: Value,
    /// Read ends (`Backing::PipeRead`). Draining the child is `p.stdout.read(n)`
    /// / `io.copy(dst, p.stdout)`.
    pub stdout: Value,
    pub stderr: Value,
}

impl Proc {
    pub fn new(args: Vec<String>, child: Child, stdin: Value, stdout: Value, stderr: Value) -> Proc {
        Proc {
            args,
            child: RefCell::new(Some(child)),
            code: Cell::new(None),
            stdin,
            stdout,
            stderr,
        }
    }

    /// `p.wait()` — block until the child exits, and answer its exit code.
    ///
    /// **This blocks the calling task** (and, like `proc.run`, the VM) until the
    /// child is gone. That is deliberate and matches `proc.run`, which reaps the
    /// same way: by the time a program calls `wait()` it has drained the output,
    /// and a child whose stdout has hit EOF has closed it — it is exiting, so
    /// the wait returns at once. The one way to make `wait()` hang is to call it
    /// *before* draining `stdout`/`stderr`: an undrained pipe fills, the child
    /// blocks writing to it, and neither side moves. Drain first — the same rule
    /// every subprocess API has.
    ///
    /// A signal-killed child has no exit code; CPython reports `-signal` for
    /// that case and this does the same, via `ExitStatus::code`'s `None`.
    pub fn wait(&self) -> std::io::Result<i64> {
        if let Some(code) = self.code.get() {
            return Ok(code);
        }
        // `take` so the child is reaped exactly once; a second `wait()` after
        // this reads the cached code above.
        let status = match self.child.borrow_mut().take() {
            Some(mut child) => child.wait()?,
            // Cannot happen — `code` is set whenever the child is taken — but a
            // panic here would be a worse answer than a defined one.
            None => return Ok(self.code.get().unwrap_or(-1)),
        };
        let code = status.code().map(i64::from).unwrap_or_else(|| signal_code(&status));
        self.code.set(Some(code));
        Ok(code)
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
    /// and reaped here, the same deterministic cleanup refcounting gives a file
    /// (§3's reason there is no `with`). A child that has already exited — the
    /// overwhelmingly common case, since `wait()` clears `child` — costs this
    /// nothing: `child` is `None` and the block is skipped. `kill()` on an
    /// already-dead process is harmless, and the `wait()` after it is the reap
    /// that turns a zombie back into nothing.
    fn drop(&mut self) {
        if let Some(child) = self.child.get_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
