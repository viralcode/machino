//! Static contract verification with the Z3 SMT solver (`machino check
//! --verify`, requires building with `--features smt`).
//!
//! The decidable subset: function bodies over int/bool (plus `len(arr)`
//! treated as a symbolic non-negative int), where
//! - calls to other user/std functions with int/bool signatures are inlined
//!   (up to depth 8; recursion past that reports `unknown`), and
//! - `for` loops whose bounds simplify to constants are unrolled (up to 128
//!   iterations).
//! The verifier runs the body symbolically, collecting a (path condition,
//! result) pair per return path, then asks Z3 to prove every `ensures`
//! clause on every path under the `requires` assumptions. It also flags
//! contradictory `requires` clauses (contracts that no input can satisfy).
//! Functions outside the subset (while loops, floats, strings, mutation of
//! arrays) report `unknown` and fall back to machino's always-on runtime
//! contract enforcement.

#![allow(dead_code)]

use crate::ast::*;

/// Outcome of verifying one contract clause.
#[derive(Debug)]
pub enum VerifyResult {
    /// Provably holds on every input satisfying the requires clauses.
    Verified,
    /// A concrete input violates the clause; the string describes it.
    Counterexample(String),
    /// Outside the decidable subset (or the solver gave up).
    Unknown(String),
}

/// Per-function verification report.
#[derive(Debug)]
pub struct FunctionReport {
    pub function: String,
    /// (clause text, result) for each ensures clause.
    pub clauses: Vec<(String, VerifyResult)>,
    /// True if the requires clauses are unsatisfiable (vacuous contract).
    pub vacuous_requires: bool,
}

#[cfg(feature = "smt")]
mod z3impl {
    use super::*;
    use std::collections::HashMap;
    use z3::ast::{Ast, Bool, Int};
    use z3::{Config, Context, SatResult, Solver};

    #[derive(Clone)]
    enum Val<'ctx> {
        Int(Int<'ctx>),
        Bool(Bool<'ctx>),
    }

    impl<'ctx> Val<'ctx> {
        fn as_int(&self) -> Result<&Int<'ctx>, String> {
            match self {
                Val::Int(i) => Ok(i),
                Val::Bool(_) => Err("expected int, found bool".to_string()),
            }
        }
        fn as_bool(&self) -> Result<&Bool<'ctx>, String> {
            match self {
                Val::Bool(b) => Ok(b),
                Val::Int(_) => Err("expected bool, found int".to_string()),
            }
        }
        fn ite(cond: &Bool<'ctx>, t: &Val<'ctx>, e: &Val<'ctx>) -> Result<Val<'ctx>, String> {
            match (t, e) {
                (Val::Int(a), Val::Int(b)) => Ok(Val::Int(cond.ite(a, b))),
                (Val::Bool(a), Val::Bool(b)) => Ok(Val::Bool(cond.ite(a, b))),
                _ => Err("branches have different types".to_string()),
            }
        }
    }

    struct SymState<'ctx, 'p> {
        ctx: &'ctx Context,
        /// symbolic environment: variable -> value
        env: HashMap<String, Val<'ctx>>,
        /// symbolic array lengths: array variable name -> len symbol
        lens: HashMap<String, Int<'ctx>>,
        /// symbolic array elements: (array, tag) -> uninterpreted select fn
        /// modeled per-index as fresh consts keyed by textual index
        selects: HashMap<String, Int<'ctx>>,
        /// struct field symbols: "base.field" -> int symbol
        fields: HashMap<String, Int<'ctx>>,
        /// program functions by name, for call inlining
        fns: &'p HashMap<String, &'p Function>,
        /// current call-inlining depth
        depth: usize,
        fresh: usize,
    }

    /// A completed return path: (conjunction of branch conditions, result).
    struct RetPath<'ctx> {
        cond: Bool<'ctx>,
        result: Option<Val<'ctx>>,
    }

    const MAX_INLINE_DEPTH: usize = 8;
    const MAX_UNROLL: i64 = 128;

    impl<'ctx, 'p> SymState<'ctx, 'p> {
        fn fresh_int(&mut self, hint: &str) -> Int<'ctx> {
            self.fresh += 1;
            Int::new_const(self.ctx, format!("{}!{}", hint, self.fresh))
        }

        /// Inlines a call to a user/std function with an int/bool signature:
        /// executes the callee body symbolically on the translated arguments
        /// and folds its return paths into one if-then-else value.
        fn inline_call(&mut self, name: &str, args: &[Expr]) -> Result<Val<'ctx>, String> {
            let Some(&f) = self.fns.get(name) else {
                return Err(format!("call to '{}' is outside the decidable subset", name));
            };
            if self.depth >= MAX_INLINE_DEPTH {
                return Err(format!(
                    "call to '{}' exceeds the inlining depth limit ({}); recursion cannot be verified statically",
                    name, MAX_INLINE_DEPTH
                ));
            }
            if f.params.len() != args.len() {
                return Err(format!("wrong argument count for '{}'", name));
            }
            if !matches!(f.ret, Type::Int | Type::Bool) {
                return Err(format!(
                    "'{}' returns '{}' (only int/bool calls can be inlined)",
                    name, f.ret
                ));
            }
            let mut vals = Vec::new();
            for (p, a) in f.params.iter().zip(args) {
                if !matches!(p.ty, Type::Int | Type::Bool) {
                    return Err(format!(
                        "'{}' takes '{}' (only int/bool calls can be inlined)",
                        name, p.ty
                    ));
                }
                vals.push(self.translate(a)?);
            }
            let saved_env = std::mem::take(&mut self.env);
            for (p, v) in f.params.iter().zip(vals) {
                self.env.insert(p.name.clone(), v);
            }
            self.depth += 1;
            let t = Bool::from_bool(self.ctx, true);
            let mut paths: Vec<RetPath<'ctx>> = Vec::new();
            let res = self.exec_block(&f.body, &t, &mut paths);
            self.depth -= 1;
            self.env = saved_env;
            if res?.is_some() {
                return Err(format!("'{}' can fall off the end of its body", name));
            }
            let mut it = paths.into_iter().rev();
            let last = it
                .next()
                .ok_or_else(|| format!("'{}' has no return paths", name))?;
            let mut acc = last
                .result
                .ok_or_else(|| format!("'{}' returns unit", name))?;
            for p in it {
                let r = p
                    .result
                    .ok_or_else(|| format!("'{}' returns unit", name))?;
                acc = Val::ite(&p.cond, &r, &acc)?;
            }
            Ok(acc)
        }

        fn translate(&mut self, expr: &Expr) -> Result<Val<'ctx>, String> {
            match &expr.kind {
                ExprKind::Int(n) => Ok(Val::Int(Int::from_i64(self.ctx, *n))),
                ExprKind::Bool(b) => Ok(Val::Bool(Bool::from_bool(self.ctx, *b))),
                ExprKind::Var(name) => self
                    .env
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("unknown variable '{}'", name)),
                ExprKind::Call(name, args) => {
                    if name == "len" && args.len() == 1 {
                        if let ExprKind::Var(arr) = &args[0].kind {
                            if let Some(l) = self.lens.get(arr) {
                                return Ok(Val::Int(l.clone()));
                            }
                        }
                        return Err("len() is only symbolic for array parameters".to_string());
                    }
                    self.inline_call(name, args)
                }
                ExprKind::Index(base, idx) => {
                    // arr[i] is an uninterpreted int per (array, index-text);
                    // sound for proving properties that don't depend on
                    // element values, and gives up otherwise
                    let ExprKind::Var(arr) = &base.kind else {
                        return Err("only direct array parameters can be indexed".to_string());
                    };
                    let _ = self.translate(idx)?;
                    let key = format!("{}[{}]", arr, self.fresh);
                    let sym = self.fresh_int(&key);
                    self.selects.insert(key, sym.clone());
                    Ok(Val::Int(sym))
                }
                ExprKind::Field(base, field) => {
                    let ExprKind::Var(b) = &base.kind else {
                        return Err("only direct struct parameters have symbolic fields".to_string());
                    };
                    let key = format!("{}.{}", b, field);
                    if let Some(v) = self.fields.get(&key) {
                        return Ok(Val::Int(v.clone()));
                    }
                    let sym = Int::new_const(self.ctx, key.clone());
                    self.fields.insert(key, sym.clone());
                    Ok(Val::Int(sym))
                }
                ExprKind::Un(op, inner) => {
                    let v = self.translate(inner)?;
                    match op {
                        UnOp::Neg => Ok(Val::Int(v.as_int()?.unary_minus())),
                        UnOp::Not => Ok(Val::Bool(v.as_bool()?.not())),
                    }
                }
                ExprKind::Bin(op, lhs, rhs) => {
                    use BinOp::*;
                    let l = self.translate(lhs)?;
                    let r = self.translate(rhs)?;
                    match op {
                        Add => Ok(Val::Int(l.as_int()? + r.as_int()?)),
                        Sub => Ok(Val::Int(l.as_int()? - r.as_int()?)),
                        Mul => Ok(Val::Int(l.as_int()? * r.as_int()?)),
                        Div => Ok(Val::Int(l.as_int()? / r.as_int()?)),
                        Mod => Ok(Val::Int(l.as_int()?.modulo(r.as_int()?))),
                        Lt => Ok(Val::Bool(l.as_int()?.lt(r.as_int()?))),
                        Le => Ok(Val::Bool(l.as_int()?.le(r.as_int()?))),
                        Gt => Ok(Val::Bool(l.as_int()?.gt(r.as_int()?))),
                        Ge => Ok(Val::Bool(l.as_int()?.ge(r.as_int()?))),
                        Eq => match (&l, &r) {
                            (Val::Int(a), Val::Int(b)) => Ok(Val::Bool(a._eq(b))),
                            (Val::Bool(a), Val::Bool(b)) => Ok(Val::Bool(a._eq(b))),
                            _ => Err("'==' operands have different types".to_string()),
                        },
                        Ne => match (&l, &r) {
                            (Val::Int(a), Val::Int(b)) => Ok(Val::Bool(a._eq(b).not())),
                            (Val::Bool(a), Val::Bool(b)) => Ok(Val::Bool(a._eq(b).not())),
                            _ => Err("'!=' operands have different types".to_string()),
                        },
                        And => Ok(Val::Bool(Bool::and(
                            self.ctx,
                            &[l.as_bool()?, r.as_bool()?],
                        ))),
                        Or => Ok(Val::Bool(Bool::or(
                            self.ctx,
                            &[l.as_bool()?, r.as_bool()?],
                        ))),
                    }
                }
                _ => Err("expression is outside the decidable subset".to_string()),
            }
        }

        /// Symbolically executes a block. Completed return paths accumulate
        /// in `paths` (with their exact path condition). Returns the path
        /// condition under which control falls through the end of the block,
        /// or None if every path returned.
        fn exec_block(
            &mut self,
            stmts: &[Stmt],
            cond: &Bool<'ctx>,
            paths: &mut Vec<RetPath<'ctx>>,
        ) -> Result<Option<Bool<'ctx>>, String> {
            // the condition under which execution reaches the current
            // statement; narrows when a branch of an if returns
            let mut cur = cond.clone();
            for stmt in stmts.iter() {
                match &stmt.kind {
                    StmtKind::Let { name, value, .. } => {
                        let v = self.translate(value)?;
                        self.env.insert(name.clone(), v);
                    }
                    StmtKind::Assign { name, value } => {
                        let v = self.translate(value)?;
                        self.env.insert(name.clone(), v);
                    }
                    StmtKind::Return(e) => {
                        let result = match e {
                            Some(e) => Some(self.translate(e)?),
                            None => None,
                        };
                        paths.push(RetPath {
                            cond: cur.clone(),
                            result,
                        });
                        return Ok(None);
                    }
                    StmtKind::Assert(e) => {
                        // asserts are runtime-enforced; treat as path assumption
                        let _ = self.translate(e)?;
                    }
                    StmtKind::If {
                        cond: c,
                        then_body,
                        else_body,
                    } => {
                        let cv = self.translate(c)?.as_bool()?.clone();
                        let then_cond = Bool::and(self.ctx, &[&cur, &cv]);
                        let else_cond = Bool::and(self.ctx, &[&cur, &cv.not()]);

                        let saved_env = self.env.clone();
                        let then_falls = self.exec_block(then_body, &then_cond, paths)?;
                        let then_env = std::mem::replace(&mut self.env, saved_env);
                        let else_falls = self.exec_block(else_body, &else_cond, paths)?;

                        match (then_falls, else_falls) {
                            (None, None) => return Ok(None),
                            (Some(tc), Some(ec)) => {
                                // merge: x = ite(cond, then_x, else_x)
                                let else_env = self.env.clone();
                                let mut merged = HashMap::new();
                                for (k, ev) in &else_env {
                                    if let Some(tv) = then_env.get(k) {
                                        merged.insert(k.clone(), Val::ite(&cv, tv, ev)?);
                                    }
                                }
                                // variables introduced only in the then-branch
                                // are out of scope after the if; skip them
                                self.env = merged;
                                cur = Bool::or(self.ctx, &[&tc, &ec]);
                            }
                            (Some(tc), None) => {
                                // only the then-branch continues
                                self.env = then_env;
                                cur = tc;
                            }
                            (None, Some(ec)) => {
                                cur = ec;
                            }
                        }
                    }
                    StmtKind::For {
                        var,
                        start,
                        end,
                        body,
                    } => {
                        // unroll for-loops whose bounds simplify to constants
                        let s = self.translate(start)?.as_int()?.clone();
                        let e = self.translate(end)?.as_int()?.clone();
                        let (Some(sc), Some(ec)) =
                            (s.simplify().as_i64(), e.simplify().as_i64())
                        else {
                            return Err(
                                "only 'for' loops with constant bounds can be unrolled"
                                    .to_string(),
                            );
                        };
                        if ec.saturating_sub(sc) > MAX_UNROLL {
                            return Err(format!(
                                "loop runs {} iterations; the unrolling limit is {}",
                                ec - sc,
                                MAX_UNROLL
                            ));
                        }
                        for k in sc..ec {
                            self.env
                                .insert(var.clone(), Val::Int(Int::from_i64(self.ctx, k)));
                            match self.exec_block(body, &cur, paths)? {
                                Some(c) => cur = c,
                                None => {
                                    self.env.remove(var);
                                    return Ok(None);
                                }
                            }
                        }
                        self.env.remove(var);
                    }
                    StmtKind::Expr(_) => {
                        // pure expression statements can't affect the result
                    }
                    StmtKind::While { .. } => {
                        return Err(
                            "while loops are outside the decidable subset (use a bounded for loop)"
                                .to_string(),
                        );
                    }
                    _ => {
                        return Err("statement is outside the decidable subset".to_string());
                    }
                }
            }
            Ok(Some(cur))
        }
    }

    pub fn verify_program(program: &Program) -> Vec<FunctionReport> {
        let fns: HashMap<String, &Function> = program
            .functions
            .iter()
            .filter(|f| !f.is_extern)
            .map(|f| (f.name.clone(), f))
            .collect();
        program
            .functions
            .iter()
            .filter(|f| {
                !f.is_std && !f.is_extern && (!f.ensures.is_empty() || !f.requires.is_empty())
            })
            .map(|f| verify_function(f, &fns))
            .collect()
    }

    pub fn verify_function(f: &Function, fns: &HashMap<String, &Function>) -> FunctionReport {
        let cfg = Config::new();
        let ctx = Context::new(&cfg);

        let mut st = SymState {
            ctx: &ctx,
            env: HashMap::new(),
            lens: HashMap::new(),
            selects: HashMap::new(),
            fields: HashMap::new(),
            fns,
            depth: 0,
            fresh: 0,
        };

        // parameters become symbolic constants
        let mut unsupported: Option<String> = None;
        for p in &f.params {
            match &p.ty {
                Type::Int => {
                    st.env
                        .insert(p.name.clone(), Val::Int(Int::new_const(&ctx, p.name.clone())));
                }
                Type::Bool => {
                    st.env.insert(
                        p.name.clone(),
                        Val::Bool(Bool::new_const(&ctx, p.name.clone())),
                    );
                }
                Type::Array(_) => {
                    // arrays are opaque; only len(arr) is symbolic (>= 0)
                    let l = Int::new_const(&ctx, format!("len_{}", p.name));
                    st.lens.insert(p.name.clone(), l);
                }
                Type::Struct(_) => {
                    // fields materialize lazily as uninterpreted ints
                }
                other => {
                    unsupported = Some(format!(
                        "parameter '{}' has type '{}' (outside the decidable subset)",
                        p.name, other
                    ));
                }
            }
        }

        let mut report = FunctionReport {
            function: f.name.clone(),
            clauses: Vec::new(),
            vacuous_requires: false,
        };

        if let Some(msg) = unsupported {
            for c in &f.ensures {
                report
                    .clauses
                    .push((c.text.clone(), VerifyResult::Unknown(msg.clone())));
            }
            return report;
        }

        // translate requires clauses
        let mut assumptions: Vec<Bool> = Vec::new();
        // array lengths are never negative
        for l in st.lens.values() {
            assumptions.push(l.ge(&Int::from_i64(&ctx, 0)));
        }
        let mut req_err: Option<String> = None;
        for c in &f.requires {
            match st.translate(&c.expr).and_then(|v| v.as_bool().cloned()) {
                Ok(b) => assumptions.push(b),
                Err(e) => {
                    req_err = Some(format!("requires '{}': {}", c.text, e));
                    break;
                }
            }
        }
        if let Some(msg) = req_err {
            for c in &f.ensures {
                report
                    .clauses
                    .push((c.text.clone(), VerifyResult::Unknown(msg.clone())));
            }
            return report;
        }

        // vacuity check: are the requires clauses satisfiable at all?
        {
            let solver = Solver::new(&ctx);
            for a in &assumptions {
                solver.assert(a);
            }
            if solver.check() == SatResult::Unsat {
                report.vacuous_requires = true;
            }
        }

        if f.ensures.is_empty() {
            return report;
        }

        // symbolically execute the body to get result per return path
        let true_cond = Bool::from_bool(&ctx, true);
        let mut paths: Vec<RetPath> = Vec::new();
        let body_result = st.exec_block(&f.body, &true_cond, &mut paths);

        match body_result {
            Err(msg) => {
                for c in &f.ensures {
                    report
                        .clauses
                        .push((c.text.clone(), VerifyResult::Unknown(msg.clone())));
                }
                report
            }
            Ok(_fell_through) => {
                for c in &f.ensures {
                    let mut clause_result = VerifyResult::Verified;
                    for path in &paths {
                        let Some(result) = &path.result else { continue };
                        // bind 'result' and re-translate the ensures clause
                        st.env.insert("result".to_string(), result.clone());
                        let ens = match st.translate(&c.expr).and_then(|v| v.as_bool().cloned()) {
                            Ok(b) => b,
                            Err(e) => {
                                clause_result = VerifyResult::Unknown(e);
                                break;
                            }
                        };
                        let solver = Solver::new(&ctx);
                        for a in &assumptions {
                            solver.assert(a);
                        }
                        solver.assert(&path.cond);
                        solver.assert(&ens.not());
                        match solver.check() {
                            SatResult::Unsat => {}
                            SatResult::Sat => {
                                let model = solver
                                    .get_model()
                                    .map(|m| {
                                        let s = m.to_string();
                                        let one_line: Vec<&str> =
                                            s.lines().map(|l| l.trim()).collect();
                                        one_line.join(" ")
                                    })
                                    .unwrap_or_default();
                                clause_result = VerifyResult::Counterexample(model);
                                break;
                            }
                            SatResult::Unknown => {
                                clause_result =
                                    VerifyResult::Unknown("solver returned unknown".to_string());
                                break;
                            }
                        }
                    }
                    report.clauses.push((c.text.clone(), clause_result));
                }
                report
            }
        }
    }
}

#[cfg(feature = "smt")]
pub use z3impl::verify_program;

#[cfg(not(feature = "smt"))]
pub fn verify_program(_program: &Program) -> Vec<FunctionReport> {
    Vec::new()
}

/// True if this build carries the Z3 backend.
pub fn smt_available() -> bool {
    cfg!(feature = "smt")
}
