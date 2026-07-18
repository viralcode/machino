//! Tree-walking interpreter. This is the reference semantics of machino and
//! the engine behind `machino run` and `machino test`. Contracts (requires /
//! ensures) and asserts are always enforced here.

use crate::ast::*;
use crate::diag::Span;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Rc<String>),
    Array(Rc<RefCell<Vec<Value>>>),
    Unit,
}

impl Value {
    pub fn display(&self) -> String {
        match self {
            Value::Int(v) => v.to_string(),
            Value::Float(v) => {
                if v.fract() == 0.0 && v.is_finite() {
                    format!("{:.1}", v)
                } else {
                    format!("{}", v)
                }
            }
            Value::Bool(v) => v.to_string(),
            Value::Str(s) => s.as_ref().clone(),
            Value::Array(_) => "<array>".to_string(),
            Value::Unit => "<unit>".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
}

impl RuntimeError {
    fn new(message: impl Into<String>, span: Span) -> Self {
        RuntimeError {
            message: message.into(),
            span,
        }
    }
}

enum Flow {
    Normal,
    Return(Value),
}

const MAX_CALL_DEPTH: usize = 4096;

pub struct Interp<'a> {
    functions: HashMap<&'a str, &'a Function>,
    depth: usize,
    pub output: Box<dyn FnMut(&str) + 'a>,
}

type RResult<T> = Result<T, RuntimeError>;

impl<'a> Interp<'a> {
    pub fn new(program: &'a Program) -> Self {
        let mut functions = HashMap::new();
        for f in &program.functions {
            functions.insert(f.name.as_str(), f);
        }
        Interp {
            functions,
            depth: 0,
            output: Box::new(|line| println!("{}", line)),
        }
    }

    pub fn run_main(&mut self) -> RResult<()> {
        let main = self.functions.get("main").copied().ok_or_else(|| {
            RuntimeError::new(
                "no 'fn main()' found: the entry point of a machino program is fn main()",
                Span::new(0, 0),
            )
        })?;
        self.call_function(main, Vec::new(), Span::new(0, 0))?;
        Ok(())
    }

    pub fn run_stmts_as_test(&mut self, stmts: &[Stmt]) -> RResult<()> {
        let mut env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
        self.exec_block(stmts, &mut env)?;
        Ok(())
    }

    fn call_function(&mut self, f: &'a Function, args: Vec<Value>, call_span: Span) -> RResult<Value> {
        if self.depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(
                format!("stack overflow: call depth exceeded {}", MAX_CALL_DEPTH),
                call_span,
            ));
        }

        if f.is_extern {
            return self.call_extern(f, args, call_span);
        }

        let mut env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
        for (p, v) in f.params.iter().zip(args.iter()) {
            env[0].insert(p.name.clone(), v.clone());
        }

        for c in &f.requires {
            let ok = self.eval(&c.expr, &mut env)?;
            if !matches!(ok, Value::Bool(true)) {
                return Err(RuntimeError::new(
                    format!(
                        "contract violation: requires '{}' failed when calling '{}'",
                        c.text, f.name
                    ),
                    call_span,
                ));
            }
        }

        self.depth += 1;
        let flow = self.exec_block(&f.body, &mut env);
        self.depth -= 1;

        let result = match flow? {
            Flow::Return(v) => v,
            Flow::Normal => Value::Unit,
        };

        if !f.ensures.is_empty() {
            // rebind params (they may have been reassigned) plus 'result'
            let mut ens_env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
            for (p, v) in f.params.iter().zip(args.iter()) {
                ens_env[0].insert(p.name.clone(), v.clone());
            }
            ens_env[0].insert("result".to_string(), result.clone());
            for c in &f.ensures {
                let ok = self.eval(&c.expr, &mut ens_env)?;
                if !matches!(ok, Value::Bool(true)) {
                    return Err(RuntimeError::new(
                        format!(
                            "contract violation: ensures '{}' failed in '{}'",
                            c.text, f.name
                        ),
                        f.span,
                    ));
                }
            }
        }

        Ok(result)
    }

    fn call_extern(&mut self, f: &'a Function, _args: Vec<Value>, call_span: Span) -> RResult<Value> {
        match f.name.as_str() {
            "clock_ms" => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                Ok(Value::Int(ms))
            }
            other => Err(RuntimeError::new(
                format!(
                    "extern '{}' is not provided by the machino interpreter (built-in externs: clock_ms). \
                     Compile with 'machino build' and provide it from your host runtime.",
                    other
                ),
                call_span,
            )),
        }
    }

    fn exec_block(&mut self, stmts: &[Stmt], env: &mut Vec<HashMap<String, Value>>) -> RResult<Flow> {
        env.push(HashMap::new());
        for s in stmts {
            match self.exec_stmt(s, env)? {
                Flow::Normal => {}
                ret => {
                    env.pop();
                    return Ok(ret);
                }
            }
        }
        env.pop();
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, env: &mut Vec<HashMap<String, Value>>) -> RResult<Flow> {
        match &stmt.kind {
            StmtKind::Let { name, value, .. } => {
                let v = self.eval(value, env)?;
                env.last_mut().unwrap().insert(name.clone(), v);
                Ok(Flow::Normal)
            }
            StmtKind::Assign { name, value } => {
                let v = self.eval(value, env)?;
                for scope in env.iter_mut().rev() {
                    if let Some(slot) = scope.get_mut(name) {
                        *slot = v;
                        return Ok(Flow::Normal);
                    }
                }
                Err(RuntimeError::new(
                    format!("unknown variable '{}'", name),
                    stmt.span,
                ))
            }
            StmtKind::IndexAssign { name, index, value } => {
                let idx = match self.eval(index, env)? {
                    Value::Int(i) => i,
                    _ => unreachable!("type checker guarantees int index"),
                };
                let v = self.eval(value, env)?;
                let arr = self.lookup(env, name, stmt.span)?;
                match arr {
                    Value::Array(cells) => {
                        let mut vec = cells.borrow_mut();
                        if idx < 0 || idx as usize >= vec.len() {
                            return Err(RuntimeError::new(
                                format!(
                                    "index out of bounds: index {} but length is {}",
                                    idx,
                                    vec.len()
                                ),
                                index.span,
                            ));
                        }
                        vec[idx as usize] = v;
                        Ok(Flow::Normal)
                    }
                    _ => unreachable!("type checker guarantees array"),
                }
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let c = self.eval(cond, env)?;
                if matches!(c, Value::Bool(true)) {
                    self.exec_block(then_body, env)
                } else {
                    self.exec_block(else_body, env)
                }
            }
            StmtKind::While { cond, body } => {
                loop {
                    let c = self.eval(cond, env)?;
                    if !matches!(c, Value::Bool(true)) {
                        break;
                    }
                    match self.exec_block(body, env)? {
                        Flow::Normal => {}
                        ret => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::Return(value) => {
                let v = match value {
                    Some(e) => self.eval(e, env)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(v))
            }
            StmtKind::Assert(expr) => {
                let v = self.eval(expr, env)?;
                if matches!(v, Value::Bool(true)) {
                    Ok(Flow::Normal)
                } else {
                    Err(RuntimeError::new("assertion failed", expr.span))
                }
            }
            StmtKind::Expr(expr) => {
                self.eval(expr, env)?;
                Ok(Flow::Normal)
            }
        }
    }

    fn lookup(&self, env: &[HashMap<String, Value>], name: &str, span: Span) -> RResult<Value> {
        for scope in env.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Ok(v.clone());
            }
        }
        Err(RuntimeError::new(
            format!("unknown variable '{}'", name),
            span,
        ))
    }

    fn eval(&mut self, expr: &Expr, env: &mut Vec<HashMap<String, Value>>) -> RResult<Value> {
        match &expr.kind {
            ExprKind::Int(v) => Ok(Value::Int(*v)),
            ExprKind::Float(v) => Ok(Value::Float(*v)),
            ExprKind::Bool(v) => Ok(Value::Bool(*v)),
            ExprKind::Str(s) => Ok(Value::Str(Rc::new(s.clone()))),
            ExprKind::Var(name) => self.lookup(env, name, expr.span),
            ExprKind::Array(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    vals.push(self.eval(e, env)?);
                }
                Ok(Value::Array(Rc::new(RefCell::new(vals))))
            }
            ExprKind::Index(base, index) => {
                let b = self.eval(base, env)?;
                let idx = match self.eval(index, env)? {
                    Value::Int(i) => i,
                    _ => unreachable!(),
                };
                match b {
                    Value::Array(cells) => {
                        let vec = cells.borrow();
                        if idx < 0 || idx as usize >= vec.len() {
                            return Err(RuntimeError::new(
                                format!(
                                    "index out of bounds: index {} but length is {}",
                                    idx,
                                    vec.len()
                                ),
                                index.span,
                            ));
                        }
                        Ok(vec[idx as usize].clone())
                    }
                    _ => unreachable!(),
                }
            }
            ExprKind::Un(op, inner) => {
                let v = self.eval(inner, env)?;
                match (op, v) {
                    (UnOp::Neg, Value::Int(i)) => {
                        i.checked_neg().map(Value::Int).ok_or_else(|| {
                            RuntimeError::new("integer overflow in negation", expr.span)
                        })
                    }
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                    _ => unreachable!(),
                }
            }
            ExprKind::Bin(op, lhs, rhs) => {
                use BinOp::*;
                // short-circuit && and ||
                if matches!(op, And | Or) {
                    let l = match self.eval(lhs, env)? {
                        Value::Bool(b) => b,
                        _ => unreachable!(),
                    };
                    match (op, l) {
                        (And, false) => return Ok(Value::Bool(false)),
                        (Or, true) => return Ok(Value::Bool(true)),
                        _ => {}
                    }
                    return self.eval(rhs, env);
                }
                let l = self.eval(lhs, env)?;
                let r = self.eval(rhs, env)?;
                self.eval_binop(*op, l, r, expr.span)
            }
            ExprKind::Call(name, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, env)?);
                }
                match name.as_str() {
                    "print" => {
                        let s = vals[0].display();
                        (self.output)(&s);
                        Ok(Value::Unit)
                    }
                    "len" => match &vals[0] {
                        Value::Array(cells) => Ok(Value::Int(cells.borrow().len() as i64)),
                        Value::Str(s) => Ok(Value::Int(s.as_bytes().len() as i64)),
                        _ => unreachable!(),
                    },
                    "push" => match &vals[0] {
                        Value::Array(cells) => {
                            let mut new_vec = cells.borrow().clone();
                            new_vec.push(vals[1].clone());
                            Ok(Value::Array(Rc::new(RefCell::new(new_vec))))
                        }
                        _ => unreachable!(),
                    },
                    "to_float" => match vals[0] {
                        Value::Int(i) => Ok(Value::Float(i as f64)),
                        _ => unreachable!(),
                    },
                    "to_int" => match vals[0] {
                        Value::Float(f) => {
                            if !f.is_finite() || f < i64::MIN as f64 || f > i64::MAX as f64 {
                                return Err(RuntimeError::new(
                                    format!("to_int: value {} is out of int range", f),
                                    expr.span,
                                ));
                            }
                            Ok(Value::Int(f.trunc() as i64))
                        }
                        _ => unreachable!(),
                    },
                    other => {
                        let f = self.functions.get(other).copied().ok_or_else(|| {
                            RuntimeError::new(format!("unknown function '{}'", other), expr.span)
                        })?;
                        self.call_function(f, vals, expr.span)
                    }
                }
            }
        }
    }

    fn eval_binop(&self, op: BinOp, l: Value, r: Value, span: Span) -> RResult<Value> {
        use BinOp::*;
        use Value::*;
        match (op, l, r) {
            (Add, Int(a), Int(b)) => a
                .checked_add(b)
                .map(Int)
                .ok_or_else(|| RuntimeError::new("integer overflow in '+'", span)),
            (Sub, Int(a), Int(b)) => a
                .checked_sub(b)
                .map(Int)
                .ok_or_else(|| RuntimeError::new("integer overflow in '-'", span)),
            (Mul, Int(a), Int(b)) => a
                .checked_mul(b)
                .map(Int)
                .ok_or_else(|| RuntimeError::new("integer overflow in '*'", span)),
            (Div, Int(a), Int(b)) => {
                if b == 0 {
                    Err(RuntimeError::new("division by zero", span))
                } else {
                    a.checked_div(b)
                        .map(Int)
                        .ok_or_else(|| RuntimeError::new("integer overflow in '/'", span))
                }
            }
            (Mod, Int(a), Int(b)) => {
                if b == 0 {
                    Err(RuntimeError::new("modulo by zero", span))
                } else {
                    a.checked_rem(b)
                        .map(Int)
                        .ok_or_else(|| RuntimeError::new("integer overflow in '%'", span))
                }
            }
            (Add, Float(a), Float(b)) => Ok(Float(a + b)),
            (Sub, Float(a), Float(b)) => Ok(Float(a - b)),
            (Mul, Float(a), Float(b)) => Ok(Float(a * b)),
            (Div, Float(a), Float(b)) => Ok(Float(a / b)),
            (Add, Str(a), Str(b)) => {
                let mut s = a.as_ref().clone();
                s.push_str(&b);
                Ok(Str(Rc::new(s)))
            }
            (Eq, a, b) => Ok(Bool(values_eq(&a, &b))),
            (Ne, a, b) => Ok(Bool(!values_eq(&a, &b))),
            (Lt, Int(a), Int(b)) => Ok(Bool(a < b)),
            (Le, Int(a), Int(b)) => Ok(Bool(a <= b)),
            (Gt, Int(a), Int(b)) => Ok(Bool(a > b)),
            (Ge, Int(a), Int(b)) => Ok(Bool(a >= b)),
            (Lt, Float(a), Float(b)) => Ok(Bool(a < b)),
            (Le, Float(a), Float(b)) => Ok(Bool(a <= b)),
            (Gt, Float(a), Float(b)) => Ok(Bool(a > b)),
            (Ge, Float(a), Float(b)) => Ok(Bool(a >= b)),
            _ => unreachable!("type checker rejects other operand combinations"),
        }
    }
}

fn values_eq(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int(x), Int(y)) => x == y,
        (Float(x), Float(y)) => x == y,
        (Bool(x), Bool(y)) => x == y,
        (Str(x), Str(y)) => x == y,
        _ => false,
    }
}
