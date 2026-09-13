//! The two values green threads add to the language: a [`TaskHandle`] (what
//! `spawn` returns) and a [`Channel`] (what `chan` returns).
//!
//! Both are *data* — refcounted cells with no behaviour of their own. Every
//! decision about them is made by the scheduler in `crate::vm::sched`, which is
//! the only thing that may move a task between the ready queue and the parked
//! map. Keeping the state here and the policy there is what lets a channel be
//! an ordinary [`Value`](crate::value::Value) that can be stored in a dict,
//! captured by a closure, or sent down another channel.
//!
//! **Neither type is user-constructible** (`docs/stdlib-server-design.md` §3):
//! there is no `Task(...)` or `Channel(...)` class, only `spawn` and `chan`.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::value::Value;

/// Identifies one task within one VM. Never reused, so a stale id in a waiter
/// queue can be recognised rather than silently resolving to a new task.
pub type TaskId = u64;

/// How a task ended, from the point of view of anyone holding its handle.
///
/// The `Failed` / `FailedJoined` split *is* §3's rule 3: an exception nobody
/// claimed is printed when the last handle dies, and one a `join` claimed is
/// not. Nothing is ever both.
pub enum TaskState {
    /// Still running, ready, or parked.
    Running,
    /// Returned normally. `join` hands this value back, as many times as asked.
    Done(Value),
    /// Raised, and no `join` has taken the exception yet. Dropping the last
    /// handle in this state prints `report` and sets the failure flag.
    Failed {
        exc: Value,
        /// The diagnostic line, rendered at the moment of death because that is
        /// the only moment the faulting line, column and script path are known.
        report: String,
    },
    /// Raised, and a `join` took it. The joiner owns the exception; a second
    /// `join` re-raises the same one, and dropping the handle prints nothing.
    FailedJoined(Value),
}

/// The `Task` value `spawn` returns.
///
/// The scheduler holds one `Rc` of this for as long as the task is alive, so a
/// discarded handle (`spawn(f, x)` as a statement) does not fire the drop
/// report before the task has even run — it fires the moment the task dies.
pub struct TaskHandle {
    pub id: TaskId,
    pub state: RefCell<TaskState>,
    /// Tasks parked in `join()` on this one, in the order they arrived.
    pub joiners: RefCell<Vec<TaskId>>,
    /// Shared with the VM: set when an unjoined failure is reported, which is
    /// what turns the process exit code into 1. A plain `Rc<Cell<bool>>` rather
    /// than a global, because a VM is a value and tests build several.
    pub failure_flag: Rc<Cell<bool>>,
}

impl TaskHandle {
    pub fn new(id: TaskId, failure_flag: Rc<Cell<bool>>) -> TaskHandle {
        TaskHandle {
            id,
            state: RefCell::new(TaskState::Running),
            joiners: RefCell::new(Vec::new()),
            failure_flag,
        }
    }

    /// Whether this task has finished (either way).
    pub fn finished(&self) -> bool {
        !matches!(*self.state.borrow(), TaskState::Running)
    }
}

/// §3 rule 3, and the reason it can be a rule at all: Oro drops
/// deterministically, so "when the handle is dropped" is a moment the program
/// can point at — the same property that removed `with` from the language.
impl Drop for TaskHandle {
    fn drop(&mut self) {
        if let TaskState::Failed { report, .. } = &*self.state.borrow() {
            eprintln!("{report}");
            self.failure_flag.set(true);
        }
    }
}

/// The `Channel` value `chan` returns.
///
/// `chan()` and `chan(cap=0)` are the same thing — a rendezvous — and `chan(cap=n)` has
/// capacity `n`. There is one constructor because there is one concept with a
/// parameter (§3).
///
/// The waiter queues hold [`TaskId`]s and nothing else. A channel therefore
/// never owns a stack segment, which is what keeps it an ordinary value: the
/// scheduler owns every parked task, so a channel dropped while tasks are
/// parked on it cannot take them with it.
pub struct Channel {
    pub cap: usize,
    pub buf: RefCell<VecDeque<Value>>,
    pub closed: Cell<bool>,
    /// Tasks parked in `recv()` (or in `for x in ch`), oldest first.
    pub recv_waiters: RefCell<VecDeque<TaskId>>,
    /// Tasks parked in `send(v)`, oldest first, each with the value it is still
    /// trying to hand over. The value lives here rather than on the parked task
    /// so a receiver can take it without reaching into another task's stack.
    pub send_waiters: RefCell<VecDeque<(TaskId, Value)>>,
    /// The position counter for `for msg in ch`: a `for` yields `(index, value)`,
    /// and a channel's index is a 0-based receive counter. Bumped once per value
    /// handed to a `for` driver, never by a plain `recv()`.
    pub iter_index: Cell<i64>,
}

impl Channel {
    pub fn new(cap: usize) -> Channel {
        Channel {
            cap,
            buf: RefCell::new(VecDeque::new()),
            closed: Cell::new(false),
            recv_waiters: RefCell::new(VecDeque::new()),
            send_waiters: RefCell::new(VecDeque::new()),
            iter_index: Cell::new(0),
        }
    }

    /// How many values are sitting in the buffer. Internal only — a channel is
    /// an endpoint, not a container, so this is not reachable as `len(ch)` from
    /// Oro (that raises, like `len` on any non-container).
    pub fn len(&self) -> usize {
        self.buf.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn repr(&self) -> String {
        if self.closed.get() {
            format!("<channel cap={} closed>", self.cap)
        } else {
            format!("<channel cap={}>", self.cap)
        }
    }
}
