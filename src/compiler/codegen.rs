//! Codegen: the second pass. Walks the AST alongside the finalized symbol table
//! (see [`super::symbols`]) and emits [`Op`]s, resolving every name to the slot,
//! cell, or free index the pre-pass assigned.
//!
//! The scope walk here is in lockstep with [`super::symbols::SymTable::resolve_module`]:
//! each scope's child scopes are consumed in source order via a per-scope
//! cursor, so a `def`/`if`/`for` here maps to exactly the scope the pre-pass
//! built for it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::{
    Arg, BinOp, BoolOp, ExceptHandler, Expr, Kwarg, MatchCase, Param, ParamKind, Pattern, Stmt,
    UnaryOp,
};
use crate::bigint::BigInt;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::value::{OroDict, Value};

use super::symbols::{Resolution, SymTable};
use super::{
    CaptureSource, ClassSpec, CodeObject, CompileError, FuncProto, Op, ParamInfo, VarTarget,
};

type CResult<T> = Result<T, CompileError>;

/// Marks that codegen is inside a loop, so `break`/`continue` are legal. The
/// jump targets live in the runtime loop block, not here.
struct LoopCtx;

struct Codegen<'a> {
    table: &'a SymTable,
    /// The file every code object this pass emits is stamped with. Cloned into
    /// each nested `Codegen`, so a lambda five scopes deep names the same file
    /// as the module body around it.
    source: Rc<str>,
    scope: usize,
    func: usize,
    ops: Vec<Op>,
    spans: Vec<(u32, u32)>,
    consts: Vec<Value>,
    /// Interned names for the name-carrying opcodes, plus the reverse map that
    /// keeps `self.x` used in ten places from being stored ten times.
    names: Vec<Rc<str>>,
    name_idx: HashMap<Rc<str>, u32>,
    classes: Vec<Rc<ClassSpec>>,
    pairs: Vec<(u32, u32)>,
    protos: Vec<Rc<FuncProto>>,
    /// Cursor into the current scope's child list.
    cursor: usize,
    loops: Vec<LoopCtx>,
    /// One entry per `finally` body being compiled in this function, holding
    /// `loops.len()` where it starts. See [`Codegen::refuse_jump_out_of_finally`].
    finally_loops: Vec<usize>,
    /// True while compiling a function body that contains `yield`.
    is_generator: bool,
}

/// Compile the module body into its top-level code object.
pub fn compile_module(
    table: &SymTable,
    body: &[Stmt],
    source: Rc<str>,
) -> CResult<Rc<CodeObject>> {
    let module = table.module;
    let mut cg = Codegen::new(table, module, source);
    cg.emit_body(body)?;
    cg.emit(Op::LoadNone, 1, 1);
    cg.emit(Op::Return, 1, 1);
    Ok(Rc::new(cg.finish("<module>".to_string(), Vec::new())))
}

impl<'a> Codegen<'a> {
    fn new(table: &'a SymTable, scope: usize, source: Rc<str>) -> Codegen<'a> {
        let func = scope; // callers pass a function/module scope
        Codegen {
            table,
            source,
            scope,
            func,
            ops: Vec::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            names: Vec::new(),
            name_idx: HashMap::new(),
            classes: Vec::new(),
            pairs: Vec::new(),
            protos: Vec::new(),
            cursor: 0,
            loops: Vec::new(),
            finally_loops: Vec::new(),
            is_generator: false,
        }
    }

    fn finish(self, name: String, params: Vec<ParamInfo>) -> CodeObject {
        CodeObject {
            name,
            source: self.source,
            ops: self.ops,
            spans: self.spans,
            consts: self.consts,
            builtin_cache: RefCell::new(vec![None; self.names.len()]),
            names: self.names,
            classes: self.classes,
            pairs: self.pairs,
            protos: self.protos,
            nlocals: self.table.scopes()[self.func].nlocals as usize,
            ncells: self.table.ncells(self.func) as usize,
            nfree: self.table.nfree(self.func) as usize,
            simple_params: params.iter().all(|p| p.kind == ParamKind::Normal),
            params,
            is_generator: self.is_generator,
            module_names: if self.func == self.table.module() {
                self.table.module_member_targets()
            } else {
                Vec::new()
            },
            shadow_hints: self.table.shadow_hints(self.func),
        }
    }

    // --- Emission helpers ----------------------------------------------------

    fn emit(&mut self, op: Op, line: usize, col: usize) -> usize {
        let idx = self.ops.len();
        self.ops.push(op);
        self.spans.push((line as u32, col as u32));
        idx
    }

    fn here(&self) -> u32 {
        self.ops.len() as u32
    }

    fn add_const(&mut self, v: Value) -> u32 {
        let idx = self.consts.len() as u32;
        self.consts.push(v);
        idx
    }

    /// Intern a name into the code object's name table, returning its index.
    /// Names carried by opcodes live here rather than inline so that `Op` stays
    /// one 8-byte `Copy` word; interning also means a name used at many call
    /// sites is stored once.
    fn add_name(&mut self, name: &str) -> u32 {
        if let Some(&i) = self.name_idx.get(name) {
            return i;
        }
        let rc: Rc<str> = Rc::from(name);
        let idx = self.names.len() as u32;
        self.names.push(rc.clone());
        self.name_idx.insert(rc, idx);
        idx
    }

    fn add_class(&mut self, spec: ClassSpec) -> u32 {
        let idx = self.classes.len() as u32;
        self.classes.push(Rc::new(spec));
        idx
    }

    /// Reserve a slot in the two-operand side table (see [`Op::MatchDispatch`]
    /// and [`Op::SetupLoop`]); the operands are patched in once known.
    fn add_pair(&mut self) -> u32 {
        let idx = self.pairs.len() as u32;
        self.pairs.push((0, 0));
        idx
    }

    fn set_target(&mut self, idx: usize, target: u32) {
        match &mut self.ops[idx] {
            Op::Jump(t)
            | Op::PopJumpIfFalse(t)
            | Op::PopJumpIfTrue(t)
            | Op::JumpIfFalseOrPop(t)
            | Op::JumpIfTrueOrPop(t)
            | Op::ForIter(t)
            | Op::SetupExcept(t)
            | Op::SetupFinally(t) => *t = target,
            _ => unreachable!("set_target on a non-jump op"),
        }
    }

    fn next_child(&mut self) -> usize {
        let child = self.table.children_of(self.scope)[self.cursor];
        self.cursor += 1;
        child
    }

    fn err(&self, msg: impl Into<String>, line: usize, col: usize) -> CompileError {
        CompileError { message: msg.into(), line, col }
    }

    // --- Statements ----------------------------------------------------------

    fn emit_body(&mut self, stmts: &[Stmt]) -> CResult<()> {
        for s in stmts {
            self.emit_stmt(s)?;
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt) -> CResult<()> {
        match stmt {
            Stmt::Expr { value, line, col } => {
                self.emit_expr(value)?;
                self.emit(Op::Pop, *line, *col);
            }
            Stmt::Assign { targets, value, .. } => {
                self.emit_expr(value)?;
                for (i, t) in targets.iter().enumerate() {
                    if i + 1 < targets.len() {
                        let (l, c) = t.pos();
                        self.emit(Op::Dup, l, c);
                    }
                    self.emit_store(t)?;
                }
            }
            Stmt::AugAssign { target, op, value, line, col } => {
                self.emit_aug_assign(target, *op, value, *line, *col)?;
            }
            Stmt::If { cond, body, elifs, orelse, .. } => {
                self.emit_if(cond, body, elifs, orelse)?;
            }
            Stmt::While { cond, body, .. } => self.emit_while(cond, body)?,
            Stmt::For { target, iter, body, .. } => self.emit_for(target, iter, body)?,
            Stmt::Match { subject, cases, line, col } => {
                self.emit_match(subject, cases, *line, *col)?
            }
            Stmt::Def { name, params, body, line, col, .. } => {
                self.emit_def(name, params, body, *line, *col)?;
            }
            Stmt::Return { value, line, col } => {
                self.refuse_jump_out_of_finally("return", false, *line, *col)?;
                match value {
                    Some(v) => self.emit_expr(v)?,
                    None => {
                        self.emit(Op::LoadNone, *line, *col);
                    }
                }
                self.emit(Op::Return, *line, *col);
            }
            Stmt::Break { line, col } => self.emit_break(*line, *col)?,
            Stmt::Continue { line, col } => self.emit_continue(*line, *col)?,
            Stmt::Pass { .. } => {}
            // A declaration only — its effect was recorded by the symbol pass,
            // so name references now resolve to module scope. No code to emit.
            Stmt::Global { .. } => {}
            Stmt::Class { name, base, body, line, col } => {
                self.emit_class(name, base, body, *line, *col)?;
            }
            Stmt::Try { body, handlers, finalbody, line, col } => {
                self.emit_try(body, handlers, finalbody, *line, *col)?;
            }
            Stmt::Raise { exc, line, col } => match exc {
                Some(e) => {
                    self.emit_expr(e)?;
                    self.emit(Op::Raise, *line, *col);
                }
                None => {
                    self.emit(Op::Reraise, *line, *col);
                }
            },
            Stmt::Import { path, alias, line, col } => {
                let bound = super::symbols::import_bound_name(path, alias)
                    .ok_or_else(|| self.err("empty import path", *line, *col))?
                    .to_string();
                let dotted = path.join(".");
                let n = self.add_name(&dotted);
                self.emit(Op::ImportModule(n), *line, *col);
                self.emit_store(&Expr::Name { name: bound, line: *line, col: *col })?;
            }
            Stmt::Yield { value, line, col } => {
                match value {
                    Some(v) => self.emit_expr(v)?,
                    None => {
                        self.emit(Op::LoadNone, *line, *col);
                    }
                }
                self.emit(Op::Yield, *line, *col);
            }
        }
        Ok(())
    }

    fn emit_if(
        &mut self,
        cond: &Expr,
        body: &[Stmt],
        elifs: &[(Expr, Vec<Stmt>)],
        orelse: &Option<Vec<Stmt>>,
    ) -> CResult<()> {
        // Jumps that should land after the whole construct.
        let mut end_jumps: Vec<usize> = Vec::new();

        // Leading `if`.
        let (cl, cc) = cond.pos();
        self.emit_expr(cond)?;
        let mut skip = self.emit(Op::PopJumpIfFalse(0), cl, cc);
        self.emit_child_block(body)?;
        end_jumps.push(self.emit(Op::Jump(0), cl, cc));

        // `elif` chain.
        for (econd, ebody) in elifs {
            let here = self.here();
            self.set_target(skip, here);
            let (el, ec) = econd.pos();
            self.emit_expr(econd)?;
            skip = self.emit(Op::PopJumpIfFalse(0), el, ec);
            self.emit_child_block(ebody)?;
            end_jumps.push(self.emit(Op::Jump(0), el, ec));
        }

        // `else`.
        let else_here = self.here();
        self.set_target(skip, else_here);
        if let Some(ebody) = orelse {
            self.emit_child_block(ebody)?;
        }

        let end = self.here();
        for j in end_jumps {
            self.set_target(j, end);
        }
        Ok(())
    }

    /// `match`. When every pattern is a literal (with at most a trailing `_`
    /// default) we compile an O(1) [`Op::MatchDispatch`] over a dict of
    /// constants; anything with a dotted name falls back to a first-match-wins
    /// compare chain. Either way there is no fall-through and the subject is
    /// left off the stack when control leaves.
    fn emit_match(
        &mut self,
        subject: &Expr,
        cases: &[MatchCase],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        if self.match_is_table_eligible(cases) {
            self.emit_match_table(subject, cases, line, col)
        } else {
            self.emit_match_chain(subject, cases)
        }
    }

    /// Table dispatch is valid only when no pattern needs a runtime lookup
    /// (dotted names) and the wildcard, if present, is the final case — so the
    /// table can never jump past an earlier-winning `_`.
    fn match_is_table_eligible(&self, cases: &[MatchCase]) -> bool {
        for (i, case) in cases.iter().enumerate() {
            match &case.pattern {
                Pattern::Literal(_) => {}
                Pattern::Wildcard => {
                    if i != cases.len() - 1 {
                        return false;
                    }
                }
                Pattern::Dotted(_) => return false,
            }
        }
        true
    }

    fn emit_match_table(
        &mut self,
        subject: &Expr,
        cases: &[MatchCase],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        self.emit_expr(subject)?;
        // Placeholder; patched once body offsets and the table are known.
        let pair = self.add_pair();
        let dispatch = self.emit(Op::MatchDispatch(pair), line, col);

        let mut end_jumps = Vec::new();
        let mut table = OroDict::new();
        let mut default_target: Option<u32> = None;

        for case in cases {
            let start = self.here();
            match &case.pattern {
                Pattern::Literal(expr) => {
                    let key = self.literal_value(expr)?;
                    // First case wins when two literals are equal keys
                    // (e.g. 1 and True, or a repeated value).
                    if self.dict_missing(&table, &key)? {
                        table
                            // A compile-time table build: the fault is a bad
                            // key, and a compile error carries prose, not a
                            // class. Take the message and leave the class.
                            .insert(key, Value::Int(start as i64))
                            .map_err(|e| self.err(e.message, line, col))?;
                    }
                }
                Pattern::Wildcard => default_target = Some(start),
                Pattern::Dotted(_) => unreachable!("table path excludes dotted patterns"),
            }
            self.emit_child_block(&case.body)?;
            end_jumps.push(self.emit(Op::Jump(0), line, col));
        }

        let end = self.here();
        let default = default_target.unwrap_or(end);
        let table_idx = self.add_const(Value::Dict(Rc::new(RefCell::new(table))));
        self.pairs[pair as usize] = (table_idx, default);
        debug_assert!(matches!(self.ops[dispatch], Op::MatchDispatch(_)));
        for j in end_jumps {
            self.set_target(j, end);
        }
        Ok(())
    }

    fn emit_match_chain(&mut self, subject: &Expr, cases: &[MatchCase]) -> CResult<()> {
        // The subject stays on the stack across the tests; each path pops it
        // exactly once before running a body (or on final no-match).
        self.emit_expr(subject)?;
        let mut end_jumps = Vec::new();
        let mut skip: Option<usize> = None; // pending false-jump to the next test
        let mut irrefutable = false;

        for case in cases {
            if irrefutable {
                // Unreachable after a `_`, but still emit the body so its child
                // scope is consumed in lockstep with the symbol pass.
                self.emit_child_block(&case.body)?;
                continue;
            }
            if let Some(s) = skip.take() {
                let here = self.here();
                self.set_target(s, here);
            }
            let (l, c) = (case.line, case.col);
            match &case.pattern {
                Pattern::Wildcard => {
                    self.emit(Op::Pop, l, c); // drop the subject
                    self.emit_child_block(&case.body)?;
                    end_jumps.push(self.emit(Op::Jump(0), l, c));
                    irrefutable = true;
                }
                Pattern::Literal(expr) | Pattern::Dotted(expr) => {
                    self.emit(Op::Dup, l, c);
                    self.emit_expr(expr)?;
                    self.emit(Op::Compare(crate::ast::CmpOp::Eq), l, c);
                    skip = Some(self.emit(Op::PopJumpIfFalse(0), l, c));
                    self.emit(Op::Pop, l, c); // matched: drop the subject
                    self.emit_child_block(&case.body)?;
                    end_jumps.push(self.emit(Op::Jump(0), l, c));
                }
            }
        }

        if let Some(s) = skip.take() {
            let here = self.here();
            self.set_target(s, here);
        }
        if !irrefutable {
            // No case matched and there was no `_`: discard the subject.
            let (l, c) = subject.pos();
            self.emit(Op::Pop, l, c);
        }
        let end = self.here();
        for j in end_jumps {
            self.set_target(j, end);
        }
        Ok(())
    }

    /// Fold a literal-pattern expression to its constant [`Value`] for a jump
    /// table key. The parser guarantees `expr` is one of the literal forms.
    fn literal_value(&self, expr: &Expr) -> CResult<Value> {
        match expr {
            Expr::Int { value, .. } => Ok(parse_int(value)),
            Expr::Float { value, line, col } => parse_float(value)
                .map(Value::Float)
                .ok_or_else(|| self.err(format!("invalid float literal `{value}`"), *line, *col)),
            Expr::Str { value, .. } => Ok(Value::str(value.clone())),
            Expr::Bytes { value, .. } => Ok(Value::bytes(value.clone())),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::NoneLit { .. } => Ok(Value::None),
            Expr::Unary { op: UnaryOp::Neg, operand, line, col } => {
                let v = self.literal_value(operand)?;
                crate::vm::arith::neg(&v).map_err(|e| self.err(e.message, *line, *col))
            }
            other => {
                let (l, c) = other.pos();
                Err(self.err("unsupported literal in a case pattern", l, c))
            }
        }
    }

    /// True when `key` is not already present in the compile-time table.
    fn dict_missing(&self, table: &OroDict, key: &Value) -> CResult<bool> {
        table
            .get(key)
            .map(|hit| hit.is_none())
            .map_err(|e| self.err(e.message, 0, 0))
    }

    /// `try` / `except` / `finally`. The try body, each handler body, and the
    /// finally body are block scopes consumed in that order (in lockstep with
    /// the symbol pass). See the VM's block/unwind machinery for the runtime
    /// side.
    fn emit_try(
        &mut self,
        body: &[Stmt],
        handlers: &[ExceptHandler],
        finalbody: &Option<Vec<Stmt>>,
        line: usize,
        col: usize,
    ) -> CResult<()> {
        let has_finally = finalbody.is_some();
        let has_except = !handlers.is_empty();

        let setup_finally = if has_finally {
            Some(self.emit(Op::SetupFinally(0), line, col))
        } else {
            None
        };
        let setup_except = if has_except {
            Some(self.emit(Op::SetupExcept(0), line, col))
        } else {
            None
        };

        // try body.
        self.emit_child_block(body)?;

        // Normal completion of the body.
        let mut to_after: Vec<usize> = Vec::new();
        if has_except {
            self.emit(Op::PopBlock, line, col);
            to_after.push(self.emit(Op::Jump(0), line, col));
        }

        // Exception dispatch.
        if let Some(s) = setup_except {
            let here = self.here();
            self.set_target(s, here);
            for h in handlers {
                self.emit(Op::LoadHandling, h.line, h.col);
                self.emit_expr(&h.exc_type)?;
                self.emit(Op::ExcMatch, h.line, h.col);
                let skip = self.emit(Op::PopJumpIfFalse(0), h.line, h.col);
                // Matched: bind `as e` (if any) and run the body in its block
                // scope, then finish handling and jump past the dispatch.
                self.emit_handler_body(h)?;
                self.emit(Op::EndHandler, h.line, h.col);
                to_after.push(self.emit(Op::Jump(0), h.line, h.col));
                let next = self.here();
                self.set_target(skip, next);
            }
            // No clause matched: re-raise (the finally block, if any, still
            // catches it on the way out).
            self.emit(Op::Reraise, line, col);
        }

        // Normal path lands here (body ok, or a handler ran).
        let after = self.here();
        for j in to_after {
            self.set_target(j, after);
        }

        if let Some(sf) = setup_finally {
            // Remove the finally block (we run it inline now) and mark the
            // normal reason, then fall into the finally body. The unwinder and
            // `return` path jump straight to the body with their own reason.
            self.emit(Op::PopBlock, line, col);
            self.emit(Op::BeginFinally, line, col);
            let finally_body = self.here();
            self.set_target(sf, finally_body);
            self.finally_loops.push(self.loops.len());
            self.emit_child_block(finalbody.as_ref().unwrap())?;
            self.finally_loops.pop();
            self.emit(Op::EndFinally, line, col);
        }
        Ok(())
    }

    /// Emit a matched `except` handler: bind `as e` (in the handler's block
    /// scope) and run its body there.
    fn emit_handler_body(&mut self, h: &ExceptHandler) -> CResult<()> {
        let child = self.next_child();
        let saved_scope = self.scope;
        let saved_cursor = self.cursor;
        self.scope = child;
        self.cursor = 0;
        if let Some(name) = &h.name {
            self.emit(Op::LoadHandling, h.line, h.col);
            self.emit_store(&Expr::Name { name: name.clone(), line: h.line, col: h.col })?;
        }
        self.emit_body(&h.body)?;
        self.scope = saved_scope;
        self.cursor = saved_cursor;
        Ok(())
    }

    fn emit_while(&mut self, cond: &Expr, body: &[Stmt]) -> CResult<()> {
        let (cl, cc) = cond.pos();
        // The loop block is pushed once, before the condition; break/continue
        // unwind to it (running any enclosing finally).
        let setup = self.add_pair();
        self.emit(Op::SetupLoop(setup), cl, cc);
        let top = self.here();
        self.patch_loop_cont(setup, top);
        self.emit_expr(cond)?;
        let exit = self.emit(Op::PopJumpIfFalse(0), cl, cc);
        self.loops.push(LoopCtx);
        self.emit_child_block(body)?;
        self.emit(Op::Jump(top), cl, cc);
        // Normal exit: drop the loop block, then land after it.
        let exit_here = self.here();
        self.set_target(exit, exit_here);
        self.emit(Op::PopBlock, cl, cc);
        let after = self.here();
        self.patch_loop_brk(setup, after);
        self.loops.pop();
        Ok(())
    }

    fn emit_for(&mut self, target: &Expr, iter: &Expr, body: &[Stmt]) -> CResult<()> {
        let (il, ic) = iter.pos();
        // SetupLoop before the iterator so the loop block's saved stack depth is
        // *below* the iterator — a `break` then removes it on the way out.
        let setup = self.add_pair();
        self.emit(Op::SetupLoop(setup), il, ic);
        self.emit_expr(iter)?;
        self.emit(Op::GetIter, il, ic);
        let top = self.here();
        self.patch_loop_cont(setup, top);
        let foriter = self.emit(Op::ForIter(0), il, ic);
        // The loop target and body live in the child block scope.
        let child = self.next_child();
        let saved_scope = self.scope;
        let saved_cursor = self.cursor;
        self.scope = child;
        self.cursor = 0;
        self.emit_store(target)?;
        self.loops.push(LoopCtx);
        self.emit_body(body)?;
        self.emit(Op::Jump(top), il, ic);
        // Normal exhaustion: ForIter pops the iterator and lands here; drop the
        // loop block, then continue after the loop.
        let exit_here = self.here();
        self.set_target(foriter, exit_here);
        self.emit(Op::PopBlock, il, ic);
        let after = self.here();
        self.patch_loop_brk(setup, after);
        self.loops.pop();
        self.scope = saved_scope;
        self.cursor = saved_cursor;
        Ok(())
    }

    /// Patch a `SetupLoop`'s continue point. `setup` is its slot in the
    /// two-operand side table, not an instruction index.
    fn patch_loop_cont(&mut self, setup: u32, target: u32) {
        self.pairs[setup as usize].1 = target;
    }

    /// Patch a `SetupLoop`'s break (after-loop) target.
    fn patch_loop_brk(&mut self, setup: u32, target: u32) {
        self.pairs[setup as usize].0 = target;
    }

    fn emit_break(&mut self, line: usize, col: usize) -> CResult<()> {
        if self.loops.is_empty() {
            return Err(self.err("`break` outside of a loop", line, col));
        }
        self.refuse_jump_out_of_finally("break", true, line, col)?;
        // The VM unwinds to the innermost loop block, running any enclosing
        // finally first, then restores the stack and jumps past the loop.
        self.emit(Op::Break, line, col);
        Ok(())
    }

    fn emit_continue(&mut self, line: usize, col: usize) -> CResult<()> {
        if self.loops.is_empty() {
            return Err(self.err("`continue` outside of a loop", line, col));
        }
        self.refuse_jump_out_of_finally("continue", true, line, col)?;
        self.emit(Op::Continue, line, col);
        Ok(())
    }

    /// Refuse a `return`, `break` or `continue` that would leave a `finally`
    /// body. Leaving one early discards the exception it is running for, so
    /// `try: raise E() / finally: return` would make the `raise` do nothing —
    /// the one construct in the language that could. CPython 3.14 warns on it
    /// (PEP 765); nothing in the tree depends on it, so Oro refuses it.
    ///
    /// A loop that starts inside the `finally` keeps its own `break` and
    /// `continue`, and a function defined there compiles in a `Codegen` of its
    /// own, so neither is caught here.
    fn refuse_jump_out_of_finally(
        &self,
        word: &str,
        loop_jump: bool,
        line: usize,
        col: usize,
    ) -> CResult<()> {
        let leaves =
            self.finally_loops.last().is_some_and(|&d| !loop_jump || self.loops.len() == d);
        if leaves {
            return Err(self.err(
                format!(
                    "`{word}` inside `finally` would discard an exception in flight — \
                     move it after the `try` statement"
                ),
                line,
                col,
            ));
        }
        Ok(())
    }

    /// Emit an `if`/`while` body that occupies a fresh child block scope.
    fn emit_child_block(&mut self, body: &[Stmt]) -> CResult<()> {
        let child = self.next_child();
        let saved_scope = self.scope;
        let saved_cursor = self.cursor;
        self.scope = child;
        self.cursor = 0;
        self.emit_body(body)?;
        self.scope = saved_scope;
        self.cursor = saved_cursor;
        Ok(())
    }

    fn emit_aug_assign(
        &mut self,
        target: &Expr,
        op: crate::ast::AugOp,
        value: &Expr,
        line: usize,
        col: usize,
    ) -> CResult<()> {
        use crate::ast::AugOp;
        let binop = match op {
            AugOp::Add => Op::BinAdd,
            AugOp::Sub => Op::BinSub,
            AugOp::Mul => Op::BinMul,
            AugOp::Div => Op::BinDiv,
        };
        match target {
            Expr::Name { .. } => {
                self.emit_expr(target)?; // load current
                self.emit_expr(value)?;
                self.emit(binop, line, col);
                self.emit_store(target)?;
            }
            Expr::Subscript { value: obj, index, .. } => {
                // Evaluate obj and index exactly once, then reuse both for the
                // load and the store.
                self.emit_expr(obj)?; // [obj]
                self.emit_expr(index)?; // [obj, idx]
                self.emit(Op::DupTwo, line, col); // [obj, idx, obj, idx]
                self.emit(Op::LoadSubscript, line, col); // [obj, idx, cur]
                self.emit_expr(value)?; // [obj, idx, cur, value]
                self.emit(binop, line, col); // [obj, idx, newval]
                self.emit(Op::RotThree, line, col); // [newval, obj, idx]
                self.emit(Op::StoreSubscript, line, col);
            }
            _ => {
                return Err(self.err(
                    "invalid target for augmented assignment",
                    line,
                    col,
                ))
            }
        }
        Ok(())
    }

    // --- Function definitions ------------------------------------------------

    fn emit_def(
        &mut self,
        name: &str,
        params: &[Param],
        body: &[Stmt],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        self.emit_make_function(name, params, body, line, col)?;
        self.emit_store(&Expr::Name { name: name.to_string(), line, col })?;
        Ok(())
    }

    /// Compile a lambda: a function whose whole body is `return <expr>`. Its
    /// scope was assigned by the resolve pass and is read from the node, rather
    /// than taken from the `def`/block child cursor.
    fn emit_lambda(&mut self, data: &crate::ast::LambdaData, line: usize, col: usize) -> CResult<()> {
        let child = data.scope.get();
        if child == usize::MAX {
            // f-string fields are parsed here at codegen time rather than by the
            // parser, so the symbol pass never walks them and never assigns a
            // scope to a lambda inside one. Rejecting is loud and correct;
            // the real fix is to parse f-string fields into the AST so there is
            // one tree for every pass to see.
            return Err(self.err(
                "a lambda cannot appear inside an f-string field — assign it to a name first,                  e.g. `doubled = xs.map(x => x * 2)` then `f\"{doubled}\"`",
                line,
                col,
            ));
        }
        let body = vec![Stmt::Return { value: Some((*data.body).clone()), line, col }];
        let proto = self.compile_function("<lambda>", &data.params, &body, child)?;
        let proto_idx = self.protos.len() as u32;
        self.protos.push(Rc::new(proto));
        self.emit(Op::MakeFunction(proto_idx), line, col);
        Ok(())
    }

    /// Compile a `def`/method body into a function prototype and emit
    /// `MakeFunction`, leaving the resulting function on the stack. Consumes one
    /// child scope (the pre-pass created one per `def`, in source order).
    fn emit_make_function(
        &mut self,
        name: &str,
        params: &[Param],
        body: &[Stmt],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        let child = self.next_child();
        let proto = self.compile_function(name, params, body, child)?;
        let proto_idx = self.protos.len() as u32;
        self.protos.push(Rc::new(proto));
        // Evaluate default values in this (enclosing) scope, in order.
        for p in params {
            if let Some(d) = &p.default {
                self.emit_expr(d)?;
            }
        }
        self.emit(Op::MakeFunction(proto_idx), line, col);
        Ok(())
    }

    /// Compile a class: push its base (if any), then each member value, then a
    /// `BuildClass`, and bind the result to the class name.
    fn emit_class(
        &mut self,
        name: &str,
        base: &Option<Expr>,
        body: &[Stmt],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        let has_base = base.is_some();
        if let Some(b) = base {
            self.emit_expr(b)?;
        }

        let mut members: Vec<Rc<str>> = Vec::new();
        for member in body {
            match member {
                Stmt::Def { name: mname, params, body: mbody, line: ml, col: mc, .. } => {
                    if let Some(why) = unsupported_dunder(mname) {
                        return Err(self.err(why, *ml, *mc));
                    }
                    self.emit_make_function(mname, params, mbody, *ml, *mc)?;
                    members.push(Rc::from(mname.as_str()));
                }
                Stmt::Assign { targets, value, line: al, col: ac } => {
                    // Class-level attributes: each target must be a bare name.
                    let mut names = Vec::new();
                    for t in targets {
                        match t {
                            Expr::Name { name, .. } if name == "__slots__" => {
                                return Err(self.err(
                                    "__slots__ is not supported in Oro — instance attributes are \
                                     always stored in a per-instance dict",
                                    *al,
                                    *ac,
                                ))
                            }
                            Expr::Name { name, .. } => names.push(name.clone()),
                            _ => {
                                return Err(self.err(
                                    "a class-body assignment target must be a plain name",
                                    *al,
                                    *ac,
                                ))
                            }
                        }
                    }
                    self.emit_expr(value)?;
                    // One stack copy per target name (all share the value).
                    for _ in 1..names.len() {
                        self.emit(Op::Dup, *al, *ac);
                    }
                    for n in names {
                        members.push(Rc::from(n.as_str()));
                    }
                }
                // Docstrings and `pass` are allowed and produce no member.
                Stmt::Pass { .. } => {}
                Stmt::Expr { value: Expr::Str { .. }, .. } => {}
                other => {
                    let (l, c) = other.pos();
                    return Err(self.err(
                        "a class body may contain only methods, attribute assignments, and \
                         docstrings in this build",
                        l,
                        c,
                    ));
                }
            }
        }

        let spec = ClassSpec { name: Rc::from(name), members, has_base };
        let idx = self.add_class(spec);
        self.emit(Op::BuildClass(idx), line, col);
        self.emit_store(&Expr::Name { name: name.to_string(), line, col })?;
        Ok(())
    }

    fn compile_function(
        &mut self,
        name: &str,
        params: &[Param],
        body: &[Stmt],
        child: usize,
    ) -> CResult<FuncProto> {
        // Build the child function's code object with its own Codegen.
        let mut inner = Codegen::new(self.table, child, self.source.clone());
        inner.func = child;
        inner.scope = child;
        // A `yield` anywhere in the body (but not in nested defs) makes this a
        // generator function.
        inner.is_generator = contains_yield(body);
        inner.emit_body(body)?;
        let (ll, cc) = body.last().map(|s| s.pos()).unwrap_or((0, 0));
        inner.emit(Op::LoadNone, ll, cc);
        inner.emit(Op::Return, ll, cc);

        // Parameter descriptors, resolved against the child scope.
        let mut infos = Vec::with_capacity(params.len());
        for p in params {
            let target = match self.table.resolve_name(child, &p.name) {
                Some(Resolution::Local(s)) => VarTarget::Local(s),
                Some(Resolution::Cell(s)) => VarTarget::Cell(s),
                // A parameter is declared in its own function's scope, so it
                // owns its storage there and can never be free or global.
                _ => unreachable!("a parameter is always a local of its function"),
            };
            infos.push(ParamInfo {
                name: Rc::from(p.name.as_str()),
                kind: p.kind,
                target,
                has_default: p.default.is_some() && p.kind == ParamKind::Normal,
            });
        }
        let n_defaults = params.iter().filter(|p| p.default.is_some()).count();

        let code = Rc::new(inner.finish(name.to_string(), infos));

        // Capture plan: for each free variable of the child, say where the
        // enclosing (this) frame keeps its cell.
        let mut captures = Vec::new();
        for &sym in &self.table.scopes()[child].freevars {
            let owner = self.table.symbols()[sym].owner;
            if owner == self.func {
                captures.push(CaptureSource::Cell(self.table.symbols()[sym].slot));
            } else {
                let idx = self.table.scopes()[self.func]
                    .freevars
                    .iter()
                    .position(|&s| s == sym)
                    // Unlike the lookup in `unthreaded_check`, this one cannot
                    // be reached by any program: `sym` is already a free
                    // variable of `child`, and the resolve pass threads a free
                    // variable through *every* function between the user and
                    // the owner, this one included.
                    .expect("free variable must be available in the enclosing function")
                    as u16;
                captures.push(CaptureSource::Free(idx));
            }
        }

        Ok(FuncProto { code, captures, n_defaults })
    }

    // --- Stores --------------------------------------------------------------

    /// Where `name` lives in the current scope, as a diagnostic rather than a
    /// panic when the resolve pass never threaded it. Every expression that can
    /// hold a name is walked by that pass, so this should be unreachable — but
    /// an f-string field went unwalked for two releases, and the abort it
    /// produced named a line in the compiler instead of a line in the program.
    fn unthreaded_check(&self, name: &str, line: usize, col: usize) -> CResult<Resolution> {
        self.table.resolve_name(self.scope, name).ok_or_else(|| {
            self.err(
                format!(
                    "internal compiler error: `{name}` refers to an enclosing function's variable \
                     that the symbol pass did not thread through this function — please report this"
                ),
                line,
                col,
            )
        })
    }


    /// Emit code that stores the value on top of the stack into `target`.
    fn emit_store(&mut self, target: &Expr) -> CResult<()> {
        match target {
            Expr::Name { name, line, col } => {
                match self.unthreaded_check(name, *line, *col)? {
                    Resolution::Local(s) => self.emit(Op::StoreFast(s), *line, *col),
                    Resolution::Cell(s) => self.emit(Op::StoreCell(s), *line, *col),
                    Resolution::Free(s) => self.emit(Op::StoreFree(s), *line, *col),
                    Resolution::Global => {
                        return Err(self.err(
                            format!("cannot assign to `{name}`"),
                            *line,
                            *col,
                        ))
                    }
                };
            }
            Expr::Subscript { value, index, line, col } => {
                self.emit_expr(value)?;
                self.emit_expr(index)?;
                self.emit(Op::StoreSubscript, *line, *col);
            }
            Expr::Tuple { elements, line, col } | Expr::List { elements, line, col } => {
                self.emit(Op::UnpackSequence(elements.len() as u32), *line, *col);
                for e in elements {
                    self.emit_store(e)?;
                }
            }
            Expr::Attribute { value, attr, line, col } => {
                // Stack for StoreAttr: value (below), then the object.
                self.emit_expr(value)?;
                let n = self.add_name(attr);
                self.emit(Op::StoreAttr(n), *line, *col);
            }
            other => {
                let (l, c) = other.pos();
                return Err(self.err("invalid assignment target", l, c));
            }
        }
        Ok(())
    }

    // --- Expressions ---------------------------------------------------------

    fn emit_expr(&mut self, expr: &Expr) -> CResult<()> {
        match expr {
            Expr::Lambda { data, line, col } => self.emit_lambda(data, *line, *col)?,
            Expr::Int { value, line, col } => {
                let v = parse_int(value);
                let idx = self.add_const(v);
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::Float { value, line, col } => {
                let f: f64 = parse_float(value).ok_or_else(|| {
                    self.err(format!("invalid float literal `{value}`"), *line, *col)
                })?;
                let idx = self.add_const(Value::Float(f));
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::Str { value, line, col, .. } => {
                let idx = self.add_const(Value::str(value.clone()));
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::Bytes { value, line, col, .. } => {
                let idx = self.add_const(Value::bytes(value.clone()));
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::FString { value, line, col } => self.emit_fstring(value, *line, *col)?,
            Expr::Bool { value, line, col } => {
                let idx = self.add_const(Value::Bool(*value));
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::NoneLit { line, col } => {
                self.emit(Op::LoadNone, *line, *col);
            }
            Expr::Name { name, line, col } => {
                // A type keyword is a constant, not a lookup. It cannot be
                // bound (`compiler::reserved` rejects that), so there is never
                // a local to shadow it, and the value it denotes is known here
                // — which also takes `range(n)` off the global-lookup path it
                // used to sit on.
                if let Some(t) = crate::value::keyword_type(name) {
                    let idx = self.add_const(Value::Type(t));
                    self.emit(Op::LoadConst(idx), *line, *col);
                    return Ok(());
                }
                match self.unthreaded_check(name, *line, *col)? {
                    Resolution::Local(s) => self.emit(Op::LoadFast(s), *line, *col),
                    Resolution::Cell(s) => self.emit(Op::LoadCell(s), *line, *col),
                    Resolution::Free(s) => self.emit(Op::LoadFree(s), *line, *col),
                    Resolution::Global => {
                        let n = self.add_name(name);
                        self.emit(Op::LoadGlobal(n), *line, *col)
                    }
                };
            }
            Expr::Unary { op, operand, line, col } => {
                self.emit_expr(operand)?;
                let o = match op {
                    UnaryOp::Neg => Op::UnaryNeg,
                    UnaryOp::Pos => Op::UnaryPos,
                    UnaryOp::Not => Op::UnaryNot,
                };
                self.emit(o, *line, *col);
            }
            Expr::Binary { op, left, right, line, col } => {
                self.emit_expr(left)?;
                self.emit_expr(right)?;
                let o = match op {
                    BinOp::Add => Op::BinAdd,
                    BinOp::Sub => Op::BinSub,
                    BinOp::Mul => Op::BinMul,
                    BinOp::Div => Op::BinDiv,
                    BinOp::FloorDiv => Op::BinFloorDiv,
                    BinOp::Mod => Op::BinMod,
                    BinOp::Pow => Op::BinPow,
                };
                self.emit(o, *line, *col);
            }
            Expr::BoolOp { op, left, right, line, col } => {
                self.emit_expr(left)?;
                let jump = match op {
                    BoolOp::And => self.emit(Op::JumpIfFalseOrPop(0), *line, *col),
                    BoolOp::Or => self.emit(Op::JumpIfTrueOrPop(0), *line, *col),
                };
                self.emit_expr(right)?;
                let end = self.here();
                self.set_target(jump, end);
            }
            Expr::Compare { first, rest, line, col } => {
                self.emit_compare(first, rest, *line, *col)?;
            }
            Expr::Call { func, args, kwargs, line, col } => {
                self.emit_call(func, args, kwargs, *line, *col, false)?;
            }
            Expr::Attribute { value, attr, line, col } => {
                self.emit_expr(value)?;
                let n = self.add_name(attr);
                self.emit(Op::LoadAttr(n), *line, *col);
            }
            Expr::Subscript { value, index, line, col } => {
                self.emit_expr(value)?;
                self.emit_expr(index)?;
                self.emit(Op::LoadSubscript, *line, *col);
            }
            Expr::Slice { value, lower, upper, step, line, col } => {
                self.emit_expr(value)?;
                self.emit_slice_part(lower, *line, *col)?;
                self.emit_slice_part(upper, *line, *col)?;
                self.emit_slice_part(step, *line, *col)?;
                self.emit(Op::LoadSlice, *line, *col);
            }
            Expr::List { elements, line, col } => {
                for e in elements {
                    self.emit_expr(e)?;
                }
                self.emit(Op::BuildList(elements.len() as u32), *line, *col);
            }
            Expr::Tuple { elements, line, col } => {
                for e in elements {
                    self.emit_expr(e)?;
                }
                self.emit(Op::BuildTuple(elements.len() as u32), *line, *col);
            }
            Expr::Dict { entries, line, col } => {
                for (k, v) in entries {
                    self.emit_expr(k)?;
                    self.emit_expr(v)?;
                }
                self.emit(Op::BuildMap(entries.len() as u32), *line, *col);
            }
        }
        Ok(())
    }

    fn emit_slice_part(&mut self, part: &Option<Box<Expr>>, line: usize, col: usize) -> CResult<()> {
        match part {
            Some(e) => self.emit_expr(e)?,
            None => {
                self.emit(Op::LoadNone, line, col);
            }
        }
        Ok(())
    }

    fn emit_compare(
        &mut self,
        first: &Expr,
        rest: &[(crate::ast::CmpOp, Expr)],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        self.emit_expr(first)?;
        if rest.len() == 1 {
            self.emit_expr(&rest[0].1)?;
            self.emit(Op::Compare(rest[0].0), line, col);
            return Ok(());
        }
        // Chained: keep each middle operand once, short-circuiting on the first
        // false result.
        let mut exit_jumps = Vec::new();
        for (i, (op, operand)) in rest.iter().enumerate() {
            self.emit_expr(operand)?;
            if i + 1 < rest.len() {
                self.emit(Op::Dup, line, col);
                self.emit(Op::RotThree, line, col);
                self.emit(Op::Compare(*op), line, col);
                exit_jumps.push(self.emit(Op::JumpIfFalseOrPop(0), line, col));
            } else {
                self.emit(Op::Compare(*op), line, col);
            }
        }
        if exit_jumps.is_empty() {
            return Ok(());
        }
        let done = self.emit(Op::Jump(0), line, col);
        let cleanup = self.here();
        for j in exit_jumps {
            self.set_target(j, cleanup);
        }
        // On short-circuit the leftover operand sits under the False result.
        self.emit(Op::RotTwo, line, col);
        self.emit(Op::Pop, line, col);
        let end = self.here();
        self.set_target(done, end);
        Ok(())
    }

    /// Emit a call. `hint` is set only by the fused-chain recursion below: it
    /// means this call is a collection step whose result is consumed by the
    /// very next step of the same chain expression and by nothing else, so the
    /// VM is free to defer it into a pipeline. See [`crate::compiler::CHAIN_HINT`].
    fn emit_call(
        &mut self,
        func: &Expr,
        args: &[Arg],
        kwargs: &[Kwarg],
        line: usize,
        col: usize,
        hint: bool,
    ) -> CResult<()> {
        // `super()` — a zero-argument call to the global name `super` — pushes
        // the current method's super proxy directly.
        if args.is_empty() && kwargs.is_empty() {
            if let Expr::Name { name, .. } = func {
                if name == "super"
                    && matches!(self.table.resolve_name(self.scope, name), Some(Resolution::Global))
                {
                    self.emit(Op::LoadSuper, line, col);
                    return Ok(());
                }
            }
        }

        let simple = args.iter().all(|a| matches!(a, Arg::Positional(_))) && kwargs.is_empty();
        // `obj.m(a, b)` — a simple call whose callee is an attribute — is the
        // one call shape that need not build a bound method to make. It emits
        // the `LoadMethod`/`CallMethod` pair instead, one instruction for one
        // instruction, so no index in the stream moves and no jump target
        // (nor any op index stored inside a `MatchDispatch` table) needs
        // relocating. The `LoadMethod` carries the attribute's own position,
        // exactly as the `LoadAttr` it replaces did, so a missing attribute
        // still reports where the attribute is written.
        if simple {
            if let Expr::Attribute { value, attr, line: aline, col: acol } = func {
                // Chain fusion, decided here because this is the only place the
                // *shape* of a chain is visible: `xs.filter(p).map(f)` is one
                // expression, and the intermediate collection it builds has no
                // name and no other reader. When this step can flush a pipeline
                // and its receiver is a step that can join one, the receiver is
                // emitted with its defer hint set and the two run as one pass.
                //
                // The receiver's arguments are evaluated *before* its own
                // `CallMethod`, so they are never in the way; this step's are
                // evaluated between the two calls, which is why they have to be
                // inert — a call there could start a second chain over the same
                // receiver while this one is still pending.
                let fusable_recv = crate::compiler::chain_flushes(attr)
                    && args.iter().all(|a| match a {
                        Arg::Positional(e) => inert(e),
                        _ => false,
                    })
                    && chain_step(value).is_some_and(crate::compiler::chain_defers);
                if fusable_recv {
                    match value.as_ref() {
                        Expr::Call { func: rf, args: ra, kwargs: rk, line: rl, col: rc } => {
                            self.emit_call(rf, ra, rk, *rl, *rc, true)?;
                        }
                        _ => unreachable!("chain_step matched a non-call"),
                    }
                } else {
                    self.emit_expr(value)?;
                }
                let n = self.add_name(attr);
                self.emit(Op::LoadMethod(n), *aline, *acol);
                for a in args {
                    if let Arg::Positional(e) = a {
                        self.emit_expr(e)?;
                    }
                }
                let pair = self.add_pair();
                let argc = args.len() as u32
                    | if hint { crate::compiler::CHAIN_HINT } else { 0 }
                    | if fusable_recv { crate::compiler::CHAIN_FLUSH } else { 0 };
                self.pairs[pair as usize] = (n, argc);
                self.emit(Op::CallMethod(pair), line, col);
                return Ok(());
            }
        }
        self.emit_expr(func)?;
        if simple {
            for a in args {
                if let Arg::Positional(e) = a {
                    self.emit_expr(e)?;
                }
            }
            self.emit(Op::Call(args.len() as u32), line, col);
            return Ok(());
        }
        // General path: assemble a positional list and a keyword dict.
        self.emit(Op::BuildList(0), line, col);
        for a in args {
            match a {
                Arg::Positional(e) => {
                    self.emit_expr(e)?;
                    self.emit(Op::ListAppend, line, col);
                }
                Arg::Star(e) => {
                    self.emit_expr(e)?;
                    self.emit(Op::ListExtend, line, col);
                }
            }
        }
        self.emit(Op::BuildMap(0), line, col);
        for k in kwargs {
            match k {
                Kwarg::Keyword(name, e) => {
                    let idx = self.add_const(Value::str(name.clone()));
                    self.emit(Op::LoadConst(idx), line, col);
                    self.emit_expr(e)?;
                    self.emit(Op::MapSetItem, line, col);
                }
                Kwarg::DoubleStar(e) => {
                    self.emit_expr(e)?;
                    self.emit(Op::MapMerge, line, col);
                }
            }
        }
        self.emit(Op::CallEx, line, col);
        Ok(())
    }

    // --- f-strings -----------------------------------------------------------

    fn emit_fstring(&mut self, raw: &str, line: usize, col: usize) -> CResult<()> {
        let pieces = scan_fstring(raw).map_err(|m| self.err(m, line, col))?;
        let mut parts = 0usize;
        for piece in pieces {
            match piece {
                FPiece::Lit(text) => {
                    let idx = self.add_const(Value::str(text));
                    self.emit(Op::LoadConst(idx), line, col);
                }
                FPiece::Field(src) => self.emit_field(&src, line, col)?,
            }
            parts += 1;
        }

        match parts {
            0 => {
                let idx = self.add_const(Value::str(String::new()));
                self.emit(Op::LoadConst(idx), line, col);
            }
            1 => {}
            n => {
                self.emit(Op::BuildString(n as u32), line, col);
            }
        }
        Ok(())
    }

    /// Emit code for one replacement field `expr[!conv][:spec]`: push the value,
    /// push the (possibly nested) format-spec string, then `FormatValue`.
    fn emit_field(&mut self, src: &str, line: usize, col: usize) -> CResult<()> {
        let field = split_field(src);
        let expr = parse_field_expr(&field.expr).map_err(|m| self.err(m, line, col))?;
        self.emit_expr(&expr)?;

        let conv = match field.conv {
            None => crate::format::CONV_NONE,
            Some('s') => crate::format::CONV_STR,
            Some('r') => crate::format::CONV_REPR,
            Some('a') => crate::format::CONV_ASCII,
            Some(c) => {
                return Err(self.err(
                    format!("f-string: invalid conversion character '{c}' (expected 's', 'r', or 'a')"),
                    line,
                    col,
                ))
            }
        };

        match field.spec {
            None => {
                let idx = self.add_const(Value::str(String::new()));
                self.emit(Op::LoadConst(idx), line, col);
            }
            Some(spec) => self.emit_spec(&spec, line, col)?,
        }

        self.emit(Op::FormatValue(conv), line, col);
        Ok(())
    }

    /// Build a format-spec string on the stack. A static spec is a single
    /// constant; a spec with nested `{expr}` fields is assembled at runtime.
    fn emit_spec(&mut self, spec: &str, line: usize, col: usize) -> CResult<()> {
        let pieces = scan_spec(spec).map_err(|m| self.err(m, line, col))?;
        let mut parts = 0usize;
        for piece in pieces {
            match piece {
                FPiece::Lit(text) => {
                    let idx = self.add_const(Value::str(text));
                    self.emit(Op::LoadConst(idx), line, col);
                }
                FPiece::Field(src) => {
                    let field = split_field(&src);
                    // Python allows exactly one level of nesting, so the inner
                    // field may not carry a spec of its own.
                    if field.spec.is_some() {
                        return Err(self.err("f-string: format spec nested too deeply", line, col));
                    }
                    let expr = parse_field_expr(&field.expr).map_err(|m| self.err(m, line, col))?;
                    self.emit_expr(&expr)?;
                    let conv = match field.conv {
                        None => crate::format::CONV_NONE,
                        Some('s') => crate::format::CONV_STR,
                        Some('r') => crate::format::CONV_REPR,
                        Some('a') => crate::format::CONV_ASCII,
                        Some(_) => crate::format::CONV_NONE,
                    };
                    let idx = self.add_const(Value::str(String::new()));
                    self.emit(Op::LoadConst(idx), line, col);
                    self.emit(Op::FormatValue(conv), line, col);
                }
            }
            parts += 1;
        }

        match parts {
            0 => {
                let idx = self.add_const(Value::str(String::new()));
                self.emit(Op::LoadConst(idx), line, col);
            }
            1 => {}
            n => {
                self.emit(Op::BuildString(n as u32), line, col);
            }
        }
        Ok(())
    }
}

/// One piece of an f-string's raw text: literal text, with escapes already
/// decoded, or the raw source of a replacement field.
///
/// Splitting the scan out from the emit is what lets the symbol pass see the
/// same fields codegen will. f-string fields are parsed here at code-generation
/// time rather than by the parser (see "Known limitations" in the README), so
/// without a shared scanner the two passes disagree about which text is an
/// expression — and the resolve pass then meets a free variable it never
/// threaded.
pub(super) enum FPiece {
    Lit(String),
    Field(String),
}

/// Push `text` as a literal piece unless it is empty (an empty literal would
/// only add a wasted `BuildString` operand).
fn flush_lit(text: &mut String, out: &mut Vec<FPiece>) {
    if !text.is_empty() {
        out.push(FPiece::Lit(std::mem::take(text)));
    }
}

/// Scan an f-string body into literal and field pieces. The single place the
/// `{`/`}` and escape rules live.
pub(super) fn scan_fstring(raw: &str) -> Result<Vec<FPiece>, String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut literal = String::new();
    let mut out = Vec::new();

    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                literal.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                literal.push('}');
                i += 2;
            }
            // Escape sequences in the literal text are decoded (the lexer keeps
            // f-string text raw so interpolation can be parsed here).
            '\\' => {
                i += 1;
                let decoded = decode_escape(&chars, &mut i).map_err(|m| format!("in f-string: {m}"))?;
                literal.push_str(&decoded);
            }
            '{' => {
                flush_lit(&mut literal, &mut out);
                i += 1;
                out.push(FPiece::Field(capture_field(&chars, &mut i)?));
            }
            '}' => return Err("single `}` in f-string".to_string()),
            c => {
                literal.push(c);
                i += 1;
            }
        }
    }
    flush_lit(&mut literal, &mut out);
    Ok(out)
}

/// Capture the raw text of a replacement field: everything up to the `}` that
/// closes it, tracking nested `{ }` (from nested format specs) so the spec's own
/// braces don't terminate the field early. `i` points just past the opening `{`
/// on entry and just past the closing `}` on return.
fn capture_field(chars: &[char], i: &mut usize) -> Result<String, String> {
    let mut depth = 1;
    let mut src = String::new();
    while *i < chars.len() && depth > 0 {
        match chars[*i] {
            '{' => {
                depth += 1;
                src.push('{');
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                src.push('}');
            }
            c => src.push(c),
        }
        *i += 1;
    }
    if depth != 0 {
        return Err("unterminated `{` in f-string".to_string());
    }
    *i += 1; // consume the closing '}'
    Ok(src)
}

/// Scan a format spec into literal and (nested) field pieces. A nested field
/// may not itself carry a nested spec — Python allows only one level — so this
/// captures up to a plain `}`, and escapes are *not* decoded here: the spec text
/// already came through [`scan_fstring`].
pub(super) fn scan_spec(spec: &str) -> Result<Vec<FPiece>, String> {
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut literal = String::new();
    let mut out = Vec::new();

    while i < chars.len() {
        match chars[i] {
            '{' if chars.get(i + 1) == Some(&'{') => {
                literal.push('{');
                i += 2;
            }
            '}' if chars.get(i + 1) == Some(&'}') => {
                literal.push('}');
                i += 2;
            }
            '{' => {
                flush_lit(&mut literal, &mut out);
                i += 1;
                let mut inner = String::new();
                while i < chars.len() && chars[i] != '}' {
                    inner.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated `{` in f-string format spec".to_string());
                }
                i += 1; // consume '}'
                out.push(FPiece::Field(inner));
            }
            '}' => return Err("single `}` in f-string format spec".to_string()),
            c => {
                literal.push(c);
                i += 1;
            }
        }
    }
    flush_lit(&mut literal, &mut out);
    Ok(out)
}

/// Lex and parse one replacement field's expression source.
fn parse_field_expr(src: &str) -> Result<Expr, String> {
    if src.trim().is_empty() {
        return Err("empty expression in f-string".to_string());
    }
    let tokens = Lexer::new(src).tokenize().map_err(|e| format!("in f-string: {}", e.message))?;
    let prog = Parser::new(tokens).parse().map_err(|e| format!("in f-string: {}", e.message))?;
    match prog.as_slice() {
        [Stmt::Expr { value, .. }] => Ok(value.clone()),
        _ => Err("f-string field must be a single expression".to_string()),
    }
}

/// Every expression an f-string literal interpolates, in source order: the
/// replacement fields, plus any nested fields inside a format spec.
///
/// Malformed text yields nothing rather than an error. [`emit_fstring`] scans
/// the same text again and reports the problem with a source location, so the
/// symbol pass — which has no diagnostics of its own — stays infallible and
/// simply has nothing to walk.
///
/// [`emit_fstring`]: Compiler::emit_fstring
pub(super) fn fstring_field_exprs(raw: &str) -> Vec<Expr> {
    let mut out = Vec::new();
    let Ok(pieces) = scan_fstring(raw) else { return out };
    for piece in pieces {
        let FPiece::Field(src) = piece else { continue };
        let field = split_field(&src);
        if let Ok(expr) = parse_field_expr(&field.expr) {
            out.push(expr);
        }
        let Some(spec) = field.spec else { continue };
        let Ok(inner) = scan_spec(&spec) else { continue };
        for piece in inner {
            let FPiece::Field(src) = piece else { continue };
            let field = split_field(&src);
            if let Ok(expr) = parse_field_expr(&field.expr) {
                out.push(expr);
            }
        }
    }
    out
}

/// The three pieces of a replacement field: the expression source, an optional
/// `!` conversion char, and an optional `:` format spec.
struct Field {
    expr: String,
    conv: Option<char>,
    spec: Option<String>,
}

/// Split `expr[!conv][:spec]`. The `!` and `:` separators are only recognised
/// at bracket depth zero and outside string literals, so slices (`a[1:2]`),
/// dict literals, and `!=` operators inside the expression are left intact.
fn split_field(src: &str) -> Field {
    let chars: Vec<char> = src.chars().collect();
    let mut depth = 0i32;
    let mut in_str: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = in_str {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => in_str = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '!' if depth == 0 => {
                // A conversion is `!` + one of r/s/a, then end-of-field or `:`.
                // Anything else (e.g. `!=`) belongs to the expression.
                if let Some(&n) = chars.get(i + 1) {
                    if matches!(n, 'r' | 's' | 'a') && matches!(chars.get(i + 2), None | Some(':')) {
                        let expr: String = chars[..i].iter().collect();
                        let spec = if chars.get(i + 2) == Some(&':') {
                            Some(chars[i + 3..].iter().collect())
                        } else {
                            None
                        };
                        return Field { expr, conv: Some(n), spec };
                    }
                }
            }
            ':' if depth == 0 => {
                let expr: String = chars[..i].iter().collect();
                let spec: String = chars[i + 1..].iter().collect();
                return Field { expr, conv: None, spec: Some(spec) };
            }
            _ => {}
        }
        i += 1;
    }
    Field { expr: src.to_string(), conv: None, spec: None }
}

/// Decode one escape sequence in f-string literal text. `i` points at the
/// character after the backslash; it is advanced past the consumed char(s).
/// Unknown escapes keep the backslash verbatim (matching the string lexer).
/// Decode one escape inside an f-string's literal text. Shares its tables with
/// the lexer (see `crate::lexer::simple_escape`) so the two can never disagree
/// about what an escape means.
fn decode_escape(chars: &[char], i: &mut usize) -> Result<String, String> {
    use crate::lexer::{decode_hex_escape, hex_escape_width, simple_escape, unknown_escape_message};
    let Some(&e) = chars.get(*i) else {
        return Err("a string may not end with a lone backslash".to_string());
    };
    *i += 1;
    if let Some(c) = simple_escape(e) {
        return Ok(c.to_string());
    }
    if let Some(width) = hex_escape_width(e) {
        let mut digits = String::new();
        while digits.len() < width {
            match chars.get(*i) {
                Some(&d) if d.is_ascii_hexdigit() => {
                    digits.push(d);
                    *i += 1;
                }
                _ => break,
            }
        }
        return decode_hex_escape(e, &digits).map(|c| c.to_string());
    }
    Err(unknown_escape_message(e))
}

/// Whether `stmts` contain a `yield` (searching nested blocks but not nested
/// `def`/`class`, which start their own function scope).
fn contains_yield(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_yields)
}

fn stmt_yields(s: &Stmt) -> bool {
    match s {
        Stmt::Yield { .. } => true,
        Stmt::If { body, elifs, orelse, .. } => {
            contains_yield(body)
                || elifs.iter().any(|(_, b)| contains_yield(b))
                || orelse.as_ref().is_some_and(|b| contains_yield(b))
        }
        Stmt::While { body, .. } | Stmt::For { body, .. } => contains_yield(body),
        Stmt::Try { body, handlers, finalbody, .. } => {
            contains_yield(body)
                || handlers.iter().any(|h| contains_yield(&h.body))
                || finalbody.as_ref().is_some_and(|b| contains_yield(b))
        }
        Stmt::Match { cases, .. } => cases.iter().any(|c| contains_yield(&c.body)),
        _ => false,
    }
}

/// The reason a class dunder is rejected, if it names a deliberately-cut hook.
fn unsupported_dunder(name: &str) -> Option<&'static str> {
    match name {
        "__new__" => Some(
            "__new__ is not supported in Oro — define __init__ instead; there is no separate \
             allocation hook",
        ),
        "__getattr__" | "__getattribute__" => Some(
            "__getattr__/__getattribute__ are not supported in Oro — attribute access is fixed so \
             it can be read directly; store data in instance fields",
        ),
        "__setattr__" | "__delattr__" => Some(
            "__setattr__/__delattr__ are not supported in Oro — attribute assignment is direct",
        ),
        // A dict key is a pure Rust projection of a value with derived
        // equality, so a lookup never calls Oro code and has nowhere to call a
        // `__hash__` from. Defining one alone would hash into the right bucket
        // and then compare by address — a miss, silently. The whole argument is
        // `docs/hash-and-equality.md`; the replacement is a tuple key.
        "__hash__" => Some(
            "__hash__ is not in Oro's dunder set — a class that defines __eq__ is a value type \
             and is not a dict key; key by the value instead, e.g. d[(self.row, self.col)]",
        ),
        _ => None,
    }
}

/// Parse an integer literal, using `i64` when it fits and promoting to `BigInt`
/// otherwise (architecture point 4 — allocation only on overflow).
fn parse_int(text: &str) -> Value {
    // The separators are the author's, not the value's. The *spelling* is kept
    // on the token — `oro fmt` reprints it — and stripped only here, where a
    // number is being made out of it.
    let clean: String = text.chars().filter(|c| *c != '_').collect();
    let (digits, radix) = split_radix(&clean);
    match i64::from_str_radix(digits, radix) {
        Ok(i) => Value::Int(i),
        // Too large for `i64`: the bignum path, which is the whole reason the
        // lexer hands over text rather than a number. `0xFFFFFFFFFFFFFFFFFF`
        // promotes exactly as `4722366482869645213695` does.
        Err(_) => match bigint_in_radix(digits, radix) {
            Some(b) => Value::from_bigint(b),
            None => Value::Int(0), // lexer guarantees digits; unreachable in practice
        },
    }
}

/// Split a separator-free integer literal into its digits and its radix.
/// Prefix letters are case-insensitive, as CPython's are.
fn split_radix(clean: &str) -> (&str, u32) {
    let mut it = clean.chars();
    if it.next() == Some('0') {
        if let Some(radix) = match it.next() {
            Some('x') | Some('X') => Some(16),
            Some('o') | Some('O') => Some(8),
            Some('b') | Some('B') => Some(2),
            _ => None,
        } {
            return (&clean[2..], radix);
        }
    }
    (clean, 10)
}

/// A `BigInt` from digits in `radix`. Horner, one digit at a time: literals are
/// short, and this needs no representation-specific arithmetic beyond the
/// `mul`/`add` [`BigInt`] already has.
fn bigint_in_radix(digits: &str, radix: u32) -> Option<BigInt> {
    if radix == 10 {
        // The decimal path is already written and already tested; going through
        // it keeps the common case on the code that has always served it.
        return BigInt::parse_decimal(digits);
    }
    let base = BigInt::from_i64(radix as i64);
    let mut acc = BigInt::zero();
    for c in digits.chars() {
        acc = acc.mul(&base).add(&BigInt::from_i64(c.to_digit(radix)? as i64));
    }
    Some(acc)
}

/// The `f64` a float literal denotes. Rust's parser has no notion of Python's
/// separators, so they come out here — the same rule [`parse_int`] follows, for
/// the same reason.
fn parse_float(text: &str) -> Option<f64> {
    if text.contains('_') {
        return text.replace('_', "").parse().ok();
    }
    text.parse().ok()
}

/// The method name of `recv.name(args)` when it is written in the simple form
/// the `LoadMethod`/`CallMethod` pair handles — the only form a chain step can
/// take. `None` for anything else, which is what stops fusion at the front of a
/// chain (`xs` itself, a subscript, a parenthesised expression).
fn chain_step(e: &Expr) -> Option<&str> {
    match e {
        Expr::Call { func, args, kwargs, .. }
            if kwargs.is_empty()
                && args.iter().all(|a| matches!(a, Arg::Positional(_))) =>
        {
            match func.as_ref() {
                Expr::Attribute { attr, .. } => Some(attr),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Whether an expression evaluates without running a line of Oro code.
///
/// A chain step's arguments are emitted *between* the deferred step's
/// `CallMethod` and the one that flushes it. Restricting them to these five
/// forms is what makes that gap safe: no user code runs in it, so no second
/// chain can start over the same receiver, and the only thing that can go wrong
/// is an unbound name — which raises, and takes the pending pipeline with it.
fn inert(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Bytes { .. }
            | Expr::Bool { .. }
            | Expr::NoneLit { .. }
            | Expr::Lambda { .. }
            | Expr::Name { .. }
    )
}
