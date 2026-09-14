//! The green-thread scheduler: the loop *above* the interpreter loop.
//!
//! `docs/stdlib-server-design.md` §3 is the specification. The division of
//! labour is:
//!
//! - [`super::Vm::run_slice`] runs **one** task until it parks or ends. It is
//!   the old `run_loop` and it has no scheduler check in it at all — the
//!   dispatch path is byte-for-byte what it was, which is the point.
//! - [`super::Vm::run_loop`] (here) owns the ready queue and the parked map,
//!   decides who runs next, and turns "this stack segment stopped" into "this
//!   *task* finished / failed / is waiting for something".
//!
//! Switching tasks is a `std::mem::replace` of `Vm::task`, exactly as M2's
//! proof-of-concept test did it by hand.
//!
//! ## Why [`Park`] is shaped the way it is
//!
//! `src/net.rs`'s module docs record a hazard: the three blocking socket calls
//! hold a `RefCell` borrow of the stream's interior *across* the syscall, which
//! becomes a `BorrowMutError` panic — not a catchable exception — the moment a
//! second task can run while the first is suspended there. Parking has to
//! release the borrow before it yields.
//!
//! That is a constraint on `Step::Park`'s shape, and it is met **by
//! construction** rather than by remembering:
//!
//! 1. `Step` and [`Park`] have no lifetime parameter, and every `Park` variant
//!    carries owned data — an `Rc`, an index, a `Value`. A `Ref<'_, T>` or
//!    `RefMut<'_, T>` cannot be stored in one, so a parking site that tried to
//!    keep its borrow alive across the suspend would not compile.
//!    `park_cannot_carry_a_borrow` in `super::tests` asserts the `'static`
//!    bound so that adding a lifetime is a test failure, not a latent panic.
//! 2. `Step::Park` is *returned* from `Vm::step`, so by the time the scheduler
//!    sees it every temporary inside the parking site is already dropped.
//! 3. Nothing hands a parked task its result by reaching back into the
//!    resource. The waker pushes the value onto the sleeping task's own operand
//!    stack (see [`Vm::wake_with_value`]), so resumption never re-borrows
//!    anything the parking site was holding.
//!
//! That prediction held exactly. A socket read parks as [`Park::Io`], whose
//! payload holds an `Rc<OroStream>` and never a borrow, and the retry takes a
//! fresh borrow inside [`attempt`]. No reshaping was needed and none was
//! permitted: the obvious "just give `Park` an `'a` and keep the readiness
//! guard you already have" would have compiled, would have read as tidier, and
//! would have reintroduced the panic. The unit test is what stands in its way,
//! which is why it is enforcement rather than decoration.
//!
//! ## The reactor
//!
//! [`Reactor`] is the M3b addition and the only place `mio` is named. The
//! division of labour is §3's, unchanged by the build: the VM owns the ready
//! queue and decides who runs next; the reactor answers exactly one question,
//! *when is this fd ready*.
//!
//! Timers are ours because mio has none, and they are four lines of idea: a
//! sorted list of deadlines whose head becomes `poll`'s timeout, where an
//! expiry wakes a task exactly the way a readiness event does.
//!
//! [`Vm::wait_for_external`] was the one-line seam and is now the reactor's
//! entry point. The deadlock condition moved with it, from "nothing ready" to
//! §3's corrected "nothing ready **and** nothing registered".
//!
//! ## A correction: "one VM per thread, nothing shared"
//!
//! This section used to end "Nothing crosses a thread boundary, so `!Send`
//! `Rc`-shaped tasks are never a problem", and §3 and the README said the same
//! thing as "one VM per OS thread, nothing shared between threads". Since
//! [`Resolver`] landed, the unqualified sentence is **false**: this process has
//! helper OS threads in it, right here in this file.
//!
//! **What the claim was overstating.** Everywhere it was load bearing, the work
//! it was actually doing is this: *`Value` is `Rc`-based and therefore `!Send`,
//! so the VM must own its own scheduling.* A ready queue of `Rc`-shaped tasks
//! cannot live in a work-stealing runtime; that is why Oro has a scheduler at
//! all instead of importing one, and it is why §3's split — VM schedules, mio
//! notifies — is forced rather than chosen. "No threads exist" was a stronger
//! statement that happened to be true at the time, and it got written down as
//! though it were the premise. It was not. It was a consequence, and it stopped
//! being true the moment a blocking-only syscall had to be waited on.
//!
//! **The invariant, stated so it can be checked.** *No Oro value ever crosses a
//! thread.* Concretely, and this is the whole list:
//!
//! * The resolver threads receive a `String` and send back a
//!   `Result<Vec<std::net::SocketAddr>, String>`. Both are `Send`, neither is a
//!   `Value`, and there is no third message.
//! * `Value` is still `Rc`-based and still `!Send`; there is still not one
//!   `Arc` around an Oro value in the codebase. The two `Arc`s that exist hold
//!   a `mio::Waker` and an `mpsc::Receiver<Lookup>` — an eventfd and a queue of
//!   strings.
//! * The VM still owns `ready`, `parked` and every decision about who runs
//!   next. A resolver thread cannot wake a task; it can only put an answer in a
//!   queue and poke an fd, and the VM decides what that means — which is the
//!   same contract the kernel has through `epoll`.
//!
//! Nothing in the architecture rested on the number of OS threads in the
//! process. Stating the invariant in terms of what crosses the boundary, rather
//! than in terms of how many boundaries there are, is what makes it something a
//! reader can check against the code.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::exc::{os_error, runtime_error, timeout_error, type_error, value_error, Exc, VErr};
use crate::stream::{Io, OroStream};
use crate::task::{Channel, TaskHandle, TaskId, TaskState};
use crate::value::{MethodKind, OroTuple, VResult, Value};

use super::{Frame, ReturnAction, RuntimeError, Step, Task, Vm, VmError};

/// Why a task is not running, and therefore what has to happen for it to run
/// again.
///
/// Every variant carries owned data only. See the module docs for why that is
/// load-bearing rather than incidental.
pub(super) enum Park {
    /// `ch.recv()` with nothing to take and no sender waiting.
    Recv(Rc<Channel>),
    /// `for x in ch` with nothing to take. Carries the `ForIter` exit target,
    /// because a closed channel ends the loop where a bare `recv()` raises.
    IterRecv(Rc<Channel>, usize),
    /// `ch.send(v)` with a full buffer and no receiver waiting. The value being
    /// offered is queued on the channel, not here, so a receiver can take it
    /// without reaching into this task's stack.
    Send(Rc<Channel>),
    /// `t.join()` on a task that has not finished.
    Join(Rc<TaskHandle>),
    /// Another task is running this module's body. See [`Vm::import_module`].
    Import(Rc<str>),
    /// A socket operation that would block. The payload is boxed because it is
    /// the largest variant by some way and every other one is a pointer.
    ///
    /// This is the variant §3 said would be the tempting place to add a
    /// lifetime. It carries an `Rc<OroStream>` and owned progress — never a
    /// `Ref`, never a `RefMut`, never a mio guard.
    Io(Box<IoWait>),
    /// A `proc.spawn` pipe read or write that would block. It carries the same
    /// [`IoWait`] a socket does and resumes through the same [`attempt`] — the
    /// only difference is *what wakes it*. A pipe fd is not an mio source, so
    /// there is no readiness event and nothing in `reactor.waiters`; instead a
    /// helper thread signals the pipe [`Waker`](mio::Waker) and
    /// [`drain_pipes`](Vm::drain_pipes) re-attempts every parked pipe op, the
    /// same way [`drain_dns`](Vm::drain_dns) settles a finished lookup. The
    /// `token`/`interest`/`seq` fields of the `IoWait` go unused here.
    Pipe(Box<IoWait>),
    /// `p.wait()` — waiting for a spawned child to exit. A reaper thread is
    /// inside `wait(2)` on this task's behalf (the child's exit is a blocking-only
    /// OS event, like `getaddrinfo`), and delivers the code through the `Proc`'s
    /// channel; [`drain_procs`](Vm::drain_procs) wakes this task when it lands.
    /// Owned `Rc<Proc>`, so `'static` like every other variant.
    ProcWait(Rc<crate::process::Proc>),
    /// `time.sleep(secs)`. Carries the park's sequence number, which is how its
    /// timer entry knows it is still the one this task is waiting on.
    Sleep(u64),
    /// `net.dial("host:port")`, waiting on the system resolver.
    ///
    /// The one park that is not waiting on the kernel: a helper thread is
    /// inside `getaddrinfo` on this task's behalf. Both fields are owned — an
    /// integer and an `Rc<str>` — so this variant is `'static` like every other
    /// one, and the address is kept because a diagnostic that says which name
    /// is being looked up is worth two words of payload.
    Dns {
        /// Which lookup. Checked against the answer the same way [`Timer`]'s
        /// `seq` is checked, so a result can never land on a task that has
        /// moved on.
        id: u64,
        addr: Rc<str>,
    },
    /// A cooperative yield inserted by the VM after a run of I/O operations that
    /// completed without blocking (see [`READY_IO_YIELD`]). Unlike [`Yield`], the
    /// operation's result is *already on the stack* — the task resumes at the
    /// next instruction with it in place — so requeueing must push nothing.
    YieldReady,
    /// `yield_now()` — the odd one out, and deliberately in this enum anyway.
    ///
    /// Every other variant names something the task is *waiting for*; this one
    /// names nothing, because a yielding task is already runnable and goes to
    /// the back of the ready queue rather than into `parked`. It rides on
    /// `Park` because the constraint that shapes `Park` is exactly the one
    /// `yield_now` needs: a suspension point cannot hold a `RefCell` borrow
    /// across the hand-off, and the way that is enforced is that `Step::Park`
    /// is the only way out of `Vm::step` that suspends. A second mechanism
    /// beside it would be a second place to get that wrong.
    Yield,
}

impl Park {
    /// A short phrase for the deadlock diagnostic. The channel's capacity and
    /// fill are in it because "recv on an empty unbuffered channel" and "send
    /// to a full one" are different bugs with the same word in front of them.
    fn what(&self) -> String {
        match self {
            Park::Recv(ch) | Park::IterRecv(ch, _) => {
                format!(
                    "recv (channel cap {}, {} buffered)",
                    ch.cap,
                    ch.buf.borrow().len()
                )
            }
            Park::Send(ch) => {
                format!(
                    "send (channel cap {}, {} buffered)",
                    ch.cap,
                    ch.buf.borrow().len()
                )
            }
            Park::Join(h) => format!("join(task {})", h.id),
            Park::Import(p) => format!("import '{p}'"),
            // Neither of these can actually reach the deadlock diagnostic — a
            // task parked on either has something registered with the reactor,
            // which is precisely the condition that says it is not a deadlock.
            // Spelled anyway, for the same reason `Park::Yield` is.
            Park::Io(w) => format!("{} on {}", w.op.what(), w.stream.repr()),
            // Unlike `Park::Io`, a pipe park *can* reach the deadlock diagnostic:
            // its wake comes from a helper thread the reactor's `is_idle` counts
            // (`no_pipes`), so it is not a deadlock while the thread is live —
            // but a child that will never produce (blocked reading a stdin the
            // program never writes) is a genuine one, and this is what it says.
            Park::Pipe(w) => format!("{} on {}", w.op.what(), w.stream.repr()),
            // Like a pipe park, reachable through the deadlock diagnostic in
            // principle but not in practice: a reaper thread the reactor counts
            // is what wakes it, so it is never idle while one is outstanding.
            Park::ProcWait(p) => format!("wait() on {}", p.repr()),
            Park::Sleep(_) => "time.sleep()".to_string(),
            // Unreachable through the deadlock diagnostic for the same reason
            // the two above are: a task waiting on a lookup has one registered
            // with the reactor, which is exactly the condition that says this
            // is not a deadlock.
            Park::Dns { addr, .. } => format!("dial() resolving '{addr}'"),
            // Not reachable through the deadlock diagnostic: a yielding task is
            // requeued, never filed under `parked`, and the yield arm in
            // `run_loop` runs before the deadlock check. Spelled rather than
            // `unreachable!` because a diagnostic that panics is worse than a
            // diagnostic that is briefly wrong.
            Park::Yield => "yield_now()".to_string(),
            // Unreachable through the deadlock diagnostic for the same reason
            // `Yield` is: a yielding task is requeued, never parked.
            Park::YieldReady => "yield (I/O fairness)".to_string(),
        }
    }
}

/// A suspended task and the reason it is suspended. Only the scheduler ever
/// holds one.
pub(super) struct Parked {
    task: Task,
    park: Park,
}

/// A socket operation that could not finish now, and everything needed to
/// finish it later.
///
/// Every field is owned or an `Rc`. That is the whole of the borrow-across-a-
/// syscall defence, and it is checked by the compiler rather than by anyone
/// re-reading this struct.
pub(super) struct IoWait {
    /// The stream, kept alive for as long as a task is parked on it — which is
    /// also why a parked reader cannot have its fd closed out from under it by
    /// a refcount reaching zero. An explicit `close()` still can, and that is
    /// handled rather than prevented.
    stream: Rc<OroStream>,
    /// What is being attempted, and how far it has got.
    op: IoOp,
    /// The readiness the last attempt asked for.
    interest: mio::Interest,
    /// The reactor token `stream` is registered under.
    token: usize,
    /// `set_timeout`'s deadline, fixed when the operation *first* blocked and
    /// then held across every re-park — so it bounds the whole operation, not
    /// each syscall inside it.
    deadline: Option<Instant>,
    /// Identifies this operation to its timer entry. Stable across re-parks,
    /// so a `read_until` that blocks four times keeps one deadline rather than
    /// arming four.
    seq: u64,
}

/// The five operations that can meet `EWOULDBLOCK`, each carrying its own
/// partial progress.
///
/// The progress is here rather than in a Rust local because there is no Rust
/// frame across a park, and it is here rather than on the stream because a
/// second task must be able to use the stream meanwhile.
pub(super) enum IoOp {
    Accept,
    /// A `connect(2)` in flight, and the addresses left to try if it fails.
    ///
    /// The list is the whole reason this carries anything. The blocking
    /// `std::net::TcpStream::connect(&addrs[..])` walked it for free, and
    /// walking it matters: `getaddrinfo` puts the AAAA record first on a
    /// dual-stack host, so a machine with no IPv6 route reaches the A record
    /// only by falling through. `localhost` is the case in this tree —
    /// `::1` first, `127.0.0.1` second — so the fallback is exercised by the
    /// test suite rather than only by other people's networks.
    Connect {
        addrs: Vec<SocketAddr>,
        next: usize,
    },
    Read(i64),
    ReadUntil {
        delim: Rc<Vec<u8>>,
        limit: i64,
        acc: Vec<u8>,
    },
    /// `done` bytes of `buf` have gone. §2's `write(b)` writes all of `b` or
    /// raises, and this counter is how that contract survives a suspension:
    /// the caller has no return value to learn about it from, and does not.
    ///
    /// The `Rc` is the caller's `bytes` object, not a copy of it: Oro's `bytes`
    /// is immutable and refcounted, so a parked write borrows nothing and
    /// copies nothing, however long it waits.
    Write {
        buf: Rc<Vec<u8>>,
        done: usize,
    },
}

impl IoOp {
    fn what(&self) -> &'static str {
        match self {
            IoOp::Accept => "accept()",
            IoOp::Connect { .. } => "connect()",
            IoOp::Read(_) => "read()",
            IoOp::ReadUntil { .. } => "read_until()",
            IoOp::Write { .. } => "write()",
        }
    }
}

/// One attempt at finishing `w`, from wherever it got to.
///
/// A free function, not a method on `Vm`, and that is deliberate: it needs
/// nothing from the VM, so it cannot accidentally acquire a second borrow of
/// anything. Each call takes a fresh borrow of the stream inside
/// `crate::stream` and has dropped it before it returns.
fn attempt(w: &mut IoWait) -> VResult<Io<Value>> {
    let stream = &w.stream;
    match &mut w.op {
        IoOp::Accept => Ok(match stream.accept()? {
            Io::Ready(s) => Io::Ready(Value::Stream(Rc::new(s))),
            Io::Block(i) => Io::Block(i),
        }),
        // The stream already exists — it was made by `start_connect`, which is
        // where the socket and the `connect(2)` call live. All that is left
        // here is to ask whether the handshake finished, and to hand the caller
        // the very stream the attempt was made on. A *failure* is the one case
        // this cannot finish alone: it has to move to the next address, and
        // that needs a new socket, so `retry_io` takes it.
        IoOp::Connect { .. } => Ok(match stream.connect_check()? {
            Io::Ready(()) => Io::Ready(Value::Stream(Rc::clone(stream))),
            Io::Block(i) => Io::Block(i),
        }),
        IoOp::Read(n) => Ok(match stream.read(*n)? {
            Io::Ready(b) => Io::Ready(Value::bytes(b)),
            Io::Block(i) => Io::Block(i),
        }),
        IoOp::ReadUntil { delim, limit, acc } => {
            Ok(match stream.read_until(delim, *limit, acc)? {
                Io::Ready(b) => Io::Ready(Value::bytes(b)),
                Io::Block(i) => Io::Block(i),
            })
        }
        // The `write_all` loop that used to live in `crate::stream`, moved to
        // where its progress can survive a park.
        IoOp::Write { buf, done } => loop {
            if *done >= buf.len() {
                return Ok(Io::Ready(Value::None));
            }
            match stream.write(&buf[*done..])? {
                Io::Ready(k) => *done += k,
                Io::Block(i) => return Ok(Io::Block(i)),
            }
        },
    }
}

/// How one slice of execution ended — what [`Vm::run_slice`] reports back.
pub(super) enum Slice {
    /// The task suspended; the scheduler must file it under `park`.
    Parked(Box<Park>),
    /// The task's outermost frame returned.
    Returned(Value),
    /// An exception reached the bottom of this task's frame stack. Carries the
    /// exception itself (rule 2 re-raises it in a joiner) and the rendered
    /// diagnostic (rule 3 prints it).
    Failed(Value, VmError),
}

// --- The reactor -------------------------------------------------------------

/// The one thing the VM asks the operating system for: *tell me when this fd is
/// ready.*
///
/// §3's framing survived the build intact. The VM owns the ready queue and
/// decides who runs next — it has to, because every Oro `Value` is an `Rc` and a
/// ready queue of `!Send` tasks cannot live in a work-stealing runtime. What is
/// left for a library is readiness notification, which is `mio::Poll` and
/// nothing else. There is no task system here, no futures, no executor: a
/// `Poll`, a `Token` per registered fd, an `Events` buffer, and one
/// `poll(&mut events, timeout)` at the point where the ready queue empties.
///
/// **Timers are ours**, because mio has none and because `epoll_wait` already
/// takes a timeout. A sorted list of deadlines whose head becomes that timeout
/// is the whole mechanism, and an expiry is a wake exactly like a readiness
/// event is. It is what `time.sleep` parks on and what `set_timeout` arms.
///
/// **Everything in here is lazy.** A program that never touches a socket or a
/// clock never creates an epoll fd, never allocates an event buffer, and pays
/// [`Reactor::is_idle`] — two `is_empty` calls — once per task switch. That is
/// the concrete form of "the reactor must not cost anything in programs that
/// never touch I/O".
#[derive(Default)]
pub(super) struct Reactor {
    /// The epoll/kqueue handle and its event buffer, created on the first
    /// registration and never before.
    os: Option<Os>,
    /// Tokens handed out. Monotonic and never reused, so a stale event for a
    /// closed stream can never be mistaken for a live one's.
    next_token: usize,
    /// Who is parked on each registered fd. At most one task per direction:
    /// two tasks reading the same socket is a program bug and is named as one,
    /// but a reader and a writer on the same socket is `io.copy` in both
    /// directions and has to work.
    waiters: HashMap<usize, Waiters>,
    /// Deadlines, soonest first. A `Vec` rather than a heap because the head is
    /// read on every poll and the list is short — it holds one entry per
    /// *sleeping or deadlined* task, not one per connection.
    timers: Vec<Timer>,
    /// The resolver pool, or `None` if no program has dialled a name yet.
    ///
    /// Lazy like everything else in here, and **boxed**, which is not tidiness.
    /// [`Reactor`] is a field of [`Vm`] by value, so every byte added here is a
    /// byte added to the struct the interpreter loop touches on every
    /// instruction. Inline, the pool's bookkeeping — two channel ends, two
    /// `Arc`s, a `HashMap` and two counters — put about 120 bytes between
    /// `Vm`'s hot fields and cost a measurable 3% on the builtin-call
    /// benchmark, in a program that never resolves anything. Behind a `Box` it
    /// is one null pointer until the first name is dialled.
    dns: Option<Box<Resolver>>,
    /// The `proc.spawn` pipe hub, or `None` until the first spawn. Boxed and
    /// lazy for exactly the reason `dns` is: a program that never spawns a child
    /// pays one null pointer for it.
    pipes: Option<Box<PipeHub>>,
}

/// The reserved token the resolver's [`mio::Waker`] is registered under.
///
/// Zero is free by construction: [`Reactor::arm`] pre-increments `next_token`,
/// so the first fd ever registered gets 1 and no stream can collide with this.
const DNS_TOKEN: usize = 0;

/// The most resolver threads that will ever exist.
///
/// Node's libuv pool defaults to 4 and DNS is what it is known for; tokio's
/// blocking pool defaults to 512, which is a number for a general-purpose
/// offload pool and not for this. Eight is the compromise: enough that a
/// handful of simultaneous outbound dials overlap instead of serialising,
/// small enough that a flood of `dial`s cannot turn into a thread-exhaustion
/// bug. Past eight, lookups queue — and a queued lookup still blocks nothing
/// but itself, which is the property the whole change is for.
///
/// Tunable, not API: §7 puts buffer sizes and pool sizes under "deliberately
/// left unfrozen" precisely so this number can move without breaking anyone.
const MAX_DNS_THREADS: usize = 8;

/// The longest a blocking poll will wait while a `proc.spawn` pipe op is parked,
/// as a backstop against a missed edge on the edge-triggered pipe `Waker`. It is
/// not the mechanism — the helper thread's wake and the pre-poll re-drain are —
/// only the bound on how long a lost edge can delay a retry. Small enough to be
/// imperceptible, large enough not to be a spin; tunable, not API.
const PIPE_POLL_BACKSTOP: Duration = Duration::from_millis(20);

/// After this many I/O operations complete in a row without blocking, the
/// running task yields so its peers get a turn.
///
/// A ready operation — a buffered read, a write the kernel takes whole — never
/// suspends, so a fast `io.copy` loop would otherwise run to EOF holding the VM,
/// and another connection's request would wait behind the whole transfer (49 ms
/// stalls were measured serving an index page during a push). Making it the VM's
/// job, per *operation* rather than per byte, means every correct caller gets it
/// for free — `io.copy` included — and none has to hand-roll a yield. Per
/// operation, not per byte: it needs no size accounting and covers read, write
/// and accept uniformly. Sixteen bounds the work between yields to ~16 chunks
/// (about 1 MiB at `io.copy`'s 64 KiB) — a few hundred microseconds, so a
/// waiting peer sees sub-millisecond latency — while amortising the yield's cost
/// so a lone transfer barely notices. Tunable, not API.
pub(super) const READY_IO_YIELD: u32 = 16;

/// A lookup on its way to a helper thread.
struct Lookup {
    id: u64,
    /// The whole `"host:port"` string, not a bare host: `resolve` wants the
    /// port too, and re-splitting it on the far side would be a second parser.
    addr: String,
}

/// A lookup on its way back.
struct Resolved {
    id: u64,
    /// Exactly what `crate::net::resolve` returned, error string and all. The
    /// message is the whole of the error mapping — a failed lookup raises the
    /// same class from a helper thread as it did when this call was made
    /// inline, because it is the same `String`.
    result: VResult<Vec<SocketAddr>>,
}

/// The work queue, shared by every resolver thread.
///
/// One `Mutex` around one `Receiver` rather than a queue per worker: a worker
/// holds the lock only across its own `recv`, so the next idle worker takes it
/// the instant a job is handed over, and a worker that is *busy* inside
/// `getaddrinfo` is holding nothing at all. A queue per worker would let one
/// slow lookup strand the jobs behind it while another worker sat idle.
type Jobs = Arc<Mutex<Receiver<Lookup>>>;

/// Name resolution, on helper OS threads.
///
/// **Why threads at all.** `getaddrinfo` is blocking-only — POSIX has no async
/// form — so someone has to wait on a thread; the choice is only whose. Using
/// the OS resolver inherits `/etc/hosts`, `resolv.conf`, search domains, VPN
/// split-DNS, IPv6/IPv4 preference, system caching and NSS plugins. The
/// alternative, a resolver written in Oro, needs UDP added to a frozen surface
/// for one internal caller plus re-implementations of all of the above, and Go
/// is the evidence that this does not end well: it ships a pure-Go resolver
/// *and* a cgo one, and switches to the OS resolver whenever the system
/// configuration looks non-trivial. `crate::net::plan_dial` carries the long
/// form of the argument.
///
/// **Lifetime.** Threads start lazily — the first `dial` of a name starts the
/// first one — and the pool grows one thread per concurrent lookup up to
/// [`MAX_DNS_THREADS`], then stops. A program that dials one name at a time
/// runs one helper thread; a program that never dials a name runs none and
/// never even creates the `Waker`. Threads are never retired: a parked
/// `recv` costs a stack and nothing else, and reaping idle workers would be
/// bookkeeping in exchange for memory nobody is short of.
///
/// **Shutdown.** Nothing is joined, deliberately. Dropping this drops `queue`,
/// the only [`Sender<Lookup>`], so every worker's `recv` returns `Err` and it
/// exits; a worker that is *inside* `getaddrinfo` finishes, finds `answers`'
/// receiver gone, and returns without waking anything. Joining instead would
/// mean a VM shutting down could wait out a five-second resolver timeout —
/// which is precisely the stall this whole mechanism exists to remove. The
/// threads hold a `String` and an eventfd, so there is nothing for a late one
/// to corrupt and nothing for the process to wait on.
struct Resolver {
    /// Hand a lookup to the pool. The only `Sender`, which is what makes
    /// dropping this the pool's shutdown signal.
    queue: Sender<Lookup>,
    /// Take an answer back. Drained by [`Vm::drain_dns`] after every poll.
    answers: Receiver<Resolved>,
    /// The far end of `answers`, kept so a newly spawned worker can be given a
    /// clone of it.
    answers_tx: Sender<Resolved>,
    /// The far end of `queue`, likewise — and the thing that makes `queue` the
    /// *only* sender, since this side never sends.
    jobs: Jobs,
    /// Poke the reactor: an answer is waiting. This is the whole of how a
    /// helper thread reaches the VM, and it carries no data — the answer went
    /// through the channel, and this only says "poll returned for a reason".
    waker: Arc<mio::Waker>,
    /// Workers started. Never decreases.
    threads: usize,
    /// Who is waiting on each lookup in flight. Its length is how many lookups
    /// are outstanding, which is what decides whether the pool grows.
    waiters: HashMap<u64, TaskId>,
    /// Lookup ids. Monotonic and never reused, for the same reason tokens are.
    next_lookup: u64,
}

/// The `proc.spawn` pipe hub: the single [`Waker`](mio::Waker) every pipe helper
/// thread signals, and the set of tasks parked on a pipe op.
///
/// **Why a hub at all, when the resolver already has a `Waker`.** A program can
/// spawn a child without ever dialling a name, so the pipe machinery cannot
/// borrow the resolver's `Waker` — it needs its own, created the first time a
/// child is spawned and registered under its own token, exactly as
/// [`start_resolver`](Reactor::start_resolver) creates the resolver's.
///
/// **Why the waiters are here and not just in `parked`.** [`Reactor::is_idle`]
/// is the deadlock test, and it must answer "not idle" while a pipe read or
/// write is outstanding — a helper thread outside the VM can still make it
/// runnable. `is_idle` is a method on the reactor and cannot see the VM's
/// `parked` map, so the count of parked pipe ops lives here, the same way
/// `Resolver::waiters` is what makes a lookup count. Its emptiness is
/// [`no_pipes`](Reactor::no_pipes).
///
/// **Lifetime and shutdown.** Like the resolver, never torn down and never
/// joined: each spawned child's helper threads own an `Arc` clone of the
/// `Waker` and a channel end, and dropping the pipe streams (their `Rc`s
/// reaching zero, or an explicit `close()`) drops the channel ends, which is
/// what tells a helper thread to exit. A thread blocked in a `read`/`write`
/// syscall on the child's fd finishes when the child dies — which the
/// kill-on-drop in [`Proc`](crate::process::Proc) guarantees.
struct PipeHub {
    /// Signalled by every pipe helper thread when it has moved a chunk. Carries
    /// no data — the bytes went through the stream's own channel — and only
    /// says "poll returned for a reason", so [`drain_pipes`](Vm::drain_pipes)
    /// re-attempts the parked pipe ops.
    waker: Arc<mio::Waker>,
    /// Tasks parked on a pipe read or write. Insert on park, remove on wake; its
    /// emptiness is what [`no_pipes`](Reactor::no_pipes) reports and thus part
    /// of the deadlock test.
    waiters: HashSet<TaskId>,
    /// Tasks parked on `p.wait()`, waiting for a child to exit. Woken by
    /// [`drain_procs`](Vm::drain_procs) when the reaper thread delivers the code.
    /// Counted in `no_pipes` for the same reason `waiters` is: a reaper thread is
    /// outside the VM and can still make the task runnable.
    proc_waiters: HashSet<TaskId>,
}

struct Os {
    poll: mio::Poll,
    events: mio::Events,
}

#[derive(Default)]
struct Waiters {
    read: Option<TaskId>,
    write: Option<TaskId>,
}

impl Waiters {
    fn is_empty(&self) -> bool {
        self.read.is_none() && self.write.is_none()
    }
}

/// A deadline: `time.sleep`'s wake, or `set_timeout`'s expiry.
struct Timer {
    at: Instant,
    task: TaskId,
    /// Which *park* of that task this deadline belongs to. A task whose read
    /// completes before its timeout leaves the entry behind; when it fires, the
    /// task's current park carries a later seq and the entry is dropped
    /// unfired. Cheaper than scrubbing the list on every wake, which would make
    /// a completed read O(number of deadlined connections).
    seq: u64,
}

/// How much of an event mattered. Copied out of mio's `Events` before anything
/// is woken, because waking needs `&mut Vm` and the events borrow the reactor.
struct ReadyFd {
    token: usize,
    readable: bool,
    writable: bool,
}

impl Reactor {
    /// Nothing registered, no deadline and no lookup in flight: nobody outside
    /// the VM can make a task runnable, which is the corrected deadlock
    /// condition from §3.
    ///
    /// A lookup counts. A resolver thread is outside the VM exactly the way the
    /// kernel is, so a task parked on one is waiting for something that *can*
    /// arrive — and reporting that as a deadlock would be the same mistake §3
    /// records for an idle server sitting in `accept`.
    fn is_idle(&self) -> bool {
        self.waiters.is_empty() && self.timers.is_empty() && self.no_lookups() && self.no_pipes()
    }

    /// Nothing is out with the resolver — which, for a program that never
    /// dialled a name, is a null check.
    fn no_lookups(&self) -> bool {
        self.dns.as_ref().is_none_or(|d| d.waiters.is_empty())
    }

    /// No task is parked on a pipe — a null check for a program that never
    /// spawned a child. A pipe op outstanding means a helper thread can still
    /// make a task runnable, so this being false keeps [`is_idle`](Self::is_idle)
    /// from calling that a deadlock.
    fn no_pipes(&self) -> bool {
        self.pipes
            .as_ref()
            .is_none_or(|p| p.waiters.is_empty() && p.proc_waiters.is_empty())
    }

    /// The pipe hub's waker, creating the hub (and its `Waker`, and — via
    /// [`os`](Self::os) — the epoll instance) the first time a child is spawned.
    ///
    /// The token is taken from `next_token` rather than a reserved constant like
    /// [`DNS_TOKEN`]: there is only ever one pipe `Waker`, made once, so a fresh
    /// token costs nothing and needs no constant carved out. It will never
    /// collide with a socket's — `next_token` is monotonic — and it is never
    /// looked up in `waiters`, so [`io_ready`](Vm::io_ready) ignores it exactly
    /// as it ignores `DNS_TOKEN`.
    pub(super) fn pipe_waker(&mut self) -> VResult<Arc<mio::Waker>> {
        if self.pipes.is_none() {
            self.next_token += 1;
            let token = self.next_token;
            let waker = mio::Waker::new(self.os()?.poll.registry(), mio::Token(token))
                .map_err(|e| crate::net::io_error(&e))?;
            self.pipes = Some(Box::new(PipeHub {
                waker: Arc::new(waker),
                waiters: HashSet::new(),
                proc_waiters: HashSet::new(),
            }));
        }
        Ok(Arc::clone(&self.pipes.as_ref().expect("just built").waker))
    }

    /// Record that `task` is parked on a pipe op (so `is_idle` counts it).
    fn pipe_wait(&mut self, task: TaskId) {
        if let Some(h) = self.pipes.as_mut() {
            h.waiters.insert(task);
        }
    }

    /// This task's pipe op has settled; it is no longer outstanding.
    fn pipe_unwait(&mut self, task: TaskId) {
        if let Some(h) = self.pipes.as_mut() {
            h.waiters.remove(&task);
        }
    }

    /// Record / clear that `task` is parked on `p.wait()`.
    fn proc_wait(&mut self, task: TaskId) {
        if let Some(h) = self.pipes.as_mut() {
            h.proc_waiters.insert(task);
        }
    }

    fn proc_unwait(&mut self, task: TaskId) {
        if let Some(h) = self.pipes.as_mut() {
            h.proc_waiters.remove(&task);
        }
    }

    fn os(&mut self) -> VResult<&mut Os> {
        if self.os.is_none() {
            let poll = mio::Poll::new().map_err(|e| crate::net::io_error(&e))?;
            // 256 is a compromise nobody has to tune: large enough that a busy
            // accept loop drains a batch per syscall, small enough that the
            // allocation is invisible next to the epoll fd it accompanies.
            self.os = Some(Os {
                poll,
                events: mio::Events::with_capacity(256),
            });
        }
        Ok(self.os.as_mut().expect("just built"))
    }

    /// Record that `task` is waiting for `interest` on `stream`, registering
    /// the fd if this is the first time it has ever had to wait.
    fn arm(
        &mut self,
        stream: &Rc<OroStream>,
        interest: mio::Interest,
        task: TaskId,
    ) -> VResult<usize> {
        let token = match stream.token() {
            Some(t) => t,
            None => {
                self.next_token += 1;
                let t = self.next_token;
                stream.register(self.os()?.poll.registry(), t)?;
                t
            }
        };
        let w = self.waiters.entry(token).or_default();
        let (slot, side) = if interest.is_readable() {
            (&mut w.read, "read")
        } else {
            (&mut w.write, "write")
        };
        match *slot {
            Some(other) if other != task => {
                // Deliberately an exception rather than a queue. Two tasks
                // reading one socket is not a workload, it is a race over whose
                // bytes are whose, and the runtime knowing which is impossible.
                return Err(runtime_error(format!(
                    "two tasks cannot {side} the same {} at once (task {other} is already \
                     waiting on it)",
                    stream.kind.type_name()
                )));
            }
            _ => *slot = Some(task),
        }
        Ok(token)
    }

    /// This task is no longer waiting on this side of this fd.
    fn disarm(&mut self, token: usize, task: TaskId) {
        let Some(w) = self.waiters.get_mut(&token) else {
            return;
        };
        if w.read == Some(task) {
            w.read = None;
        }
        if w.write == Some(task) {
            w.write = None;
        }
        if w.is_empty() {
            self.waiters.remove(&token);
        }
    }

    /// Every task waiting on this fd, forgetting all of them. `close()`'s half
    /// of the hazard: the fd is about to go, so nobody can be left parked on it.
    fn take_waiters(&mut self, token: usize) -> Vec<TaskId> {
        let Some(w) = self.waiters.remove(&token) else {
            return Vec::new();
        };
        w.read.into_iter().chain(w.write).collect()
    }

    /// Hand `addr` to the resolver pool on `task`'s behalf, and answer with the
    /// lookup's id.
    ///
    /// Growing the pool here rather than up front is what keeps the common
    /// shape honest: one thread for a program that dials one name at a time,
    /// none at all for a program that never dials one. The rule is one worker
    /// per *concurrent* lookup, capped — `dns_waiters` is exactly the count of
    /// lookups already outstanding, so `+ 1` is this one.
    fn lookup(&mut self, addr: String, task: TaskId) -> VResult<u64> {
        self.start_resolver()?;
        let r = self.dns.as_mut().expect("just started");
        let want = (r.waiters.len() + 1).min(MAX_DNS_THREADS);
        r.next_lookup += 1;
        let id = r.next_lookup;
        while r.threads < want {
            match spawn_resolver_thread(
                Arc::clone(&r.jobs),
                r.answers_tx.clone(),
                Arc::clone(&r.waker),
            ) {
                Ok(()) => r.threads += 1,
                // The pool already has a worker: it will get to this lookup,
                // just without the extra parallelism. Failing the dial because
                // the *second* thread could not start would turn a resource
                // shortage into an exception at an unrelated call site.
                Err(_) if r.threads > 0 => break,
                Err(e) => return Err(crate::net::io_error(&e)),
            }
        }
        // Cannot fail: `jobs` holds the receiver for as long as the `Resolver`
        // lives, so the channel outlives every send made through it.
        r.queue
            .send(Lookup { id, addr })
            .map_err(|_| runtime_error("internal: the DNS resolver pool has stopped"))?;
        r.waiters.insert(id, task);
        Ok(id)
    }

    /// Build the resolver — channels, `Waker`, no threads yet — the first time
    /// a name is dialled.
    fn start_resolver(&mut self) -> VResult<()> {
        if self.dns.is_some() {
            return Ok(());
        }
        // The `Waker` registers itself, which is why this is the one place
        // outside `arm` that forces the epoll instance into existence.
        let waker = mio::Waker::new(self.os()?.poll.registry(), mio::Token(DNS_TOKEN))
            .map_err(|e| crate::net::io_error(&e))?;
        let (queue, jobs) = channel::<Lookup>();
        let (answers_tx, answers) = channel::<Resolved>();
        self.dns = Some(Box::new(Resolver {
            queue,
            answers,
            answers_tx,
            jobs: Arc::new(Mutex::new(jobs)),
            waker: Arc::new(waker),
            threads: 0,
            waiters: HashMap::new(),
            next_lookup: 0,
        }));
        Ok(())
    }

    fn add_timer(&mut self, at: Instant, task: TaskId, seq: u64) {
        let i = self.timers.partition_point(|t| t.at <= at);
        self.timers.insert(i, Timer { at, task, seq });
    }

    /// Wait for readiness or a deadline, and report what became ready.
    ///
    /// `timeout` of `None` blocks until something happens, which is exactly
    /// what a server sitting in `accept` should do.
    fn poll(&mut self, timeout: Option<Duration>) -> VResult<Vec<ReadyFd>> {
        if self.waiters.is_empty() && self.no_lookups() && self.no_pipes() {
            // Nothing but deadlines: there is no fd to wait on, so waiting on
            // one would mean creating an epoll instance to sleep in. A program
            // whose only concurrency is `time.sleep` never makes one.
            //
            // A lookup or a pipe op in flight excludes this path. Each has a
            // `Waker` that is an fd and is registered, so there *is* something to
            // wait on, and sleeping through it would add the child's (or the
            // lookup's) own latency to every chunk.
            if let Some(d) = timeout {
                std::thread::sleep(d);
            }
            return Ok(Vec::new());
        }
        let os = self.os()?;
        match os.poll.poll(&mut os.events, timeout) {
            Ok(()) => {}
            // A signal, not a fault. Nothing became ready; the scheduler will
            // come straight back here.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => return Ok(Vec::new()),
            Err(e) => return Err(crate::net::io_error(&e)),
        }
        Ok(os
            .events
            .iter()
            .map(|ev| ReadyFd {
                token: ev.token().0,
                // A hang-up or an error is reported on whichever side is
                // waiting: the retry then gets the real errno from the syscall
                // itself, which is a better exception than anything that could
                // be synthesised from the event flags.
                readable: ev.is_readable() || ev.is_read_closed() || ev.is_error(),
                writable: ev.is_writable() || ev.is_write_closed() || ev.is_error(),
            })
            .collect())
    }

    /// How many resolver threads this VM has started. Zero until a name is
    /// dialled, and the observable end of "a literal `ip:port` takes no lookup
    /// path at all" — see `resolving_only_happens_for_a_name` in
    /// `super::tests`.
    ///
    /// Test-only, and deliberately not a runtime accessor: nothing in the
    /// language should be able to see how many helper threads exist, because
    /// the number is an implementation property §7 leaves unfrozen.
    #[cfg(test)]
    pub(super) fn resolver_threads(&self) -> usize {
        self.dns.as_ref().map_or(0, |r| r.threads)
    }

    /// The soonest deadline, or `None` if nothing is on a clock.
    fn next_deadline(&self) -> Option<Instant> {
        self.timers.first().map(|t| t.at)
    }
}

/// Start one resolver thread.
///
/// The whole of what crosses the boundary is in this function's signature: a
/// queue of `String`s in, a queue of `SocketAddr`s or error `String`s out, and
/// an eventfd to poke. No `Value`, no `Rc`, no reference to the VM — which is
/// the invariant the module docs' correction states, expressed as a type.
///
/// The loop is `recv` → resolve → `send` → `wake`, and every exit is the
/// channel closing. `send` before `wake` is the ordering that matters: the
/// answer is in the queue before the VM is told to look, so a wake can be
/// spurious but never empty-handed.
fn spawn_resolver_thread(
    jobs: Jobs,
    answers: Sender<Resolved>,
    waker: Arc<mio::Waker>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("oro-dns".to_string())
        .spawn(move || loop {
            let job = {
                // Poisoning cannot happen — the guarded value is a `Receiver` and
                // nothing in this block can panic — but `into_inner` is the honest
                // answer to it anyway. An `unwrap` here would turn a panic in one
                // worker into a hang in every other one.
                let rx = jobs.lock().unwrap_or_else(|e| e.into_inner());
                match rx.recv() {
                    Ok(j) => j,
                    // The `Resolver` was dropped: the VM is going away.
                    Err(_) => return,
                }
            };
            let result = crate::net::resolve(&job.addr, "dial");
            if answers.send(Resolved { id: job.id, result }).is_err() {
                return;
            }
            // A failed wake means the reactor is gone, which the next `recv` will
            // say properly. There is nothing useful to do about it here.
            let _ = waker.wake();
        })?;
    Ok(())
}

/// What starting a connect produced.
enum Connecting {
    /// It finished inside `connect(2)`. Loopback usually does, which is why
    /// every test in this tree that dials `127.0.0.1` never reaches the
    /// reactor at all.
    Done(Value),
    /// `EINPROGRESS`: park on this.
    Wait(Box<IoWait>),
}

/// Open a socket and start connecting, walking `addrs` from `from` until one
/// gets as far as being in flight.
///
/// A free function taking no `&mut Vm`, for the same reason [`attempt`] is one:
/// it is called from the parking site, where the dialling task is current, and
/// from the resolver wake, where it is not. Everything that differs between
/// those two — arming the reactor, pushing the result — is the caller's.
///
/// `from` is not always zero. A connect that fails re-enters here at the next
/// address, which is how the address list gets walked at all now that the
/// walking is spread across parks instead of happening inside one blocking
/// `TcpStream::connect(&addrs[..])`.
fn start_connect(addrs: Vec<SocketAddr>, from: usize) -> VResult<Connecting> {
    let mut last: Option<VErr> = None;
    for i in from..addrs.len() {
        let target = addrs[i];
        // `mio::TcpStream::connect` is `socket` + `set_nonblocking` +
        // `connect`, and it never waits: an `Ok` here means the handshake has
        // *started*, not that it finished.
        let sock = match mio::net::TcpStream::connect(target) {
            Ok(s) => s,
            // Some failures are immediate and local — `ENETUNREACH` for an
            // IPv6 address on a host with no IPv6 route is the one that
            // matters here. Next address.
            Err(e) => {
                last = Some(crate::net::io_error(&e));
                continue;
            }
        };
        let stream = Rc::new(OroStream::connecting(sock, target));
        match stream.connect_check() {
            Ok(Io::Ready(())) => return Ok(Connecting::Done(Value::Stream(stream))),
            Ok(Io::Block(interest)) => {
                return Ok(Connecting::Wait(Box::new(IoWait {
                    stream,
                    op: IoOp::Connect { addrs, next: i + 1 },
                    interest,
                    token: 0,
                    deadline: None,
                    seq: 0,
                })))
            }
            Err(msg) => {
                last = Some(msg);
                continue;
            }
        }
    }
    // `last` is `None` only for an empty list, which `crate::net::resolve`
    // already refuses — it is spelled rather than `unreachable!` because a
    // panic is a worse answer than a slightly generic exception.
    Err(last.unwrap_or_else(|| os_error("[Errno -2] Name or service not known")))
}

impl Vm {
    // --- Task bookkeeping ----------------------------------------------------

    fn next_id(&mut self) -> TaskId {
        self.next_task_id += 1;
        self.next_task_id
    }

    /// Whether the running task is the main one. Main has no handle because
    /// nothing can `join` it, which is also exactly what makes its outcome the
    /// *program's* outcome rather than a task's.
    fn current_is_main(&self) -> bool {
        self.task.handle.is_none()
    }

    /// File the current task under `park` and leave `Vm::task` spent. The
    /// caller must make another task current immediately.
    fn park_current(&mut self, park: Park) {
        let task = std::mem::replace(&mut self.task, Task::new());
        self.parked.insert(task.id, Parked { task, park });
    }

    /// Wake `id` by pushing `v` onto its operand stack — the value its parked
    /// call was going to return.
    ///
    /// This is the whole resume protocol. A parking site pops its arguments
    /// before it parks, so the sleeping frame's stack is already at the depth
    /// where "push one value" completes the operation. Nothing re-enters the
    /// resource, so nothing re-borrows it.
    fn wake_with_value(&mut self, id: TaskId, v: Value) {
        let Some(mut p) = self.parked.remove(&id) else {
            return;
        };
        p.task
            .frames
            .last_mut()
            .expect("a parked task always has a frame")
            .stack
            .push(v);
        self.ready.push_back(p.task);
    }

    /// Put the running task straight back on the ready queue, with `v` as the
    /// value its call produces.
    ///
    /// The same resume protocol [`Vm::wake_with_value`] uses, for a task that
    /// was never not-runnable: `yield_now()` hands over the CPU without ever
    /// being blocked on anything.
    fn requeue_current(&mut self, v: Value) {
        let mut task = std::mem::replace(&mut self.task, Task::new());
        task.frames
            .last_mut()
            .expect("a yielding task always has a frame")
            .stack
            .push(v);
        self.ready.push_back(task);
    }

    /// Requeue the current task without pushing a value — the I/O fairness yield,
    /// where the operation's result is already on the stack.
    fn requeue_current_bare(&mut self) {
        let task = std::mem::replace(&mut self.task, Task::new());
        self.ready.push_back(task);
    }

    /// Wake `id` and raise `exc` in it. The exception cannot be unwound from
    /// here — unwinding walks `Vm::task` — so it rides on the task until the
    /// scheduler makes it current.
    fn wake_with_raise(&mut self, id: TaskId, exc: Value) {
        let Some(mut p) = self.parked.remove(&id) else {
            return;
        };
        p.task.pending_raise = Some(exc);
        self.ready.push_back(p.task);
    }

    /// Wake a task whose channel closed under it. What that means depends on
    /// how it was waiting: `recv()` raises, `for x in ch` ends the loop
    /// cleanly (§3), and a blocked `send` raises.
    fn wake_channel_closed(&mut self, id: TaskId) {
        let Some(p) = self.parked.get(&id) else {
            return;
        };
        match &p.park {
            Park::IterRecv(_, target) => {
                let target = *target;
                let mut p = self.parked.remove(&id).expect("just looked it up");
                let frame = p.task.frames.last_mut().expect("a parked task has a frame");
                // The channel is still sitting there as the `ForIter` operand.
                frame.stack.pop();
                frame.pc = target;
                self.ready.push_back(p.task);
            }
            _ => {
                let exc = self.channel_closed_exc();
                self.wake_with_raise(id, exc);
            }
        }
    }

    /// Raise `class` with `msg`, as a `Step` rather than an `Err`.
    ///
    /// The difference from [`Vm::err`] is not the class — every fault names its
    /// own class now — but *where the exception is built*: this one produces the
    /// instance here, for the sites that are already returning a `Step` and have
    /// no error position to attach. Both converge in `run_slice`.
    pub(super) fn raise(&self, class: Exc, msg: impl Into<String>) -> Step {
        let class = self.excs[class.name()].clone();
        Step::Raise(self.make_exception_instance(class, vec![Value::str(msg.into())]))
    }

    fn channel_closed_exc(&self) -> Value {
        let class = self.excs["ChannelClosed"].clone();
        self.make_exception_instance(class, vec![Value::str("channel is closed")])
    }

    /// Retire the running task: publish its outcome, hand it to anyone waiting
    /// in `join`, and drop the scheduler's handle — which is what fires §3's
    /// rule 3 for a task nobody kept.
    fn finish_task(&mut self, outcome: Result<Value, (Value, VmError)>) {
        let mut task = std::mem::replace(&mut self.task, Task::new());
        let handle = task
            .handle
            .take()
            .expect("only a spawned task is retired here");
        let joiners = std::mem::take(&mut *handle.joiners.borrow_mut());

        // A joined failure belongs to the joiner (rule 2); an unjoined one is
        // still the task's to report when its last handle dies (rule 3).
        *handle.state.borrow_mut() = match &outcome {
            Ok(v) => TaskState::Done(v.clone()),
            Err((exc, err)) if joiners.is_empty() => TaskState::Failed {
                exc: exc.clone(),
                report: self.render_report(err),
            },
            Err((exc, _)) => TaskState::FailedJoined(exc.clone()),
        };

        for j in joiners {
            match &outcome {
                Ok(v) => self.wake_with_value(j, v.clone()),
                Err((exc, _)) => self.wake_with_raise(j, exc.clone()),
            }
        }
        // Explicit, because the ordering is the semantics: the state is
        // published and the joiners are served *before* the last handle can
        // die and print.
        drop(handle);
    }

    /// Render an uncaught-in-a-task exception for §3 rule 3: the location, the
    /// exception and the script path the CLI prepends, behind two words that
    /// say whose failure it is.
    ///
    /// §3 asked for "the same rendering an uncaught top-level exception gets",
    /// and that was implemented literally, so an unjoined failed task printed
    ///
    /// ```text
    /// app.oro:42:9: KeyError: 'user'
    /// ```
    ///
    /// which is the line a program prints *as it dies*. Here the program did
    /// not die — it is still serving the other 9,999 connections, which is the
    /// property §3 exists to guarantee — and in a server log, the only place
    /// this line will ever be read, it says the wrong thing about the most
    /// important fact in it. §7 item 12 recorded the fix rather than patching
    /// it in passing, because it is a user-visible output format.
    ///
    /// `task failed: ` and nothing more. Not the task's id, which appears
    /// nowhere else in a log unless the program printed a `Task` itself; not
    /// "unjoined", which describes why you are *seeing* the line rather than
    /// what happened. The location, the exception text and the exit code are
    /// unchanged.
    fn render_report(&self, err: &RuntimeError) -> String {
        format!("task failed: {err}")
    }

    // --- The scheduler loop --------------------------------------------------

    /// Run the program: the main task, every task it spawns, and the implicit
    /// join-all at the end (§3, "When the program is finished").
    ///
    /// Returns the main task's value. The main task is not joinable, so its
    /// uncaught exception is the *program's* error rather than a stored
    /// outcome — there is nobody it could be re-raised in.
    pub(super) fn run_loop(&mut self) -> Result<Value, VmError> {
        let mut main_result: Option<Value> = None;
        let mut finished_main: Option<Task> = None;
        loop {
            let slice = self.run_slice();
            // Captured together, from the same task, before anything switches:
            // a deadlock is reported from the site that caused it, and the file
            // has to come from the same place the line and column do.
            let (line, col) = (self.task.line, self.task.col);
            let source = self.err_source();
            match slice {
                Slice::Parked(park) if matches!(*park, Park::Yield) => {
                    // A yield is not a wait. The task goes to the back of the
                    // ready queue with its `null` already pushed, so with a
                    // peer waiting the two alternate, and with nothing else
                    // ready it is picked straight back up — a `yield_now()` in
                    // a single-task program is a no-op, never a deadlock.
                    self.requeue_current(Value::None);
                }
                Slice::Parked(park) if matches!(*park, Park::YieldReady) => {
                    // The I/O fairness yield: the operation's result is already
                    // on the stack, so this requeues without pushing anything.
                    self.requeue_current_bare();
                }
                Slice::Parked(park) => {
                    // §3's corrected deadlock condition: nothing ready *and*
                    // nothing registered. A task that parked on I/O or a
                    // deadline armed the reactor on its way here, so
                    // `is_idle()` is already false for it and the wait happens
                    // below, in the one place that waits. What is left in this
                    // arm is the genuine case — everyone blocked on each
                    // other — reported from the site that caused it while that
                    // site is still the current task, which is what makes the
                    // line and column the useful ones.
                    if self.ready.is_empty() && self.reactor.is_idle() {
                        return Err(self.deadlock(&park, source, line, col));
                    }
                    self.park_current(*park);
                }
                Slice::Returned(v) => {
                    if self.current_is_main() {
                        main_result = Some(v);
                        finished_main = Some(std::mem::replace(&mut self.task, Task::new()));
                    } else {
                        self.finish_task(Ok(v));
                    }
                }
                Slice::Failed(exc, err) => {
                    // `sys.exit` is the one way to abandon in-flight work on
                    // purpose (§3): it ends the program from whichever task ran
                    // it, rather than killing only that task.
                    if self.exit_code.is_some() || self.current_is_main() {
                        if let Some(main) = finished_main {
                            self.task = main;
                        }
                        return Err(err);
                    }
                    self.finish_task(Err((exc, err)));
                }
            }

            // Readiness that arrived while other tasks were still runnable.
            // Costs nothing until something is registered.
            self.poll_reactor_briefly();

            // Pick the next task. Written as a loop rather than a match so
            // that when `wait_for_external` becomes the reactor — the only
            // thing that can make a task runnable without another task doing
            // it — the tasks it wakes are picked up here instead of being
            // stranded behind a spent placeholder.
            let next = loop {
                if let Some(t) = self.ready.pop_front() {
                    break Some(t);
                }
                if self.parked.is_empty() {
                    break None;
                }
                if !self.wait_for_external() {
                    // Main returned (or a task ended) with peers still blocked
                    // forever: the implicit join-all can never complete.
                    return Err(self.deadlock_stuck(source, line, col));
                }
            };
            match next {
                Some(t) => self.task = t,
                None => break,
            }
        }
        // Restore the main task so `last_locals` — the module namespace — is
        // where `run`, `run_main` and the VM unit tests expect it.
        if let Some(main) = finished_main {
            self.task = main;
        }
        Ok(main_result.unwrap_or(Value::None))
    }

    /// Block until something outside the VM makes a task runnable: an fd
    /// became ready, or a deadline expired.
    ///
    /// `false` means nothing outside the VM *can* — nothing is registered and
    /// nothing is on a clock — which is §3's corrected deadlock condition and
    /// the only thing this function's boolean has ever meant.
    ///
    /// `true` does not promise a task was woken. A spurious readiness, an
    /// `EINTR`, or a retry that blocked again all answer `true` and send the
    /// caller round its loop to poll once more, which is exactly right: the
    /// alternative would be reporting a deadlock because a signal arrived.
    fn wait_for_external(&mut self) -> bool {
        if self.reactor.is_idle() {
            return false;
        }
        // Re-check the pipes before committing to a blocking wait. A pipe op's
        // wake comes from a helper thread, and there is a window between a task's
        // `try_recv`/`try_send` answering "not yet" and its park being filed in
        // which the thread can make progress and signal. Draining here first
        // settles anything that landed in that window, so the wait below is only
        // ever entered when there is genuinely nothing to do — the standard
        // close of the lost-wakeup race for a condition read across threads.
        self.drain_pipes();
        self.drain_procs();
        if !self.ready.is_empty() {
            return true;
        }
        let deadline = self
            .reactor
            .next_deadline()
            .map(|at| at.saturating_duration_since(Instant::now()));
        let timeout = if self.reactor.no_pipes() {
            deadline
        } else {
            // While any off-thread work is outstanding — a pipe op or a
            // `p.wait()` — never block longer than the backstop, *even with a
            // timer pending*. The pipe/proc `Waker` is edge-triggered (mio
            // registers every fd `EPOLLET`) and an edge races the park it is
            // meant to catch; the pre-poll drain closes that in the common case
            // and this bounds the pathological one. Taking the *earlier* of the
            // timer and the backstop is the fix for the bug where a single
            // sleeping task let a missed pipe wake wait out the whole sleep
            // (round-trips seen at 4-60s against a 22ms worst case). `try_recv`
            // is the source of truth; this only decides how soon it is consulted.
            Some(deadline.map_or(PIPE_POLL_BACKSTOP, |d| d.min(PIPE_POLL_BACKSTOP)))
        };
        match self.reactor.poll(timeout) {
            Ok(ready) => {
                for r in ready {
                    self.io_ready(&r);
                }
            }
            // `epoll_wait` itself failing is not something a program can be
            // asked to handle at a call site it cannot see, so it is raised in
            // every task that was waiting on the reactor rather than swallowed
            // into a hang.
            Err(msg) => self.fail_all_io(&msg),
        }
        self.drain_dns();
        self.drain_pipes();
        self.drain_procs();
        self.fire_timers();
        true
    }

    /// A non-blocking sweep of the reactor, run every so often while tasks are
    /// still runnable.
    ///
    /// Without it, cooperative scheduling has a sharp edge: a task that sleeps
    /// 10 ms while another spins on `yield_now()` never wakes, because
    /// [`wait_for_external`](Self::wait_for_external) is only reached when the
    /// ready queue empties. One `epoll_wait` with a zero timeout every 64 task
    /// switches bounds that latency without putting a check in the dispatch
    /// loop, which is the thing §3 promised never to do.
    ///
    /// A program that never touches I/O never gets past the `is_idle` test, so
    /// this costs it two `is_empty` calls per *task switch* — and a program
    /// with one task switches once.
    fn poll_reactor_briefly(&mut self) {
        if self.reactor.is_idle() {
            return;
        }
        self.tick += 1;
        if !self.tick.is_multiple_of(64) {
            return;
        }
        match self.reactor.poll(Some(Duration::ZERO)) {
            Ok(ready) => {
                for r in ready {
                    self.io_ready(&r);
                }
            }
            Err(msg) => self.fail_all_io(&msg),
        }
        self.drain_dns();
        self.drain_pipes();
        self.drain_procs();
        self.fire_timers();
    }

    /// An fd is ready: hand the news to whoever is parked on that side of it.
    ///
    /// [`DNS_TOKEN`] falls out of this on its own — nothing is ever filed under
    /// it in `waiters` — and is handled by [`drain_dns`](Self::drain_dns),
    /// which runs after every poll rather than off this event. That is
    /// deliberate: an answer can land in the queue without a wake being seen
    /// (a `poll` that returned for another fd first), and draining
    /// unconditionally means such an answer is never held until the next one
    /// arrives to fetch it.
    fn io_ready(&mut self, r: &ReadyFd) {
        let Some(w) = self.reactor.waiters.get(&r.token) else {
            return;
        };
        let (rd, wr) = (w.read, w.write);
        if r.readable {
            if let Some(id) = rd {
                self.retry_io(id);
            }
        }
        if r.writable {
            if let Some(id) = wr.filter(|id| Some(*id) != rd || !r.readable) {
                self.retry_io(id);
            }
        }
    }

    /// Try to finish task `id`'s parked operation, now that its fd says it can
    /// make progress.
    ///
    /// The task is *not* current while this runs, which is why the result is
    /// pushed onto its own operand stack rather than returned: that is the same
    /// resume protocol a channel wake uses, and the reason resumption never
    /// re-borrows anything the parking site was holding.
    fn retry_io(&mut self, id: TaskId) {
        let Some(mut p) = self.parked.remove(&id) else {
            return;
        };
        let mut w = match std::mem::replace(&mut p.park, Park::Yield) {
            Park::Io(w) => w,
            other => {
                p.park = other;
                self.parked.insert(id, p);
                return;
            }
        };
        // Read before the failure path can move out of `w.op`.
        let token = w.token;
        match attempt(&mut w) {
            Ok(Io::Ready(v)) => {
                self.reactor.disarm(token, id);
                p.task
                    .frames
                    .last_mut()
                    .expect("a parked task always has a frame")
                    .stack
                    .push(v);
                self.ready.push_back(p.task);
            }
            Ok(Io::Block(i)) => {
                // Still not enough: a readable socket can hand over fewer bytes
                // than `read_until` needs, and a writable one can take fewer
                // than `write` has left. The accumulator in `w.op` is exactly
                // what makes going round again cheap instead of wrong.
                w.interest = i;
                match self.reactor.arm(&w.stream, i, id) {
                    Ok(token) => {
                        w.token = token;
                        p.park = Park::Io(w);
                        self.parked.insert(id, p);
                    }
                    Err(msg) => self.resume_failed(p, id, w.token, &msg),
                }
            }
            Err(msg) => {
                // A failed connect is the one failure that may not be final:
                // there can be another address to try, and trying it needs a
                // fresh socket, which `attempt` has no way to produce.
                if let IoOp::Connect { addrs, next } = w.op {
                    self.reactor.disarm(token, id);
                    // Back into `parked` with a placeholder, because
                    // `connect_woken` settles the task through the same
                    // `wake_*`/`repark_io` protocol every other waker uses and
                    // those all expect to find it there.
                    p.park = Park::Yield;
                    self.parked.insert(id, p);
                    self.connect_woken(id, addrs, next, msg);
                    return;
                }
                self.resume_failed(p, id, token, &msg)
            }
        }
    }

    /// A helper thread signalled the pipe `Waker`: re-attempt every task parked
    /// on a pipe op, exactly as [`drain_dns`](Self::drain_dns) settles finished
    /// lookups after every poll.
    ///
    /// It re-attempts *all* of them rather than only the one whose chunk arrived
    /// because the `Waker` carries no id — it says "a pipe made progress", not
    /// which. A retry that still blocks is cheap (one `try_recv`/`try_send` that
    /// answers "empty"/"full") and re-parks, so scanning the set is the whole
    /// cost, and the set holds one entry per *active* pipe stream, not one per
    /// spawned child. A snapshot of the ids is taken first so the borrow of
    /// `parked` is over before `retry_pipe` mutates it.
    fn drain_pipes(&mut self) {
        let Some(h) = self.reactor.pipes.as_ref() else {
            return;
        };
        if h.waiters.is_empty() {
            return;
        }
        let ids: Vec<TaskId> = h.waiters.iter().copied().collect();
        for id in ids {
            self.retry_pipe(id);
        }
    }

    /// Try to finish task `id`'s parked pipe op. The pipe twin of
    /// [`retry_io`](Self::retry_io), and deliberately simpler: a pipe fd is not
    /// registered with mio, so there is no token to arm or disarm — the whole of
    /// re-parking is leaving the task in `parked` with the `waiter` entry
    /// standing. `Connect` cannot occur on a pipe, so the one branch `retry_io`
    /// keeps for it is gone too.
    fn retry_pipe(&mut self, id: TaskId) {
        let Some(mut p) = self.parked.remove(&id) else {
            return;
        };
        let mut w = match std::mem::replace(&mut p.park, Park::Yield) {
            Park::Pipe(w) => w,
            other => {
                p.park = other;
                self.parked.insert(id, p);
                return;
            }
        };
        match attempt(&mut w) {
            Ok(Io::Ready(v)) => {
                self.reactor.pipe_unwait(id);
                p.task
                    .frames
                    .last_mut()
                    .expect("a parked task always has a frame")
                    .stack
                    .push(v);
                self.ready.push_back(p.task);
            }
            // Still nothing available (read) or no room (write): leave it parked,
            // its `waiter` entry untouched, for the next signal.
            Ok(Io::Block(_)) => {
                p.park = Park::Pipe(w);
                self.parked.insert(id, p);
            }
            Err(e) => {
                self.reactor.pipe_unwait(id);
                p.task.pending_raise = Some(self.error_to_exception(&self.err(e.clone())));
                self.ready.push_back(p.task);
            }
        }
    }

    /// Wake every `p.wait()` whose child has been reaped. The proc twin of
    /// [`drain_pipes`](Self::drain_pipes), run after every poll: the reaper
    /// thread signals the same `Waker`, so this re-checks each waiting task's
    /// `Proc` for a delivered exit code.
    fn drain_procs(&mut self) {
        let Some(h) = self.reactor.pipes.as_ref() else {
            return;
        };
        if h.proc_waiters.is_empty() {
            return;
        }
        let ids: Vec<TaskId> = h.proc_waiters.iter().copied().collect();
        for id in ids {
            self.retry_proc(id);
        }
    }

    /// Deliver `id`'s exit code if the reaper has it. Caching on the `Proc` means
    /// several tasks may wait on one child: the first delivery caches the code
    /// and every waiter reads it, this pass or the next.
    fn retry_proc(&mut self, id: TaskId) {
        let proc = match self.parked.get(&id).map(|p| &p.park) {
            Some(Park::ProcWait(proc)) => proc.clone(),
            _ => return,
        };
        if let Some(code) = proc.try_code() {
            self.reactor.proc_unwait(id);
            self.wake_with_value(id, Value::Int(code));
        }
    }

    /// `p.wait()` — park the calling task until the child exits, never blocking
    /// the VM. The reaper thread does the blocking `wait(2)` off-thread and
    /// delivers the code, exactly as the pipes stream bytes and the resolver
    /// resolves names. If the code is already known the task does not park at all.
    pub(super) fn do_proc_wait(&mut self, p: Rc<crate::process::Proc>) -> Result<Step, VmError> {
        if let Some(code) = p.cached_code() {
            self.push(Value::Int(code));
            return Ok(Step::Next);
        }
        let waker = match self.reactor.pipe_waker() {
            Ok(w) => w,
            Err(msg) => return Err(self.err(msg)),
        };
        p.ensure_waiter(waker);
        // The child may have exited between spawn and now; take the code without
        // a park if it is already there.
        if let Some(code) = p.try_code() {
            self.push(Value::Int(code));
            return Ok(Step::Next);
        }
        self.reactor.proc_wait(self.task.id);
        Ok(Step::Park(Box::new(Park::ProcWait(p))))
    }

    /// Everything the resolver threads have finished, handed to the tasks that
    /// asked for it.
    ///
    /// Answers are matched by lookup id against the park, the same way
    /// [`fire_timers`](Self::fire_timers) matches a deadline by `seq`. A task
    /// parked on a lookup cannot run, so it cannot re-park on something else
    /// and there is no known way for an answer to arrive late — the check is
    /// here because "cannot happen" is a claim, and a mismatched wake would
    /// push a `TcpStream` onto some unrelated task's operand stack.
    fn drain_dns(&mut self) {
        // Taken out of the reactor whole, so the borrow is over before anything
        // below touches the VM.
        let mut done: Vec<(TaskId, u64, VResult<Vec<SocketAddr>>)> = Vec::new();
        if let Some(r) = self.reactor.dns.as_mut() {
            while let Ok(d) = r.answers.try_recv() {
                if let Some(task) = r.waiters.remove(&d.id) {
                    done.push((task, d.id, d.result));
                }
            }
        }
        for (task, lookup, result) in done {
            match self.parked.get(&task).map(|p| &p.park) {
                Some(Park::Dns { id, .. }) if *id == lookup => {}
                _ => continue,
            }
            match result {
                // The lookup is over and the connect begins, without the task
                // running in between. It has to be this way round: the task's
                // `dial` call has one value to be handed, and that value is the
                // connected stream, so the addresses can never be pushed onto
                // its stack for it to do something with.
                // `from == 0`, so `last` is never read: there is always an
                // address left to try.
                Ok(addrs) => {
                    self.connect_woken(task, addrs, 0, os_error("[Errno -2] no address tried"))
                }
                Err(msg) => {
                    let exc = self.error_to_exception(&self.err(msg));
                    self.wake_with_raise(task, exc);
                }
            }
        }
    }

    /// Start (or continue) a connect on behalf of a task that is parked, and
    /// settle it: woken with the stream, re-parked on the handshake, or woken
    /// with the exception.
    ///
    /// `last` is the failure that sent us to `from`, and is what gets raised if
    /// there is nothing left to try. It is carried rather than regenerated
    /// because "connection refused" from the address that actually refused is a
    /// better exception than anything this function could synthesise once the
    /// socket is gone.
    fn connect_woken(&mut self, task: TaskId, addrs: Vec<SocketAddr>, from: usize, last: VErr) {
        let started = if from < addrs.len() {
            start_connect(addrs, from)
        } else {
            Err(last)
        };
        match started {
            Ok(Connecting::Done(v)) => self.wake_with_value(task, v),
            Ok(Connecting::Wait(w)) => self.repark_io(task, w),
            Err(msg) => {
                let exc = self.error_to_exception(&self.err(msg));
                self.wake_with_raise(task, exc);
            }
        }
    }

    /// Move an already-parked task from whatever it was waiting on to this I/O.
    ///
    /// The one park transition that does not go through `Vm::step`, because the
    /// task is not current and cannot be made current to do it — a lookup
    /// finishing has to become a connect without the program in between. It
    /// still holds the shape the module docs demand: `w` is owned, nothing
    /// borrowed from the stream crosses the transition, and the resume protocol
    /// is untouched.
    fn repark_io(&mut self, task: TaskId, mut w: Box<IoWait>) {
        if let Err(msg) = self.arm_io(task, &mut w) {
            let exc = self.error_to_exception(&self.err(msg));
            self.wake_with_raise(task, exc);
            return;
        }
        if let Some(p) = self.parked.get_mut(&task) {
            p.park = Park::Io(w);
        }
    }

    /// A parked task's operation failed while it was not current: hand it the
    /// exception to raise the moment it is.
    fn resume_failed(&mut self, mut p: Parked, id: TaskId, token: usize, e: &VErr) {
        self.reactor.disarm(token, id);
        p.task.pending_raise = Some(self.error_to_exception(&self.err(e.clone())));
        self.ready.push_back(p.task);
    }

    /// The reactor itself failed. Every task waiting on it learns why.
    ///
    /// Lookups are in this too. The answer comes back through the reactor, so a
    /// dead `epoll` strands a task parked on a name exactly as surely as one
    /// parked on a socket, and leaving it out would turn the one failure the
    /// program cannot handle into the one thing it cannot even see.
    fn fail_all_io(&mut self, e: &VErr) {
        let ids: Vec<TaskId> = self
            .parked
            .iter()
            .filter(|(_, p)| {
                matches!(
                    p.park,
                    Park::Io(_) | Park::Dns { .. } | Park::Pipe(_) | Park::ProcWait(_)
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let exc = self.error_to_exception(&self.err(e.clone()));
            self.wake_with_raise(id, exc);
        }
        self.reactor.waiters.clear();
        if let Some(d) = self.reactor.dns.as_mut() {
            d.waiters.clear();
        }
        // A pipe read and a `p.wait()` both wait on the same epoll's `Waker`; a
        // dead reactor strands them as surely as a socket, so both are cleared.
        if let Some(h) = self.reactor.pipes.as_mut() {
            h.waiters.clear();
            h.proc_waiters.clear();
        }
    }

    /// Wake everything whose deadline has passed.
    ///
    /// A timer whose task has since moved on — its read completed, or it parked
    /// on something else — is dropped unfired, recognised by the sequence
    /// number rather than by scrubbing the list every time a task wakes.
    fn fire_timers(&mut self) {
        let now = Instant::now();
        while self.reactor.timers.first().is_some_and(|t| t.at <= now) {
            let t = self.reactor.timers.remove(0);
            match self.parked.get(&t.task).map(|p| &p.park) {
                Some(Park::Sleep(seq)) if *seq == t.seq => {
                    self.wake_with_value(t.task, Value::None);
                }
                Some(Park::Io(w)) if w.seq == t.seq => {
                    let token = w.token;
                    self.reactor.disarm(token, t.task);
                    // The same exception `set_timeout` has always raised, with
                    // the same message: CPython's bare "timed out", which the
                    // classifier already keys `TimeoutError` on.
                    let exc = self.error_to_exception(&self.err(timeout_error("timed out")));
                    self.wake_with_raise(t.task, exc);
                }
                _ => {}
            }
        }
    }

    fn deadlock(&self, park: &Park, source: Rc<str>, line: u32, col: u32) -> VmError {
        let mut waits: Vec<String> = self.parked.values().map(|p| p.park.what()).collect();
        waits.push(park.what());
        waits.sort();
        Box::new(RuntimeError {
            class: Exc::RuntimeError,
            message: format!(
                "deadlock: every task is blocked and nothing can wake them ({})",
                waits.join(", ")
            )
            .into_boxed_str(),
            source,
            line,
            col,
        })
    }

    /// The same failure seen from the other side: a task *ended*, and what is
    /// left cannot proceed. Whether main is among the blocked is deliberately
    /// not claimed — it may well be.
    fn deadlock_stuck(&self, source: Rc<str>, line: u32, col: u32) -> VmError {
        let mut waits: Vec<String> = self.parked.values().map(|p| p.park.what()).collect();
        waits.sort();
        Box::new(RuntimeError {
            class: Exc::RuntimeError,
            message: format!(
                "deadlock: nothing is runnable and {} task(s) are blocked forever ({})",
                self.parked.len(),
                waits.join(", ")
            )
            .into_boxed_str(),
            source,
            line,
            col,
        })
    }

    // --- `spawn` -------------------------------------------------------------

    /// `spawn(f, *args, **kwargs)` — start `f(*args, **kwargs)` as a task and
    /// hand back its handle.
    ///
    /// The spawner keeps running and the new task goes to the back of the ready
    /// queue. That is the only reading of §3 that fits "returns a `Task`": if
    /// the callee ran first, `spawn` could not have returned yet.
    ///
    /// Arguments are bound here, in the spawner, by the same `bind_call` a
    /// direct call uses — so every binding error (an unexpected keyword, a
    /// missing argument, too many positionals) is the direct call's error,
    /// raised at the `spawn` line, before a task exists. `spawn` has no
    /// keyword parameters of its own, so every keyword belongs to `f`.
    pub(super) fn do_spawn(
        &mut self,
        mut args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        if args.is_empty() {
            return Ok(self.raise(
                Exc::TypeError,
                "spawn() takes at least 1 argument (0 given)",
            ));
        }
        let callee = args.remove(0);

        // A task exists to be able to suspend, and only an Oro frame can. A
        // builtin runs to completion without ever reaching a park point, so
        // spawning one would be a slower way of calling it.
        let frame = match &callee {
            Value::Func(f) if !f.code.is_generator => self.bind_call(f, None, args, kwargs)?,
            Value::Func(_) => {
                return Ok(self.raise(
                    Exc::TypeError,
                    "spawn() cannot start a generator function as a task",
                ))
            }
            Value::Method(m) => match &m.kind {
                MethodKind::User { func, defclass } if !func.code.is_generator => {
                    let mut frame = self.bind_call(func, Some(m.receiver.clone()), args, kwargs)?;
                    frame.super_ctx = Some((defclass.clone(), m.receiver.clone()));
                    frame
                }
                _ => {
                    return Ok(self.raise(Exc::TypeError, "spawn() needs a function defined in Oro"))
                }
            },
            other => {
                return Ok(self.raise(
                    Exc::TypeError,
                    format!(
                        "spawn() needs a function defined in Oro, not '{}'",
                        other.type_label()
                    ),
                ))
            }
        };

        let id = self.next_id();
        let handle = Rc::new(TaskHandle::new(id, Rc::clone(&self.failure_flag)));
        let mut task = Task::new();
        task.id = id;
        task.handle = Some(Rc::clone(&handle));
        task.frames.push(frame);
        self.ready.push_back(task);
        self.push(Value::Task(handle));
        Ok(Step::Next)
    }

    // --- `yield_now` ---------------------------------------------------------

    /// `yield_now()` — hand the CPU to the next ready task, and answer `null`.
    ///
    /// The only way to yield used to be `spawn(nothing).join()`: two
    /// allocations, a stack segment and a scheduler round trip to express a
    /// no-op. §7 item 11 reopened it, and this is the answer.
    ///
    /// Named `yield_now` because `yield` is a keyword (`def yield()` does not
    /// parse) and because it is the term of art — Rust's `yield_now`, Go's
    /// `Gosched`.
    ///
    /// It cannot be a plain native builtin: a builtin returns a `Value`, and
    /// the whole content of this one is the `Step` it returns instead.
    pub(super) fn do_yield_now(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        if !kwargs.is_empty() {
            return Ok(self.raise(Exc::TypeError, "yield_now() takes no keyword arguments"));
        }
        if !args.is_empty() {
            return Ok(self.raise(
                Exc::TypeError,
                format!(
                    "yield_now() takes 0 argument(s) but {} were given",
                    args.len()
                ),
            ));
        }
        // Nothing is pushed here: the scheduler pushes the `null` when it
        // requeues the task, which is the same resume protocol every other
        // park point uses.
        Ok(Step::Park(Box::new(Park::Yield)))
    }

    /// `t.join()` — §3's rules 1 and 2.
    pub(super) fn task_join(&mut self, handle: Rc<TaskHandle>) -> Result<Step, VmError> {
        if handle.id == self.task.id {
            return Ok(self.raise(Exc::RuntimeError, "a task cannot join itself"));
        }
        // Taking the outcome and re-publishing it are two borrows, never one
        // held across the `raise` — a joined failure mutates the handle.
        let taken = match &*handle.state.borrow() {
            TaskState::Running => None,
            TaskState::Done(v) => Some(Ok(v.clone())),
            TaskState::Failed { exc, .. } | TaskState::FailedJoined(exc) => Some(Err(exc.clone())),
        };
        match taken {
            Some(Ok(v)) => {
                self.push(v);
                Ok(Step::Next)
            }
            Some(Err(exc)) => {
                // Claimed: the joiner owns it now, so the handle's death is
                // silent. `join` is idempotent — a second one re-raises the
                // same exception rather than reporting "already joined".
                *handle.state.borrow_mut() = TaskState::FailedJoined(exc.clone());
                Ok(Step::Raise(exc))
            }
            None => {
                handle.joiners.borrow_mut().push(self.task.id);
                Ok(Step::Park(Box::new(Park::Join(handle))))
            }
        }
    }

    // --- Channels ------------------------------------------------------------

    /// `chan()` / `chan(cap=n)`. The capacity has a default, so it is passed by
    /// name: `chan(8)` does not say what the 8 is, and `repr` already prints it
    /// as `cap=`. `chan(cap=0)` is an explicit spelling of the default, not an
    /// error (§3); `chan(cap=null)` is not a third one.
    pub(super) fn do_chan(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        if let Some(first) = args.first() {
            let spelled = match first {
                Value::Int(n) => format!("chan(cap={n})"),
                _ => "chan(cap=n)".to_string(),
            };
            return Ok(self.raise(
                Exc::TypeError,
                format!(
                "chan() takes no positional arguments — the capacity is passed by name: {spelled}"
            ),
            ));
        }
        let mut cap = 0;
        for (k, v) in &kwargs {
            cap = match (k.as_str(), v) {
                ("cap", Value::Int(n)) if *n >= 0 => *n as usize,
                ("cap", Value::Int(n)) => {
                    return Ok(self.raise(
                        Exc::ValueError,
                        format!("chan() cap must not be negative ({n})"),
                    ))
                }
                ("cap", Value::None) => return Ok(self.raise(
                    Exc::TypeError,
                    "chan() cap must be an int, not null — leave it out for a rendezvous: chan()",
                )),
                ("cap", other) => {
                    return Ok(self.raise(
                        Exc::TypeError,
                        format!("chan() cap must be an int, not '{}'", other.type_label()),
                    ))
                }
                (other, _) => {
                    return Ok(self.raise(
                        Exc::TypeError,
                        format!("chan() got an unexpected keyword argument '{other}'"),
                    ))
                }
            };
        }
        self.push(Value::Channel(Rc::new(Channel::new(cap))));
        Ok(Step::Next)
    }

    /// `ch.send(v)`.
    ///
    /// A waiting receiver is handed the value directly and the sender does not
    /// block — an unbuffered send completes the instant a receiver is known to
    /// be there, which is what "rendezvous" means.
    pub(super) fn chan_send(&mut self, ch: Rc<Channel>, v: Value) -> Result<Step, VmError> {
        if ch.closed.get() {
            return Ok(Step::Raise(self.channel_closed_exc()));
        }
        // Borrow, decide, drop — never a borrow held across `wake_*`.
        let waiter = ch.recv_waiters.borrow_mut().pop_front();
        if let Some(rid) = waiter {
            // A `for msg in ch` receiver is woken with an (index, value) pair,
            // like every `for`; a plain `recv()` receiver is woken with the bare
            // value. The parked task's `Park` says which.
            let is_iter = matches!(
                self.parked.get(&rid).map(|p| &p.park),
                Some(Park::IterRecv(..))
            );
            let delivered = if is_iter {
                let n = ch.iter_index.get();
                ch.iter_index.set(n + 1);
                Value::Tuple(OroTuple::new(vec![Value::Int(n), v]))
            } else {
                v
            };
            self.wake_with_value(rid, delivered);
            self.push(Value::None);
            return Ok(Step::Next);
        }
        let leftover = {
            let mut buf = ch.buf.borrow_mut();
            if buf.len() < ch.cap {
                buf.push_back(v);
                None
            } else {
                Some(v)
            }
        };
        match leftover {
            None => {
                self.push(Value::None);
                Ok(Step::Next)
            }
            Some(v) => {
                ch.send_waiters.borrow_mut().push_back((self.task.id, v));
                Ok(Step::Park(Box::new(Park::Send(ch))))
            }
        }
    }

    /// The value a `recv` can take right now, if any, plus the bookkeeping that
    /// goes with taking it. `None` means "would block".
    fn chan_take(&mut self, ch: &Rc<Channel>) -> Option<Value> {
        let buffered = ch.buf.borrow_mut().pop_front();
        if let Some(v) = buffered {
            // A slot just freed up: let the oldest blocked sender fill it.
            let waiting = ch.send_waiters.borrow_mut().pop_front();
            if let Some((sid, sv)) = waiting {
                ch.buf.borrow_mut().push_back(sv);
                self.wake_with_value(sid, Value::None);
            }
            return Some(v);
        }
        // Unbuffered: the value is on the sender, and taking it completes the
        // rendezvous for both sides.
        let waiting = ch.send_waiters.borrow_mut().pop_front();
        if let Some((sid, sv)) = waiting {
            self.wake_with_value(sid, Value::None);
            return Some(sv);
        }
        None
    }

    /// `ch.recv()`. A closed *and drained* channel raises; a closed channel
    /// with items still hands them out first (§3).
    pub(super) fn chan_recv(&mut self, ch: Rc<Channel>) -> Result<Step, VmError> {
        if let Some(v) = self.chan_take(&ch) {
            self.push(v);
            return Ok(Step::Next);
        }
        if ch.closed.get() {
            return Ok(Step::Raise(self.channel_closed_exc()));
        }
        ch.recv_waiters.borrow_mut().push_back(self.task.id);
        Ok(Step::Park(Box::new(Park::Recv(ch))))
    }

    /// One step of `for msg in ch`. Identical to `recv()` except that a closed
    /// and drained channel ends the loop instead of raising — which is what
    /// makes the producer/consumer shape read like a `for` over a generator.
    pub(super) fn chan_iter_next(
        &mut self,
        ch: Rc<Channel>,
        target: usize,
    ) -> Result<Step, VmError> {
        if let Some(v) = self.chan_take(&ch) {
            // Every `for` yields (index, value); a channel's index is a 0-based
            // receive counter held on the Channel.
            let n = ch.iter_index.get();
            ch.iter_index.set(n + 1);
            self.push(Value::Tuple(OroTuple::new(vec![Value::Int(n), v])));
            return Ok(Step::Next);
        }
        if ch.closed.get() {
            self.pop(); // the channel, standing in as its own iterator
            self.top().pc = target;
            return Ok(Step::Next);
        }
        ch.recv_waiters.borrow_mut().push_back(self.task.id);
        Ok(Step::Park(Box::new(Park::IterRecv(ch, target))))
    }

    /// `ch.close()`. Idempotent: closing a closed channel is a no-op, because
    /// in a language whose answer to `with` is deterministic drop, `close()`
    /// gets written defensively and a second one is not a bug.
    pub(super) fn chan_close(&mut self, ch: Rc<Channel>) -> Result<Step, VmError> {
        if !ch.closed.get() {
            ch.closed.set(true);
            let receivers: Vec<TaskId> = ch.recv_waiters.borrow_mut().drain(..).collect();
            let senders: Vec<TaskId> = ch
                .send_waiters
                .borrow_mut()
                .drain(..)
                .map(|(id, _)| id)
                .collect();
            for id in receivers {
                self.wake_channel_closed(id);
            }
            for id in senders {
                let exc = self.channel_closed_exc();
                self.wake_with_raise(id, exc);
            }
        }
        self.push(Value::None);
        Ok(Step::Next)
    }

    // --- The import rendezvous ----------------------------------------------

    /// A module body finished or died: release the path and settle everyone who
    /// parked waiting for it.
    ///
    /// `outcome` is the module value, or the exception the body raised.
    pub(super) fn release_import(&mut self, path: &str, outcome: Result<Value, Value>) {
        self.importing.remove(path);
        let Some(waiters) = self.import_waiters.remove(path) else {
            return;
        };
        for id in waiters {
            match &outcome {
                Ok(m) => self.wake_with_value(id, m.clone()),
                Err(exc) => self.wake_with_raise(id, exc.clone()),
            }
        }
    }

    /// A module body is already running. If *this* task is the one running it,
    /// that is a genuine cycle; if another task is, this task simply has to
    /// wait for it — the same `import` in two tasks is not a circular import.
    pub(super) fn await_import(&mut self, path: &str, owner: TaskId) -> Step {
        if owner == self.task.id {
            let class = self.excs["ImportError"].clone();
            let msg = Value::str(format!("circular import detected while importing '{path}'"));
            return Step::Raise(self.make_exception_instance(class, vec![msg]));
        }
        self.import_waiters
            .entry(path.to_string())
            .or_default()
            .push(self.task.id);
        Step::Park(Box::new(Park::Import(Rc::from(path))))
    }

    // --- Frame-level helpers the scheduler owns -----------------------------

    // --- I/O: the three parking sites, and `time.sleep` ---------------------

    /// The five stream methods the VM has to own.
    ///
    /// Four of them can park, and a parking primitive cannot be a plain
    /// `Builtin`: a builtin must answer with a `Value`, and the whole content
    /// of these is the `Step` they answer with instead. That is §3's "one hard
    /// constraint on implementation", and it is the same reason `spawn`,
    /// `chan`, `yield_now` and `proc.run` are dispatched here.
    ///
    /// The fifth is `close()`, which cannot block and is here anyway, because
    /// it is the other half of the hazard: closing a stream a task is parked on
    /// has to *raise in that task*, and only the scheduler can reach it.
    ///
    /// `Ok(None)` means "not one of mine, carry on" — the same protocol
    /// [`Vm::task_or_channel_method`] uses.
    pub(super) fn stream_io_method(
        &mut self,
        recv: &Value,
        name: &str,
        args: &[Value],
        kwargs: &[(String, Value)],
    ) -> Result<Option<Step>, VmError> {
        let Value::Stream(s) = recv else {
            return Ok(None);
        };
        if !matches!(name, "read" | "write" | "read_until" | "accept" | "close") {
            return Ok(None);
        }
        if !kwargs.is_empty() {
            return Ok(Some(self.raise(
                Exc::TypeError,
                format!("{name}() takes no keyword arguments"),
            )));
        }
        let s = Rc::clone(s);
        let op = match name {
            "read" => match args {
                [Value::Int(n)] => IoOp::Read(*n),
                // `read()` with no argument would be a second behaviour under
                // one name, and an unbounded read is a memory footgun on a
                // server. Reading a whole stream is `io.read(r)`.
                [] => {
                    return Err(self.err(type_error(
                        "read() takes a size — use io.read(r) to read a whole stream",
                    )))
                }
                _ => {
                    return Err(self.err(type_error(format!(
                        "read() size argument must be int, not '{}'",
                        crate::builtins::type_of(args, 0)
                    ))))
                }
            },
            // Bytes only, in both directions, everywhere in the language. The
            // spellings for text are `print("hi")` and `w.write(s.to_bytes())`.
            "write" => IoOp::Write {
                buf: self.wrap(crate::builtins::bytes_arg(args, 0, "write"))?,
                done: 0,
            },
            "read_until" => {
                let delim = self.wrap(crate::builtins::bytes_arg(args, 0, "read_until"))?;
                let limit = match args.get(1) {
                    Some(Value::Int(n)) => *n,
                    // The limit is required, not defaulted: it is what stops a
                    // client sending an unbounded header block, and a default
                    // would be a number nobody chose.
                    None => {
                        return Err(
                            self.err(type_error("read_until() takes a delimiter and a limit"))
                        )
                    }
                    Some(_) => {
                        return Err(self.err(type_error(format!(
                            "read_until() limit argument must be int, not '{}'",
                            crate::builtins::type_of(args, 1)
                        ))))
                    }
                };
                self.wrap(crate::builtins::exactly(args, 2, "read_until"))?;
                IoOp::ReadUntil {
                    delim,
                    limit,
                    acc: Vec::new(),
                }
            }
            "accept" => {
                self.wrap(crate::builtins::exactly(args, 0, "accept"))?;
                IoOp::Accept
            }
            _ => {
                self.wrap(crate::builtins::exactly(args, 0, "close"))?;
                return self.stream_close(&s).map(Some);
            }
        };
        self.begin_io(s, op).map(Some)
    }

    /// `close()`, and the task that may be parked on the stream being closed.
    ///
    /// This is the hazard from `src/net.rs`, met head-on. The parked task holds
    /// an `Rc` and no borrow, so `close()` cannot panic; what it can do is
    /// strand a task on an fd that is about to stop existing, so every waiter
    /// is woken with the exception it would have got had it called the method
    /// one instruction later — `read() on a closed TcpStream`, a plain
    /// `ValueError`, catchable like any other.
    fn stream_close(&mut self, s: &Rc<OroStream>) -> Result<Step, VmError> {
        self.wrap(s.close())?;
        if let Some(token) = s.token() {
            for id in self.reactor.take_waiters(token) {
                let what = match self.parked.get(&id).map(|p| &p.park) {
                    Some(Park::Io(w)) => w.op.what(),
                    _ => continue,
                };
                // Character for character the message `borrow_open` would
                // have produced one instruction later, so the same close is
                // the same exception however the timing falls.
                let msg = format!("{what} on a closed {}", s.kind.type_name());
                let exc = self.error_to_exception(&self.err(value_error(msg)));
                self.wake_with_raise(id, exc);
            }
        }
        // A pipe has no reactor token — it is not an mio fd — so a task parked
        // on the very stream just closed cannot be found through `take_waiters`,
        // and `close()` has already dropped the channel end that its helper
        // thread would signal through. Left alone it would wait for a wake that
        // can never come. So the pipe waiters are scanned for one parked on this
        // exact stream and woken with the same closed-stream exception the
        // socket path gives, one instruction early. Same close, same error.
        if s.is_pipe() {
            let hit: Vec<TaskId> = self
                .parked
                .iter()
                .filter_map(|(id, p)| match &p.park {
                    Park::Pipe(w) if Rc::ptr_eq(&w.stream, s) => Some((*id, w.op.what())),
                    _ => None,
                })
                .map(|(id, _)| id)
                .collect();
            for id in hit {
                let what = match self.parked.get(&id).map(|p| &p.park) {
                    Some(Park::Pipe(w)) => w.op.what(),
                    _ => continue,
                };
                self.reactor.pipe_unwait(id);
                let msg = format!("{what} on a closed {}", s.kind.type_name());
                let exc = self.error_to_exception(&self.err(value_error(msg)));
                self.wake_with_raise(id, exc);
            }
        }
        self.push(Value::None);
        Ok(Step::Next)
    }

    /// Try an operation, and park on the reactor if it cannot finish now.
    ///
    /// The overwhelmingly common case is that it finishes: buffered bytes, a
    /// connection already in the backlog, a write the send buffer takes whole.
    /// Nothing is registered and no `Park` is built for those.
    fn begin_io(&mut self, stream: Rc<OroStream>, op: IoOp) -> Result<Step, VmError> {
        let mut w = IoWait {
            stream,
            op,
            interest: mio::Interest::READABLE,
            token: 0,
            deadline: None,
            seq: 0,
        };
        match attempt(&mut w) {
            Ok(Io::Ready(v)) => {
                self.push(v);
                // A ready operation never suspends, so a tight copy loop would
                // hold the VM to EOF. Count the run of them and, every
                // `READY_IO_YIELD`, hand control back so peers get a turn — the
                // fairness `io.copy` used to need a hand-rolled `yield_now` for.
                self.ready_io_run += 1;
                if self.ready_io_run >= READY_IO_YIELD {
                    self.ready_io_run = 0;
                    Ok(Step::Park(Box::new(Park::YieldReady)))
                } else {
                    Ok(Step::Next)
                }
            }
            Ok(Io::Block(i)) => {
                // A block is already a yield; the run of ready ops ends here.
                self.ready_io_run = 0;
                w.interest = i;
                // A pipe would block for a reason the reactor cannot watch — no
                // chunk yet, or no room in the writer's channel — so it parks on
                // the pipe hub's `Waker` and a helper thread, not on an mio fd.
                if w.stream.is_pipe() {
                    self.park_pipe(w)
                } else {
                    self.park_io(w)
                }
            }
            Err(msg) => Err(self.err(msg)),
        }
    }

    /// Suspend the running task on a pipe op. The pipe twin of
    /// [`park_io`](Self::park_io): it ensures the pipe hub (and its `Waker`)
    /// exists so a helper thread has something to signal, records the task as an
    /// outstanding pipe waiter so [`is_idle`](Reactor::is_idle) does not call the
    /// wait a deadlock, and returns the `Park`. There is no fd to register and
    /// no deadline to arm — `set_timeout` is a socket knob — so this is the whole
    /// of it.
    fn park_pipe(&mut self, w: IoWait) -> Result<Step, VmError> {
        let task = self.task.id;
        if let Err(msg) = self.reactor.pipe_waker() {
            return Err(self.err(msg));
        }
        self.reactor.pipe_wait(task);
        Ok(Step::Park(Box::new(Park::Pipe(Box::new(w)))))
    }

    /// Register `w`'s stream with the reactor and suspend the running task on
    /// it.
    ///
    /// Note what is *not* held here: this function takes no borrow of the
    /// stream at all, and the `Step` it returns is handed back through
    /// `Vm::step`, so every temporary at the parking site is dropped before the
    /// scheduler ever sees the `Park`.
    fn park_io(&mut self, mut w: IoWait) -> Result<Step, VmError> {
        let task = self.task.id;
        if let Err(msg) = self.arm_io(task, &mut w) {
            return Err(self.err(msg));
        }
        Ok(Step::Park(Box::new(Park::Io(Box::new(w)))))
    }

    /// Register `w`'s stream with the reactor on `task`'s behalf, and arm its
    /// deadline the first time it blocks.
    ///
    /// Split out of [`park_io`](Self::park_io) because a connect that begins
    /// when a *lookup* finishes has to be armed for a task that is not the
    /// current one. Nothing else about it changed.
    ///
    /// The deadline is fixed on the *first* block and kept across every
    /// re-park, so `set_timeout(5)` bounds the whole `read_until` rather than
    /// granting five seconds per packet. A connect never has one: the stream is
    /// made inside the dial, so there was nowhere for the program to have put a
    /// `set_timeout` on it, and the kernel's own connect timeout stands.
    fn arm_io(&mut self, task: TaskId, w: &mut IoWait) -> VResult<()> {
        w.token = self.reactor.arm(&w.stream, w.interest, task)?;
        if w.seq == 0 {
            self.park_seq += 1;
            w.seq = self.park_seq;
            w.deadline = w.stream.timeout().map(|d| Instant::now() + d);
            if let Some(at) = w.deadline {
                self.reactor.add_timer(at, task, w.seq);
            }
        }
        Ok(())
    }

    /// `net.dial(addr)` — connect, and hand back a stream, without stopping
    /// anything but the calling task.
    ///
    /// Like `time.sleep` and `yield_now` it cannot be a plain native builtin:
    /// the whole content of it is the `Step` it returns. It used to be one,
    /// which is exactly how it came to freeze the VM twice over — once in
    /// `getaddrinfo` and once in `connect(2)` — with no way to say so, because
    /// a `Builtin` has no vocabulary for "wait".
    ///
    /// Three outcomes, and which one you get is decided by
    /// [`crate::net::plan_dial`] without touching the network:
    ///
    /// 1. A malformed address raises **here**, synchronously, at the call site,
    ///    because it is a programming error and no lookup can change the
    ///    answer.
    /// 2. A literal `ip:port` goes straight to `connect(2)`, which on loopback
    ///    usually finishes inside the call — so `net.dial("127.0.0.1:8080")`
    ///    still costs one syscall and never reaches the reactor. That is why
    ///    the old blocking implementation was invisible to every test in this
    ///    tree, and it is preserved on purpose rather than routed through the
    ///    resolver for uniformity's sake.
    /// 3. A hostname parks on the resolver pool, and the connect that follows
    ///    is started by [`drain_dns`](Self::drain_dns) when the answer lands.
    pub(super) fn do_dial(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        // Character for character what the generic builtin path said when
        // `net.dial` was one, so moving the dispatch changed no diagnostic.
        if !kwargs.is_empty() {
            return Err(self.err(type_error("net.dial() takes no keyword arguments")));
        }
        let addr = self.wrap(super::modules::one_addr(&args, "dial"))?;
        match self.wrap(crate::net::plan_dial(&addr))? {
            crate::net::DialPlan::Connect(addrs) => match self.wrap(start_connect(addrs, 0))? {
                Connecting::Done(v) => {
                    self.push(v);
                    Ok(Step::Next)
                }
                Connecting::Wait(w) => self.park_io(*w),
            },
            crate::net::DialPlan::Lookup(addr) => {
                let task = self.task.id;
                let id = match self.reactor.lookup(addr.clone(), task) {
                    Ok(id) => id,
                    Err(msg) => return Err(self.err(msg)),
                };
                Ok(Step::Park(Box::new(Park::Dns {
                    id,
                    addr: Rc::from(addr),
                })))
            }
        }
    }

    /// `time.sleep(secs)` — park this task for `secs`, and let every other one
    /// run meanwhile.
    ///
    /// It was `std::thread::sleep`, which stopped the whole VM: one task
    /// sleeping meant ten thousand connections sleeping. Like `yield_now` it
    /// cannot be a plain native builtin, because the whole content of it is the
    /// `Step` it returns.
    pub(super) fn do_sleep(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, VmError> {
        if !kwargs.is_empty() {
            return Ok(self.raise(Exc::TypeError, "sleep() takes no keyword arguments"));
        }
        let secs = match args.as_slice() {
            [Value::Float(f)] => *f,
            [Value::Int(i)] => *i as f64,
            [Value::Bool(b)] => *b as i64 as f64,
            _ => return Err(self.err(type_error("sleep() takes one number of seconds"))),
        };
        // `>= 0.0` rather than `!(< 0.0)`: NaN is not a length, and the
        // negative message is the right one for it.
        if !(secs.is_finite() && secs >= 0.0) {
            return Err(self.err(value_error("sleep length must be non-negative")));
        }
        self.park_seq += 1;
        let seq = self.park_seq;
        // `sleep(0)` is a deadline of now: the task goes back to runnable at
        // the next sweep, having let everything else have a turn. That is what
        // CPython's `time.sleep(0)` means too, and it costs no special case.
        let at = Instant::now() + Duration::from_secs_f64(secs.min(f64::from(u32::MAX)));
        self.reactor.add_timer(at, self.task.id, seq);
        Ok(Step::Park(Box::new(Park::Sleep(seq))))
    }

    /// A module body frame is being discarded by an unwinding exception. The
    /// path has to leave `importing` here, or a *retried* import reports a
    /// spurious `circular import detected` instead of the real error — and any
    /// task parked on the rendezvous waits forever.
    pub(super) fn abort_module_frame(&mut self, frame: &Frame, exc: &Value) {
        if let ReturnAction::BuildModule(path) = &frame.ret_action {
            let path = path.to_string();
            self.release_import(&path, Err(exc.clone()));
        }
    }
}
