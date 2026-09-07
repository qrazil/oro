//! Codegen: the second pass. Walks the AST alongside the finalized symbol table
//! (see [`super::symbols`]) and emits [`Op`]s, resolving every name to the slot,
//! cell, or free index the pre-pass assigned.
//!
//! The scope walk here is in lockstep with [`super::symbols::SymTable::resolve_module`]:
//! each scope's child scopes are consumed in source order via a per-scope
//! cursor, so a `def`/`if`/`for` here maps to exactly the scope the pre-pass
//! built for it.

use std::cell::RefCell;
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
use super::{CaptureSource, CodeObject, CompileError, FuncProto, Op, ParamInfo, VarTarget};

type CResult<T> = Result<T, CompileError>;

/// Per-loop bookkeeping so `break`/`continue` can emit patched jumps.
struct LoopCtx {
    /// Where `continue` jumps to (loop test, or the `ForIter`).
    continue_target: usize,
    /// True inside a `for` loop, where the iterator sits on the stack and must
    /// be popped before a `break` leaves.
    iter_on_stack: bool,
    /// Indices of `break` jumps awaiting the after-loop target.
    breaks: Vec<usize>,
}

struct Codegen<'a> {
    table: &'a SymTable,
    scope: usize,
    func: usize,
    ops: Vec<Op>,
    spans: Vec<(u32, u32)>,
    consts: Vec<Value>,
    protos: Vec<Rc<FuncProto>>,
    /// Cursor into the current scope's child list.
    cursor: usize,
    loops: Vec<LoopCtx>,
    /// True while compiling a function body that contains `yield`.
    is_generator: bool,
}

/// Compile the module body into its top-level code object.
pub fn compile_module(table: &SymTable, body: &[Stmt]) -> CResult<Rc<CodeObject>> {
    let module = table.module;
    let mut cg = Codegen::new(table, module);
    cg.emit_body(body)?;
    cg.emit(Op::LoadNone, 1, 1);
    cg.emit(Op::Return, 1, 1);
    Ok(Rc::new(cg.finish("<module>".to_string(), Vec::new())))
}

impl<'a> Codegen<'a> {
    fn new(table: &'a SymTable, scope: usize) -> Codegen<'a> {
        let func = scope; // callers pass a function/module scope
        Codegen {
            table,
            scope,
            func,
            ops: Vec::new(),
            spans: Vec::new(),
            consts: Vec::new(),
            protos: Vec::new(),
            cursor: 0,
            loops: Vec::new(),
            is_generator: false,
        }
    }

    fn finish(self, name: String, params: Vec<ParamInfo>) -> CodeObject {
        CodeObject {
            name,
            ops: self.ops,
            spans: self.spans,
            consts: self.consts,
            protos: self.protos,
            nlocals: self.table.scopes()[self.func].nlocals as usize,
            ncells: self.table.ncells(self.func) as usize,
            nfree: self.table.nfree(self.func) as usize,
            params,
            is_generator: self.is_generator,
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

    fn here(&self) -> usize {
        self.ops.len()
    }

    fn add_const(&mut self, v: Value) -> usize {
        let idx = self.consts.len();
        self.consts.push(v);
        idx
    }

    fn set_target(&mut self, idx: usize, target: usize) {
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
                self.emit(Op::ImportModule(Rc::from(dotted.as_str())), *line, *col);
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
        let dispatch = self.emit(Op::MatchDispatch { table: 0, default: 0 }, line, col);

        let mut end_jumps = Vec::new();
        let mut table = OroDict::new();
        let mut default_target: Option<usize> = None;

        for case in cases {
            let start = self.here();
            match &case.pattern {
                Pattern::Literal(expr) => {
                    let key = self.literal_value(expr)?;
                    // First case wins when two literals are equal keys
                    // (e.g. 1 and True, or a repeated value).
                    if self.dict_missing(&table, &key)? {
                        table
                            .insert(key, Value::Int(start as i64))
                            .map_err(|e| self.err(e, line, col))?;
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
        self.ops[dispatch] = Op::MatchDispatch { table: table_idx, default };
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
            Expr::Float { value, line, col } => value
                .parse::<f64>()
                .map(Value::Float)
                .map_err(|_| self.err(format!("invalid float literal `{value}`"), *line, *col)),
            Expr::Str { value, .. } => Ok(Value::str(value.clone())),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::NoneLit { .. } => Ok(Value::None),
            Expr::Unary { op: UnaryOp::Neg, operand, line, col } => {
                let v = self.literal_value(operand)?;
                crate::vm::arith::neg(&v).map_err(|e| self.err(e, *line, *col))
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
            .map_err(|e| self.err(e, 0, 0))
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
            self.emit_child_block(finalbody.as_ref().unwrap())?;
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
        let top = self.here();
        let (cl, cc) = cond.pos();
        self.emit_expr(cond)?;
        let exit = self.emit(Op::PopJumpIfFalse(0), cl, cc);
        self.loops.push(LoopCtx { continue_target: top, iter_on_stack: false, breaks: Vec::new() });
        self.emit_child_block(body)?;
        self.emit(Op::Jump(top), cl, cc);
        let after = self.here();
        self.set_target(exit, after);
        let ctx = self.loops.pop().unwrap();
        for b in ctx.breaks {
            self.set_target(b, after);
        }
        Ok(())
    }

    fn emit_for(&mut self, target: &Expr, iter: &Expr, body: &[Stmt]) -> CResult<()> {
        let (il, ic) = iter.pos();
        self.emit_expr(iter)?;
        self.emit(Op::GetIter, il, ic);
        let top = self.here();
        let foriter = self.emit(Op::ForIter(0), il, ic);
        // The loop target and body live in the child block scope.
        let child = self.next_child();
        let saved_scope = self.scope;
        let saved_cursor = self.cursor;
        self.scope = child;
        self.cursor = 0;
        self.emit_store(target)?;
        self.loops.push(LoopCtx { continue_target: top, iter_on_stack: true, breaks: Vec::new() });
        self.emit_body(body)?;
        self.emit(Op::Jump(top), il, ic);
        let after = self.here();
        self.set_target(foriter, after);
        let ctx = self.loops.pop().unwrap();
        for b in ctx.breaks {
            self.set_target(b, after);
        }
        self.scope = saved_scope;
        self.cursor = saved_cursor;
        Ok(())
    }

    fn emit_break(&mut self, line: usize, col: usize) -> CResult<()> {
        let iter_on_stack = match self.loops.last() {
            Some(l) => l.iter_on_stack,
            None => return Err(self.err("`break` outside of a loop", line, col)),
        };
        if iter_on_stack {
            self.emit(Op::Pop, line, col);
        }
        let j = self.emit(Op::Jump(0), line, col);
        self.loops.last_mut().unwrap().breaks.push(j);
        Ok(())
    }

    fn emit_continue(&mut self, line: usize, col: usize) -> CResult<()> {
        let target = match self.loops.last() {
            Some(l) => l.continue_target,
            None => return Err(self.err("`continue` outside of a loop", line, col)),
        };
        self.emit(Op::Jump(target), line, col);
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
        let proto_idx = self.protos.len();
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

        self.emit(Op::BuildClass { name: Rc::from(name), members, has_base }, line, col);
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
        let mut inner = Codegen::new(self.table, child);
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
                Resolution::Local(s) => VarTarget::Local(s),
                Resolution::Cell(s) => VarTarget::Cell(s),
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
                    .expect("free variable must be available in the enclosing function")
                    as u16;
                captures.push(CaptureSource::Free(idx));
            }
        }

        Ok(FuncProto { code, captures, n_defaults })
    }

    // --- Stores --------------------------------------------------------------

    /// Emit code that stores the value on top of the stack into `target`.
    fn emit_store(&mut self, target: &Expr) -> CResult<()> {
        match target {
            Expr::Name { name, line, col } => {
                match self.table.resolve_name(self.scope, name) {
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
                self.emit(Op::UnpackSequence(elements.len()), *line, *col);
                for e in elements {
                    self.emit_store(e)?;
                }
            }
            Expr::Attribute { value, attr, line, col } => {
                // Stack for StoreAttr: value (below), then the object.
                self.emit_expr(value)?;
                self.emit(Op::StoreAttr(Rc::from(attr.as_str())), *line, *col);
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
            Expr::Int { value, line, col } => {
                let v = parse_int(value);
                let idx = self.add_const(v);
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::Float { value, line, col } => {
                let f: f64 = value.parse().map_err(|_| {
                    self.err(format!("invalid float literal `{value}`"), *line, *col)
                })?;
                let idx = self.add_const(Value::Float(f));
                self.emit(Op::LoadConst(idx), *line, *col);
            }
            Expr::Str { value, line, col } => {
                let idx = self.add_const(Value::str(value.clone()));
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
                match self.table.resolve_name(self.scope, name) {
                    Resolution::Local(s) => self.emit(Op::LoadFast(s), *line, *col),
                    Resolution::Cell(s) => self.emit(Op::LoadCell(s), *line, *col),
                    Resolution::Free(s) => self.emit(Op::LoadFree(s), *line, *col),
                    Resolution::Global => {
                        self.emit(Op::LoadGlobal(Rc::from(name.as_str())), *line, *col)
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
                self.emit_call(func, args, kwargs, *line, *col)?;
            }
            Expr::Attribute { value, attr, line, col } => {
                self.emit_expr(value)?;
                self.emit(Op::LoadAttr(Rc::from(attr.as_str())), *line, *col);
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
                self.emit(Op::BuildList(elements.len()), *line, *col);
            }
            Expr::Tuple { elements, line, col } => {
                for e in elements {
                    self.emit_expr(e)?;
                }
                self.emit(Op::BuildTuple(elements.len()), *line, *col);
            }
            Expr::Set { elements, line, col } => {
                for e in elements {
                    self.emit_expr(e)?;
                }
                self.emit(Op::BuildSet(elements.len()), *line, *col);
            }
            Expr::Dict { entries, line, col } => {
                for (k, v) in entries {
                    self.emit_expr(k)?;
                    self.emit_expr(v)?;
                }
                self.emit(Op::BuildMap(entries.len()), *line, *col);
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

    fn emit_call(
        &mut self,
        func: &Expr,
        args: &[Arg],
        kwargs: &[Kwarg],
        line: usize,
        col: usize,
    ) -> CResult<()> {
        // `super()` — a zero-argument call to the global name `super` — pushes
        // the current method's super proxy directly.
        if args.is_empty() && kwargs.is_empty() {
            if let Expr::Name { name, .. } = func {
                if name == "super"
                    && matches!(self.table.resolve_name(self.scope, name), Resolution::Global)
                {
                    self.emit(Op::LoadSuper, line, col);
                    return Ok(());
                }
            }
        }

        let simple = args.iter().all(|a| matches!(a, Arg::Positional(_))) && kwargs.is_empty();
        self.emit_expr(func)?;
        if simple {
            for a in args {
                if let Arg::Positional(e) = a {
                    self.emit_expr(e)?;
                }
            }
            self.emit(Op::Call(args.len()), line, col);
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
        let chars: Vec<char> = raw.chars().collect();
        let mut i = 0;
        let mut literal = String::new();
        let mut parts = 0usize;

        macro_rules! flush {
            () => {
                if !literal.is_empty() {
                    let idx = self.add_const(Value::str(std::mem::take(&mut literal)));
                    self.emit(Op::LoadConst(idx), line, col);
                    parts += 1;
                }
            };
        }

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
                // Escape sequences in the literal text are decoded (the lexer
                // keeps f-string text raw so interpolation can be parsed here).
                '\\' => {
                    i += 1;
                    let decoded = decode_escape(&chars, &mut i);
                    literal.push_str(&decoded);
                }
                '{' => {
                    flush!();
                    i += 1;
                    let src = self.capture_field(&chars, &mut i, line, col)?;
                    self.emit_field(&src, line, col)?;
                    parts += 1;
                }
                '}' => {
                    return Err(self.err("single `}` in f-string", line, col));
                }
                c => {
                    literal.push(c);
                    i += 1;
                }
            }
        }
        flush!();

        match parts {
            0 => {
                let idx = self.add_const(Value::str(String::new()));
                self.emit(Op::LoadConst(idx), line, col);
            }
            1 => {}
            n => {
                self.emit(Op::BuildString(n), line, col);
            }
        }
        Ok(())
    }

    /// Capture the raw text of a replacement field: everything up to the `}`
    /// that closes it, tracking nested `{ }` (from nested format specs) so the
    /// spec's own braces don't terminate the field early. `i` points just past
    /// the opening `{` on entry and just past the closing `}` on return.
    fn capture_field(
        &self,
        chars: &[char],
        i: &mut usize,
        line: usize,
        col: usize,
    ) -> CResult<String> {
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
            return Err(self.err("unterminated `{` in f-string", line, col));
        }
        *i += 1; // consume the closing '}'
        Ok(src)
    }

    /// Emit code for one replacement field `expr[!conv][:spec]`: push the value,
    /// push the (possibly nested) format-spec string, then `FormatValue`.
    fn emit_field(&mut self, src: &str, line: usize, col: usize) -> CResult<()> {
        let field = split_field(src);
        let expr = self.parse_fstring_expr(&field.expr, line, col)?;
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
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        let mut literal = String::new();
        let mut parts = 0usize;

        macro_rules! flush {
            () => {
                if !literal.is_empty() {
                    let idx = self.add_const(Value::str(std::mem::take(&mut literal)));
                    self.emit(Op::LoadConst(idx), line, col);
                    parts += 1;
                }
            };
        }

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
                    flush!();
                    i += 1;
                    // A nested field may not itself carry a nested spec (Python
                    // allows only one level), so capture up to a plain `}`.
                    let mut inner = String::new();
                    while i < chars.len() && chars[i] != '}' {
                        inner.push(chars[i]);
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(self.err("unterminated `{` in f-string format spec", line, col));
                    }
                    i += 1; // consume '}'
                    let field = split_field(&inner);
                    if field.spec.is_some() {
                        return Err(self.err(
                            "f-string: format spec nested too deeply",
                            line,
                            col,
                        ));
                    }
                    let expr = self.parse_fstring_expr(&field.expr, line, col)?;
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
                    parts += 1;
                }
                '}' => {
                    return Err(self.err("single `}` in f-string format spec", line, col));
                }
                c => {
                    literal.push(c);
                    i += 1;
                }
            }
        }
        flush!();

        match parts {
            0 => {
                let idx = self.add_const(Value::str(String::new()));
                self.emit(Op::LoadConst(idx), line, col);
            }
            1 => {}
            n => {
                self.emit(Op::BuildString(n), line, col);
            }
        }
        Ok(())
    }

    fn parse_fstring_expr(&self, src: &str, line: usize, col: usize) -> CResult<Expr> {
        if src.trim().is_empty() {
            return Err(self.err("empty expression in f-string", line, col));
        }
        let tokens = Lexer::new(src)
            .tokenize()
            .map_err(|e| self.err(format!("in f-string: {}", e.message), line, col))?;
        let prog = Parser::new(tokens)
            .parse()
            .map_err(|e| self.err(format!("in f-string: {}", e.message), line, col))?;
        match prog.as_slice() {
            [Stmt::Expr { value, .. }] => Ok(value.clone()),
            _ => Err(self.err("f-string field must be a single expression", line, col)),
        }
    }
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
fn decode_escape(chars: &[char], i: &mut usize) -> String {
    let Some(&e) = chars.get(*i) else {
        return "\\".to_string();
    };
    *i += 1;
    match e {
        'n' => "\n".to_string(),
        't' => "\t".to_string(),
        'r' => "\r".to_string(),
        '\\' => "\\".to_string(),
        '\'' => "'".to_string(),
        '"' => "\"".to_string(),
        '0' => "\0".to_string(),
        other => format!("\\{other}"),
    }
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
        _ => None,
    }
}

/// Parse an integer literal, using `i64` when it fits and promoting to `BigInt`
/// otherwise (architecture point 4 — allocation only on overflow).
fn parse_int(text: &str) -> Value {
    match text.parse::<i64>() {
        Ok(i) => Value::Int(i),
        Err(_) => match BigInt::parse_decimal(text) {
            Some(b) => Value::from_bigint(b),
            None => Value::Int(0), // lexer guarantees digits; unreachable in practice
        },
    }
}
