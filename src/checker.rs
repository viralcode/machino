//! Static type checker. Machino has no implicit conversions, no undefined
//! behavior, and no dynamic typing: everything an agent writes is either
//! provably well-typed or rejected with a coded, actionable diagnostic.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

pub struct Checker<'a> {
    pub program: &'a Program,
    pub signatures: HashMap<String, Signature>,
    pub structs: HashMap<String, Vec<Param>>,
    diags: Vec<Diagnostic>,
    loop_depth: u32,
}

pub const BUILTINS: &[&str] = &[
    "print", "len", "push", "to_float", "to_int", "char_at", "substr", "chr",
];

struct Scope {
    vars: Vec<HashMap<String, Type>>,
    /// Frame indices where enclosing lambdas begin. Variables declared below
    /// the innermost boundary are captured (read-only inside the lambda).
    boundaries: Vec<usize>,
}

impl Scope {
    fn new() -> Self {
        Scope {
            vars: vec![HashMap::new()],
            boundaries: Vec::new(),
        }
    }
    fn push(&mut self) {
        self.vars.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.vars.pop();
    }
    fn declare(&mut self, name: &str, ty: Type) -> bool {
        self.vars
            .last_mut()
            .unwrap()
            .insert(name.to_string(), ty)
            .is_none()
    }
    fn lookup(&self, name: &str) -> Option<&Type> {
        self.vars.iter().rev().find_map(|m| m.get(name))
    }
    /// True if `name` resolves to a variable declared outside the innermost
    /// enclosing lambda (i.e. it would be captured by value).
    fn is_captured(&self, name: &str) -> bool {
        let Some(&boundary) = self.boundaries.last() else {
            return false;
        };
        for (i, frame) in self.vars.iter().enumerate().rev() {
            if frame.contains_key(name) {
                return i < boundary;
            }
        }
        false
    }
}

impl<'a> Checker<'a> {
    pub fn new(program: &'a Program) -> Self {
        Checker {
            program,
            signatures: HashMap::new(),
            structs: HashMap::new(),
            diags: Vec::new(),
            loop_depth: 0,
        }
    }

    pub fn check(mut self) -> Result<(), Vec<Diagnostic>> {
        // pass 0: collect structs
        for s in &self.program.structs {
            if BUILTINS.contains(&s.name.as_str()) || s.name == "result" || s.name == "memory" {
                self.diags.push(Diagnostic::new(
                    "E020",
                    format!("'{}' is a reserved name and cannot be used for a struct", s.name),
                    s.span,
                ));
                continue;
            }
            if self.structs.contains_key(&s.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("struct '{}' is defined more than once", s.name),
                    s.span,
                ));
                continue;
            }
            if s.fields.len() > 60 {
                self.diags.push(
                    Diagnostic::new(
                        "E050",
                        format!(
                            "struct '{}' has {} fields; the maximum is 60",
                            s.name,
                            s.fields.len()
                        ),
                        s.span,
                    )
                    .with_help("split large structs into nested structs"),
                );
                continue;
            }
            let mut seen: HashMap<&str, ()> = HashMap::new();
            for f in &s.fields {
                if seen.insert(&f.name, ()).is_some() {
                    self.diags.push(Diagnostic::new(
                        "E023",
                        format!("duplicate field '{}' in struct '{}'", f.name, s.name),
                        f.span,
                    ));
                }
            }
            self.structs.insert(s.name.clone(), s.fields.clone());
        }
        // validate field types now that all struct names are known
        for s in &self.program.structs {
            for f in &s.fields {
                self.validate_type(&f.ty, f.span);
            }
        }

        // pass 1: collect function signatures
        let mut first_def: HashMap<String, (Span, bool)> = HashMap::new();
        for f in &self.program.functions {
            if BUILTINS.contains(&f.name.as_str()) {
                self.diags.push(
                    Diagnostic::new(
                        "E020",
                        format!("'{}' is a builtin function and cannot be redefined", f.name),
                        f.span,
                    )
                    .with_help(format!("builtins: {}", BUILTINS.join(", "))),
                );
                continue;
            }
            if f.name == "memory" || f.name == "result" {
                self.diags.push(
                    Diagnostic::new(
                        "E020",
                        format!("'{}' is a reserved name and cannot be used for a function", f.name),
                        f.span,
                    )
                    .with_help("'memory' is the WebAssembly memory export; 'result' is bound in ensures clauses"),
                );
                continue;
            }
            if self.structs.contains_key(&f.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("'{}' is already the name of a struct", f.name),
                    f.span,
                ));
                continue;
            }
            if let Some((orig_span, orig_is_std)) = first_def.get(&f.name).cloned() {
                // report std collisions at the user's definition, not inside the prelude
                if f.is_std && !orig_is_std {
                    self.diags.push(
                        Diagnostic::new(
                            "E021",
                            format!(
                                "'{}' is a machino standard library function and cannot be redefined",
                                f.name
                            ),
                            orig_span,
                        )
                        .with_help("pick a different name; std names are listed in docs/agent-guide.md"),
                    );
                } else {
                    self.diags.push(Diagnostic::new(
                        "E021",
                        format!("function '{}' is defined more than once", f.name),
                        f.span,
                    ));
                }
                continue;
            }
            first_def.insert(f.name.clone(), (f.span, f.is_std));
            for p in &f.params {
                self.validate_type(&p.ty, p.span);
            }
            self.validate_type(&f.ret, f.span);
            if f.is_extern {
                self.validate_extern_types(f);
            }
            self.signatures.insert(
                f.name.clone(),
                Signature {
                    params: f.params.iter().map(|p| p.ty.clone()).collect(),
                    ret: f.ret.clone(),
                },
            );
        }

        // pass 2: check bodies, contracts, tests
        for f in &self.program.functions {
            self.check_function(f);
        }
        let mut test_names: HashMap<&str, ()> = HashMap::new();
        for t in &self.program.tests {
            if test_names.insert(&t.name, ()).is_some() {
                self.diags.push(Diagnostic::new(
                    "E022",
                    format!("duplicate test name \"{}\"", t.name),
                    t.span,
                ));
            }
            let mut scope = Scope::new();
            self.check_stmts(&t.body, &mut scope, &Type::Unit, true);
        }

        if self.diags.is_empty() {
            Ok(())
        } else {
            Err(self.diags)
        }
    }

    fn validate_type(&mut self, ty: &Type, span: Span) {
        match ty {
            Type::Struct(name) => {
                if !self.structs.contains_key(name) {
                    self.diags.push(
                        Diagnostic::new("E018", format!("unknown type '{}'", name), span).with_help(
                            "valid types: int, float, bool, str, [T], fn(T...) -> R, or a declared struct",
                        ),
                    );
                }
            }
            Type::Array(inner) => self.validate_type(inner, span),
            Type::Fn(params, ret) => {
                for p in params {
                    self.validate_type(p, span);
                }
                self.validate_type(ret, span);
            }
            _ => {}
        }
    }

    fn validate_extern_types(&mut self, f: &Function) {
        let ok = |t: &Type| {
            matches!(
                t,
                Type::Int | Type::Float | Type::Bool | Type::Str | Type::Unit
            ) || matches!(t, Type::Array(inner) if matches!(**inner, Type::Int | Type::Float | Type::Bool | Type::Str))
        };
        for p in &f.params {
            if !ok(&p.ty) {
                self.diags.push(
                    Diagnostic::new(
                        "E026",
                        format!("extern parameter type '{}' is not host-transferable", p.ty),
                        p.span,
                    )
                    .with_help("extern signatures may use int, float, bool, str, and arrays of those"),
                );
            }
        }
        if !ok(&f.ret) {
            self.diags.push(Diagnostic::new(
                "E026",
                format!("extern return type '{}' is not host-transferable", f.ret),
                f.span,
            ));
        }
    }

    fn check_function(&mut self, f: &Function) {
        let mut scope = Scope::new();
        for p in &f.params {
            if !scope.declare(&p.name, p.ty.clone()) {
                self.diags.push(Diagnostic::new(
                    "E023",
                    format!("duplicate parameter name '{}'", p.name),
                    p.span,
                ));
            }
        }

        for c in &f.requires {
            let ty = self.infer(&c.expr, &mut scope, None);
            self.expect_bool(ty, c.expr.span, "requires clause");
        }
        {
            // 'result' is in scope for ensures clauses
            let mut ens_scope = Scope::new();
            for p in &f.params {
                ens_scope.declare(&p.name, p.ty.clone());
            }
            if f.ret != Type::Unit {
                ens_scope.declare("result", f.ret.clone());
            } else if !f.ensures.is_empty() {
                self.diags.push(
                    Diagnostic::new(
                        "E024",
                        format!(
                            "function '{}' has an ensures clause but returns nothing",
                            f.name
                        ),
                        f.span,
                    )
                    .with_help("ensures constrains 'result'; add a return type or remove it"),
                );
            }
            for c in &f.ensures {
                let ty = self.infer(&c.expr, &mut ens_scope, None);
                self.expect_bool(ty, c.expr.span, "ensures clause");
            }
        }

        if f.is_extern {
            return;
        }

        self.check_stmts(&f.body, &mut scope, &f.ret, false);

        if f.ret != Type::Unit && !always_returns(&f.body) {
            self.diags.push(
                Diagnostic::new(
                    "E025",
                    format!(
                        "function '{}' returns '{}' but not all paths return a value",
                        f.name, f.ret
                    ),
                    f.span,
                )
                .with_help("add a 'return <expr>' at the end of the function"),
            );
        }
    }

    fn check_stmts(&mut self, stmts: &[Stmt], scope: &mut Scope, ret: &Type, in_test: bool) {
        scope.push();
        for s in stmts {
            self.check_stmt(s, scope, ret, in_test);
        }
        scope.pop();
    }

    fn check_stmt(&mut self, stmt: &Stmt, scope: &mut Scope, ret: &Type, in_test: bool) {
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                if let Some(t) = ty {
                    self.validate_type(t, stmt.span);
                }
                let inferred = self.infer(value, scope, ty.as_ref());
                let final_ty = match (ty, inferred) {
                    (Some(annotated), Some(actual)) => {
                        if annotated != &actual {
                            self.diags.push(
                                Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: '{}' is declared as '{}' but the value has type '{}'",
                                        name, annotated, actual
                                    ),
                                    value.span,
                                ),
                            );
                        }
                        annotated.clone()
                    }
                    (Some(annotated), None) => annotated.clone(),
                    (None, Some(actual)) => actual,
                    (None, None) => Type::Int, // error already reported
                };
                if final_ty == Type::Unit {
                    self.diags.push(
                        Diagnostic::new(
                            "E031",
                            format!("cannot bind '{}' to a unit (no-value) expression", name),
                            value.span,
                        )
                        .with_help("this call returns nothing; call it as a statement instead"),
                    );
                }
                scope.declare(name, final_ty);
            }
            StmtKind::Assign { name, value } => {
                if scope.is_captured(name) {
                    self.diags.push(
                        Diagnostic::new(
                            "E049",
                            format!("cannot assign to captured variable '{}'", name),
                            stmt.span,
                        )
                        .with_help(
                            "lambdas capture by value; to share mutable state, capture a struct or array and mutate its contents",
                        ),
                    );
                }
                let var_ty = scope.lookup(name).cloned();
                match var_ty {
                    Some(t) => {
                        if let Some(actual) = self.infer(value, scope, Some(&t)) {
                            if actual != t {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: '{}' has type '{}' but the value has type '{}'",
                                        name, t, actual
                                    ),
                                    value.span,
                                ));
                            }
                        }
                    }
                    None => {
                        self.diags.push(
                            Diagnostic::new(
                                "E032",
                                format!("assignment to undeclared variable '{}'", name),
                                stmt.span,
                            )
                            .with_help(format!("declare it first: let {} = ...", name)),
                        );
                    }
                }
            }
            StmtKind::IndexAssign { base, index, value } => {
                let base_ty = self.infer(base, scope, None);
                if let Some(ity) = self.infer(index, scope, Some(&Type::Int)) {
                    if ity != Type::Int {
                        self.diags.push(Diagnostic::new(
                            "E033",
                            format!("array index must be 'int', found '{}'", ity),
                            index.span,
                        ));
                    }
                }
                match base_ty {
                    Some(Type::Array(elem)) => {
                        if let Some(vty) = self.infer(value, scope, Some(&elem)) {
                            if vty != *elem {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: array elements are '{}' but the value has type '{}'",
                                        elem, vty
                                    ),
                                    value.span,
                                ));
                            }
                        }
                    }
                    Some(other) => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("cannot index-assign into a value of type '{}'", other),
                            stmt.span,
                        ));
                    }
                    None => {}
                }
            }
            StmtKind::FieldAssign { base, field, value } => {
                let base_ty = self.infer(base, scope, None);
                match base_ty {
                    Some(Type::Struct(sname)) => {
                        match self.field_type(&sname, field) {
                            Some(fty) => {
                                if let Some(vty) = self.infer(value, scope, Some(&fty)) {
                                    if vty != fty {
                                        self.diags.push(Diagnostic::new(
                                            "E030",
                                            format!(
                                                "type mismatch: field '{}.{}' is '{}' but the value has type '{}'",
                                                sname, field, fty, vty
                                            ),
                                            value.span,
                                        ));
                                    }
                                }
                            }
                            None => {
                                self.diags.push(self.no_field(&sname, field, stmt.span));
                            }
                        }
                    }
                    Some(other) => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("type '{}' has no fields to assign", other),
                            stmt.span,
                        ));
                    }
                    None => {}
                }
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let cty = self.infer(cond, scope, Some(&Type::Bool));
                self.expect_bool(cty, cond.span, "if condition");
                self.check_stmts(then_body, scope, ret, in_test);
                self.check_stmts(else_body, scope, ret, in_test);
            }
            StmtKind::While { cond, body } => {
                let cty = self.infer(cond, scope, Some(&Type::Bool));
                self.expect_bool(cty, cond.span, "while condition");
                self.loop_depth += 1;
                self.check_stmts(body, scope, ret, in_test);
                self.loop_depth -= 1;
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                for (e, what) in [(start, "range start"), (end, "range end")] {
                    if let Some(t) = self.infer(e, scope, Some(&Type::Int)) {
                        if t != Type::Int {
                            self.diags.push(Diagnostic::new(
                                "E033",
                                format!("{} must be 'int', found '{}'", what, t),
                                e.span,
                            ));
                        }
                    }
                }
                scope.push();
                scope.declare(var, Type::Int);
                self.loop_depth += 1;
                self.check_stmts(body, scope, ret, in_test);
                self.loop_depth -= 1;
                scope.pop();
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.loop_depth == 0 {
                    let word = if matches!(stmt.kind, StmtKind::Break) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.diags.push(Diagnostic::new(
                        "E027",
                        format!("'{}' outside of a loop", word),
                        stmt.span,
                    ));
                }
            }
            StmtKind::Return(value) => {
                if in_test {
                    self.diags.push(Diagnostic::new(
                        "E036",
                        "'return' is not allowed inside a test block",
                        stmt.span,
                    ));
                    return;
                }
                match (value, ret) {
                    (None, Type::Unit) => {}
                    (None, expected) => {
                        self.diags.push(Diagnostic::new(
                            "E037",
                            format!("this function must return a value of type '{}'", expected),
                            stmt.span,
                        ));
                    }
                    (Some(e), expected) => {
                        if *expected == Type::Unit {
                            self.diags.push(
                                Diagnostic::new(
                                    "E038",
                                    "this function has no return type but returns a value",
                                    e.span,
                                )
                                .with_help("add '-> <type>' to the function signature"),
                            );
                        } else if let Some(actual) = self.infer(e, scope, Some(expected)) {
                            if actual != *expected {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: function returns '{}' but this value has type '{}'",
                                        expected, actual
                                    ),
                                    e.span,
                                ));
                            }
                        }
                    }
                }
            }
            StmtKind::Assert(expr) => {
                let ty = self.infer(expr, scope, Some(&Type::Bool));
                self.expect_bool(ty, expr.span, "assert");
            }
            StmtKind::Expr(expr) => {
                self.infer(expr, scope, None);
            }
        }
    }

    fn expect_bool(&mut self, ty: Option<Type>, span: Span, what: &str) {
        if let Some(t) = ty {
            if t != Type::Bool {
                self.diags.push(Diagnostic::new(
                    "E039",
                    format!("{} must be 'bool', found '{}'", what, t),
                    span,
                ));
            }
        }
    }

    fn field_type(&self, struct_name: &str, field: &str) -> Option<Type> {
        self.structs
            .get(struct_name)?
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.ty.clone())
    }

    fn no_field(&self, struct_name: &str, field: &str, span: Span) -> Diagnostic {
        let fields = self
            .structs
            .get(struct_name)
            .map(|fs| {
                fs.iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        Diagnostic::new(
            "E048",
            format!("struct '{}' has no field '{}'", struct_name, field),
            span,
        )
        .with_help(format!("fields of {}: {}", struct_name, fields))
    }

    /// Infers an expression type. Returns None if an error was already
    /// reported for this subtree (to avoid cascading diagnostics).
    fn infer(&mut self, expr: &Expr, scope: &mut Scope, expected: Option<&Type>) -> Option<Type> {
        match &expr.kind {
            ExprKind::Int(_) => Some(Type::Int),
            ExprKind::Float(_) => Some(Type::Float),
            ExprKind::Bool(_) => Some(Type::Bool),
            ExprKind::Str(_) => Some(Type::Str),
            ExprKind::Var(name) => {
                if let Some(t) = scope.lookup(name) {
                    return Some(t.clone());
                }
                // a bare function name is a first-class function value
                if let Some(sig) = self.signatures.get(name) {
                    return Some(Type::Fn(sig.params.clone(), Box::new(sig.ret.clone())));
                }
                let mut d =
                    Diagnostic::new("E035", format!("unknown variable '{}'", name), expr.span);
                if self.structs.contains_key(name) {
                    d = d.with_help(format!(
                        "'{}' is a struct; construct it: {}(...)",
                        name, name
                    ));
                }
                self.diags.push(d);
                None
            }
            ExprKind::Array(elems) => {
                let expected_elem = match expected {
                    Some(Type::Array(e)) => Some(e.as_ref()),
                    _ => None,
                };
                if elems.is_empty() {
                    return match expected_elem {
                        Some(e) => Some(Type::Array(Box::new(e.clone()))),
                        None => {
                            self.diags.push(
                                Diagnostic::new(
                                    "E040",
                                    "cannot infer the element type of an empty array literal",
                                    expr.span,
                                )
                                .with_help("annotate it: let xs: [int] = []"),
                            );
                            None
                        }
                    };
                }
                let first = self.infer(&elems[0], scope, expected_elem)?;
                for e in &elems[1..] {
                    if let Some(t) = self.infer(e, scope, Some(&first)) {
                        if t != first {
                            self.diags.push(Diagnostic::new(
                                "E041",
                                format!(
                                    "array elements must all have the same type: first is '{}', this is '{}'",
                                    first, t
                                ),
                                e.span,
                            ));
                        }
                    }
                }
                Some(Type::Array(Box::new(first)))
            }
            ExprKind::Index(base, index) => {
                let base_ty = self.infer(base, scope, None)?;
                if let Some(ity) = self.infer(index, scope, Some(&Type::Int)) {
                    if ity != Type::Int {
                        self.diags.push(Diagnostic::new(
                            "E033",
                            format!("array index must be 'int', found '{}'", ity),
                            index.span,
                        ));
                    }
                }
                match base_ty {
                    Type::Array(elem) => Some(*elem),
                    other => {
                        self.diags.push(
                            Diagnostic::new(
                                "E034",
                                format!("cannot index into a value of type '{}'", other),
                                base.span,
                            )
                            .with_help("only arrays [T] can be indexed; use char_at(s, i) for strings"),
                        );
                        None
                    }
                }
            }
            ExprKind::Field(base, field) => {
                let base_ty = self.infer(base, scope, None)?;
                match base_ty {
                    Type::Struct(sname) => match self.field_type(&sname, field) {
                        Some(t) => Some(t),
                        None => {
                            self.diags.push(self.no_field(&sname, field, expr.span));
                            None
                        }
                    },
                    other => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("type '{}' has no fields", other),
                            expr.span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Un(op, inner) => {
                let ty = self.infer(inner, scope, None)?;
                match op {
                    UnOp::Neg => match ty {
                        Type::Int | Type::Float => Some(ty),
                        other => {
                            self.diags.push(Diagnostic::new(
                                "E042",
                                format!("unary '-' requires 'int' or 'float', found '{}'", other),
                                expr.span,
                            ));
                            None
                        }
                    },
                    UnOp::Not => {
                        if ty != Type::Bool {
                            self.diags.push(Diagnostic::new(
                                "E042",
                                format!("'!' requires 'bool', found '{}'", ty),
                                expr.span,
                            ));
                            return None;
                        }
                        Some(Type::Bool)
                    }
                }
            }
            ExprKind::Bin(op, lhs, rhs) => {
                let lt = self.infer(lhs, scope, None)?;
                let rt = self.infer(rhs, scope, Some(&lt))?;
                use BinOp::*;
                match op {
                    Add => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        (Type::Float, Type::Float) => Some(Type::Float),
                        (Type::Str, Type::Str) => Some(Type::Str),
                        _ => {
                            self.diags.push(self.numeric_mismatch("+", &lt, &rt, expr.span));
                            None
                        }
                    },
                    Sub | Mul | Div => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        (Type::Float, Type::Float) => Some(Type::Float),
                        _ => {
                            self.diags
                                .push(self.numeric_mismatch(op_name(*op), &lt, &rt, expr.span));
                            None
                        }
                    },
                    Mod => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        _ => {
                            self.diags.push(
                                Diagnostic::new(
                                    "E043",
                                    format!("'%' requires int operands, found '{}' and '{}'", lt, rt),
                                    expr.span,
                                ),
                            );
                            None
                        }
                    },
                    Lt | Le | Gt | Ge => match (&lt, &rt) {
                        (Type::Int, Type::Int) | (Type::Float, Type::Float) => Some(Type::Bool),
                        _ => {
                            self.diags
                                .push(self.numeric_mismatch(op_name(*op), &lt, &rt, expr.span));
                            None
                        }
                    },
                    Eq | Ne => {
                        if lt != rt {
                            self.diags.push(Diagnostic::new(
                                "E043",
                                format!(
                                    "cannot compare '{}' with '{}': operand types must match",
                                    lt, rt
                                ),
                                expr.span,
                            ));
                            return None;
                        }
                        if matches!(lt, Type::Array(_) | Type::Struct(_) | Type::Fn(_, _)) {
                            self.diags.push(
                                Diagnostic::new(
                                    "E044",
                                    format!("values of type '{}' cannot be compared with '==' or '!='", lt),
                                    expr.span,
                                )
                                .with_help("compare field-by-field or element-by-element"),
                            );
                            return None;
                        }
                        Some(Type::Bool)
                    }
                    And | Or => {
                        if lt != Type::Bool || rt != Type::Bool {
                            self.diags.push(Diagnostic::new(
                                "E043",
                                format!(
                                    "'{}' requires bool operands, found '{}' and '{}'",
                                    op_name(*op),
                                    lt,
                                    rt
                                ),
                                expr.span,
                            ));
                            return None;
                        }
                        Some(Type::Bool)
                    }
                }
            }
            ExprKind::Call(name, args) => self.check_call(name, args, expr.span, scope, expected),
            ExprKind::Lambda(lambda) => {
                for p in &lambda.params {
                    self.validate_type(&p.ty, p.span);
                }
                self.validate_type(&lambda.ret, expr.span);
                // the body is a new function scope: it can read (capture)
                // enclosing variables but not reassign them, and break /
                // continue / return refer to the lambda itself
                scope.push();
                scope.boundaries.push(scope.vars.len() - 1);
                let mut seen: HashMap<String, ()> = HashMap::new();
                for p in &lambda.params {
                    if seen.insert(p.name.clone(), ()).is_some() {
                        self.diags.push(Diagnostic::new(
                            "E023",
                            format!("duplicate parameter name '{}'", p.name),
                            p.span,
                        ));
                    }
                    scope.declare(&p.name, p.ty.clone());
                }
                let saved_loop = self.loop_depth;
                self.loop_depth = 0;
                self.check_stmts(&lambda.body, scope, &lambda.ret.clone(), false);
                self.loop_depth = saved_loop;
                if lambda.ret != Type::Unit && !always_returns(&lambda.body) {
                    self.diags.push(
                        Diagnostic::new(
                            "E025",
                            format!(
                                "lambda returns '{}' but not all paths return a value",
                                lambda.ret
                            ),
                            expr.span,
                        )
                        .with_help("add a 'return <expr>' at the end of the lambda body"),
                    );
                }
                scope.boundaries.pop();
                scope.pop();
                Some(Type::Fn(
                    lambda.params.iter().map(|p| p.ty.clone()).collect(),
                    Box::new(lambda.ret.clone()),
                ))
            }
        }
    }

    fn check_args(&mut self, name: &str, params: &[Type], args: &[Expr], span: Span, scope: &mut Scope) {
        if args.len() != params.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "'{}' takes {} argument(s), found {}",
                    name,
                    params.len(),
                    args.len()
                ),
                span,
            ));
        }
        for (i, arg) in args.iter().enumerate() {
            if let Some(param_ty) = params.get(i) {
                if let Some(actual) = self.infer(arg, scope, Some(param_ty)) {
                    if actual != *param_ty {
                        self.diags.push(Diagnostic::new(
                            "E030",
                            format!(
                                "type mismatch: argument {} of '{}' expects '{}', found '{}'",
                                i + 1,
                                name,
                                param_ty,
                                actual
                            ),
                            arg.span,
                        ));
                    }
                }
            } else {
                self.infer(arg, scope, None);
            }
        }
    }

    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
        expected: Option<&Type>,
    ) -> Option<Type> {
        // 1. a local variable holding a function value shadows everything
        if let Some(Type::Fn(params, ret)) = scope.lookup(name).cloned() {
            self.check_args(name, &params, args, span, scope);
            return Some(*ret);
        }
        match name {
            "print" => {
                if args.len() != 1 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("print takes exactly 1 argument, found {}", args.len()),
                        span,
                    ));
                    return Some(Type::Unit);
                }
                let ty = self.infer(&args[0], scope, None)?;
                if matches!(ty, Type::Array(_) | Type::Struct(_) | Type::Fn(_, _)) {
                    self.diags.push(
                        Diagnostic::new(
                            "E046",
                            format!("print does not accept values of type '{}'", ty),
                            args[0].span,
                        )
                        .with_help("print scalar fields/elements, or build a str with str_of_int and '+'"),
                    );
                }
                Some(Type::Unit)
            }
            "len" => {
                if args.len() != 1 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("len takes exactly 1 argument, found {}", args.len()),
                        span,
                    ));
                    return Some(Type::Int);
                }
                let ty = self.infer(&args[0], scope, None)?;
                match ty {
                    Type::Array(_) | Type::Str => Some(Type::Int),
                    other => {
                        self.diags.push(Diagnostic::new(
                            "E046",
                            format!("len requires an array or str, found '{}'", other),
                            args[0].span,
                        ));
                        Some(Type::Int)
                    }
                }
            }
            "push" => {
                if args.len() != 2 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("push takes exactly 2 arguments, found {}", args.len()),
                        span,
                    ));
                    return None;
                }
                let arr_ty = self.infer(&args[0], scope, expected)?;
                match arr_ty {
                    Type::Array(elem) => {
                        if let Some(vty) = self.infer(&args[1], scope, Some(&elem)) {
                            if vty != *elem {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: pushing '{}' into an array of '{}'",
                                        vty, elem
                                    ),
                                    args[1].span,
                                ));
                            }
                        }
                        Some(Type::Array(elem))
                    }
                    other => {
                        self.diags.push(
                            Diagnostic::new(
                                "E046",
                                format!("push requires an array as its first argument, found '{}'", other),
                                args[0].span,
                            )
                            .with_help("push returns a new array: let xs2 = push(xs, v)"),
                        );
                        None
                    }
                }
            }
            "to_float" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Float, "to_float")
            }
            "to_int" => {
                self.check_builtin_sig(args, span, scope, &[Type::Float], Type::Int, "to_int")
            }
            "char_at" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int],
                Type::Int,
                "char_at",
            ),
            "substr" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int, Type::Int],
                Type::Str,
                "substr",
            ),
            "chr" => self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Str, "chr"),
            _ => {
                // 2. struct constructor
                if let Some(fields) = self.structs.get(name).cloned() {
                    let params: Vec<Type> = fields.iter().map(|f| f.ty.clone()).collect();
                    self.check_args(name, &params, args, span, scope);
                    return Some(Type::Struct(name.to_string()));
                }
                // 3. named function or extern
                let (params, ret) = match self.signatures.get(name) {
                    Some(sig) => (sig.params.clone(), sig.ret.clone()),
                    None => {
                        self.diags.push(
                            Diagnostic::new(
                                "E047",
                                format!("unknown function '{}'", name),
                                span,
                            )
                            .with_help(format!("builtins: {}", BUILTINS.join(", "))),
                        );
                        return None;
                    }
                };
                self.check_args(name, &params, args, span, scope);
                Some(ret)
            }
        }
    }

    fn check_builtin_sig(
        &mut self,
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
        params: &[Type],
        ret: Type,
        name: &str,
    ) -> Option<Type> {
        if args.len() != params.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "{} takes exactly {} argument(s), found {}",
                    name,
                    params.len(),
                    args.len()
                ),
                span,
            ));
            return Some(ret);
        }
        for (arg, want) in args.iter().zip(params) {
            if let Some(ty) = self.infer(arg, scope, Some(want)) {
                if &ty != want {
                    self.diags.push(Diagnostic::new(
                        "E030",
                        format!("{} requires '{}', found '{}'", name, want, ty),
                        arg.span,
                    ));
                }
            }
        }
        Some(ret)
    }

    fn numeric_mismatch(&self, op: &str, lt: &Type, rt: &Type, span: Span) -> Diagnostic {
        let mut d = Diagnostic::new(
            "E043",
            format!(
                "'{}' cannot be applied to '{}' and '{}'",
                op, lt, rt
            ),
            span,
        );
        if (lt == &Type::Int && rt == &Type::Float) || (lt == &Type::Float && rt == &Type::Int) {
            d = d.with_help(
                "machino has no implicit numeric conversion; use to_float(x) or to_int(x)",
            );
        }
        d
    }
}

fn op_name(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
    }
}

pub fn always_returns(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(_) => return true,
            StmtKind::If {
                then_body,
                else_body,
                ..
            } => {
                if !else_body.is_empty()
                    && always_returns(then_body)
                    && always_returns(else_body)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}
