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
//! When the reactor lands, a socket read parks as `Park::Io { stream: Rc<..>,
//! .. }` — an `Rc`, never a borrow — and the retry takes a fresh borrow inside
//! the resumed call. No reshaping needed.
//!
//! ## What is *not* here
//!
//! No reactor, no timers, no I/O readiness, no `mio`, no new dependency. Tasks
//! park and wake on scheduler-internal events only: a channel operation that
//! must block, a `join`, and an in-progress import. [`Vm::wait_for_external`]
//! is the one-line seam where the reactor will block instead of the scheduler
//! declaring a deadlock.

use std::rc::Rc;

use crate::task::{Channel, TaskHandle, TaskId, TaskState};
use crate::value::{MethodKind, Value};

use super::{Frame, ReturnAction, RuntimeError, Step, Task, Vm};

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
}

impl Park {
    /// A short phrase for the deadlock diagnostic. The channel's capacity and
    /// fill are in it because "recv on an empty unbuffered channel" and "send
    /// to a full one" are different bugs with the same word in front of them.
    fn what(&self) -> String {
        match self {
            Park::Recv(ch) | Park::IterRecv(ch, _) => {
                format!("recv (channel cap {}, {} buffered)", ch.cap, ch.buf.borrow().len())
            }
            Park::Send(ch) => {
                format!("send (channel cap {}, {} buffered)", ch.cap, ch.buf.borrow().len())
            }
            Park::Join(h) => format!("join(task {})", h.id),
            Park::Import(p) => format!("import '{p}'"),
        }
    }
}

/// A suspended task and the reason it is suspended. Only the scheduler ever
/// holds one.
pub(super) struct Parked {
    task: Task,
    park: Park,
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
    Failed(Value, RuntimeError),
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
        let Some(mut p) = self.parked.remove(&id) else { return };
        p.task
            .frames
            .last_mut()
            .expect("a parked task always has a frame")
            .stack
            .push(v);
        self.ready.push_back(p.task);
    }

    /// Wake `id` and raise `exc` in it. The exception cannot be unwound from
    /// here — unwinding walks `Vm::task` — so it rides on the task until the
    /// scheduler makes it current.
    fn wake_with_raise(&mut self, id: TaskId, exc: Value) {
        let Some(mut p) = self.parked.remove(&id) else { return };
        p.task.pending_raise = Some(exc);
        self.ready.push_back(p.task);
    }

    /// Wake a task whose channel closed under it. What that means depends on
    /// how it was waiting: `recv()` raises, `for x in ch` ends the loop
    /// cleanly (§3), and a blocked `send` raises.
    fn wake_channel_closed(&mut self, id: TaskId) {
        let Some(p) = self.parked.get(&id) else { return };
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

    /// Raise `class` with `msg`.
    ///
    /// The concurrency surface names its exception classes rather than
    /// spelling a message that `classify_error` will recognise. Everything
    /// here is new, so there is no reason to route a brand-new diagnostic
    /// through a substring table built for the old ones.
    pub(super) fn raise(&self, class: &str, msg: impl Into<String>) -> Step {
        let class = self.excs[class].clone();
        Step::Raise(self.make_exception_instance(class, vec![Value::str(msg.into())]))
    }

    fn channel_closed_exc(&self) -> Value {
        let class = self.excs["ChannelClosed"].clone();
        self.make_exception_instance(class, vec![Value::str("channel is closed")])
    }

    /// Retire the running task: publish its outcome, hand it to anyone waiting
    /// in `join`, and drop the scheduler's handle — which is what fires §3's
    /// rule 3 for a task nobody kept.
    fn finish_task(&mut self, outcome: Result<Value, (Value, RuntimeError)>) {
        let mut task = std::mem::replace(&mut self.task, Task::new());
        let handle = task.handle.take().expect("only a spawned task is retired here");
        let joiners = std::mem::take(&mut *handle.joiners.borrow_mut());

        // A joined failure belongs to the joiner (rule 2); an unjoined one is
        // still the task's to report when its last handle dies (rule 3).
        *handle.state.borrow_mut() = match &outcome {
            Ok(v) => TaskState::Done(v.clone()),
            Err((exc, err)) if joiners.is_empty() => {
                TaskState::Failed { exc: exc.clone(), report: self.render_report(err) }
            }
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

    /// Render an uncaught-in-a-task exception the way an uncaught top-level one
    /// is rendered (§3 rule 3), including the script path the CLI prepends.
    fn render_report(&self, err: &RuntimeError) -> String {
        match self.argv.first() {
            Some(script) => format!("{script}:{err}"),
            None => err.to_string(),
        }
    }

    // --- The scheduler loop --------------------------------------------------

    /// Run the program: the main task, every task it spawns, and the implicit
    /// join-all at the end (§3, "When the program is finished").
    ///
    /// Returns the main task's value. The main task is not joinable, so its
    /// uncaught exception is the *program's* error rather than a stored
    /// outcome — there is nobody it could be re-raised in.
    pub(super) fn run_loop(&mut self) -> Result<Value, RuntimeError> {
        let mut main_result: Option<Value> = None;
        let mut finished_main: Option<Task> = None;
        loop {
            let slice = self.run_slice();
            let (line, col) = (self.task.line, self.task.col);
            match slice {
                Slice::Parked(park) => {
                    // Nothing else is runnable and, with no reactor, nothing
                    // outside the VM can wake anyone: this is a real deadlock,
                    // reported from the site that caused it while that site is
                    // still the current task.
                    if self.ready.is_empty() && !self.wait_for_external() {
                        return Err(self.deadlock(&park, line, col));
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
                    return Err(self.deadlock_stuck(line, col));
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

    /// Block until something outside the VM makes a task runnable.
    ///
    /// This milestone has no reactor, so nothing outside the VM exists and the
    /// answer is always "no". When the mio reactor lands this is where it
    /// blocks on readiness and moves woken tasks onto the ready queue; the
    /// deadlock condition then correctly becomes "nothing ready *and* nothing
    /// registered".
    fn wait_for_external(&mut self) -> bool {
        false
    }

    fn deadlock(&self, park: &Park, line: u32, col: u32) -> RuntimeError {
        let mut waits: Vec<String> = self.parked.values().map(|p| p.park.what()).collect();
        waits.push(park.what());
        waits.sort();
        RuntimeError {
            message: format!(
                "deadlock: every task is blocked and nothing can wake them ({})",
                waits.join(", ")
            ),
            line: line as usize,
            col: col as usize,
        }
    }

    /// The same failure seen from the other side: a task *ended*, and what is
    /// left cannot proceed. Whether main is among the blocked is deliberately
    /// not claimed — it may well be.
    fn deadlock_stuck(&self, line: u32, col: u32) -> RuntimeError {
        let mut waits: Vec<String> = self.parked.values().map(|p| p.park.what()).collect();
        waits.sort();
        RuntimeError {
            message: format!(
                "deadlock: nothing is runnable and {} task(s) are blocked forever ({})",
                self.parked.len(),
                waits.join(", ")
            ),
            line: line as usize,
            col: col as usize,
        }
    }

    // --- `spawn` -------------------------------------------------------------

    /// `spawn(f, *args)` — start `f(*args)` as a task and hand back its handle.
    ///
    /// The spawner keeps running and the new task goes to the back of the ready
    /// queue. That is the only reading of §3 that fits "returns a `Task`": if
    /// the callee ran first, `spawn` could not have returned yet.
    pub(super) fn do_spawn(
        &mut self,
        mut args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, RuntimeError> {
        if !kwargs.is_empty() {
            return Ok(self.raise("TypeError", "spawn() takes no keyword arguments"));
        }
        if args.is_empty() {
            return Ok(self.raise("TypeError", "spawn() takes at least 1 argument (0 given)"));
        }
        let callee = args.remove(0);

        // A task exists to be able to suspend, and only an Oro frame can. A
        // builtin runs to completion without ever reaching a park point, so
        // spawning one would be a slower way of calling it.
        let frame = match &callee {
            Value::Func(f) if !f.code.is_generator => self.bind_call(f, None, args, Vec::new())?,
            Value::Func(_) => {
                return Ok(self.raise(
                    "TypeError",
                    "spawn() cannot start a generator function as a task",
                ))
            }
            Value::Method(m) => match &m.kind {
                MethodKind::User { func, defclass } if !func.code.is_generator => {
                    let mut frame =
                        self.bind_call(func, Some(m.receiver.clone()), args, Vec::new())?;
                    frame.super_ctx = Some((defclass.clone(), m.receiver.clone()));
                    frame
                }
                _ => {
                    return Ok(self
                        .raise("TypeError", "spawn() needs a function defined in Oro"))
                }
            },
            other => {
                return Ok(self.raise(
                    "TypeError",
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

    /// `t.join()` — §3's rules 1 and 2.
    pub(super) fn task_join(&mut self, handle: Rc<TaskHandle>) -> Result<Step, RuntimeError> {
        if handle.id == self.task.id {
            return Ok(self.raise("RuntimeError", "a task cannot join itself"));
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

    /// `chan()` / `chan(n)`. `chan(0)` is an explicit spelling of the default,
    /// not an error (§3).
    pub(super) fn do_chan(
        &mut self,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Step, RuntimeError> {
        if !kwargs.is_empty() {
            return Ok(self.raise("TypeError", "chan() takes no keyword arguments"));
        }
        let cap = match args.as_slice() {
            [] => 0,
            [Value::Int(n)] if *n >= 0 => *n as usize,
            [Value::Int(n)] => {
                return Ok(self
                    .raise("ValueError", format!("chan() capacity must not be negative ({n})")))
            }
            [other] => {
                return Ok(self.raise(
                    "TypeError",
                    format!("chan() capacity must be an int, not '{}'", other.type_label()),
                ))
            }
            _ => return Ok(self.raise("TypeError", "chan() takes at most 1 argument")),
        };
        self.push(Value::Channel(Rc::new(Channel::new(cap))));
        Ok(Step::Next)
    }

    /// `ch.send(v)`.
    ///
    /// A waiting receiver is handed the value directly and the sender does not
    /// block — an unbuffered send completes the instant a receiver is known to
    /// be there, which is what "rendezvous" means.
    pub(super) fn chan_send(
        &mut self,
        ch: Rc<Channel>,
        v: Value,
    ) -> Result<Step, RuntimeError> {
        if ch.closed.get() {
            return Ok(Step::Raise(self.channel_closed_exc()));
        }
        // Borrow, decide, drop — never a borrow held across `wake_*`.
        let waiter = ch.recv_waiters.borrow_mut().pop_front();
        if let Some(rid) = waiter {
            self.wake_with_value(rid, v);
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
    pub(super) fn chan_recv(&mut self, ch: Rc<Channel>) -> Result<Step, RuntimeError> {
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
    ) -> Result<Step, RuntimeError> {
        if let Some(v) = self.chan_take(&ch) {
            self.push(v);
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
    pub(super) fn chan_close(&mut self, ch: Rc<Channel>) -> Result<Step, RuntimeError> {
        if !ch.closed.get() {
            ch.closed.set(true);
            let receivers: Vec<TaskId> = ch.recv_waiters.borrow_mut().drain(..).collect();
            let senders: Vec<TaskId> =
                ch.send_waiters.borrow_mut().drain(..).map(|(id, _)| id).collect();
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
        let Some(waiters) = self.import_waiters.remove(path) else { return };
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
        self.import_waiters.entry(path.to_string()).or_default().push(self.task.id);
        Step::Park(Box::new(Park::Import(Rc::from(path))))
    }

    // --- Frame-level helpers the scheduler owns -----------------------------

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
