//! The symbol pre-pass (architecture point 3).
//!
//! Three sub-steps, run before any codegen:
//!
//! * [`SymTable::build_module`] builds the scope tree and, for every scope,
//!   collects the names it *binds*, honouring Oro's block scoping.
//! * [`SymTable::resolve_module`] walks every name *reference* to discover which
//!   locals are captured by an inner function (closures) and to thread free
//!   variables through the intervening functions.
//! * [`SymTable::allocate`] numbers each variable: a plain local slot, or a cell
//!   index if it is captured.
//!
//! Codegen then asks [`SymTable::resolve_name`] where each name lives.

use std::collections::{HashMap, HashSet};

use crate::ast::{Arg, Expr, Kwarg, Stmt};

/// The kind of a lexical scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// The module body. Treated as a function for resolution purposes.
    Module,
    /// A `def` body.
    Function,
    /// An `if` / `elif` / `else` / `while` / `for` body. Not a closure
    /// boundary, but its names do not leak to the enclosing block.
    Block,
}

/// A variable binding.
#[derive(Debug)]
pub struct Symbol {
    /// The function (or module) scope that owns this variable's storage.
    pub owner: usize,
    /// True when an inner function reads this variable (so it needs a cell).
    pub captured: bool,
    /// Local slot index, or cell index when `captured`. Filled by `allocate`.
    pub slot: u16,
}

/// A lexical scope.
#[derive(Debug)]
pub struct Scope {
    pub kind: ScopeKind,
    pub parent: Option<usize>,
    /// Nearest enclosing function/module scope (itself for functions/modules).
    pub func: usize,
    /// Names bound directly in this scope → symbol id.
    pub decls: HashMap<String, usize>,
    /// Child scopes in source order (consumed identically by later passes).
    pub children: Vec<usize>,

    // Function/module scopes only:
    /// Symbols owned here, in declaration order.
    pub owned: Vec<usize>,
    /// Owned symbols that are captured, in slot order (filled by `allocate`).
    pub cellvars: Vec<usize>,
    /// Symbols owned by an ancestor and referenced/threaded here, in first-use
    /// order.
    pub freevars: Vec<usize>,
    pub nlocals: u16,
    /// Names declared `global` in this function; they bind to module scope and
    /// an assignment to them does not create a local. Function scopes only.
    pub globals: HashSet<String>,
}

impl Scope {
    fn new(kind: ScopeKind, parent: Option<usize>, func: usize) -> Scope {
        Scope {
            kind,
            parent,
            func,
            decls: HashMap::new(),
            children: Vec::new(),
            owned: Vec::new(),
            cellvars: Vec::new(),
            freevars: Vec::new(),
            nlocals: 0,
            globals: HashSet::new(),
        }
    }

    fn is_function(&self) -> bool {
        matches!(self.kind, ScopeKind::Module | ScopeKind::Function)
    }
}

/// Where codegen should read/write a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Local(u16),
    Cell(u16),
    Free(u16),
    /// Not bound in any enclosing scope: a builtin (Oro's only globals).
    Global,
}

pub struct SymTable {
    pub scopes: Vec<Scope>,
    pub symbols: Vec<Symbol>,
    pub module: usize,
}

impl SymTable {
    pub fn new() -> SymTable {
        let mut t = SymTable { scopes: Vec::new(), symbols: Vec::new(), module: 0 };
        t.module = t.new_scope(ScopeKind::Module, None, 0);
        // The module scope is its own function.
        t.scopes[t.module].func = t.module;
        t
    }

    fn new_scope(&mut self, kind: ScopeKind, parent: Option<usize>, func: usize) -> usize {
        let id = self.scopes.len();
        self.scopes.push(Scope::new(kind, parent, func));
        id
    }

    // --- Pass 1: build the scope tree and collect bindings -------------------

    pub fn build_module(&mut self, body: &[Stmt]) -> Result<(), super::CompileError> {
        let m = self.module;
        self.build_scope(m, body, &[]);
        Ok(())
    }

    /// Populate scope `scope_id` from `stmts`, with `predeclared` names (params
    /// or `for` targets) forced into this scope first. Direct-level bindings are
    /// collected before descending into child blocks, so a nested block that
    /// rebinds a name sees the enclosing binding.
    fn build_scope(&mut self, scope_id: usize, stmts: &[Stmt], predeclared: &[String]) {
        // Step 0: for a function/module, gather every `global` declaration in
        // its body (including nested blocks, but not nested defs) before any
        // bindings are collected, so an assignment to a global name is never
        // mistaken for a local — regardless of source order.
        if self.scopes[scope_id].is_function() {
            self.collect_globals(scope_id, stmts);
        }
        for name in predeclared {
            self.declare_here(scope_id, name);
        }
        // Step 1: collect every name bound directly at this level.
        for stmt in stmts {
            self.collect_bindings(scope_id, stmt);
        }
        // Step 2: descend, creating child scopes in source order.
        for stmt in stmts {
            self.build_children(scope_id, stmt);
        }
    }

    /// Register every `global` name found in `stmts` (recursing into `if`/`for`/
    /// `while` bodies but not nested `def`s) onto function scope `func`, and
    /// ensure each is bound at module scope — matching Python, where
    /// `global x; x = 1` creates the module-level `x` if it did not exist.
    fn collect_globals(&mut self, func: usize, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Global { names, .. } => {
                    for name in names {
                        self.scopes[func].globals.insert(name.clone());
                        let m = self.module;
                        self.declare_here(m, name);
                    }
                }
                Stmt::If { body, elifs, orelse, .. } => {
                    self.collect_globals(func, body);
                    for (_, ebody) in elifs {
                        self.collect_globals(func, ebody);
                    }
                    if let Some(ebody) = orelse {
                        self.collect_globals(func, ebody);
                    }
                }
                Stmt::While { body, .. } | Stmt::For { body, .. } => {
                    self.collect_globals(func, body);
                }
                Stmt::Match { cases, .. } => {
                    for case in cases {
                        self.collect_globals(func, &case.body);
                    }
                }
                // A `def`/`class` starts a new function scope with its own
                // `global` declarations — do not descend.
                _ => {}
            }
        }
    }

    fn collect_bindings(&mut self, scope_id: usize, stmt: &Stmt) {
        match stmt {
            Stmt::Assign { targets, .. } => {
                for t in targets {
                    self.collect_target_names(scope_id, t);
                }
            }
            Stmt::AugAssign { target, .. } => self.collect_target_names(scope_id, target),
            Stmt::Def { name, .. } => self.maybe_declare(scope_id, name),
            // For/While/If bind nothing at this level (targets live in the child
            // block). Import/Class/Try/Raise/Yield are handled — or rejected —
            // by codegen; they introduce no reachable bindings here.
            _ => {}
        }
    }

    fn collect_target_names(&mut self, scope_id: usize, target: &Expr) {
        match target {
            Expr::Name { name, .. } => self.maybe_declare(scope_id, name),
            Expr::Tuple { elements, .. } | Expr::List { elements, .. } => {
                for e in elements {
                    self.collect_target_names(scope_id, e);
                }
            }
            // Subscript / attribute targets store into an existing object; they
            // bind no new name.
            _ => {}
        }
    }

    /// Declare `name` in this scope unless it is already visible within the same
    /// function (in which case the assignment rebinds the existing variable).
    fn maybe_declare(&mut self, scope_id: usize, name: &str) {
        // A name declared `global` in this function binds to module scope; an
        // assignment must not shadow it with a local.
        let func = self.scopes[scope_id].func;
        if self.scopes[func].globals.contains(name) {
            return;
        }
        if self.lookup_in_function(scope_id, name).is_some() {
            return;
        }
        self.declare_here(scope_id, name);
    }

    /// Unconditionally declare `name` in this scope (params, `for` targets).
    fn declare_here(&mut self, scope_id: usize, name: &str) {
        if self.scopes[scope_id].decls.contains_key(name) {
            return;
        }
        let owner = self.scopes[scope_id].func;
        let sym = self.symbols.len();
        self.symbols.push(Symbol { owner, captured: false, slot: 0 });
        self.scopes[scope_id].decls.insert(name.to_string(), sym);
        self.scopes[owner].owned.push(sym);
    }

    /// Search from `scope_id` outward, stopping at the enclosing function
    /// boundary (inclusive), for a binding of `name`.
    fn lookup_in_function(&self, scope_id: usize, name: &str) -> Option<usize> {
        let func = self.scopes[scope_id].func;
        let mut cur = Some(scope_id);
        while let Some(c) = cur {
            if let Some(&sym) = self.scopes[c].decls.get(name) {
                return Some(sym);
            }
            if c == func {
                break;
            }
            cur = self.scopes[c].parent;
        }
        None
    }

    fn build_children(&mut self, scope_id: usize, stmt: &Stmt) {
        let func = self.scopes[scope_id].func;
        match stmt {
            Stmt::If { body, elifs, orelse, .. } => {
                self.child_block(scope_id, func, body, &[]);
                for (_, ebody) in elifs {
                    self.child_block(scope_id, func, ebody, &[]);
                }
                if let Some(ebody) = orelse {
                    self.child_block(scope_id, func, ebody, &[]);
                }
            }
            Stmt::While { body, .. } => {
                self.child_block(scope_id, func, body, &[]);
            }
            Stmt::For { target, body, .. } => {
                let mut names = Vec::new();
                target_names(target, &mut names);
                self.child_block(scope_id, func, body, &names);
            }
            Stmt::Def { params, body, .. } => {
                // A function is a new closure boundary.
                let fid = self.new_scope(ScopeKind::Function, Some(scope_id), 0);
                self.scopes[fid].func = fid;
                self.scopes[scope_id].children.push(fid);
                let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
                self.build_scope(fid, body, &param_names);
            }
            Stmt::Match { cases, .. } => {
                // Each `case` body is a block scope, like an `if` body. The
                // subject and patterns bind nothing.
                for case in cases {
                    self.child_block(scope_id, func, &case.body, &[]);
                }
            }
            _ => {}
        }
    }

    fn child_block(&mut self, parent: usize, func: usize, body: &[Stmt], predeclared: &[String]) {
        let bid = self.new_scope(ScopeKind::Block, Some(parent), func);
        self.scopes[parent].children.push(bid);
        self.build_scope(bid, body, predeclared);
    }

    // --- Pass 2: resolve references, discovering captures --------------------

    pub fn resolve_module(&mut self, body: &[Stmt]) {
        let m = self.module;
        let mut cursor = 0usize;
        self.resolve_block(m, body, &mut cursor);
    }

    /// Resolve references in `stmts` belonging to scope `scope_id`, consuming
    /// child scopes from `scope_id`'s children list via `cursor`.
    fn resolve_block(&mut self, scope_id: usize, stmts: &[Stmt], cursor: &mut usize) {
        for stmt in stmts {
            self.resolve_stmt(scope_id, stmt, cursor);
        }
    }

    fn next_child(&self, scope_id: usize, cursor: &mut usize) -> usize {
        let child = self.scopes[scope_id].children[*cursor];
        *cursor += 1;
        child
    }

    fn resolve_stmt(&mut self, scope_id: usize, stmt: &Stmt, cursor: &mut usize) {
        match stmt {
            Stmt::Expr { value, .. } => self.resolve_expr(scope_id, value),
            Stmt::Assign { targets, value, .. } => {
                self.resolve_expr(scope_id, value);
                for t in targets {
                    self.resolve_store_target(scope_id, t);
                }
            }
            Stmt::AugAssign { target, value, .. } => {
                // The target is both read and written.
                self.resolve_expr(scope_id, target);
                self.resolve_expr(scope_id, value);
                self.resolve_store_target(scope_id, target);
            }
            Stmt::If { cond, body, elifs, orelse, .. } => {
                self.resolve_expr(scope_id, cond);
                let child = self.next_child(scope_id, cursor);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
                for (econd, ebody) in elifs {
                    self.resolve_expr(scope_id, econd);
                    let ec = self.next_child(scope_id, cursor);
                    let mut cc = 0;
                    self.resolve_block(ec, ebody, &mut cc);
                }
                if let Some(ebody) = orelse {
                    let elc = self.next_child(scope_id, cursor);
                    let mut cc = 0;
                    self.resolve_block(elc, ebody, &mut cc);
                }
            }
            Stmt::While { cond, body, .. } => {
                self.resolve_expr(scope_id, cond);
                let child = self.next_child(scope_id, cursor);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
            }
            Stmt::For { target, iter, body, .. } => {
                self.resolve_expr(scope_id, iter);
                let child = self.next_child(scope_id, cursor);
                // Store to the loop target resolves in the child block.
                self.resolve_store_target(child, target);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
            }
            Stmt::Def { params, body, .. } => {
                // Defaults are evaluated in the enclosing scope.
                for p in params {
                    if let Some(d) = &p.default {
                        self.resolve_expr(scope_id, d);
                    }
                }
                let child = self.next_child(scope_id, cursor);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
            }
            Stmt::Return { value: Some(v), .. } => self.resolve_expr(scope_id, v),
            Stmt::Match { subject, cases, .. } => {
                self.resolve_expr(scope_id, subject);
                for case in cases {
                    // A dotted pattern references a name (`Cmd` in `Cmd.QUIT`);
                    // literals and `_` reference nothing.
                    if let crate::ast::Pattern::Dotted(expr) = &case.pattern {
                        self.resolve_expr(scope_id, expr);
                    }
                    let child = self.next_child(scope_id, cursor);
                    let mut c = 0;
                    self.resolve_block(child, &case.body, &mut c);
                }
            }
            _ => {}
        }
    }

    fn resolve_store_target(&mut self, scope_id: usize, target: &Expr) {
        match target {
            // A store to a plain name is a reference too: for an ordinary local
            // this is a no-op, but for a `global` name it resolves to the module
            // symbol and must mark it captured and thread the cell as a free var
            // (otherwise a write-only global would never set up its storage).
            Expr::Name { name, .. } => self.reference(scope_id, name),
            Expr::Tuple { elements, .. } | Expr::List { elements, .. } => {
                for e in elements {
                    self.resolve_store_target(scope_id, e);
                }
            }
            Expr::Subscript { value, index, .. } => {
                self.resolve_expr(scope_id, value);
                self.resolve_expr(scope_id, index);
            }
            Expr::Attribute { value, .. } => self.resolve_expr(scope_id, value),
            other => self.resolve_expr(scope_id, other),
        }
    }

    fn resolve_expr(&mut self, scope_id: usize, expr: &Expr) {
        match expr {
            Expr::Name { name, .. } => self.reference(scope_id, name),
            Expr::Unary { operand, .. } => self.resolve_expr(scope_id, operand),
            Expr::Binary { left, right, .. } | Expr::BoolOp { left, right, .. } => {
                self.resolve_expr(scope_id, left);
                self.resolve_expr(scope_id, right);
            }
            Expr::Compare { first, rest, .. } => {
                self.resolve_expr(scope_id, first);
                for (_, e) in rest {
                    self.resolve_expr(scope_id, e);
                }
            }
            Expr::Call { func, args, kwargs, .. } => {
                self.resolve_expr(scope_id, func);
                for a in args {
                    match a {
                        Arg::Positional(e) | Arg::Star(e) => self.resolve_expr(scope_id, e),
                    }
                }
                for k in kwargs {
                    match k {
                        Kwarg::Keyword(_, e) | Kwarg::DoubleStar(e) => {
                            self.resolve_expr(scope_id, e)
                        }
                    }
                }
            }
            Expr::Attribute { value, .. } => self.resolve_expr(scope_id, value),
            Expr::Subscript { value, index, .. } => {
                self.resolve_expr(scope_id, value);
                self.resolve_expr(scope_id, index);
            }
            Expr::Slice { value, lower, upper, step, .. } => {
                self.resolve_expr(scope_id, value);
                for part in [lower, upper, step].into_iter().flatten() {
                    self.resolve_expr(scope_id, part);
                }
            }
            Expr::List { elements, .. }
            | Expr::Tuple { elements, .. }
            | Expr::Set { elements, .. } => {
                for e in elements {
                    self.resolve_expr(scope_id, e);
                }
            }
            Expr::Dict { entries, .. } => {
                for (k, v) in entries {
                    self.resolve_expr(scope_id, k);
                    self.resolve_expr(scope_id, v);
                }
            }
            // Literals reference nothing.
            _ => {}
        }
    }

    /// Record a read reference to `name`, marking a capture when it resolves to
    /// an enclosing function's variable.
    fn reference(&mut self, scope_id: usize, name: &str) {
        let sym = match self.lookup_anywhere(scope_id, name) {
            Some(s) => s,
            None => return, // global / builtin
        };
        let curfunc = self.scopes[scope_id].func;
        let owner = self.symbols[sym].owner;
        if owner == curfunc {
            return; // ordinary local access
        }
        // A free reference: the owner must provide a cell, and every function
        // between the user and the owner must thread it through as a free var.
        self.symbols[sym].captured = true;
        let mut f = curfunc;
        while f != owner {
            if !self.scopes[f].freevars.contains(&sym) {
                self.scopes[f].freevars.push(sym);
            }
            f = self.enclosing_function(f);
        }
    }

    /// Search from `scope_id` outward across function boundaries.
    fn lookup_anywhere(&self, scope_id: usize, name: &str) -> Option<usize> {
        let mut cur = Some(scope_id);
        while let Some(c) = cur {
            if let Some(&sym) = self.scopes[c].decls.get(name) {
                return Some(sym);
            }
            cur = self.scopes[c].parent;
        }
        None
    }

    fn enclosing_function(&self, func: usize) -> usize {
        let parent = self.scopes[func].parent.expect("only the module has no parent function");
        self.scopes[parent].func
    }

    // --- Pass 3: allocate slots ---------------------------------------------

    pub fn allocate(&mut self) {
        for id in 0..self.scopes.len() {
            if !self.scopes[id].is_function() {
                continue;
            }
            let owned = self.scopes[id].owned.clone();
            let mut local_i: u16 = 0;
            let mut cell_i: u16 = 0;
            for sym in owned {
                if self.symbols[sym].captured {
                    self.symbols[sym].slot = cell_i;
                    self.scopes[id].cellvars.push(sym);
                    cell_i += 1;
                } else {
                    self.symbols[sym].slot = local_i;
                    local_i += 1;
                }
            }
            self.scopes[id].nlocals = local_i;
        }
    }

    // --- Queries for codegen -------------------------------------------------

    /// Resolve a name reference within `scope_id` to its storage location.
    pub fn resolve_name(&self, scope_id: usize, name: &str) -> Resolution {
        let sym = match self.lookup_anywhere(scope_id, name) {
            Some(s) => s,
            None => return Resolution::Global,
        };
        let curfunc = self.scopes[scope_id].func;
        let owner = self.symbols[sym].owner;
        if owner == curfunc {
            if self.symbols[sym].captured {
                Resolution::Cell(self.symbols[sym].slot)
            } else {
                Resolution::Local(self.symbols[sym].slot)
            }
        } else {
            let idx = self.scopes[curfunc]
                .freevars
                .iter()
                .position(|&s| s == sym)
                .expect("free variable must be threaded through this function") as u16;
            Resolution::Free(idx)
        }
    }

    /// Local slots of function `func` whose name is also bound at module scope
    /// but was made local by assignment (i.e. not declared `global`). Codegen
    /// stores these on the [`super::CodeObject`] so an unbound-local error can
    /// point the user at `global`. Empty for the module scope itself.
    pub fn shadow_hints(&self, func: usize) -> Vec<(u16, std::rc::Rc<str>)> {
        if func == self.module {
            return Vec::new();
        }
        let mut hints = Vec::new();
        for s in 0..self.scopes.len() {
            if self.scopes[s].func != func {
                continue;
            }
            for (name, &sym) in &self.scopes[s].decls {
                // Only plain (uncaptured) locals reach the LoadFast unbound path,
                // and only names that actually collide with a module binding.
                if self.symbols[sym].owner == func
                    && !self.symbols[sym].captured
                    && self.scopes[self.module].decls.contains_key(name)
                {
                    hints.push((self.symbols[sym].slot, std::rc::Rc::from(name.as_str())));
                }
            }
        }
        hints
    }

    pub fn ncells(&self, func: usize) -> u16 {
        self.scopes[func].cellvars.len() as u16
    }

    pub fn nfree(&self, func: usize) -> u16 {
        self.scopes[func].freevars.len() as u16
    }

    /// The child scopes of `scope_id`, in source order. Codegen consumes these
    /// with its own cursor, in lockstep with the resolve pass.
    pub fn children_of(&self, scope_id: usize) -> &[usize] {
        &self.scopes[scope_id].children
    }

    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }
}

/// Collect the plain names bound by an assignment/`for` target.
pub fn target_names(target: &Expr, out: &mut Vec<String>) {
    match target {
        Expr::Name { name, .. } => out.push(name.clone()),
        Expr::Tuple { elements, .. } | Expr::List { elements, .. } => {
            for e in elements {
                target_names(e, out);
            }
        }
        _ => {}
    }
}
