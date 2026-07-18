//! Static type checker. Machino has no implicit conversions, no undefined
//! behavior, and no dynamic typing: everything an agent writes is either
//! provably well-typed or rejected with a coded, actionable diagnostic.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::HashMap;

pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

pub struct Checker<'a> {
    pub program: &'a Program,
    pub signatures: HashMap<String, Signature>,
    diags: Vec<Diagnostic>,
}

const BUILTINS: &[&str] = &["print", "len", "push", "to_float", "to_int"];

struct Scope {
    vars: Vec<HashMap<String, Type>>,
}

impl Scope {
    fn new() -> Self {
        Scope {
            vars: vec![HashMap::new()],
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
}

impl<'a> Checker<'a> {
    pub fn new(program: &'a Program) -> Self {
        Checker {
            program,
            signatures: HashMap::new(),
            diags: Vec::new(),
        }
    }

    pub fn check(mut self) -> Result<(), Vec<Diagnostic>> {
        // pass 1: collect signatures
        for f in &self.program.functions {
            if BUILTINS.contains(&f.name.as_str()) {
                self.diags.push(
                    Diagnostic::new(
                        "E020",
                        format!("'{}' is a builtin function and cannot be redefined", f.name),
                        f.span,
                    )
                    .with_help("builtins: print, len, push, to_float, to_int"),
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
            if self.signatures.contains_key(&f.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("function '{}' is defined more than once", f.name),
                    f.span,
                ));
                continue;
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
            let ty = self.infer(&c.expr, &scope, None);
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
                let ty = self.infer(&c.expr, &ens_scope, None);
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
            StmtKind::IndexAssign { name, index, value } => {
                let var_ty = scope.lookup(name).cloned();
                match var_ty {
                    Some(Type::Array(elem)) => {
                        if let Some(ity) = self.infer(index, scope, Some(&Type::Int)) {
                            if ity != Type::Int {
                                self.diags.push(Diagnostic::new(
                                    "E033",
                                    format!("array index must be 'int', found '{}'", ity),
                                    index.span,
                                ));
                            }
                        }
                        if let Some(vty) = self.infer(value, scope, Some(&elem)) {
                            if vty != *elem {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: elements of '{}' are '{}' but the value has type '{}'",
                                        name, elem, vty
                                    ),
                                    value.span,
                                ));
                            }
                        }
                    }
                    Some(other) => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("cannot index-assign into '{}' of type '{}'", name, other),
                            stmt.span,
                        ));
                    }
                    None => {
                        self.diags.push(Diagnostic::new(
                            "E035",
                            format!("unknown variable '{}'", name),
                            stmt.span,
                        ));
                    }
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
                self.check_stmts(body, scope, ret, in_test);
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

    /// Infers an expression type. Returns None if an error was already
    /// reported for this subtree (to avoid cascading diagnostics).
    fn infer(&mut self, expr: &Expr, scope: &Scope, expected: Option<&Type>) -> Option<Type> {
        match &expr.kind {
            ExprKind::Int(_) => Some(Type::Int),
            ExprKind::Float(_) => Some(Type::Float),
            ExprKind::Bool(_) => Some(Type::Bool),
            ExprKind::Str(_) => Some(Type::Str),
            ExprKind::Var(name) => match scope.lookup(name) {
                Some(t) => Some(t.clone()),
                None => {
                    let mut d = Diagnostic::new(
                        "E035",
                        format!("unknown variable '{}'", name),
                        expr.span,
                    );
                    if self.signatures.contains_key(name) {
                        d = d.with_help(format!("'{}' is a function; call it: {}(...)", name, name));
                    }
                    self.diags.push(d);
                    None
                }
            },
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
                            .with_help("only arrays [T] can be indexed"),
                        );
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
                        if matches!(lt, Type::Array(_)) {
                            self.diags.push(
                                Diagnostic::new(
                                    "E044",
                                    "arrays cannot be compared with '==' or '!='",
                                    expr.span,
                                )
                                .with_help("compare element-by-element in a loop"),
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
        }
    }

    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
        span: Span,
        scope: &Scope,
        expected: Option<&Type>,
    ) -> Option<Type> {
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
                if matches!(ty, Type::Array(_)) {
                    self.diags.push(
                        Diagnostic::new(
                            "E046",
                            "print does not accept arrays",
                            args[0].span,
                        )
                        .with_help("print elements individually in a loop"),
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
                self.check_conversion(args, span, scope, Type::Int, Type::Float, "to_float")
            }
            "to_int" => {
                self.check_conversion(args, span, scope, Type::Float, Type::Int, "to_int")
            }
            _ => {
                let (params, ret) = match self.signatures.get(name) {
                    Some(sig) => (sig.params.clone(), sig.ret.clone()),
                    None => {
                        self.diags.push(
                            Diagnostic::new(
                                "E047",
                                format!("unknown function '{}'", name),
                                span,
                            )
                            .with_help("builtins: print, len, push, to_float, to_int"),
                        );
                        return None;
                    }
                };
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
                Some(ret)
            }
        }
    }

    fn check_conversion(
        &mut self,
        args: &[Expr],
        span: Span,
        scope: &Scope,
        from: Type,
        to: Type,
        name: &str,
    ) -> Option<Type> {
        if args.len() != 1 {
            self.diags.push(Diagnostic::new(
                "E045",
                format!("{} takes exactly 1 argument, found {}", name, args.len()),
                span,
            ));
            return Some(to);
        }
        if let Some(ty) = self.infer(&args[0], scope, Some(&from)) {
            if ty != from {
                self.diags.push(Diagnostic::new(
                    "E030",
                    format!("{} requires '{}', found '{}'", name, from, ty),
                    args[0].span,
                ));
            }
        }
        Some(to)
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
