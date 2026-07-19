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
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use ureq;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Strings are byte strings (usually UTF-8), matching the WASM backend.
    Str(Rc<Vec<u8>>),
    /// Index into `Interp::heap` (reference type; mutable through the heap slot).
    Array(u32),
    /// Index into `Interp::heap` (reference type; mutable through the heap slot).
    Struct(u32),
    /// An enum variant: (enum_name, variant_name, payloads)
    EnumVariant(String, String, Vec<Value>),
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

// ---- concurrency (spawn / join_*) ----

/// The program spawned threads execute against. Set once per process by the
/// CLI before running user code; spawn fails cleanly if unset.
static SPAWN_PROGRAM: std::sync::OnceLock<Program> = std::sync::OnceLock::new();

pub fn set_spawn_program(p: Program) {
    let _ = SPAWN_PROGRAM.set(p);
}

/// A Send-able deep copy of a Value. Machino values use Rc internally, so
/// crossing a thread boundary copies the whole object graph — spawned tasks
/// share nothing with their parent.
#[derive(Debug, Clone)]
enum SendVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(Vec<u8>),
    Array(Vec<SendVal>),
    Struct(Vec<(String, SendVal)>),
    EnumVariant(String, String, Vec<SendVal>),
    Fn(String),
    Closure(usize, Vec<(String, SendVal)>),
    Unit,
}

// ---- channels (shared across spawn threads) ----

struct ChanInner {
    queue: Mutex<VecDeque<SendVal>>,
    closed: std::sync::atomic::AtomicBool,
    cvar: Condvar,
}

fn channels() -> &'static Mutex<HashMap<i64, Arc<ChanInner>>> {
    static CHANNELS: std::sync::OnceLock<Mutex<HashMap<i64, Arc<ChanInner>>>> =
        std::sync::OnceLock::new();
    CHANNELS.get_or_init(|| Mutex::new(HashMap::new()))
}

static NEXT_CHAN: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);

fn channel_new() -> i64 {
    let id = NEXT_CHAN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ch = Arc::new(ChanInner {
        queue: Mutex::new(VecDeque::new()),
        closed: std::sync::atomic::AtomicBool::new(false),
        cvar: Condvar::new(),
    });
    channels().lock().unwrap().insert(id, ch);
    id
}

fn channel_close(id: i64) -> Result<(), String> {
    let map = channels().lock().unwrap();
    let ch = map
        .get(&id)
        .ok_or_else(|| format!("no channel with handle {}", id))?;
    ch.closed
        .store(true, std::sync::atomic::Ordering::Release);
    ch.cvar.notify_all();
    Ok(())
}

fn channel_send(id: i64, v: SendVal) -> Result<(), String> {
    let ch = {
        let map = channels().lock().unwrap();
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("no channel with handle {}", id))?
    };
    if ch.closed.load(std::sync::atomic::Ordering::Acquire) {
        return Err(format!("send on closed channel {}", id));
    }
    ch.queue.lock().unwrap().push_back(v);
    ch.cvar.notify_one();
    Ok(())
}

fn channel_recv(id: i64) -> Result<SendVal, String> {
    let ch = {
        let map = channels().lock().unwrap();
        map.get(&id)
            .cloned()
            .ok_or_else(|| format!("no channel with handle {}", id))?
    };
    let mut q = ch.queue.lock().unwrap();
    loop {
        if let Some(v) = q.pop_front() {
            return Ok(v);
        }
        if ch.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(format!("receive on closed empty channel {}", id));
        }
        q = ch.cvar.wait(q).unwrap();
    }
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
            Value::EnumVariant(enum_name, variant_name, payloads) => {
                if payloads.is_empty() {
                    format!("{}::{}", enum_name, variant_name)
                } else {
                    format!("{}::{}(...)", enum_name, variant_name)
                }
            }
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
    pub fn new(message: impl Into<String>, span: Span) -> Self {
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

/// Heap object stored by the interpreter arena (arrays and structs).
enum HeapObj {
    Array(Vec<Value>),
    Struct(HashMap<String, Value>),
}

struct HeapSlot {
    marked: bool,
    obj: Option<HeapObj>,
}

/// Collect after this many heap allocations (when roots are known).
const HEAP_GC_ALLOC_THRESHOLD: u32 = 256;

pub struct Interp<'a> {
    functions: HashMap<&'a str, &'a Function>,
    struct_fields: HashMap<&'a str, &'a [Param]>,
    enum_variants: HashMap<&'a str, &'a [EnumVariant]>,
    lambdas: HashMap<usize, &'a Lambda>,
    depth: usize,
    max_depth: usize,
    pub output: Box<dyn FnMut(&str) + 'a>,
    /// When set, receives one JSON object per call/return of a user-defined
    /// (non-std) function. Enabled by `machino run --trace`.
    pub trace: Option<Box<dyn FnMut(&str) + 'a>>,
    /// Program arguments exposed through the args() extern.
    pub args: Vec<String>,
    listeners: HashMap<i64, TcpListener>,
    conns: HashMap<i64, TcpStream>,
    next_handle: i64,
    /// Running spawned tasks, keyed by the handle spawn returned.
    tasks: HashMap<i64, std::thread::JoinHandle<Result<SendVal, String>>>,
    next_task: i64,
    /// Virtual DOM for `dom_*` externs (native runtime / tests).
    dom_nodes: HashMap<i64, VDomNode>,
    next_dom: i64,
    dom_listeners: HashMap<(i64, String), String>,
    dom_last_event_type: String,
    dom_last_event_target: i64,
    dom_last_event_x: i64,
    dom_last_event_y: i64,
    dom_last_event_key: String,
    dom_last_event_button: i64,
    dom_last_event_value: String,
    db_conns: HashMap<i64, crate::db_runtime::DbConn>,
    next_db: i64,
    heap: Vec<HeapSlot>,
    heap_allocs_since_gc: u32,
    heap_high_water: usize,
    /// Call-stack environments scanned as GC roots (caller + callee frames).
    env_stack: Vec<*const Vec<HashMap<String, Value>>>,
}

#[derive(Clone)]
struct VDomNode {
    tag: String,
    text: String,
    attrs: HashMap<String, String>,
    styles: HashMap<String, String>,
    children: Vec<i64>,
    parent: i64,
    width: i64,
    height: i64,
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
        let mut enum_variants = HashMap::new();
        for e in &program.enums {
            enum_variants.insert(e.name.as_str(), e.variants.as_slice());
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
            enum_variants,
            lambdas,
            depth: 0,
            max_depth: max_call_depth(),
            output: Box::new(|line| println!("{}", line)),
            trace: None,
            args: Vec::new(),
            listeners: HashMap::new(),
            conns: HashMap::new(),
            next_handle: 1,
            tasks: HashMap::new(),
            next_task: 1,
            dom_nodes: {
                let mut m = HashMap::new();
                m.insert(
                    1,
                    VDomNode {
                        tag: "#document".to_string(),
                        text: String::new(),
                        attrs: HashMap::new(),
                        styles: HashMap::new(),
                        children: Vec::new(),
                        parent: 0,
                        width: 800,
                        height: 600,
                    },
                );
                m
            },
            next_dom: 2,
            dom_listeners: HashMap::new(),
            dom_last_event_type: String::new(),
            dom_last_event_target: 0,
            dom_last_event_x: 0,
            dom_last_event_y: 0,
            dom_last_event_key: String::new(),
            dom_last_event_button: 0,
            dom_last_event_value: String::new(),
            db_conns: HashMap::new(),
            next_db: 1,
            heap: Vec::new(),
            heap_allocs_since_gc: 0,
            heap_high_water: 0,
            env_stack: Vec::new(),
        }
    }

    fn push_env(&mut self, env: &Vec<HashMap<String, Value>>) {
        self.env_stack.push(env as *const _);
    }

    fn pop_env(&mut self) {
        self.env_stack.pop();
    }

    fn alloc_array(
        &mut self,
        data: Vec<Value>,
        _roots: Option<&[HashMap<String, Value>]>,
    ) -> Value {
        let id = self.alloc_heap(HeapObj::Array(data));
        Value::Array(id)
    }

    fn alloc_struct(
        &mut self,
        fields: HashMap<String, Value>,
        _roots: Option<&[HashMap<String, Value>]>,
    ) -> Value {
        let id = self.alloc_heap(HeapObj::Struct(fields));
        Value::Struct(id)
    }

    fn alloc_heap(&mut self, obj: HeapObj) -> u32 {
        let id = self.heap.len() as u32;
        self.heap.push(HeapSlot {
            marked: false,
            obj: Some(obj),
        });
        self.heap_allocs_since_gc += 1;
        id
    }

    fn maybe_collect_at_stmt(&mut self, _env: &[HashMap<String, Value>]) {
        if self.heap_allocs_since_gc >= HEAP_GC_ALLOC_THRESHOLD {
            self.gc_collect(&[]);
        }
    }

    /// Mark-sweep cycle collector. Roots are bindings in `extra_roots` plus every
    /// frames (typically the active call stack's lexical environment).
    pub fn gc_collect(&mut self, extra_roots: &[HashMap<String, Value>]) {
        for slot in &mut self.heap {
            slot.marked = false;
        }
        for frame in extra_roots {
            for v in frame.values() {
                self.mark_value(v);
            }
        }
        let stacks: Vec<*const Vec<HashMap<String, Value>>> = self.env_stack.clone();
        for ptr in stacks {
            // SAFETY: pushed for the duration of eval/calls on this thread.
            let env = unsafe { &*ptr };
            for frame in env {
                for v in frame.values() {
                    self.mark_value(v);
                }
            }
        }
        self.sweep_heap();
        self.heap_allocs_since_gc = 0;
    }

    /// Live heap objects (array + struct slots that are allocated).
    pub fn heap_live_count(&self) -> usize {
        self.heap
            .iter()
            .filter(|s| s.obj.is_some())
            .count()
    }

    fn mark_value(&mut self, v: &Value) {
        match v {
            Value::Array(id) => self.mark_array(*id),
            Value::Struct(id) => self.mark_struct(*id),
            Value::EnumVariant(_, _, payloads) => {
                for p in payloads {
                    self.mark_value(p);
                }
            }
            Value::Closure(c) => {
                for (_, cap) in &c.captured {
                    self.mark_value(cap);
                }
            }
            _ => {}
        }
    }

    fn mark_array(&mut self, id: u32) {
        let Some(slot) = self.heap.get_mut(id as usize) else {
            return;
        };
        if slot.marked {
            return;
        }
        slot.marked = true;
        if let Some(HeapObj::Array(data)) = slot.obj.as_ref() {
            let items: Vec<Value> = data.clone();
            for v in items {
                self.mark_value(&v);
            }
        }
    }

    fn mark_struct(&mut self, id: u32) {
        let Some(slot) = self.heap.get_mut(id as usize) else {
            return;
        };
        if slot.marked {
            return;
        }
        slot.marked = true;
        if let Some(HeapObj::Struct(fields)) = slot.obj.as_ref() {
            let vals: Vec<Value> = fields.values().cloned().collect();
            for v in vals {
                self.mark_value(&v);
            }
        }
    }

    fn sweep_heap(&mut self) {
        for slot in &mut self.heap {
            if slot.obj.is_some() && !slot.marked {
                slot.obj = None;
            }
        }
    }

    fn array_len(&self, id: u32) -> usize {
        match self.heap.get(id as usize).and_then(|s| s.obj.as_ref()) {
            Some(HeapObj::Array(data)) => data.len(),
            _ => 0,
        }
    }

    fn array_get(&self, id: u32, idx: usize) -> Option<Value> {
        match self.heap.get(id as usize).and_then(|s| s.obj.as_ref()) {
            Some(HeapObj::Array(data)) => data.get(idx).cloned(),
            _ => None,
        }
    }

    fn array_set(&mut self, id: u32, idx: usize, val: Value) -> bool {
        match self.heap.get_mut(id as usize).and_then(|s| s.obj.as_mut()) {
            Some(HeapObj::Array(data)) if idx < data.len() => {
                data[idx] = val;
                true
            }
            _ => false,
        }
    }

    fn array_clone_data(&self, id: u32) -> Vec<Value> {
        match self.heap.get(id as usize).and_then(|s| s.obj.as_ref()) {
            Some(HeapObj::Array(data)) => data.clone(),
            _ => Vec::new(),
        }
    }

    fn struct_get(&self, id: u32, field: &str) -> Option<Value> {
        match self.heap.get(id as usize).and_then(|s| s.obj.as_ref()) {
            Some(HeapObj::Struct(fields)) => fields.get(field).cloned(),
            _ => None,
        }
    }

    fn struct_set(&mut self, id: u32, field: &str, val: Value) -> bool {
        match self.heap.get_mut(id as usize).and_then(|s| s.obj.as_mut()) {
            Some(HeapObj::Struct(fields)) => {
                fields.insert(field.to_string(), val);
                true
            }
            _ => false,
        }
    }

    /// Build an array value (used by fuzzing and tests outside eval).
    pub fn alloc_array_value(&mut self, vals: Vec<Value>) -> Value {
        self.alloc_array(vals, None)
    }

    /// Build a struct value (used by fuzzing and tests outside eval).
    pub fn alloc_struct_value(&mut self, fields: HashMap<String, Value>) -> Value {
        self.alloc_struct(fields, None)
    }

    pub fn array_elements(&self, id: u32) -> Vec<Value> {
        self.array_clone_data(id)
    }

    pub fn struct_fields(&self, id: u32) -> Vec<(String, Value)> {
        match self.heap.get(id as usize).and_then(|s| s.obj.as_ref()) {
            Some(HeapObj::Struct(fields)) => fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            _ => Vec::new(),
        }
    }

    fn to_sendval(&self, v: &Value) -> SendVal {
        match v {
            Value::Int(i) => SendVal::Int(*i),
            Value::Float(f) => SendVal::Float(*f),
            Value::Bool(b) => SendVal::Bool(*b),
            Value::Str(s) => SendVal::Str(s.as_ref().clone()),
            Value::Array(id) => {
                SendVal::Array(self.array_clone_data(*id).iter().map(|x| self.to_sendval(x)).collect())
            }
            Value::Struct(id) => SendVal::Struct(
                match self.heap.get(*id as usize).and_then(|s| s.obj.as_ref()) {
                    Some(HeapObj::Struct(fields)) => fields
                        .iter()
                        .map(|(k, v)| (k.clone(), self.to_sendval(v)))
                        .collect(),
                    _ => Vec::new(),
                },
            ),
            Value::EnumVariant(e, n, payloads) => SendVal::EnumVariant(
                e.clone(),
                n.clone(),
                payloads.iter().map(|p| self.to_sendval(p)).collect(),
            ),
            Value::Fn(name) => SendVal::Fn(name.clone()),
            Value::Closure(c) => SendVal::Closure(
                c.lambda_id,
                c.captured
                    .iter()
                    .map(|(k, v)| (k.clone(), self.to_sendval(v)))
                    .collect(),
            ),
            Value::Unit => SendVal::Unit,
        }
    }

    fn from_sendval(&mut self, v: SendVal) -> Value {
        match v {
            SendVal::Int(i) => Value::Int(i),
            SendVal::Float(f) => Value::Float(f),
            SendVal::Bool(b) => Value::Bool(b),
            SendVal::Str(s) => Value::Str(Rc::new(s)),
            SendVal::Array(xs) => {
                let vals: Vec<Value> = xs.into_iter().map(|x| self.from_sendval(x)).collect();
                self.alloc_array(vals, None)
            }
            SendVal::Struct(fields) => {
                let map: HashMap<String, Value> = fields
                    .into_iter()
                    .map(|(k, v)| (k, self.from_sendval(v)))
                    .collect();
                self.alloc_struct(map, None)
            }
            SendVal::EnumVariant(e, n, payloads) => {
                Value::EnumVariant(e, n, payloads.into_iter().map(|p| self.from_sendval(p)).collect())
            }
            SendVal::Fn(name) => Value::Fn(name),
            SendVal::Closure(id, captured) => Value::Closure(Rc::new(ClosureData {
                lambda_id: id,
                captured: captured
                    .into_iter()
                    .map(|(k, v)| (k, self.from_sendval(v)))
                    .collect(),
            })),
            SendVal::Unit => Value::Unit,
        }
    }

    fn dom_html_of(&self, h: i64) -> String {
        let Some(n) = self.dom_nodes.get(&h) else {
            return String::new();
        };
        if n.tag == "#text" || n.tag == "#document" {
            let mut s = n.text.clone();
            for &c in &n.children {
                s.push_str(&self.dom_html_of(c));
            }
            return s;
        }
        let mut s = format!("<{}", n.tag);
        for (k, v) in &n.attrs {
            s.push_str(&format!(" {}=\"{}\"", k, v));
        }
        s.push('>');
        s.push_str(&n.text);
        for &c in &n.children {
            s.push_str(&self.dom_html_of(c));
        }
        s.push_str(&format!("</{}>", n.tag));
        s
    }

    fn dom_fire_event(
        &mut self,
        h: i64,
        event: &str,
        x: i64,
        y: i64,
        key: &str,
        button: i64,
        value: &str,
    ) -> RResult<()> {
        self.dom_last_event_type = event.to_string();
        self.dom_last_event_target = h;
        self.dom_last_event_x = x;
        self.dom_last_event_y = y;
        self.dom_last_event_key = key.to_string();
        self.dom_last_event_button = button;
        self.dom_last_event_value = value.to_string();
        let handler = self.dom_listeners.get(&(h, event.to_string())).cloned();
        if let Some(name) = handler {
            self.call_by_name(&name, Vec::new())?;
        }
        Ok(())
    }

    pub fn run_main(&mut self) -> RResult<()> {
        let main = self.functions.get("main").copied().ok_or_else(|| {
            RuntimeError::new(
                "no 'fn main()' found: the entry point of a machino program is fn main()",
                Span::new(0, 0),
            )
        })?;
        self.call_function(main, Vec::new(), Span::new(0, 0))?;
        self.finish_top_level_call(&[]);
        Ok(())
    }

    pub fn run_stmts_as_test(&mut self, stmts: &[Stmt]) -> RResult<()> {
        let mut env: Vec<HashMap<String, Value>> = vec![HashMap::new()];
        self.push_env(&env);
        self.exec_block(stmts, &mut env)?;
        self.pop_env();
        self.finish_top_level_call(&env);
        Ok(())
    }

    /// Calls a named function with already-constructed values. Used by
    /// `machino fuzz` to drive contract-based property testing.
    pub fn call_by_name(&mut self, name: &str, args: Vec<Value>) -> RResult<Value> {
        let f = self.functions.get(name).copied().ok_or_else(|| {
            RuntimeError::new(format!("no function named '{}'", name), Span::new(0, 0))
        })?;
        self.call_function(f, args, Span::new(0, 0))
    }

    /// Calls a first-class function value (named function or closure).
    /// Used by spawned tasks.
    pub fn call_value(&mut self, callee: &Value, args: Vec<Value>) -> RResult<Value> {
        match callee {
            Value::Fn(name) => self.call_by_name(name, args),
            Value::Closure(c) => self.call_closure(c, args, Span::new(0, 0)),
            _ => Err(RuntimeError::new(
                "spawn requires a function value",
                Span::new(0, 0),
            )),
        }
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

        let tracing = self.trace.is_some() && !f.is_std;
        if tracing {
            let arg_strs: Vec<String> = args
                .iter()
                .map(|v| format!("\"{}\"", crate::diag::json_escape(&v.display())))
                .collect();
            let line = format!(
                "{{\"event\":\"call\",\"fn\":\"{}\",\"depth\":{},\"args\":[{}]}}",
                crate::diag::json_escape(&f.name),
                self.depth,
                arg_strs.join(",")
            );
            if let Some(t) = self.trace.as_mut() {
                t(&line);
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
            self.push_env(&env);
            let flow = self.exec_block(&f.body, &mut env);
            self.pop_env();
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

        if tracing {
            let line = format!(
                "{{\"event\":\"return\",\"fn\":\"{}\",\"depth\":{},\"value\":\"{}\"}}",
                crate::diag::json_escape(&f.name),
                self.depth,
                crate::diag::json_escape(&result.display())
            );
            if let Some(t) = self.trace.as_mut() {
                t(&line);
            }
        }

        Ok(result)
    }

    fn finish_top_level_call(&mut self, extra_roots: &[HashMap<String, Value>]) {
        let live = self.heap_live_count();
        if live > self.heap_high_water {
            self.gc_collect(extra_roots);
            self.heap_high_water = self.heap_live_count();
        }
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
        self.push_env(&env);
        let flow = self.exec_block(&lambda.body, &mut env);
        self.pop_env();
        self.depth -= 1;
        let result = match flow? {
            Flow::Return(v) => v,
            _ => Value::Unit,
        };
        Ok(result)
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
            "http_get" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Str,
                    "extern fn http_get(url: str) -> str"
                );
                let url = as_string(&args[0]);
                let body = match ureq::get(&url).call() {
                    Ok(resp) => resp.into_string().unwrap_or_default(),
                    Err(ureq::Error::Status(_, resp)) => resp.into_string().unwrap_or_default(),
                    Err(_) => String::new(),
                };
                Ok(Value::str_value(&body))
            }
            "args" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Array(Box::new(Type::Str)),
                    "extern fn args() -> [str]"
                );
                let vals: Vec<Value> = self.args.iter().map(|a| Value::str_value(a)).collect();
                Ok(self.alloc_array(vals, None))
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
            "dom_document" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn dom_document() -> int"
                );
                Ok(Value::Int(1))
            }
            "dom_create_element" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Int,
                    "extern fn dom_create_element(tag: str) -> int"
                );
                let tag = as_string(&args[0]);
                let h = self.next_dom;
                self.next_dom += 1;
                self.dom_nodes.insert(
                    h,
                    VDomNode {
                        tag,
                        text: String::new(),
                        attrs: HashMap::new(),
                        styles: HashMap::new(),
                        children: Vec::new(),
                        parent: 0,
                        width: 100,
                        height: 40,
                    },
                );
                Ok(Value::Int(h))
            }
            "dom_get_element_by_id" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Int,
                    "extern fn dom_get_element_by_id(id: str) -> int"
                );
                let id = as_string(&args[0]);
                let found = self
                    .dom_nodes
                    .iter()
                    .find(|(_, n)| n.attrs.get("id").map(|v| v == &id).unwrap_or(false))
                    .map(|(h, _)| *h)
                    .unwrap_or(0);
                Ok(Value::Int(found))
            }
            "dom_query_selector" => {
                check_sig!(
                    vec![Type::Str],
                    Type::Int,
                    "extern fn dom_query_selector(sel: str) -> int"
                );
                let sel = as_string(&args[0]);
                if let Some(id) = sel.strip_prefix('#').filter(|s| !s.is_empty()) {
                    let found = self
                        .dom_nodes
                        .iter()
                        .find(|(_, n)| n.attrs.get("id").map(|v| v == id).unwrap_or(false))
                        .map(|(h, _)| *h)
                        .unwrap_or(0);
                    return Ok(Value::Int(found));
                }
                let found = self
                    .dom_nodes
                    .iter()
                    .find(|(_, n)| n.tag == sel)
                    .map(|(h, _)| *h)
                    .unwrap_or(0);
                Ok(Value::Int(found))
            }
            "dom_set_text" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_set_text(el: int, text: str)"
                );
                if let Value::Int(h) = &args[0] {
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.text = as_string(&args[1]);
                        n.children.clear();
                    }
                }
                Ok(Value::Unit)
            }
            "dom_get_text" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Str,
                    "extern fn dom_get_text(el: int) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let text = self
                    .dom_nodes
                    .get(&h)
                    .map(|n| n.text.clone())
                    .unwrap_or_default();
                Ok(Value::str_value(&text))
            }
            "dom_set_html" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_set_html(el: int, html: str)"
                );
                if let Value::Int(h) = &args[0] {
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.text = as_string(&args[1]);
                        n.children.clear();
                    }
                }
                Ok(Value::Unit)
            }
            "dom_get_html" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Str,
                    "extern fn dom_get_html(el: int) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                Ok(Value::str_value(&self.dom_html_of(h)))
            }
            "dom_set_attr" => {
                check_sig!(
                    vec![Type::Int, Type::Str, Type::Str],
                    Type::Unit,
                    "extern fn dom_set_attr(el: int, name: str, value: str)"
                );
                if let Value::Int(h) = &args[0] {
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.attrs.insert(as_string(&args[1]), as_string(&args[2]));
                    }
                }
                Ok(Value::Unit)
            }
            "dom_get_attr" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn dom_get_attr(el: int, name: str) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let name = as_string(&args[1]);
                let v = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.attrs.get(&name).cloned())
                    .unwrap_or_default();
                Ok(Value::str_value(&v))
            }
            "dom_append_child" => {
                check_sig!(
                    vec![Type::Int, Type::Int],
                    Type::Unit,
                    "extern fn dom_append_child(parent: int, child: int)"
                );
                let p = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let c = match &args[1] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                if p != 0 && c != 0 && self.dom_nodes.contains_key(&p) && self.dom_nodes.contains_key(&c)
                {
                    if let Some(child) = self.dom_nodes.get_mut(&c) {
                        let old = child.parent;
                        child.parent = p;
                        if old != 0 {
                            if let Some(op) = self.dom_nodes.get_mut(&old) {
                                op.children.retain(|x| *x != c);
                            }
                        }
                    }
                    if let Some(parent) = self.dom_nodes.get_mut(&p) {
                        if !parent.children.contains(&c) {
                            parent.children.push(c);
                        }
                    }
                }
                Ok(Value::Unit)
            }
            "dom_remove_child" => {
                check_sig!(
                    vec![Type::Int, Type::Int],
                    Type::Unit,
                    "extern fn dom_remove_child(parent: int, child: int)"
                );
                let p = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let c = match &args[1] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                if let Some(parent) = self.dom_nodes.get_mut(&p) {
                    parent.children.retain(|x| *x != c);
                }
                if let Some(child) = self.dom_nodes.get_mut(&c) {
                    child.parent = 0;
                }
                Ok(Value::Unit)
            }
            "dom_add_class" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_add_class(el: int, cls: str)"
                );
                if let Value::Int(h) = &args[0] {
                    let cls = as_string(&args[1]);
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        let cur = n.attrs.entry("class".to_string()).or_default();
                        if cur.is_empty() {
                            *cur = cls;
                        } else if !cur.split_whitespace().any(|c| c == cls) {
                            cur.push(' ');
                            cur.push_str(&cls);
                        }
                    }
                }
                Ok(Value::Unit)
            }
            "dom_set_style" => {
                check_sig!(
                    vec![Type::Int, Type::Str, Type::Str],
                    Type::Unit,
                    "extern fn dom_set_style(el: int, prop: str, value: str)"
                );
                if let Value::Int(h) = &args[0] {
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.styles.insert(as_string(&args[1]), as_string(&args[2]));
                    }
                }
                Ok(Value::Unit)
            }
            "dom_get_style" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn dom_get_style(el: int, prop: str) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let prop = as_string(&args[1]);
                let v = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.styles.get(&prop).cloned())
                    .unwrap_or_default();
                Ok(Value::str_value(&v))
            }
            "dom_get_computed_style" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn dom_get_computed_style(el: int, prop: str) -> str"
                );
                // Virtual DOM: same as inline style.
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let prop = as_string(&args[1]);
                let v = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.styles.get(&prop).cloned())
                    .unwrap_or_default();
                Ok(Value::str_value(&v))
            }
            "dom_remove_class" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_remove_class(el: int, cls: str)"
                );
                if let Value::Int(h) = &args[0] {
                    let cls = as_string(&args[1]);
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        let next: Vec<&str> = n
                            .attrs
                            .get("class")
                            .map(|c| c.split_whitespace().filter(|c| *c != cls).collect())
                            .unwrap_or_default();
                        n.attrs.insert("class".into(), next.join(" "));
                    }
                }
                Ok(Value::Unit)
            }
            "dom_toggle_class" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Bool,
                    "extern fn dom_toggle_class(el: int, cls: str) -> bool"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let cls = as_string(&args[1]);
                let mut on = false;
                if let Some(n) = self.dom_nodes.get_mut(&h) {
                    let mut parts: Vec<String> = n
                        .attrs
                        .get("class")
                        .map(|c| c.split_whitespace().map(|s| s.to_string()).collect())
                        .unwrap_or_default();
                    if let Some(i) = parts.iter().position(|c| c == &cls) {
                        parts.remove(i);
                        on = false;
                    } else {
                        parts.push(cls);
                        on = true;
                    }
                    n.attrs.insert("class".into(), parts.join(" "));
                }
                Ok(Value::Bool(on))
            }
            "dom_has_class" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Bool,
                    "extern fn dom_has_class(el: int, cls: str) -> bool"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let cls = as_string(&args[1]);
                let on = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.attrs.get("class"))
                    .map(|c| c.split_whitespace().any(|x| x == cls))
                    .unwrap_or(false);
                Ok(Value::Bool(on))
            }
            "dom_client_width" | "dom_offset_width" => {
                check_sig!(vec![Type::Int], Type::Int, "extern fn dom_*_width(el: int) -> int");
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                Ok(Value::Int(
                    self.dom_nodes.get(&h).map(|n| n.width).unwrap_or(0),
                ))
            }
            "dom_client_height" | "dom_offset_height" => {
                check_sig!(vec![Type::Int], Type::Int, "extern fn dom_*_height(el: int) -> int");
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                Ok(Value::Int(
                    self.dom_nodes.get(&h).map(|n| n.height).unwrap_or(0),
                ))
            }
            "dom_get_bounding_rect" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Str,
                    "extern fn dom_get_bounding_rect(el: int) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let (w, ht) = self
                    .dom_nodes
                    .get(&h)
                    .map(|n| (n.width, n.height))
                    .unwrap_or((0, 0));
                Ok(Value::str_value(&format!("0,0,{},{}", w, ht)))
            }
            "dom_focus" | "dom_blur" => {
                check_sig!(vec![Type::Int], Type::Unit, "extern fn dom_focus/blur(el: int)");
                Ok(Value::Unit)
            }
            "dom_scroll_to" => {
                check_sig!(
                    vec![Type::Int, Type::Int, Type::Int],
                    Type::Unit,
                    "extern fn dom_scroll_to(el: int, x: int, y: int)"
                );
                Ok(Value::Unit)
            }
            "dom_parent" => {
                check_sig!(vec![Type::Int], Type::Int, "extern fn dom_parent(el: int) -> int");
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                Ok(Value::Int(
                    self.dom_nodes.get(&h).map(|n| n.parent).unwrap_or(0),
                ))
            }
            "dom_child_count" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Int,
                    "extern fn dom_child_count(el: int) -> int"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                Ok(Value::Int(
                    self.dom_nodes
                        .get(&h)
                        .map(|n| n.children.len() as i64)
                        .unwrap_or(0),
                ))
            }
            "dom_child_at" => {
                check_sig!(
                    vec![Type::Int, Type::Int],
                    Type::Int,
                    "extern fn dom_child_at(el: int, index: int) -> int"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let i = match &args[1] {
                    Value::Int(i) => *i,
                    _ => 0,
                };
                let child = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.children.get(i as usize).copied())
                    .unwrap_or(0);
                Ok(Value::Int(child))
            }
            "dom_clear_children" => {
                check_sig!(
                    vec![Type::Int],
                    Type::Unit,
                    "extern fn dom_clear_children(el: int)"
                );
                if let Value::Int(h) = &args[0] {
                    let kids = self
                        .dom_nodes
                        .get(h)
                        .map(|n| n.children.clone())
                        .unwrap_or_default();
                    for c in kids {
                        if let Some(ch) = self.dom_nodes.get_mut(&c) {
                            ch.parent = 0;
                        }
                    }
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.children.clear();
                        n.text.clear();
                    }
                }
                Ok(Value::Unit)
            }
            "dom_dataset_set" => {
                check_sig!(
                    vec![Type::Int, Type::Str, Type::Str],
                    Type::Unit,
                    "extern fn dom_dataset_set(el: int, key: str, value: str)"
                );
                if let Value::Int(h) = &args[0] {
                    let key = format!("data-{}", as_string(&args[1]));
                    if let Some(n) = self.dom_nodes.get_mut(h) {
                        n.attrs.insert(key, as_string(&args[2]));
                    }
                }
                Ok(Value::Unit)
            }
            "dom_dataset_get" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn dom_dataset_get(el: int, key: str) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let key = format!("data-{}", as_string(&args[1]));
                let v = self
                    .dom_nodes
                    .get(&h)
                    .and_then(|n| n.attrs.get(&key).cloned())
                    .unwrap_or_default();
                Ok(Value::str_value(&v))
            }
            "dom_add_listener" => {
                check_sig!(
                    vec![Type::Int, Type::Str, Type::Str],
                    Type::Unit,
                    "extern fn dom_add_listener(el: int, event: str, handler: str)"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let event = as_string(&args[1]);
                let handler = as_string(&args[2]);
                self.dom_listeners.insert((h, event), handler);
                Ok(Value::Unit)
            }
            "dom_remove_listener" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_remove_listener(el: int, event: str)"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let event = as_string(&args[1]);
                self.dom_listeners.remove(&(h, event));
                Ok(Value::Unit)
            }
            "dom_dispatch" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Unit,
                    "extern fn dom_dispatch(el: int, event: str)"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let event = as_string(&args[1]);
                self.dom_fire_event(h, &event, 0, 0, "", 0, "")?;
                Ok(Value::Unit)
            }
            "dom_dispatch_event" => {
                check_sig!(
                    vec![
                        Type::Int,
                        Type::Str,
                        Type::Int,
                        Type::Int,
                        Type::Str,
                        Type::Int,
                        Type::Str,
                    ],
                    Type::Unit,
                    "extern fn dom_dispatch_event(el: int, event: str, x: int, y: int, key: str, button: int, value: str)"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let event = as_string(&args[1]);
                let x = match &args[2] {
                    Value::Int(v) => *v,
                    _ => 0,
                };
                let y = match &args[3] {
                    Value::Int(v) => *v,
                    _ => 0,
                };
                let key = as_string(&args[4]);
                let button = match &args[5] {
                    Value::Int(v) => *v,
                    _ => 0,
                };
                let value = as_string(&args[6]);
                self.dom_fire_event(h, &event, x, y, &key, button, &value)?;
                Ok(Value::Unit)
            }
            "dom_last_event_type" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Str,
                    "extern fn dom_last_event_type() -> str"
                );
                Ok(Value::str_value(&self.dom_last_event_type.clone()))
            }
            "dom_last_event_target" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn dom_last_event_target() -> int"
                );
                Ok(Value::Int(self.dom_last_event_target))
            }
            "dom_last_event_x" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn dom_last_event_x() -> int"
                );
                Ok(Value::Int(self.dom_last_event_x))
            }
            "dom_last_event_y" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn dom_last_event_y() -> int"
                );
                Ok(Value::Int(self.dom_last_event_y))
            }
            "dom_last_event_key" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Str,
                    "extern fn dom_last_event_key() -> str"
                );
                Ok(Value::str_value(&self.dom_last_event_key))
            }
            "dom_last_event_button" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn dom_last_event_button() -> int"
                );
                Ok(Value::Int(self.dom_last_event_button))
            }
            "dom_last_event_value" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Str,
                    "extern fn dom_last_event_value() -> str"
                );
                Ok(Value::str_value(&self.dom_last_event_value))
            }
            "db_open" => {
                check_sig!(
                    vec![Type::Str, Type::Str],
                    Type::Int,
                    "extern fn db_open(driver: str, conn: str) -> int"
                );
                let driver = as_string(&args[0]);
                let conn = as_string(&args[1]);
                match crate::db_runtime::open(&driver, &conn) {
                    Ok(c) => {
                        let h = self.next_db;
                        self.next_db += 1;
                        self.db_conns.insert(h, c);
                        Ok(Value::Int(h))
                    }
                    Err(e) => Err(RuntimeError::new(format!("db_open: {}", e), call_span)),
                }
            }
            "db_close" => {
                check_sig!(vec![Type::Int], Type::Unit, "extern fn db_close(h: int)");
                if let Value::Int(h) = &args[0] {
                    self.db_conns.remove(h);
                }
                Ok(Value::Unit)
            }
            "db_exec" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn db_exec(h: int, sql: str) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let sql = as_string(&args[1]);
                let Some(conn) = self.db_conns.get_mut(&h) else {
                    return Ok(Value::str_value(
                        "{\"ok\":false,\"error\":\"invalid db handle\"}",
                    ));
                };
                Ok(Value::str_value(&crate::db_runtime::exec(conn, &sql)))
            }
            "db_query" => {
                check_sig!(
                    vec![Type::Int, Type::Str],
                    Type::Str,
                    "extern fn db_query(h: int, sql: str) -> str"
                );
                let h = match &args[0] {
                    Value::Int(h) => *h,
                    _ => 0,
                };
                let sql = as_string(&args[1]);
                let Some(conn) = self.db_conns.get_mut(&h) else {
                    return Ok(Value::str_value(
                        "{\"ok\":false,\"error\":\"invalid db handle\"}",
                    ));
                };
                Ok(Value::str_value(&crate::db_runtime::query(conn, &sql)))
            }
            "gc_collect" => {
                check_sig!(Vec::<Type>::new(), Type::Unit, "extern fn gc_collect()");
                self.gc_collect(&[]);
                Ok(Value::Unit)
            }
            "heap_live_count" => {
                check_sig!(
                    Vec::<Type>::new(),
                    Type::Int,
                    "extern fn heap_live_count() -> int"
                );
                Ok(Value::Int(self.heap_live_count() as i64))
            }
            other => Err(RuntimeError::new(
                format!(
                    "extern '{}' is not provided by the machino native runtime. Available externs: \
                     clock_ms, sleep_ms, files/env/http/args/exit/tcp_*, dom_*, db_open/db_close/db_exec/db_query. \
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
                Flow::Normal => {
                    self.maybe_collect_at_stmt(env);
                }
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
                    Value::Array(id) => {
                        let len = self.array_len(id);
                        if idx < 0 || idx as usize >= len {
                            return Err(RuntimeError::new(
                                format!(
                                    "index out of bounds: index {} but length is {}",
                                    idx, len
                                ),
                                index.span,
                            ));
                        }
                        if !self.array_set(id, idx as usize, v) {
                            return Err(RuntimeError::new(
                                "index assign on invalid array",
                                index.span,
                            ));
                        }
                        Ok(Flow::Normal)
                    }
                    _ => unreachable!("type checker guarantees array"),
                }
            }
            StmtKind::FieldAssign { base, field, value } => {
                let b = self.eval(base, env)?;
                let v = self.eval(value, env)?;
                match b {
                    Value::Struct(id) => {
                        if !self.struct_set(id, field, v) {
                            return Err(RuntimeError::new(
                                "field assign on invalid struct",
                                stmt.span,
                            ));
                        }
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
            StmtKind::While { cond, invariant: _, body } => {
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
        // check if this is an enum variant without payload (Enum::Variant)
        if let Some(colon_pos) = name.rfind("::") {
            let enum_name = &name[..colon_pos];
            let variant_name = &name[colon_pos + 2..];
            if let Some(variants) = self.enum_variants.get(enum_name) {
                if let Some(variant) = variants.iter().find(|v| v.name == variant_name) {
                    if variant.payloads.is_empty() {
                        return Ok(Value::EnumVariant(
                            enum_name.to_string(),
                            variant_name.to_string(),
                            Vec::new(),
                        ));
                    }
                }
            }
        }
        Err(RuntimeError::new(
            format!("unknown variable '{}'", name),
            span,
        ))
    }

    fn eval(&mut self, expr: &Expr, env: &mut Vec<HashMap<String, Value>>) -> RResult<Value> {
        self.push_env(env);
        let result = self.eval_expr(expr, env);
        self.pop_env();
        result
    }

    fn eval_expr(&mut self, expr: &Expr, env: &mut Vec<HashMap<String, Value>>) -> RResult<Value> {
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
                Ok(self.alloc_array(vals, Some(env)))
            }
            ExprKind::Index(base, index) => {
                let b = self.eval(base, env)?;
                let idx = match self.eval(index, env)? {
                    Value::Int(i) => i,
                    _ => unreachable!(),
                };
                match b {
                    Value::Array(id) => {
                        let len = self.array_len(id);
                        if idx < 0 || idx as usize >= len {
                            return Err(RuntimeError::new(
                                format!(
                                    "index out of bounds: index {} but length is {}",
                                    idx, len
                                ),
                                index.span,
                            ));
                        }
                        Ok(self
                            .array_get(id, idx as usize)
                            .expect("type checker guarantees in-bounds index"))
                    }
                    _ => unreachable!(),
                }
            }
            ExprKind::Field(base, field) => {
                let b = self.eval(base, env)?;
                match b {
                    Value::Struct(id) => Ok(self
                        .struct_get(id, field)
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
            ExprKind::Call(name, _type_args, args) => {
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
                        Value::Array(id) => Ok(Value::Int(self.array_len(*id) as i64)),
                        Value::Str(s) => Ok(Value::Int(s.len() as i64)),
                        _ => unreachable!(),
                    },
                    "push" => match &vals[0] {
                        Value::Array(id) => {
                            let mut new_vec = self.array_clone_data(*id);
                            new_vec.push(vals[1].clone());
                            Ok(self.alloc_array(new_vec, Some(env)))
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
                    "len_cp" => match &vals[0] {
                        Value::Str(s) => {
                            let st = std::str::from_utf8(s).map_err(|_| {
                                RuntimeError::new("invalid UTF-8 in string", expr.span)
                            })?;
                            Ok(Value::Int(st.chars().count() as i64))
                        }
                        _ => unreachable!(),
                    },
                    "char_at_cp" => match (&vals[0], &vals[1]) {
                        (Value::Str(s), Value::Int(i)) => {
                            let st = std::str::from_utf8(s).map_err(|_| {
                                RuntimeError::new("invalid UTF-8 in string", expr.span)
                            })?;
                            if *i < 0 {
                                return Err(RuntimeError::new(
                                    format!("codepoint index out of bounds: index {}", i),
                                    expr.span,
                                ));
                            }
                            st.chars()
                                .nth(*i as usize)
                                .map(|c| Value::Int(c as u32 as i64))
                                .ok_or_else(|| {
                                    RuntimeError::new(
                                        format!(
                                            "codepoint index out of bounds: index {} but length is {}",
                                            i,
                                            st.chars().count()
                                        ),
                                        expr.span,
                                    )
                                })
                        }
                        _ => unreachable!(),
                    },
                    "substr_cp" => match (&vals[0], &vals[1], &vals[2]) {
                        (Value::Str(s), Value::Int(a), Value::Int(b)) => {
                            let st = std::str::from_utf8(s).map_err(|_| {
                                RuntimeError::new("invalid UTF-8 in string", expr.span)
                            })?;
                            let chars: Vec<char> = st.chars().collect();
                            if *a < 0 || *a > *b || *b as usize > chars.len() {
                                return Err(RuntimeError::new(
                                    format!(
                                        "codepoint index out of bounds: [{}, {}) on a string of length {}",
                                        a,
                                        b,
                                        chars.len()
                                    ),
                                    expr.span,
                                ));
                            }
                            let out: String = chars[*a as usize..*b as usize].iter().collect();
                            Ok(Value::Str(Rc::new(out.into_bytes())))
                        }
                        _ => unreachable!(),
                    },
                    "chr_cp" => match vals[0] {
                        Value::Int(c) => {
                            if c < 0 || c > 0x10FFFF || (0xD800..=0xDFFF).contains(&c) {
                                return Err(RuntimeError::new(
                                    format!("invalid Unicode scalar value: {}", c),
                                    expr.span,
                                ));
                            }
                            let ch = char::from_u32(c as u32).ok_or_else(|| {
                                RuntimeError::new(
                                    format!("invalid Unicode scalar value: {}", c),
                                    expr.span,
                                )
                            })?;
                            let mut buf = [0u8; 4];
                            let encoded = ch.encode_utf8(&mut buf);
                            Ok(Value::Str(Rc::new(encoded.as_bytes().to_vec())))
                        }
                        _ => unreachable!(),
                    },
                    "hash" => match &vals[0] {
                        Value::Int(n) => {
                            let h = (*n as u64).wrapping_mul(11400714819323198485) % 1_000_000_007;
                            Ok(Value::Int(h as i64))
                        }
                        Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
                        Value::Str(s) => {
                            let mut h: i64 = 0;
                            for &b in s.iter() {
                                h = (h * 31 + b as i64).rem_euclid(1_000_000_007);
                            }
                            Ok(Value::Int(h))
                        }
                        _ => unreachable!(),
                    },
                    "chan_new" => {
                        let id = channel_new();
                        Ok(Value::Int(id))
                    }
                    "chan_close" => match vals[0] {
                        Value::Int(id) => {
                            channel_close(id).map_err(|m| RuntimeError::new(m, expr.span))?;
                            Ok(Value::Unit)
                        }
                        _ => unreachable!(),
                    },
                    "chan_send_int" | "chan_send_float" | "chan_send_bool" | "chan_send_str" => {
                        let Value::Int(id) = vals[0] else { unreachable!() };
                        channel_send(id, self.to_sendval(&vals[1]))
                            .map_err(|m| RuntimeError::new(m, expr.span))?;
                        Ok(Value::Unit)
                    }
                    "chan_recv_int" | "chan_recv_float" | "chan_recv_bool" | "chan_recv_str" => {
                        let Value::Int(id) = vals[0] else { unreachable!() };
                        let sv = channel_recv(id).map_err(|m| RuntimeError::new(m, expr.span))?;
                        let val = self.from_sendval(sv);
                        let ok = matches!(
                            (name.as_str(), &val),
                            ("chan_recv_int", Value::Int(_))
                                | ("chan_recv_float", Value::Float(_))
                                | ("chan_recv_bool", Value::Bool(_))
                                | ("chan_recv_str", Value::Str(_))
                        );
                        if !ok {
                            return Err(RuntimeError::new(
                                format!(
                                    "{}: channel delivered a value of a different type ({})",
                                    name,
                                    val.display()
                                ),
                                expr.span,
                            ));
                        }
                        Ok(val)
                    }
                    "spawn" => {
                        let Some(program) = SPAWN_PROGRAM.get() else {
                            return Err(RuntimeError::new(
                                "spawn is only available under 'machino run' and 'machino test'",
                                expr.span,
                            ));
                        };
                        let func = self.to_sendval(&vals[0]);
                        let send_args: Vec<SendVal> =
                            vals[1..].iter().map(|v| self.to_sendval(v)).collect();
                        let handle = std::thread::spawn(move || {
                            let mut interp = Interp::new(program);
                            let args: Vec<Value> = send_args
                                .into_iter()
                                .map(|sv| interp.from_sendval(sv))
                                .collect();
                            let callee = interp.from_sendval(func);
                            let result = interp
                                .call_value(&callee, args)
                                .map_err(|e| e.message)?;
                            Ok(interp.to_sendval(&result))
                        });
                        let id = self.next_task;
                        self.next_task += 1;
                        self.tasks.insert(id, handle);
                        Ok(Value::Int(id))
                    }
                    "join_int" | "join_float" | "join_bool" | "join_str" => {
                        let Value::Int(id) = vals[0] else { unreachable!() };
                        let handle = self.tasks.remove(&id).ok_or_else(|| {
                            RuntimeError::new(
                                format!("no running task with handle {}", id),
                                expr.span,
                            )
                        })?;
                        let joined = handle.join().map_err(|_| {
                            RuntimeError::new("spawned task panicked", expr.span)
                        })?;
                        let val = self.from_sendval(joined.map_err(|msg| {
                            RuntimeError::new(
                                format!("spawned task failed: {}", msg),
                                expr.span,
                            )
                        })?);
                        let ok = matches!(
                            (name.as_str(), &val),
                            ("join_int", Value::Int(_))
                                | ("join_float", Value::Float(_))
                                | ("join_bool", Value::Bool(_))
                                | ("join_str", Value::Str(_))
                        );
                        if !ok {
                            return Err(RuntimeError::new(
                                format!(
                                    "{}: task returned a value of a different type ({})",
                                    name,
                                    val.display()
                                ),
                                expr.span,
                            ));
                        }
                        Ok(val)
                    }
                    other => {
                        // check if this is an enum variant constructor (Enum::Variant)
                        if let Some(colon_pos) = other.rfind("::") {
                            let enum_name = &other[..colon_pos];
                            let variant_name = &other[colon_pos + 2..];
                            if let Some(variants) = self.enum_variants.get(enum_name) {
                                let variant = variants.iter().find(|v| v.name == variant_name);
                                if let Some(v) = variant {
                                    let n = v.payloads.len();
                                    if vals.len() != n {
                                        return Err(RuntimeError::new(
                                            format!(
                                                "{}::{} expects {} argument(s), found {}",
                                                enum_name, variant_name, n, vals.len()
                                            ),
                                            expr.span,
                                        ));
                                    }
                                    return Ok(Value::EnumVariant(
                                        enum_name.to_string(),
                                        variant_name.to_string(),
                                        vals,
                                    ));
                                }
                            }
                        }
                        if let Some(fields) = self.struct_fields.get(other) {
                            // struct constructor
                            let mut map = HashMap::with_capacity(fields.len());
                            for (fld, v) in fields.iter().zip(vals) {
                                map.insert(fld.name.clone(), v);
                            }
                            return Ok(self.alloc_struct(map, Some(env)));
                        }
                        let f = self.functions.get(other).copied().ok_or_else(|| {
                            RuntimeError::new(format!("unknown function '{}'", other), expr.span)
                        })?;
                        self.call_function(f, vals, expr.span)
                    }
                }
            }
            ExprKind::Match(m) => {
                let scrutinee = self.eval(&m.scrutinee, env)?;
                for arm in &m.arms {
                    if let Some(bindings) = self.match_pattern(&arm.pattern, &scrutinee) {
                        // pattern matched, evaluate body with bindings
                        env.push(bindings);
                        let result = self.eval(&arm.body, env)?;
                        env.pop();
                        return Ok(result);
                    }
                }
                // should be caught by exhaustiveness checking, but just in case:
                Err(RuntimeError::new(
                    "match expression did not match any pattern",
                    expr.span,
                ))
            }
        }
    }

    /// Try to match a pattern against a value. Returns Some(bindings) if match succeeds.
    fn match_pattern(&self, pattern: &Pattern, value: &Value) -> Option<HashMap<String, Value>> {
        match (pattern, value) {
            (Pattern::Wildcard, _) => Some(HashMap::new()),
            (Pattern::Var(name), v) => {
                let mut bindings = HashMap::new();
                bindings.insert(name.clone(), v.clone());
                Some(bindings)
            }
            (Pattern::Int(p), Value::Int(v)) if p == v => Some(HashMap::new()),
            (Pattern::Bool(p), Value::Bool(v)) if p == v => Some(HashMap::new()),
            (Pattern::Str(p), Value::Str(v)) if p.as_bytes() == v.as_slice() => Some(HashMap::new()),
            (Pattern::Variant(enum_name, variant_name), Value::EnumVariant(e, v, payloads)) => {
                if enum_name == e && variant_name == v && payloads.is_empty() {
                    Some(HashMap::new())
                } else {
                    None
                }
            }
            (Pattern::VariantPayload(enum_name, variant_name, inners), Value::EnumVariant(e, v, payloads)) => {
                if enum_name == e && variant_name == v && inners.len() == payloads.len() {
                    let mut bindings = HashMap::new();
                    for (inner, pval) in inners.iter().zip(payloads.iter()) {
                        match self.match_pattern(inner, pval) {
                            Some(b) => bindings.extend(b),
                            None => return None,
                        }
                    }
                    Some(bindings)
                } else {
                    None
                }
            }
            _ => None,
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
            StmtKind::While { cond, invariant: _, body } => {
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
        ExprKind::Call(_, _, args) => {
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
