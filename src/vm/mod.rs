//! The Oro virtual machine: one flat interpreter loop over a stack of heap
//! [`Frame`]s (architecture point 1).
//!
//! **Calling an Oro function never recurses in Rust.** A call pushes a new
//! [`Frame`] onto `frames` and the same loop keeps turning; `Return` pops the
//! frame and hands the value back to the caller's operand stack. This is what
//! makes 5000-deep recursion (and, later, generators/coroutines) possible
//! without growing the native stack.

pub mod arith;

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::CmpOp;
use crate::compiler::{CaptureSource, CodeObject, Op, ParamInfo, VarTarget};
use crate::value::{
    Function, IterState, OroDict, OroSet, RangeVal, Value,
};

/// A runtime error carrying the source position of the faulting instruction.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// A guard against runaway recursion. Chosen far above the required 5000 so it
/// only ever trips on genuine infinite recursion, turning an eventual OOM into a
/// clean error.
const MAX_FRAMES: usize = 200_000;

/// A single activation record. Everything a running function needs lives here,
/// on the heap, in the `frames` vector — never on the Rust call stack.
struct Frame {
    code: Rc<CodeObject>,
    pc: usize,
    /// Plain local slots, sized once by the compiler's pre-pass.
    locals: Vec<Value>,
    /// This frame's own captured cells (shared with inner closures).
    cells: Vec<Rc<RefCell<Value>>>,
    /// Cells captured from enclosing frames.
    free: Vec<Rc<RefCell<Value>>>,
    /// The operand stack.
    stack: Vec<Value>,
}

/// The virtual machine.
pub struct Vm {
    frames: Vec<Frame>,
    line: u32,
    col: u32,
    /// The module frame's locals, captured when the top-level frame returns.
    /// Written exactly once (at program end); used only by tests.
    last_locals: Vec<Value>,
}

/// Add two values with the VM's numeric/sequence `+` semantics. Exposed for
/// the `sum` builtin so it need not reimplement the numeric tower.
pub fn add_values(a: &Value, b: &Value) -> Result<Value, String> {
    arith::binary(&Op::BinAdd, a, b)
}

/// Run a compiled module to completion, returning its (ignored) result.
pub fn run(code: Rc<CodeObject>) -> Result<Value, RuntimeError> {
    let mut vm = Vm { frames: Vec::new(), line: 0, col: 0, last_locals: Vec::new() };
    let frame = Frame {
        locals: vec![Value::Unbound; code.nlocals],
        cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
        free: Vec::new(),
        stack: Vec::new(),
        pc: 0,
        code,
    };
    vm.frames.push(frame);
    vm.run_loop()
}

impl Vm {
    // --- Operand-stack helpers (always the top frame) ------------------------

    fn top(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("no active frame")
    }

    fn push(&mut self, v: Value) {
        self.top().stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.top().stack.pop().expect("operand stack underflow")
    }

    fn popn(&mut self, n: usize) -> Vec<Value> {
        let stack = &mut self.top().stack;
        stack.split_off(stack.len() - n)
    }

    fn err(&self, message: impl Into<String>) -> RuntimeError {
        RuntimeError { message: message.into(), line: self.line as usize, col: self.col as usize }
    }

    fn wrap<T>(&self, r: Result<T, String>) -> Result<T, RuntimeError> {
        r.map_err(|m| self.err(m))
    }

    /// The list at the top of the stack (left in place), for the incremental
    /// call-argument assembly ops.
    fn expect_list_tos(&mut self, who: &str) -> Result<Rc<RefCell<Vec<Value>>>, RuntimeError> {
        match self.top().stack.last() {
            Some(Value::List(l)) => Ok(l.clone()),
            _ => Err(self.err(format!("internal: {who} on non-list"))),
        }
    }

    fn expect_dict_tos(&mut self, who: &str) -> Result<Rc<RefCell<OroDict>>, RuntimeError> {
        match self.top().stack.last() {
            Some(Value::Dict(d)) => Ok(d.clone()),
            _ => Err(self.err(format!("internal: {who} on non-dict"))),
        }
    }

    // --- The interpreter loop ------------------------------------------------

    fn run_loop(&mut self) -> Result<Value, RuntimeError> {
        loop {
            // Fetch. A short borrow reads the instruction and its span, then we
            // release it so call/return can restructure `frames`.
            let (op, pc) = {
                let frame = self.frames.last().expect("no active frame");
                let pc = frame.pc;
                let op = frame.code.ops[pc].clone();
                let (l, c) = frame.code.spans[pc];
                self.line = l;
                self.col = c;
                (op, pc)
            };
            self.frames.last_mut().unwrap().pc = pc + 1;

            match op {
                Op::LoadConst(i) => {
                    let v = self.frames.last().unwrap().code.consts[i].clone();
                    self.push(v);
                }
                Op::LoadNone => self.push(Value::None),
                Op::LoadFast(s) => {
                    let v = self.top().locals[s as usize].clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err(self.unbound_local_msg(s)));
                    }
                    self.push(v);
                }
                Op::StoreFast(s) => {
                    let v = self.pop();
                    self.top().locals[s as usize] = v;
                }
                Op::LoadCell(s) => {
                    let v = self.top().cells[s as usize].borrow().clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err("local variable referenced before assignment"));
                    }
                    self.push(v);
                }
                Op::StoreCell(s) => {
                    let v = self.pop();
                    *self.top().cells[s as usize].borrow_mut() = v;
                }
                Op::LoadFree(s) => {
                    let v = self.top().free[s as usize].borrow().clone();
                    if matches!(v, Value::Unbound) {
                        return Err(self.err("free variable referenced before assignment"));
                    }
                    self.push(v);
                }
                Op::StoreFree(s) => {
                    let v = self.pop();
                    *self.top().free[s as usize].borrow_mut() = v;
                }
                Op::LoadGlobal(name) => match crate::builtins::lookup(&name) {
                    Some(v) => self.push(v),
                    None => return Err(self.err(format!("name '{name}' is not defined"))),
                },
                Op::Pop => {
                    self.pop();
                }
                Op::Dup => {
                    let v = self.top().stack.last().expect("dup on empty stack").clone();
                    self.push(v);
                }
                Op::DupTwo => {
                    let n = self.top().stack.len();
                    let a = self.top().stack[n - 2].clone();
                    let b = self.top().stack[n - 1].clone();
                    self.push(a);
                    self.push(b);
                }
                Op::RotTwo => {
                    let s = &mut self.top().stack;
                    let n = s.len();
                    s.swap(n - 1, n - 2);
                }
                Op::RotThree => {
                    // [a, b, c] -> [c, a, b]
                    let s = &mut self.top().stack;
                    let n = s.len();
                    s[n - 3..].rotate_right(1);
                }
                Op::UnaryNeg => {
                    let v = self.pop();
                    let r = self.wrap(arith::neg(&v))?;
                    self.push(r);
                }
                Op::UnaryPos => {
                    let v = self.pop();
                    let r = self.wrap(arith::pos(&v))?;
                    self.push(r);
                }
                Op::UnaryNot => {
                    let v = self.pop();
                    self.push(Value::Bool(!v.truthy()));
                }
                Op::BinAdd
                | Op::BinSub
                | Op::BinMul
                | Op::BinDiv
                | Op::BinFloorDiv
                | Op::BinMod
                | Op::BinPow => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.wrap(arith::binary(&op, &a, &b))?;
                    self.push(r);
                }
                Op::Compare(cmp) => {
                    let b = self.pop();
                    let a = self.pop();
                    let r = self.wrap(compare(cmp, &a, &b))?;
                    self.push(Value::Bool(r));
                }
                Op::Jump(t) => self.top().pc = t,
                Op::PopJumpIfFalse(t) => {
                    let v = self.pop();
                    if !v.truthy() {
                        self.top().pc = t;
                    }
                }
                Op::PopJumpIfTrue(t) => {
                    let v = self.pop();
                    if v.truthy() {
                        self.top().pc = t;
                    }
                }
                Op::JumpIfFalseOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.pop();
                    } else {
                        self.top().pc = t;
                    }
                }
                Op::JumpIfTrueOrPop(t) => {
                    if self.top().stack.last().unwrap().truthy() {
                        self.top().pc = t;
                    } else {
                        self.pop();
                    }
                }
                Op::BuildList(n) => {
                    let items = self.popn(n);
                    self.push(Value::List(Rc::new(RefCell::new(items))));
                }
                Op::BuildTuple(n) => {
                    let items = self.popn(n);
                    self.push(Value::Tuple(Rc::new(items)));
                }
                Op::BuildSet(n) => {
                    let items = self.popn(n);
                    let mut set = OroSet::new();
                    for it in items {
                        self.wrap(set.insert(it))?;
                    }
                    self.push(Value::Set(Rc::new(RefCell::new(set))));
                }
                Op::BuildMap(n) => {
                    let items = self.popn(2 * n);
                    let mut dict = OroDict::new();
                    let mut it = items.into_iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        self.wrap(dict.insert(k, v))?;
                    }
                    self.push(Value::Dict(Rc::new(RefCell::new(dict))));
                }
                Op::ListAppend => {
                    let v = self.pop();
                    let list = self.expect_list_tos("ListAppend")?;
                    list.borrow_mut().push(v);
                }
                Op::ListExtend => {
                    let iterable = self.pop();
                    let items = self.wrap(iterate_to_vec(&iterable))?;
                    let list = self.expect_list_tos("ListExtend")?;
                    list.borrow_mut().extend(items);
                }
                Op::MapSetItem => {
                    let v = self.pop();
                    let k = self.pop();
                    let dict = self.expect_dict_tos("MapSetItem")?;
                    self.wrap(dict.borrow_mut().insert(k, v))?;
                }
                Op::MapMerge => {
                    let mapping = self.pop();
                    let pairs = self.wrap(dict_pairs(&mapping))?;
                    let dict = self.expect_dict_tos("MapMerge")?;
                    for (k, v) in pairs {
                        self.wrap(dict.borrow_mut().insert(k, v))?;
                    }
                }
                Op::LoadSubscript => {
                    let index = self.pop();
                    let obj = self.pop();
                    let r = self.wrap(subscript_get(&obj, &index))?;
                    self.push(r);
                }
                Op::StoreSubscript => {
                    let index = self.pop();
                    let obj = self.pop();
                    let value = self.pop();
                    self.wrap(subscript_set(&obj, &index, value))?;
                }
                Op::LoadSlice => {
                    let step = self.pop();
                    let upper = self.pop();
                    let lower = self.pop();
                    let obj = self.pop();
                    let r = self.wrap(slice_get(&obj, &lower, &upper, &step))?;
                    self.push(r);
                }
                Op::LoadAttr(name) => {
                    let obj = self.pop();
                    let r = self.wrap(load_attr(obj, &name))?;
                    self.push(r);
                }
                Op::UnpackSequence(n) => {
                    let seq = self.pop();
                    let items = self.wrap(iterate_to_vec(&seq))?;
                    if items.len() != n {
                        let msg = if items.len() < n {
                            format!("not enough values to unpack (expected {n}, got {})", items.len())
                        } else {
                            format!("too many values to unpack (expected {n})")
                        };
                        return Err(self.err(msg));
                    }
                    for v in items.into_iter().rev() {
                        self.push(v);
                    }
                }
                Op::FormatValue(conv) => {
                    let spec = self.pop();
                    let value = self.pop();
                    let spec_str = match &spec {
                        Value::Str(s) => s.s.clone(),
                        other => {
                            let msg = format!(
                                "format spec must be a string, not '{}'",
                                other.type_name()
                            );
                            return Err(self.err(msg));
                        }
                    };
                    let out = self.wrap(crate::format::format_value(&value, conv, &spec_str))?;
                    self.push(Value::str(out));
                }
                Op::BuildString(n) => {
                    let parts = self.popn(n);
                    let mut s = String::new();
                    for p in parts {
                        s.push_str(&p.display());
                    }
                    self.push(Value::str(s));
                }
                Op::MatchDispatch { table, default } => {
                    let subject = self.pop();
                    let dict = match &self.top().code.consts[table] {
                        Value::Dict(d) => d.clone(),
                        _ => unreachable!("MatchDispatch table is always a dict const"),
                    };
                    // An unhashable subject cannot equal any literal key, so it
                    // takes the default — matching the compare-chain path.
                    let target = match dict.borrow().get(&subject) {
                        Ok(Some(Value::Int(t))) => t as usize,
                        _ => default,
                    };
                    self.top().pc = target;
                }
                Op::GetIter => {
                    let v = self.pop();
                    let it = self.wrap(get_iter(&v))?;
                    self.push(it);
                }
                Op::ForIter(target) => {
                    let it = self.top().stack.last().expect("ForIter on empty stack").clone();
                    let next = self.wrap(iter_next(&it))?;
                    match next {
                        Some(v) => self.push(v),
                        None => {
                            self.pop(); // discard the exhausted iterator
                            self.top().pc = target;
                        }
                    }
                }
                Op::MakeFunction(idx) => self.make_function(idx)?,
                Op::Call(n) => self.do_call(n)?,
                Op::CallEx => self.do_call_ex()?,
                Op::Return => {
                    let value = self.pop();
                    let frame = self.frames.pop().expect("return with no frame");
                    if self.frames.is_empty() {
                        self.last_locals = frame.locals;
                        return Ok(value);
                    }
                    self.push(value);
                }
            }
        }
    }

    fn unbound_local_msg(&self, slot: u16) -> String {
        let code = &self.frames.last().unwrap().code;
        // A name that also exists at module scope but was made local by an
        // assignment (no `global`) is the classic footgun — teach the fix.
        for (s, name) in &code.shadow_hints {
            if *s == slot {
                return format!(
                    "local variable '{name}' referenced before assignment: '{name}' is assigned \
                     inside this function, which makes it local and shadows the module-level \
                     '{name}'. To read and update the module value, declare `global {name}` at \
                     the top of the function; otherwise keep the state on an object, or rename \
                     the local."
                );
            }
        }
        // Recover the variable's name from its parameter descriptor when we can,
        // for a friendlier message.
        for p in &code.params {
            if let VarTarget::Local(s) = p.target {
                if s == slot {
                    return format!("local variable '{}' referenced before assignment", p.name);
                }
            }
        }
        "local variable referenced before assignment".to_string()
    }

    // --- Closures ------------------------------------------------------------

    fn make_function(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let proto = self.frames.last().unwrap().code.protos[idx].clone();
        let defaults = self.popn(proto.n_defaults);
        let frame = self.frames.last().unwrap();
        let freevars: Vec<Rc<RefCell<Value>>> = proto
            .captures
            .iter()
            .map(|c| match c {
                CaptureSource::Cell(i) => frame.cells[*i as usize].clone(),
                CaptureSource::Free(i) => frame.free[*i as usize].clone(),
            })
            .collect();
        let func = Function { code: proto.code.clone(), defaults, freevars };
        self.push(Value::Func(Rc::new(func)));
        Ok(())
    }

    // --- Calls ---------------------------------------------------------------

    fn do_call(&mut self, n: usize) -> Result<(), RuntimeError> {
        let args = self.popn(n);
        let callee = self.pop();
        self.invoke(callee, args, Vec::new())
    }

    fn do_call_ex(&mut self) -> Result<(), RuntimeError> {
        let kwdict = self.pop();
        let poslist = self.pop();
        let callee = self.pop();
        let args = match poslist {
            Value::List(l) => l.borrow().clone(),
            _ => return Err(self.err("internal: CallEx positional list malformed")),
        };
        let kwargs = match kwdict {
            Value::Dict(d) => {
                let d = d.borrow();
                let mut out = Vec::with_capacity(d.len());
                for (k, v) in d.items() {
                    match k {
                        Value::Str(s) => out.push((s.s.clone(), v.clone())),
                        _ => return Err(self.err("keywords must be strings")),
                    }
                }
                out
            }
            _ => return Err(self.err("internal: CallEx keyword dict malformed")),
        };
        self.invoke(callee, args, kwargs)
    }

    /// Dispatch a call. Builtins and bound methods execute natively (they never
    /// re-enter Oro), so only Oro functions push a new frame — keeping the one
    /// flat loop intact.
    fn invoke(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<(), RuntimeError> {
        match callee {
            Value::Builtin(b) => {
                if !kwargs.is_empty() {
                    return Err(self.err(format!("{}() takes no keyword arguments", b.name)));
                }
                let r = self.wrap((b.func)(args))?;
                self.push(r);
                Ok(())
            }
            Value::Method(m) => {
                if !kwargs.is_empty() {
                    return Err(self.err("methods take no keyword arguments in this build"));
                }
                let r = self.wrap(crate::builtins::call_method(&m.receiver, &m.name, args))?;
                self.push(r);
                Ok(())
            }
            Value::Func(f) => {
                if self.frames.len() >= MAX_FRAMES {
                    return Err(self.err("maximum recursion depth exceeded"));
                }
                let frame = self.bind_call(&f, args, kwargs)?;
                self.frames.push(frame);
                Ok(())
            }
            other => Err(self.err(format!("'{}' object is not callable", other.type_name()))),
        }
    }

    /// Bind arguments to a fresh frame's slots and cells.
    ///
    /// Two paths, as the spec calls out: the **static** path fills positional
    /// parameters straight into their numbered slots; the **dynamic** path is
    /// taken when keyword arguments are present or the function has `*args` /
    /// `**kwargs`, where some argument names are only known at runtime and must
    /// be matched by name against the parameter list.
    fn bind_call(
        &self,
        func: &Rc<Function>,
        args: Vec<Value>,
        kwargs: Vec<(String, Value)>,
    ) -> Result<Frame, RuntimeError> {
        let code = &func.code;
        let mut frame = Frame {
            locals: vec![Value::Unbound; code.nlocals],
            cells: (0..code.ncells).map(|_| Rc::new(RefCell::new(Value::Unbound))).collect(),
            free: func.freevars.clone(),
            stack: Vec::new(),
            pc: 0,
            code: code.clone(),
        };

        let normal: Vec<&ParamInfo> =
            code.params.iter().filter(|p| p.kind == crate::ast::ParamKind::Normal).collect();
        let var_param = code.params.iter().find(|p| p.kind == crate::ast::ParamKind::VarArgs);
        let kw_param = code.params.iter().find(|p| p.kind == crate::ast::ParamKind::KwArgs);

        // Slots for normal params, filled as we go (None = still missing).
        let mut filled: Vec<Option<Value>> = vec![None; normal.len()];

        // 1. Positional arguments fill normal params left to right.
        if args.len() > normal.len() && var_param.is_none() {
            return Err(self.err(format!(
                "{}() takes {} positional argument{} but {} were given",
                code.name,
                normal.len(),
                if normal.len() == 1 { "" } else { "s" },
                args.len()
            )));
        }
        let mut extra_positional = Vec::new();
        for (i, a) in args.into_iter().enumerate() {
            if i < normal.len() {
                filled[i] = Some(a);
            } else {
                extra_positional.push(a);
            }
        }

        // 2. Keyword arguments: match by name, else collect for **kwargs.
        let mut extra_kw = OroDict::new();
        for (name, value) in kwargs {
            if let Some(pos) = normal.iter().position(|p| *p.name == name) {
                if filled[pos].is_some() {
                    return Err(self.err(format!(
                        "{}() got multiple values for argument '{name}'",
                        code.name
                    )));
                }
                filled[pos] = Some(value);
            } else if kw_param.is_some() {
                self.wrap(extra_kw.insert(Value::str(name), value))?;
            } else {
                return Err(self.err(format!(
                    "{}() got an unexpected keyword argument '{name}'",
                    code.name
                )));
            }
        }

        // 3. Defaults fill any remaining normal params; error if none.
        //    `func.defaults` aligns with the trailing defaulted params.
        let n_defaults = func.defaults.len();
        let first_defaulted = normal.len() - n_defaults;
        for (i, slot) in filled.iter_mut().enumerate() {
            if slot.is_none() {
                if i >= first_defaulted {
                    *slot = Some(func.defaults[i - first_defaulted].clone());
                } else {
                    return Err(self.err(format!(
                        "{}() missing required argument: '{}'",
                        code.name, normal[i].name
                    )));
                }
            }
        }

        // 4. Write bound values into the frame via each parameter's target.
        for (p, value) in normal.iter().zip(filled) {
            store_param(&mut frame, p.target, value.expect("all normal params filled"));
        }
        if let Some(p) = var_param {
            store_param(&mut frame, p.target, Value::Tuple(Rc::new(extra_positional)));
        }
        if let Some(p) = kw_param {
            store_param(&mut frame, p.target, Value::Dict(Rc::new(RefCell::new(extra_kw))));
        }

        Ok(frame)
    }
}

fn store_param(frame: &mut Frame, target: VarTarget, value: Value) {
    match target {
        VarTarget::Local(s) => frame.locals[s as usize] = value,
        VarTarget::Cell(s) => *frame.cells[s as usize].borrow_mut() = value,
    }
}

// --- Iteration --------------------------------------------------------------

fn get_iter(v: &Value) -> Result<Value, String> {
    let state = match v {
        Value::Range(r) => IterState::Range { cur: r.start, stop: r.stop, step: r.step },
        Value::List(l) => {
            IterState::List { list: l.clone(), idx: 0, orig_len: l.borrow().len() }
        }
        Value::Tuple(t) => IterState::Tuple { tuple: t.clone(), idx: 0 },
        Value::Str(s) => {
            let chars = s.s.chars().map(|c| c.to_string()).collect();
            IterState::Str { chars, idx: 0 }
        }
        Value::Dict(d) => IterState::Snapshot { items: d.borrow().keys(), idx: 0 },
        Value::Set(s) => IterState::Snapshot { items: s.borrow().items().to_vec(), idx: 0 },
        Value::Iter(_) => return Ok(v.clone()),
        other => return Err(format!("'{}' object is not iterable", other.type_name())),
    };
    Ok(Value::Iter(Rc::new(RefCell::new(state))))
}

fn iter_next(it: &Value) -> Result<Option<Value>, String> {
    let it = match it {
        Value::Iter(i) => i,
        _ => return Err("internal: ForIter target is not an iterator".to_string()),
    };
    let mut st = it.borrow_mut();
    match &mut *st {
        IterState::Range { cur, stop, step } => {
            let go = if *step > 0 { *cur < *stop } else { *cur > *stop };
            if go {
                let v = *cur;
                *cur += *step;
                Ok(Some(Value::Int(v)))
            } else {
                Ok(None)
            }
        }
        IterState::List { list, idx, orig_len } => {
            let cur_len = list.borrow().len();
            if cur_len != *orig_len {
                return Err("list changed size during iteration".to_string());
            }
            if *idx < cur_len {
                let v = list.borrow()[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Tuple { tuple, idx } => {
            if *idx < tuple.len() {
                let v = tuple[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Str { chars, idx } => {
            if *idx < chars.len() {
                let v = Value::str(chars[*idx].clone());
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        IterState::Snapshot { items, idx } => {
            if *idx < items.len() {
                let v = items[*idx].clone();
                *idx += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
    }
}

/// Collect every element of an iterable into a vector (for unpacking, `*args`
/// spreading, and `**` merging).
pub fn iterate_to_vec(v: &Value) -> Result<Vec<Value>, String> {
    let it = get_iter(v)?;
    let mut out = Vec::new();
    while let Some(x) = iter_next(&it)? {
        out.push(x);
    }
    Ok(out)
}

fn dict_pairs(v: &Value) -> Result<Vec<(Value, Value)>, String> {
    match v {
        Value::Dict(d) => Ok(d.borrow().items().to_vec()),
        other => Err(format!("argument after ** must be a mapping, not '{}'", other.type_name())),
    }
}

// --- Indexing and slicing ---------------------------------------------------

fn as_index(v: &Value) -> Result<i64, String> {
    match v {
        Value::Bool(b) => Ok(*b as i64),
        Value::Int(i) => Ok(*i),
        other => Err(format!("indices must be integers, not '{}'", other.type_name())),
    }
}

/// Resolve a possibly-negative index against `len`, returning the non-negative
/// position or an out-of-range error.
fn resolve_index(idx: i64, len: usize, kind: &str) -> Result<usize, String> {
    let adj = if idx < 0 { idx + len as i64 } else { idx };
    if adj < 0 || adj as usize >= len {
        Err(format!("{kind} index out of range"))
    } else {
        Ok(adj as usize)
    }
}

fn subscript_get(obj: &Value, index: &Value) -> Result<Value, String> {
    match obj {
        Value::List(l) => {
            let l = l.borrow();
            let i = resolve_index(as_index(index)?, l.len(), "list")?;
            Ok(l[i].clone())
        }
        Value::Tuple(t) => {
            let i = resolve_index(as_index(index)?, t.len(), "tuple")?;
            Ok(t[i].clone())
        }
        Value::Str(s) => {
            let n = s.char_len();
            let i = resolve_index(as_index(index)?, n, "string")?;
            Ok(Value::str(s.char_at(i).expect("index checked in range")))
        }
        Value::Dict(d) => match d.borrow().get(index)? {
            Some(v) => Ok(v),
            None => Err(format!("key error: {}", index.repr())),
        },
        other => Err(format!("'{}' object is not subscriptable", other.type_name())),
    }
}

fn subscript_set(obj: &Value, index: &Value, value: Value) -> Result<(), String> {
    match obj {
        Value::List(l) => {
            let mut l = l.borrow_mut();
            let len = l.len();
            let i = resolve_index(as_index(index)?, len, "list")?;
            l[i] = value;
            Ok(())
        }
        Value::Dict(d) => d.borrow_mut().insert(index.clone(), value),
        other => {
            Err(format!("'{}' object does not support item assignment", other.type_name()))
        }
    }
}

fn slice_get(
    obj: &Value,
    lower: &Value,
    upper: &Value,
    step: &Value,
) -> Result<Value, String> {
    let opt = |v: &Value| -> Result<Option<i64>, String> {
        match v {
            Value::None => Ok(None),
            other => Ok(Some(as_index(other)?)),
        }
    };
    let (lo, hi, st) = (opt(lower)?, opt(upper)?, opt(step)?);
    let step = st.unwrap_or(1);
    if step == 0 {
        return Err("slice step cannot be zero".to_string());
    }
    match obj {
        Value::Str(s) => {
            let chars: Vec<char> = s.s.chars().collect();
            let idxs = slice_indices(chars.len(), lo, hi, step);
            let out: String = idxs.into_iter().map(|i| chars[i]).collect();
            Ok(Value::str(out))
        }
        Value::List(l) => {
            let l = l.borrow();
            let idxs = slice_indices(l.len(), lo, hi, step);
            Ok(Value::List(Rc::new(RefCell::new(idxs.into_iter().map(|i| l[i].clone()).collect()))))
        }
        Value::Tuple(t) => {
            let idxs = slice_indices(t.len(), lo, hi, step);
            Ok(Value::Tuple(Rc::new(idxs.into_iter().map(|i| t[i].clone()).collect())))
        }
        other => Err(format!("'{}' object is not sliceable", other.type_name())),
    }
}

/// Compute the concrete indices a slice selects, applying Python's clamping and
/// negative-index rules for either direction of `step`.
fn slice_indices(len: usize, lower: Option<i64>, upper: Option<i64>, step: i64) -> Vec<usize> {
    let len = len as i64;
    let clamp = |i: i64, lo: i64, hi: i64| i.max(lo).min(hi);
    let (mut start, stop);
    if step > 0 {
        start = match lower {
            Some(l) => clamp(if l < 0 { l + len } else { l }, 0, len),
            None => 0,
        };
        stop = match upper {
            Some(u) => clamp(if u < 0 { u + len } else { u }, 0, len),
            None => len,
        };
        let mut out = Vec::new();
        while start < stop {
            out.push(start as usize);
            start += step;
        }
        out
    } else {
        start = match lower {
            Some(l) => clamp(if l < 0 { l + len } else { l }, -1, len - 1),
            None => len - 1,
        };
        stop = match upper {
            Some(u) => clamp(if u < 0 { u + len } else { u }, -1, len - 1),
            None => -1,
        };
        let mut out = Vec::new();
        while start > stop {
            out.push(start as usize);
            start += step;
        }
        out
    }
}

// --- Attributes -------------------------------------------------------------

fn load_attr(obj: Value, name: &str) -> Result<Value, String> {
    if crate::builtins::method_exists(&obj, name) {
        Ok(Value::Method(Rc::new(crate::value::BoundMethod {
            receiver: obj,
            name: Rc::from(name),
        })))
    } else {
        Err(format!("'{}' object has no attribute '{}'", obj.type_name(), name))
    }
}

// --- Comparison -------------------------------------------------------------

fn compare(op: CmpOp, a: &Value, b: &Value) -> Result<bool, String> {
    use std::cmp::Ordering;
    Ok(match op {
        CmpOp::Eq => a.equals(b),
        CmpOp::NotEq => !a.equals(b),
        CmpOp::Lt => a.compare(b)? == Ordering::Less,
        CmpOp::Gt => a.compare(b)? == Ordering::Greater,
        CmpOp::LtEq => a.compare(b)? != Ordering::Greater,
        CmpOp::GtEq => a.compare(b)? != Ordering::Less,
        CmpOp::Is => value_is(a, b),
        CmpOp::IsNot => !value_is(a, b),
        CmpOp::In => contains(b, a)?,
        CmpOp::NotIn => !contains(b, a)?,
    })
}

/// Identity comparison. For the immutable scalars Oro shares by value this is
/// value equality; for heap objects it is `Rc` pointer identity.
fn value_is(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::None, Value::None) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => Rc::ptr_eq(x, y),
        (Value::List(x), Value::List(y)) => Rc::ptr_eq(x, y),
        (Value::Tuple(x), Value::Tuple(y)) => Rc::ptr_eq(x, y),
        (Value::Dict(x), Value::Dict(y)) => Rc::ptr_eq(x, y),
        (Value::Set(x), Value::Set(y)) => Rc::ptr_eq(x, y),
        (Value::Func(x), Value::Func(y)) => Rc::ptr_eq(x, y),
        _ => false,
    }
}

fn contains(container: &Value, item: &Value) -> Result<bool, String> {
    match container {
        Value::Str(hay) => match item {
            Value::Str(needle) => Ok(hay.s.contains(&needle.s)),
            _ => Err("'in <string>' requires string as left operand".to_string()),
        },
        Value::List(l) => Ok(l.borrow().iter().any(|v| v.equals(item))),
        Value::Tuple(t) => Ok(t.iter().any(|v| v.equals(item))),
        Value::Set(s) => s.borrow().contains(item),
        Value::Dict(d) => d.borrow().contains(item),
        Value::Range(r) => Ok(range_contains(r, item)),
        other => Err(format!("argument of type '{}' is not iterable", other.type_name())),
    }
}

fn range_contains(r: &RangeVal, item: &Value) -> bool {
    let n = match item {
        Value::Int(i) => *i,
        Value::Bool(b) => *b as i64,
        _ => return false,
    };
    if r.step > 0 {
        n >= r.start && n < r.stop && (n - r.start) % r.step == 0
    } else {
        n <= r.start && n > r.stop && (r.start - n) % (-r.step) == 0
    }
}

#[cfg(test)]
mod tests;
