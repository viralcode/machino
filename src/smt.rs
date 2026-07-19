//! Static contract verification with the Z3 SMT solver (`machino check
//! --verify`, requires building with `--features smt`).
//!
//! The decidable subset: loop-free function bodies over int/bool (plus
//! `len(arr)` treated as a symbolic non-negative int). The verifier runs the
//! body symbolically, collecting a (path condition, result) pair per return
//! path, then asks Z3 to prove every `ensures` clause on every path under the
//! `requires` assumptions. It also flags contradictory `requires` clauses
//! (contracts that no input can satisfy). Functions outside the subset
//! (loops, calls, floats, strings, mutation of arrays) report `unknown` and
//! fall back to machino's always-on runtime contract enforcement.

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

    struct SymState<'ctx> {
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
        fresh: usize,
    }

    /// A completed return path: (conjunction of branch conditions, result).
    struct RetPath<'ctx> {
        cond: Bool<'ctx>,
        result: Option<Val<'ctx>>,
    }

    impl<'ctx> SymState<'ctx> {
        fn fresh_int(&mut self, hint: &str) -> Int<'ctx> {
            self.fresh += 1;
            Int::new_const(self.ctx, format!("{}!{}", hint, self.fresh))
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
                    Err(format!("call to '{}' is outside the decidable subset", name))
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
        /// in `paths` (with `cond` = the path condition). Returns Ok(true)
        /// if control can fall through the end of the block.
        fn exec_block(
            &mut self,
            stmts: &[Stmt],
            cond: &Bool<'ctx>,
            paths: &mut Vec<RetPath<'ctx>>,
        ) -> Result<bool, String> {
            for (i, stmt) in stmts.iter().enumerate() {
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
                            cond: cond.clone(),
                            result,
                        });
                        return Ok(false);
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
                        let then_cond = Bool::and(self.ctx, &[cond, &cv]);
                        let else_cond = Bool::and(self.ctx, &[cond, &cv.not()]);

                        let saved_env = self.env.clone();
                        let then_falls = self.exec_block(then_body, &then_cond, paths)?;
                        let then_env = std::mem::replace(&mut self.env, saved_env);
                        let else_falls = self.exec_block(else_body, &else_cond, paths)?;

                        match (then_falls, else_falls) {
                            (false, false) => return Ok(false),
                            (true, true) => {
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
                            }
                            (true, false) => {
                                // only the then-branch continues
                                self.env = then_env;
                                let rest = &stmts[i + 1..];
                                return self.exec_block(rest, &then_cond, paths);
                            }
                            (false, true) => {
                                let rest = &stmts[i + 1..];
                                return self.exec_block(rest, &else_cond, paths);
                            }
                        }
                    }
                    StmtKind::Expr(_) => {
                        // pure expression statements can't affect the result
                    }
                    StmtKind::While { .. } | StmtKind::For { .. } => {
                        return Err("loops are outside the decidable subset".to_string());
                    }
                    _ => {
                        return Err("statement is outside the decidable subset".to_string());
                    }
                }
            }
            Ok(true)
        }
    }

    pub fn verify_program(program: &Program) -> Vec<FunctionReport> {
        program
            .functions
            .iter()
            .filter(|f| {
                !f.is_std && !f.is_extern && (!f.ensures.is_empty() || !f.requires.is_empty())
            })
            .map(verify_function)
            .collect()
    }

    pub fn verify_function(f: &Function) -> FunctionReport {
        let cfg = Config::new();
        let ctx = Context::new(&cfg);

        let mut st = SymState {
            ctx: &ctx,
            env: HashMap::new(),
            lens: HashMap::new(),
            selects: HashMap::new(),
            fields: HashMap::new(),
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
