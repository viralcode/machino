//! Tree-walking interpreter. This is the reference semantics of machino and
//! the engine behind `machino run` and `machino test`. Contracts (requires /
//! ensures) and asserts are always enforced here.
//!
//! The interpreter doubles as machino's **native runtime**: it provides a set
//! of host externs (files, stdin, environment, TCP sockets, clock) so that
//! programs — including long-running servers — can run directly with
//! `machino run`, no WASM host required.

use crate::ast::*;
use crate::diag::Span;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Strings are byte strings (usually UTF-8), matching the WASM backend.
    Str(Rc<Vec<u8>>),
    Array(Rc<RefCell<Vec<Value>>>),
    Struct(Rc<RefCell<HashMap<String, Value>>>),
    /// A first-class named function value.
    Fn(String),
    /// A lambda with its by-value captured environment.
    Closure(Rc<ClosureData>),
    Unit,
}

#[derive(Debug)]
pub struct ClosureData {
    pub lambda_id: usize,
    pub captured: Vec<(String, Value)>,
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
            Value::Str(s) => String::from_utf8_lossy(s).into_owned(),
            Value::Array(_) => "<array>".to_string(),
            Value::Struct(_) => "<struct>".to_string(),
            Value::Fn(name) => format!("<fn {}>", name),
            Value::Closure(_) => "<fn>".to_string(),
            Value::Unit => "<unit>".to_string(),
        }
    }

    fn str_value(text: &str) -> Value {
        Value::Str(Rc::new(text.as_bytes().to_vec()))
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
    Break,
    Continue,
    Return(Value),
}

fn max_call_depth() -> usize {
    std::env::var("MACHINO_MAX_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4096)
}

pub struct Interp<'a> {
    functions: HashMap<&'a str, &'a Function>,
    struct_fields: HashMap<&'a str, &'a [Param]>,
    lambdas: HashMap<usize, &'a Lambda>,
    depth: usize,
    max_depth: usize,
    pub output: Box<dyn FnMut(&str) + 'a>,
    /// Program arguments exposed through the args() extern.
    pub args: Vec<String>,
    listeners: HashMap<i64, TcpListener>,
    conns: HashMap<i64, TcpStream>,
    next_handle: i64,
}

type RResult<T> = Result<T, RuntimeError>;

impl<'a> Interp<'a> {
    pub fn new(program: &'a Program) -> Self {
        let mut functions = HashMap::new();
        for f in &program.functions {
            functions.insert(f.name.as_str(), f);
        }
        let mut struct_fields = HashMap::new();
        for s in &program.structs {
            struct_fields.insert(s.name.as_str(), s.fields.as_slice());
        }
        let mut lambdas = HashMap::new();
        for f in &program.functions {
            collect_lambdas_stmts(&f.body, &mut lambdas);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                collect_lambdas_expr(&c.expr, &mut lambdas);
            }
        }
        for t in &program.tests {
            collect_lambdas_stmts(&t.body, &mut lambdas);
        }
        Interp {
            functions,
            struct_fields,
            lambdas,
            depth: 0,
            max_depth: max_call_depth(),
            output: Box::new(|line| println!("{}", line)),
            args: Vec::new(),
            listeners: HashMap::new(),
            conns: HashMap::new(),
            next_handle: 1,
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
        if self.depth >= self.max_depth {
            return Err(RuntimeError::new(
                format!(
                    "stack overflow: call depth exceeded {} (override with MACHINO_MAX_DEPTH)",
                    self.max_depth
                ),
                call_span,
            ));
        }

        // requires clauses run against the incoming arguments, for externs too
        if !f.requires.is_empty() {
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
        }

        let result = if f.is_extern {
            self.call_extern(f, &args, call_span)?
        } else {
            let mut env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
            for (p, v) in f.params.iter().zip(args.iter()) {
                env[0].insert(p.name.clone(), v.clone());
            }
            self.depth += 1;
            let flow = self.exec_block(&f.body, &mut env);
            self.depth -= 1;
            match flow? {
                Flow::Return(v) => v,
                _ => Value::Unit,
            }
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

    fn call_closure(&mut self, c: &ClosureData, args: Vec<Value>, call_span: Span) -> RResult<Value> {
        if self.depth >= self.max_depth {
            return Err(RuntimeError::new(
                format!(
                    "stack overflow: call depth exceeded {} (override with MACHINO_MAX_DEPTH)",
                    self.max_depth
                ),
                call_span,
            ));
        }
        let lambda = *self.lambdas.get(&c.lambda_id).expect("lambda registered");
        let mut env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
        for (name, v) in &c.captured {
            env[0].insert(name.clone(), v.clone());
        }
        // params shadow captures of the same name
        for (p, v) in lambda.params.iter().zip(args) {
            env[0].insert(p.name.clone(), v);
        }
        self.depth += 1;
        let flow = self.exec_block(&lambda.body, &mut env);
        self.depth -= 1;
        match flow? {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Unit),
        }
    }

    // ---- native externs (the machino native runtime) ----

    fn call_extern(&mut self, f: &'a Function, args: &[Value], call_span: Span) -> RResult<Value> {
        let sig_err = |expected: &str| {
            RuntimeError::new(
                format!(
                    "extern '{}' is declared with the wrong signature; the native runtime provides: {}",
                    f.name, expected
                ),
                f.span,
            )
        };
        macro_rules! check_sig {
            ($params:expr, $ret:expr, $desc:expr) => {
                if f.params.iter().map(|p| &p.ty).ne($params.iter()) || f.ret != $ret {
                    return Err(sig_err($desc));
                }
            };
        }

        match f.name.as_str() {
            "clock_ms" => {
                check_sig!(Vec::<Type>::new(), Type::Int, "extern fn clock_ms() -> int");
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                Ok(Value::Int(ms))
            }
            "sleep_ms" => {
                check_sig!(vec![Type::Int], Type::Unit, "extern fn sleep_ms(ms: int)");
                if let Value::Int(ms) = &args[0] {
                    std::thread::sleep(std::time::Duration::from_millis((*ms).max(0) as u64));
                }
                Ok(Value::Unit)
            }
            "read_file" => {
                check_sig!(vec![Type::Str], Type::Str, "extern fn read_file(path: str) -> str");
                let path = as_string(&args[0]);
                match std::fs::read(&path) {
                    Ok(bytes) => Ok(Value::Str(Rc::new(bytes))),
                    Err(e) => Err(RuntimeError::new(
                        format!("read_file: cannot read '{}': {}", path, e),
                        call_span,
                    )),
                }
            }
            "write_file" => {
                check_sig!(
                    vec![Type::Str, Type::Str],
                    Type::Bool,
                    "extern fn write_file(path: str, data: str) -> bool"
                );
                let path = as_string(&args[0]);
                let data = match &args[1] {
                    Value::Str(b) => b.as_ref().clone(),
                    _ => Vec::new(),
                };
                Ok(Value::Bool(std::fs::write(&path, data).is_ok()))
            }
            "file_exists" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Bool,
                    "extern fn file_exists(path: str) -> bool"
                );
                Ok(Value::Bool(
                    std::path::Path::new(&as_string(&args[0])).exists(),
                ))
            }
            "read_line" => {
                check_sig!(Vec::<Type>::new(), Type::Str, "extern fn read_line() -> str");
                let mut line = String::new();
                let n = std::io::stdin().lock().read_line(&mut line).unwrap_or(0);
                if n == 0 {
                    return Ok(Value::str_value(""));
                }
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                Ok(Value::str_value(&line))
            }
            "getenv" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Str,
                    "extern fn getenv(name: str) -> str"
                );
                let val = std::env::var(as_string(&args[0])).unwrap_or_default();
                Ok(Value::str_value(&val))
            }
            "args" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Array(Box::new(Type::Str)),
                    "extern fn args() -> [str]"
                );
                let vals: Vec<Value> = self.args.iter().map(|a| Value::str_value(a)).collect();
                Ok(Value::Array(Rc::new(RefCell::new(vals))))
            }
            "exit" => {
                check_sig!(vec![Type::Int], Type::Unit, "extern fn exit(code: int)");
                let code = match &args[0] {
                    Value::Int(c) => *c as i32,
                    _ => 0,
                };
                std::process::exit(code);
            }
            "tcp_listen" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Int,
                    "extern fn tcp_listen(port: int) -> int"
                );
                let port = match &args[0] {
                    Value::Int(p) => *p,
                    _ => 0,
                };
                let listener = TcpListener::bind(("0.0.0.0", port as u16)).map_err(|e| {
                    RuntimeError::new(
                        format!("tcp_listen: cannot bind port {}: {}", port, e),
                        call_span,
                    )
                })?;
                let h = self.next_handle;
                self.next_handle += 1;
                self.listeners.insert(h, listener);
                Ok(Value::Int(h))
            }
            "tcp_accept" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Int,
                    "extern fn tcp_accept(listener: int) -> int"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let listener = self.listeners.get(&h).ok_or_else(|| {
                    RuntimeError::new(format!("tcp_accept: invalid listener handle {}", h), call_span)
                })?;
                let (stream, _) = listener.accept().map_err(|e| {
                    RuntimeError::new(format!("tcp_accept: {}", e), call_span)
                })?;
                let ch = self.next_handle;
                self.next_handle += 1;
                self.conns.insert(ch, stream);
                Ok(Value::Int(ch))
            }
            "tcp_read" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Str,
                    "extern fn tcp_read(conn: int) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let stream = self.conns.get_mut(&h).ok_or_else(|| {
                    RuntimeError::new(format!("tcp_read: invalid connection handle {}", h), call_span)
                })?;
                let mut buf = vec![0u8; 65536];
                let n = stream.read(&mut buf).map_err(|e| {
                    RuntimeError::new(format!("tcp_read: {}", e), call_span)
                })?;
                buf.truncate(n);
                Ok(Value::Str(Rc::new(buf)))
            }
            "tcp_write" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Int,
                    "extern fn tcp_write(conn: int, data: str) -> int"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let data = match &args[1] {
                    Value::Str(b) => b.clone(),
                    _ => Rc::new(Vec::new()),
                };
                let stream = self.conns.get_mut(&h).ok_or_else(|| {
                    RuntimeError::new(format!("tcp_write: invalid connection handle {}", h), call_span)
                })?;
                stream.write_all(&data).map_err(|e| {
                    RuntimeError::new(format!("tcp_write: {}", e), call_span)
                })?;
                Ok(Value::Int(data.len() as i64))
            }
            "tcp_close" => {
                check_sig!(vec![Type::Int], Type::Unit, "extern fn tcp_close(handle: int)");
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                self.conns.remove(&h);
                self.listeners.remove(&h);
                Ok(Value::Unit)
            }
            other => Err(RuntimeError::new(
                format!(
                    "extern '{}' is not provided by the machino native runtime. Available externs: \
                     clock_ms, sleep_ms, read_file, write_file, file_exists, read_line, getenv, \
                     args, exit, tcp_listen, tcp_accept, tcp_read, tcp_write, tcp_close. \
                     For other capabilities, compile with 'machino build' and provide the import \
                     from your own host.",
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
                other => {
                    env.pop();
                    return Ok(other);
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
            StmtKind::IndexAssign { base, index, value } => {
                let b = self.eval(base, env)?;
                let idx = match self.eval(index, env)? {
                    Value::Int(i) => i,
                    _ => unreachable!("type checker guarantees int index"),
                };
                let v = self.eval(value, env)?;
                match b {
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
            StmtKind::FieldAssign { base, field, value } => {
                let b = self.eval(base, env)?;
                let v = self.eval(value, env)?;
                match b {
                    Value::Struct(fields) => {
                        fields.borrow_mut().insert(field.clone(), v);
                        Ok(Flow::Normal)
                    }
                    _ => unreachable!("type checker guarantees struct"),
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
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::Normal)
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                let s = match self.eval(start, env)? {
                    Value::Int(v) => v,
                    _ => unreachable!(),
                };
                let e = match self.eval(end, env)? {
                    Value::Int(v) => v,
                    _ => unreachable!(),
                };
                env.push(HashMap::new());
                let mut i = s;
                while i < e {
                    env.last_mut().unwrap().insert(var.clone(), Value::Int(i));
                    match self.exec_block(body, env)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => {
                            env.pop();
                            return Ok(ret);
                        }
                    }
                    // read the variable back: assignments to the loop variable
                    // inside the body affect iteration (same as compiled code)
                    let current = match env.last().unwrap().get(var) {
                        Some(Value::Int(v)) => *v,
                        _ => i,
                    };
                    i = current.checked_add(1).ok_or_else(|| {
                        RuntimeError::new("integer overflow in for-loop counter", stmt.span)
                    })?;
                }
                env.pop();
                Ok(Flow::Normal)
            }
            StmtKind::Break => Ok(Flow::Break),
            StmtKind::Continue => Ok(Flow::Continue),
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
        // a bare function name evaluates to a function value
        if self.functions.contains_key(name) {
            return Ok(Value::Fn(name.to_string()));
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
            ExprKind::Str(s) => Ok(Value::str_value(s)),
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
            ExprKind::Field(base, field) => {
                let b = self.eval(base, env)?;
                match b {
                    Value::Struct(fields) => Ok(fields
                        .borrow()
                        .get(field)
                        .cloned()
                        .expect("type checker guarantees field exists")),
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
            ExprKind::Lambda(l) => {
                // capture the lambda's free variables by value, now
                let mut captured = Vec::new();
                for name in l.free_names() {
                    let found = env
                        .iter()
                        .rev()
                        .find_map(|scope| scope.get(&name))
                        .cloned();
                    if let Some(v) = found {
                        captured.push((name, v));
                    }
                    // names that don't resolve to variables are global
                    // functions; the body resolves them at call time
                }
                Ok(Value::Closure(Rc::new(ClosureData {
                    lambda_id: l.id,
                    captured,
                })))
            }
            ExprKind::Call(name, args) => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, env)?);
                }
                // a local variable holding a function value shadows everything
                let local_val = env
                    .iter()
                    .rev()
                    .find_map(|scope| scope.get(name.as_str()))
                    .and_then(|v| match v {
                        Value::Fn(_) | Value::Closure(_) => Some(v.clone()),
                        _ => None,
                    });
                match local_val {
                    Some(Value::Fn(fname)) => {
                        let f = self.functions.get(fname.as_str()).copied().ok_or_else(|| {
                            RuntimeError::new(format!("unknown function '{}'", fname), expr.span)
                        })?;
                        return self.call_function(f, vals, expr.span);
                    }
                    Some(Value::Closure(c)) => {
                        return self.call_closure(&c, vals, expr.span);
                    }
                    _ => {}
                }
                match name.as_str() {
                    "print" => {
                        let s = vals[0].display();
                        (self.output)(&s);
                        Ok(Value::Unit)
                    }
                    "len" => match &vals[0] {
                        Value::Array(cells) => Ok(Value::Int(cells.borrow().len() as i64)),
                        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
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
                            if !f.is_finite() || f < i64::MIN as f64 || f >= i64::MAX as f64 {
                                return Err(RuntimeError::new(
                                    format!("to_int: value {} is out of int range", f),
                                    expr.span,
                                ));
                            }
                            Ok(Value::Int(f.trunc() as i64))
                        }
                        _ => unreachable!(),
                    },
                    "char_at" => match (&vals[0], &vals[1]) {
                        (Value::Str(s), Value::Int(i)) => {
                            if *i < 0 || *i as usize >= s.len() {
                                return Err(RuntimeError::new(
                                    format!(
                                        "char_at out of bounds: index {} but length is {}",
                                        i,
                                        s.len()
                                    ),
                                    expr.span,
                                ));
                            }
                            Ok(Value::Int(s[*i as usize] as i64))
                        }
                        _ => unreachable!(),
                    },
                    "substr" => match (&vals[0], &vals[1], &vals[2]) {
                        (Value::Str(s), Value::Int(a), Value::Int(b)) => {
                            if *a < 0 || a > b || *b as usize > s.len() {
                                return Err(RuntimeError::new(
                                    format!(
                                        "substr out of range: [{}, {}) on a string of length {}",
                                        a,
                                        b,
                                        s.len()
                                    ),
                                    expr.span,
                                ));
                            }
                            Ok(Value::Str(Rc::new(s[*a as usize..*b as usize].to_vec())))
                        }
                        _ => unreachable!(),
                    },
                    "chr" => match vals[0] {
                        Value::Int(c) => {
                            if !(0..=255).contains(&c) {
                                return Err(RuntimeError::new(
                                    format!("chr: byte value {} is out of range 0..=255", c),
                                    expr.span,
                                ));
                            }
                            Ok(Value::Str(Rc::new(vec![c as u8])))
                        }
                        _ => unreachable!(),
                    },
                    other => {
                        if let Some(fields) = self.struct_fields.get(other) {
                            // struct constructor
                            let mut map = HashMap::with_capacity(fields.len());
                            for (fld, v) in fields.iter().zip(vals) {
                                map.insert(fld.name.clone(), v);
                            }
                            return Ok(Value::Struct(Rc::new(RefCell::new(map))));
                        }
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
                s.extend_from_slice(&b);
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

fn collect_lambdas_stmts<'a>(stmts: &'a [Stmt], out: &mut HashMap<usize, &'a Lambda>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Assign { value, .. } => {
                collect_lambdas_expr(value, out)
            }
            StmtKind::IndexAssign { base, index, value } => {
                collect_lambdas_expr(base, out);
                collect_lambdas_expr(index, out);
                collect_lambdas_expr(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                collect_lambdas_expr(base, out);
                collect_lambdas_expr(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_lambdas_expr(cond, out);
                collect_lambdas_stmts(then_body, out);
                collect_lambdas_stmts(else_body, out);
            }
            StmtKind::While { cond, body } => {
                collect_lambdas_expr(cond, out);
                collect_lambdas_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                collect_lambdas_expr(start, out);
                collect_lambdas_expr(end, out);
                collect_lambdas_stmts(body, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                collect_lambdas_expr(e, out)
            }
            _ => {}
        }
    }
}

fn collect_lambdas_expr<'a>(expr: &'a Expr, out: &mut HashMap<usize, &'a Lambda>) {
    match &expr.kind {
        ExprKind::Array(elems) => {
            for e in elems {
                collect_lambdas_expr(e, out);
            }
        }
        ExprKind::Index(a, b) => {
            collect_lambdas_expr(a, out);
            collect_lambdas_expr(b, out);
        }
        ExprKind::Field(a, _) => collect_lambdas_expr(a, out),
        ExprKind::Bin(_, a, b) => {
            collect_lambdas_expr(a, out);
            collect_lambdas_expr(b, out);
        }
        ExprKind::Un(_, a) => collect_lambdas_expr(a, out),
        ExprKind::Call(_, args) => {
            for a in args {
                collect_lambdas_expr(a, out);
            }
        }
        ExprKind::Lambda(l) => {
            out.insert(l.id, l);
            collect_lambdas_stmts(&l.body, out);
        }
        _ => {}
    }
}

fn as_string(v: &Value) -> String {
    match v {
        Value::Str(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
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
