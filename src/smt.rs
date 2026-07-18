//! SMT-based contract verification using Z3.
//! Translates a subset of machino contracts to SMT-LIB for static verification.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};

#[cfg(feature = "smt")]
use z3::*;

/// Result of SMT verification.
#[derive(Debug)]
pub enum VerifyResult {
    /// Contract is provably satisfied.
    Verified,
    /// Found a counterexample showing the contract can be violated.
    Counterexample(String),
    /// Could not verify (timeout, unknown, or unsupported operations).
    Unknown(String),
}

/// Verify contracts for a function using SMT.
#[cfg(feature = "smt")]
pub fn verify_function(func: &Function) -> Vec<(String, VerifyResult)> {
    let cfg = Config::new();
    let ctx = Context::new(&cfg);
    let solver = Solver::new(&ctx);
    
    let mut results = Vec::new();
    
    // For each ensures clause, try to prove it holds given all requires clauses
    for (idx, ensures) in func.ensures.iter().enumerate() {
        let result = verify_postcondition(&ctx, &solver, func, ensures);
        results.push((format!("ensures clause {}", idx + 1), result));
    }
    
    results
}

#[cfg(feature = "smt")]
fn verify_postcondition(ctx: &Context, solver: &Solver, func: &Function, ensures: &Contract) -> VerifyResult {
    // Reset solver for this verification attempt
    solver.reset();
    
    // Create SMT variables for function parameters
    let mut env: std::collections::HashMap<String, Dynamic> = std::collections::HashMap::new();
    for param in &func.params {
        let var = match param.ty {
            Type::Int => Dynamic::from_ast(&ctx.named_int_const(&param.name)),
            Type::Bool => Dynamic::from_ast(&ctx.named_bool_const(&param.name)),
            _ => {
                // Unsupported type for SMT
                return VerifyResult::Unknown(format!("unsupported parameter type: {}", param.ty));
            }
        };
        env.insert(param.name.clone(), var);
    }
    
    // Add requires clauses as assumptions
    for req in &func.requires {
        match translate_expr(ctx, &req.expr, &env) {
            Ok(smt_expr) => {
                if let Some(bool_expr) = smt_expr.as_bool() {
                    solver.assert(&bool_expr);
                } else {
                    return VerifyResult::Unknown("requires clause is not boolean".to_string());
                }
            }
            Err(msg) => return VerifyResult::Unknown(msg),
        }
    }
    
    // Try to prove the ensures clause (by negating it and checking unsat)
    match translate_expr(ctx, &ensures.expr, &env) {
        Ok(smt_expr) => {
            if let Some(bool_expr) = smt_expr.as_bool() {
                // Assert NOT(ensures) and check if UNSAT
                solver.assert(&bool_expr.not());
                
                match solver.check() {
                    SatResult::Unsat => VerifyResult::Verified,
                    SatResult::Sat => {
                        // Found counterexample
                        if let Some(model) = solver.get_model() {
                            VerifyResult::Counterexample(format!("counterexample: {}", model))
                        } else {
                            VerifyResult::Counterexample("satisfiable but no model".to_string())
                        }
                    }
                    SatResult::Unknown => VerifyResult::Unknown("solver returned unknown".to_string()),
                }
            } else {
                VerifyResult::Unknown("ensures clause is not boolean".to_string())
            }
        }
        Err(msg) => VerifyResult::Unknown(msg),
    }
}

#[cfg(feature = "smt")]
fn translate_expr(ctx: &Context, expr: &Expr, env: &std::collections::HashMap<String, Dynamic>) -> Result<Dynamic, String> {
    match &expr.kind {
        ExprKind::Int(n) => Ok(Dynamic::from_ast(&ctx.from_i64(*n))),
        ExprKind::Bool(b) => Ok(Dynamic::from_ast(&ctx.from_bool(*b))),
        ExprKind::Var(name) => {
            env.get(name).cloned().ok_or_else(|| format!("unknown variable: {}", name))
        }
        ExprKind::Bin(op, lhs, rhs) => {
            let l = translate_expr(ctx, lhs, env)?;
            let r = translate_expr(ctx, rhs, env)?;
            
            use BinOp::*;
            match op {
                Add => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.add(&[&r_int])))
                    } else {
                        Err("add requires int operands".to_string())
                    }
                }
                Sub => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.sub(&[&r_int])))
                    } else {
                        Err("sub requires int operands".to_string())
                    }
                }
                Mul => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.mul(&[&r_int])))
                    } else {
                        Err("mul requires int operands".to_string())
                    }
                }
                Eq => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int._eq(&r_int)))
                    } else if let (Some(l_bool), Some(r_bool)) = (l.as_bool(), r.as_bool()) {
                        Ok(Dynamic::from_ast(&l_bool._eq(&r_bool)))
                    } else {
                        Err("eq requires matching operand types".to_string())
                    }
                }
                Ne => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int._eq(&r_int).not()))
                    } else if let (Some(l_bool), Some(r_bool)) = (l.as_bool(), r.as_bool()) {
                        Ok(Dynamic::from_ast(&l_bool._eq(&r_bool).not()))
                    } else {
                        Err("ne requires matching operand types".to_string())
                    }
                }
                Lt => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.lt(&r_int)))
                    } else {
                        Err("lt requires int operands".to_string())
                    }
                }
                Le => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.le(&r_int)))
                    } else {
                        Err("le requires int operands".to_string())
                    }
                }
                Gt => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.gt(&r_int)))
                    } else {
                        Err("gt requires int operands".to_string())
                    }
                }
                Ge => {
                    if let (Some(l_int), Some(r_int)) = (l.as_int(), r.as_int()) {
                        Ok(Dynamic::from_ast(&l_int.ge(&r_int)))
                    } else {
                        Err("ge requires int operands".to_string())
                    }
                }
                And => {
                    if let (Some(l_bool), Some(r_bool)) = (l.as_bool(), r.as_bool()) {
                        Ok(Dynamic::from_ast(&l_bool.and(&[&r_bool])))
                    } else {
                        Err("and requires bool operands".to_string())
                    }
                }
                Or => {
                    if let (Some(l_bool), Some(r_bool)) = (l.as_bool(), r.as_bool()) {
                        Ok(Dynamic::from_ast(&l_bool.or(&[&r_bool])))
                    } else {
                        Err("or requires bool operands".to_string())
                    }
                }
                _ => Err(format!("unsupported operator: {:?}", op)),
            }
        }
        ExprKind::Un(op, inner) => {
            let val = translate_expr(ctx, inner, env)?;
            match op {
                UnOp::Not => {
                    if let Some(b) = val.as_bool() {
                        Ok(Dynamic::from_ast(&b.not()))
                    } else {
                        Err("not requires bool operand".to_string())
                    }
                }
                UnOp::Neg => {
                    if let Some(i) = val.as_int() {
                        Ok(Dynamic::from_ast(&i.unary_minus()))
                    } else {
                        Err("neg requires int operand".to_string())
                    }
                }
            }
        }
        _ => Err(format!("unsupported expression for SMT: {:?}", expr.kind)),
    }
}

#[cfg(not(feature = "smt"))]
pub fn verify_function(_func: &Function) -> Vec<(String, VerifyResult)> {
    vec![]
}

#[cfg(not(feature = "smt"))]
pub fn verify_function_stub(_func: &Function) -> String {
    "SMT verification not available (compile with --features smt)".to_string()
}
