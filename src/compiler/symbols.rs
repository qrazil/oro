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

use crate::ast::{Expr, Stmt};

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
        let mut t = SymTable {
            scopes: Vec::new(),
            symbols: Vec::new(),
            module: 0,
        };
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
                Stmt::If {
                    body,
                    elifs,
                    orelse,
                    ..
                } => {
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
                Stmt::Try {
                    body,
                    handlers,
                    finalbody,
                    ..
                } => {
                    self.collect_globals(func, body);
                    for h in handlers {
                        self.collect_globals(func, &h.body);
                    }
                    if let Some(fb) = finalbody {
                        self.collect_globals(func, fb);
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
            // A class binds its own name in the enclosing scope; its methods are
            // not names here (they are reached via the class or an instance).
            Stmt::Class { name, .. } => self.maybe_declare(scope_id, name),
            // `import a.b.c` / `import x as y` binds one name (the alias, else
            // the last path segment — Go-style).
            Stmt::Import { path, alias, .. } => {
                if let Some(bound) = import_bound_name(path, alias) {
                    self.maybe_declare(scope_id, bound);
                }
            }
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
        self.symbols.push(Symbol {
            owner,
            captured: false,
            slot: 0,
        });
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
            Stmt::If {
                body,
                elifs,
                orelse,
                ..
            } => {
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
                self.build_function_scope(scope_id, params, body);
            }
            Stmt::Class { body, .. } => {
                // Each method is a function scope of the *enclosing* scope, not
                // of the class — Python method bodies do not see class-body
                // names as free variables. Class-level attributes bind nothing.
                for member in body {
                    if let Stmt::Def { params, body, .. } = member {
                        self.build_function_scope(scope_id, params, body);
                    }
                }
            }
            Stmt::Match { cases, .. } => {
                // Each `case` body is a block scope, like an `if` body. The
                // subject and patterns bind nothing.
                for case in cases {
                    self.child_block(scope_id, func, &case.body, &[]);
                }
            }
            Stmt::Try {
                body,
                handlers,
                finalbody,
                ..
            } => {
                // try body, each handler body (with its `as e` predeclared),
                // then the finally body — each a block scope, in this order.
                self.child_block(scope_id, func, body, &[]);
                for h in handlers {
                    let pre: Vec<String> = h.name.iter().cloned().collect();
                    self.child_block(scope_id, func, &h.body, &pre);
                }
                if let Some(fb) = finalbody {
                    self.child_block(scope_id, func, fb, &[]);
                }
            }
            _ => {}
        }
    }

    /// Create a `Function` scope (a closure boundary) for a `def`/method body
    /// as a child of `scope_id`, with its parameters predeclared.
    fn build_function_scope(
        &mut self,
        scope_id: usize,
        params: &[crate::ast::Param],
        body: &[Stmt],
    ) {
        let fid = self.new_scope(ScopeKind::Function, Some(scope_id), 0);
        self.scopes[fid].func = fid;
        self.scopes[scope_id].children.push(fid);
        let param_names: Vec<String> = params.iter().map(|p| p.name.clone()).collect();
        self.build_scope(fid, body, &param_names);
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
            Stmt::If {
                cond,
                body,
                elifs,
                orelse,
                ..
            } => {
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
            Stmt::For {
                target, iter, body, ..
            } => {
                self.resolve_expr(scope_id, iter);
                let child = self.next_child(scope_id, cursor);
                // Store to the loop target resolves in the child block.
                self.resolve_store_target(child, target);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
            }
            Stmt::Def { params, body, .. } => {
                self.resolve_function(scope_id, params, body, cursor);
            }
            Stmt::Class { base, body, .. } => {
                // The base and any class-attribute values are evaluated in the
                // enclosing scope; each method resolves in its own child scope.
                if let Some(b) = base {
                    self.resolve_expr(scope_id, b);
                }
                for member in body {
                    match member {
                        Stmt::Def { params, body, .. } => {
                            self.resolve_function(scope_id, params, body, cursor);
                        }
                        Stmt::Assign { value, .. } => self.resolve_expr(scope_id, value),
                        _ => {}
                    }
                }
            }
            Stmt::Return { value: Some(v), .. } => self.resolve_expr(scope_id, v),
            Stmt::Raise { exc: Some(e), .. } => self.resolve_expr(scope_id, e),
            Stmt::Yield { value: Some(v), .. } => self.resolve_expr(scope_id, v),
            Stmt::Try {
                body,
                handlers,
                finalbody,
                ..
            } => {
                let child = self.next_child(scope_id, cursor);
                let mut c = 0;
                self.resolve_block(child, body, &mut c);
                for h in handlers {
                    // The exception type is evaluated in the enclosing scope.
                    self.resolve_expr(scope_id, &h.exc_type);
                    let hchild = self.next_child(scope_id, cursor);
                    // The `as e` binding stores into the handler block scope.
                    if let Some(name) = &h.name {
                        self.reference(hchild, name);
                    }
                    let mut hc = 0;
                    self.resolve_block(hchild, &h.body, &mut hc);
                }
                if finalbody.is_some() {
                    let fchild = self.next_child(scope_id, cursor);
                    let mut fc = 0;
                    self.resolve_block(fchild, finalbody.as_ref().unwrap(), &mut fc);
                }
            }
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

    /// Resolve a `def`/method in the next child scope: its default expressions
    /// first, then its body.
    ///
    /// The defaults resolve in the **child** scope, not the enclosing one,
    /// because that is where they now run: a default is evaluated in the
    /// callee's frame, on each call that omits the argument. So a name in a
    /// default is captured the way a name in the body is — and it reads the
    /// value that name has when the call happens, not the one it had when the
    /// `def` ran.
    fn resolve_function(
        &mut self,
        scope_id: usize,
        params: &[crate::ast::Param],
        body: &[Stmt],
        cursor: &mut usize,
    ) {
        let child = self.next_child(scope_id, cursor);
        for p in params {
            if let Some(d) = &p.default {
                self.resolve_expr(child, d);
            }
        }
        let mut c = 0;
        self.resolve_block(child, body, &mut c);
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
            // A lambda body is a single expression, so it can bind nothing — no
            // walrus, no comprehensions. That means its scope needs no build
            // pass: creating it here, where every expression is already visited,
            // is both complete and simpler. It is deliberately NOT pushed into
            // the parent's `children`, so the def/block cursor is unaffected.
            Expr::Lambda { data, .. } => {
                let fid = self.new_scope(ScopeKind::Function, Some(scope_id), 0);
                self.scopes[fid].func = fid;
                for p in &data.params {
                    self.declare_here(fid, &p.name);
                }
                data.scope.set(fid);
                self.resolve_expr(fid, &data.body);
            }
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
            Expr::Ternary {
                cond, then, orelse, ..
            } => {
                self.resolve_expr(scope_id, cond);
                self.resolve_expr(scope_id, then);
                self.resolve_expr(scope_id, orelse);
            }
            Expr::Call {
                func, args, kwargs, ..
            } => {
                self.resolve_expr(scope_id, func);
                for a in args {
                    self.resolve_expr(scope_id, a);
                }
                for (_, e) in kwargs {
                    self.resolve_expr(scope_id, e);
                }
            }
            Expr::Attribute { value, .. } => self.resolve_expr(scope_id, value),
            Expr::Subscript { value, index, .. } => {
                self.resolve_expr(scope_id, value);
                self.resolve_expr(scope_id, index);
            }
            Expr::Slice {
                value,
                lower,
                upper,
                step,
                ..
            } => {
                self.resolve_expr(scope_id, value);
                for part in [lower, upper, step].into_iter().flatten() {
                    self.resolve_expr(scope_id, part);
                }
            }
            Expr::List { elements, .. } | Expr::Tuple { elements, .. } => {
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
            // An f-string's fields are ordinary expressions, and a name used
            // *only* inside one is an ordinary reference — but the parser
            // leaves the field text unparsed (see "Known limitations"), so
            // there is nothing in the AST to walk. Parse the fields exactly as
            // codegen will and resolve what comes back. Skipping this used to
            // leave codegen resolving a free variable the pass had never
            // threaded, which aborted the process with no source location.
            Expr::FString { value, .. } => {
                for field in super::codegen::fstring_field_exprs(value) {
                    self.resolve_expr(scope_id, &field);
                }
            }
            // Literals reference nothing. Listed rather than caught by `_` so
            // that a new expression kind cannot join the language without this
            // walk being updated — an unwalked name is exactly the bug above.
            Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::Str { .. }
            | Expr::Bytes { .. }
            | Expr::Bool { .. }
            | Expr::NoneLit { .. } => {}
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
        let parent = self.scopes[func]
            .parent
            .expect("only the module has no parent function");
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
    ///
    /// `None` means the reference is to an enclosing function's variable that
    /// [`resolve_module`](Self::resolve_module) never threaded through this
    /// function — a hole in the resolve walk, not a mistake in the program.
    /// Codegen turns it into a diagnostic carrying the source location rather
    /// than aborting, so a walk that misses a name is a report, never a crash.
    pub fn resolve_name(&self, scope_id: usize, name: &str) -> Option<Resolution> {
        let sym = match self.lookup_anywhere(scope_id, name) {
            Some(s) => s,
            None => return Some(Resolution::Global),
        };
        let curfunc = self.scopes[scope_id].func;
        let owner = self.symbols[sym].owner;
        if owner == curfunc {
            if self.symbols[sym].captured {
                Some(Resolution::Cell(self.symbols[sym].slot))
            } else {
                Some(Resolution::Local(self.symbols[sym].slot))
            }
        } else {
            let idx = self.scopes[curfunc]
                .freevars
                .iter()
                .position(|&s| s == sym)? as u16;
            Some(Resolution::Free(idx))
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

    /// The module scope's own bound names and where each is stored, so an
    /// `import` can read the module's namespace after its body runs.
    pub fn module_member_targets(&self) -> Vec<(std::rc::Rc<str>, super::VarTarget)> {
        let m = self.module;
        let mut out = Vec::new();
        for (name, &sym) in &self.scopes[m].decls {
            if self.symbols[sym].owner != m {
                continue;
            }
            let target = if self.symbols[sym].captured {
                super::VarTarget::Cell(self.symbols[sym].slot)
            } else {
                super::VarTarget::Local(self.symbols[sym].slot)
            };
            out.push((std::rc::Rc::from(name.as_str()), target));
        }
        out
    }

    pub fn module(&self) -> usize {
        self.module
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

/// The single name an `import` binds: its alias, or the last path segment.
pub fn import_bound_name<'a>(path: &'a [String], alias: &'a Option<String>) -> Option<&'a str> {
    alias.as_deref().or_else(|| path.last().map(|s| s.as_str()))
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
