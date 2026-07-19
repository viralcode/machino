//! WASM-GC backend (`machino build --gc`): compiles to the WebAssembly GC
//! proposal, using reference-typed GC arrays instead of the linear-memory
//! mark-sweep collector in wasm.rs. The host's collector manages all memory.
//!
//! Supported subset (diagnostic E070 otherwise, with the construct named):
//!   - int (i64), float (f64), bool (i32)
//!   - str as an immutable GC byte array: literals, +, ==/!=, len,
//!     char_at, substr, chr
//!   - [int] and [float]: literals, indexing, index-assign, len, push
//!   - all control flow (if/while/for/break/continue), recursion, asserts
//!   - requires/ensures contracts (enforced with traps)
//!   - print for int/float/bool/str
//!
//! Not yet in the subset: structs, enums/match, closures, arrays of
//! references, string externs (files/sockets). Use the default backend for
//! those. Integer arithmetic wraps (the default backend traps on overflow).
//!
//! Run the output with `node runners/run-gc.mjs out.wasm` (Node 22+ / any
//! host with WASM GC).

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

// value types
const I32: u8 = 0x7f;
const I64: u8 = 0x7e;
const F64: u8 = 0x7c;
const REF_NULL: u8 = 0x63; // (ref null $t) followed by type index

// GC type indices (fixed layout in the type section)
const TY_BYTES: u32 = 0; // array (mut i8)   — strings
const TY_ARR_I64: u32 = 1; // array (mut i64)
const TY_ARR_F64: u32 = 2; // array (mut f64)
const FIRST_FUNC_TYPE: u32 = 3;

// opcodes
const OP_UNREACHABLE: u8 = 0x00;
const OP_BLOCK: u8 = 0x02;
const OP_LOOP: u8 = 0x03;
const OP_IF: u8 = 0x04;
const OP_ELSE: u8 = 0x05;
const OP_END: u8 = 0x0b;
const OP_BR: u8 = 0x0c;
const OP_BR_IF: u8 = 0x0d;
const OP_RETURN: u8 = 0x0f;
const OP_CALL: u8 = 0x10;
const OP_DROP: u8 = 0x1a;
const OP_LOCAL_GET: u8 = 0x20;
const OP_LOCAL_SET: u8 = 0x21;
const OP_LOCAL_TEE: u8 = 0x22;
const OP_I32_CONST: u8 = 0x41;
const OP_I64_CONST: u8 = 0x42;
const OP_F64_CONST: u8 = 0x44;
const OP_I32_EQZ: u8 = 0x45;
const OP_I32_EQ: u8 = 0x46;
const OP_I32_NE: u8 = 0x47;
const OP_I64_EQZ: u8 = 0x50;
const OP_I64_EQ: u8 = 0x51;
const OP_I64_NE: u8 = 0x52;
const OP_I64_LT_S: u8 = 0x53;
const OP_I64_GT_S: u8 = 0x55;
const OP_I64_LE_S: u8 = 0x57;
const OP_I64_GE_S: u8 = 0x59;
const OP_F64_EQ: u8 = 0x61;
const OP_F64_NE: u8 = 0x62;
const OP_F64_LT: u8 = 0x63;
const OP_F64_GT: u8 = 0x64;
const OP_F64_LE: u8 = 0x65;
const OP_F64_GE: u8 = 0x66;
const OP_I32_ADD: u8 = 0x6a;
const OP_I64_ADD: u8 = 0x7c;
const OP_I64_SUB: u8 = 0x7d;
const OP_I64_MUL: u8 = 0x7e;
const OP_I64_DIV_S: u8 = 0x7f;
const OP_I64_REM_S: u8 = 0x81;
const OP_F64_NEG: u8 = 0x9a;
const OP_F64_ADD: u8 = 0xa0;
const OP_F64_SUB: u8 = 0xa1;
const OP_F64_MUL: u8 = 0xa2;
const OP_F64_DIV: u8 = 0xa3;
const OP_I32_WRAP_I64: u8 = 0xa7;
const OP_I64_EXTEND_I32_S: u8 = 0xac;
const OP_I64_TRUNC_F64_S: u8 = 0xb0;
const OP_F64_CONVERT_I64_S: u8 = 0xb9;
const GC_PREFIX: u8 = 0xfb;
const GC_ARRAY_NEW_DEFAULT: u8 = 0x07;
const GC_ARRAY_NEW_FIXED: u8 = 0x08;
const GC_ARRAY_NEW_DATA: u8 = 0x09;
const GC_ARRAY_GET: u8 = 0x0b;
const GC_ARRAY_GET_U: u8 = 0x0d;
const GC_ARRAY_SET: u8 = 0x0e;
const GC_ARRAY_LEN: u8 = 0x0f;
const GC_ARRAY_COPY: u8 = 0x11;

fn uleb(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut n: i64) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        let sign = b & 0x40 != 0;
        if (n == 0 && !sign) || (n == -1 && sign) {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn section(module: &mut Vec<u8>, id: u8, payload: &[u8]) {
    module.push(id);
    uleb(module, payload.len() as u64);
    module.extend_from_slice(payload);
}

fn e070(what: &str, span: Span) -> Diagnostic {
    Diagnostic::new(
        "E070",
        format!("{} is not yet supported by the WASM-GC backend", what),
        span,
    )
    .with_help("use the default backend (machino build, without --gc) for the full language")
}

/// Writes the machino type as a WASM value type. Returns None for Unit.
fn valtype(ty: &Type, span: Span) -> Result<Option<Vec<u8>>, Diagnostic> {
    Ok(match ty {
        Type::Int => Some(vec![I64]),
        Type::Float => Some(vec![F64]),
        Type::Bool => Some(vec![I32]),
        Type::Str => Some(vec![REF_NULL, TY_BYTES as u8]),
        Type::Array(inner) => match inner.as_ref() {
            Type::Int => Some(vec![REF_NULL, TY_ARR_I64 as u8]),
            Type::Float => Some(vec![REF_NULL, TY_ARR_F64 as u8]),
            other => return Err(e070(&format!("arrays of '{}'", other), span)),
        },
        Type::Unit => None,
        other => return Err(e070(&format!("the type '{}'", other), span)),
    })
}

fn array_type_index(elem: &Type, span: Span) -> Result<u32, Diagnostic> {
    match elem {
        Type::Int => Ok(TY_ARR_I64),
        Type::Float => Ok(TY_ARR_F64),
        other => Err(e070(&format!("arrays of '{}'", other), span)),
    }
}

/// (import index space) host imports, in order.
const IMPORTS: &[(&str, &[u8], &[u8])] = &[
    // (name, param valtypes, result valtypes) — refs written as 2 bytes
    ("print_i64", &[I64], &[]),
    ("print_f64", &[F64], &[]),
    ("print_bool", &[I32], &[]),
    ("print_str", &[REF_NULL, TY_BYTES as u8], &[]),
];
const IMP_PRINT_I64: u32 = 0;
const IMP_PRINT_F64: u32 = 1;
const IMP_PRINT_BOOL: u32 = 2;
const IMP_PRINT_STR: u32 = 3;
const N_IMPORTS: u32 = 4;

// helper functions generated by the compiler, placed before user functions
const N_HELPERS: u32 = 7;
const HELP_CONCAT: u32 = N_IMPORTS; // (bytes, bytes) -> bytes
const HELP_STREQ: u32 = N_IMPORTS + 1; // (bytes, bytes) -> i32
const HELP_SUBSTR: u32 = N_IMPORTS + 2; // (bytes, i64, i64) -> bytes
const HELP_PUSH_I64: u32 = N_IMPORTS + 3; // (arr_i64, i64) -> arr_i64
const HELP_PUSH_F64: u32 = N_IMPORTS + 4; // (arr_f64, f64) -> arr_f64
const HELP_STR_LEN: u32 = N_IMPORTS + 5; // (bytes) -> i32, exported for the host
const HELP_STR_AT: u32 = N_IMPORTS + 6; // (bytes, i32) -> i32, exported for the host

pub fn compile(program: &Program) -> Result<Vec<u8>, Diagnostic> {
    // ---- reachability from main (dead-code elimination) ----
    let by_name: HashMap<&str, &Function> = program
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    let main = by_name.get("main").copied().ok_or_else(|| {
        Diagnostic::new(
            "E047",
            "'machino build --gc' requires a 'fn main()' entry point",
            Span::new(0, 0),
        )
    })?;
    let mut reachable: Vec<&Function> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack = vec![main];
    seen.insert("main");
    while let Some(f) = stack.pop() {
        reachable.push(f);
        let mut callees: Vec<String> = Vec::new();
        collect_calls_stmts(&f.body, &mut callees);
        for c in f.requires.iter().chain(f.ensures.iter()) {
            collect_calls_expr(&c.expr, &mut callees);
        }
        for callee in callees {
            if let Some(g) = by_name.get(callee.as_str()) {
                if seen.insert(g.name.as_str()) {
                    stack.push(g);
                }
            }
        }
    }
    // stable order: as declared in the program
    reachable.sort_by_key(|f| f.span.start);
    for f in &reachable {
        if f.is_extern {
            return Err(e070(&format!("the extern function '{}'", f.name), f.span));
        }
    }

    let mut c = Compiler {
        func_index: HashMap::new(),
        signatures: HashMap::new(),
        func_types: Vec::new(),
        data_segments: Vec::new(),
    };
    for (i, f) in reachable.iter().enumerate() {
        c.func_index
            .insert(f.name.clone(), N_IMPORTS + N_HELPERS + i as u32);
        c.signatures.insert(
            f.name.clone(),
            (
                f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(),
                f.ret.clone(),
            ),
        );
    }

    // ---- compile function bodies first (they register func types/data) ----
    let helper_bodies = c.helper_bodies();
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut type_indices: Vec<u32> = Vec::new();
    for f in &reachable {
        let (type_idx, body) = c.compile_function(f)?;
        type_indices.push(type_idx);
        bodies.push(body);
    }

    // ---- assemble the module ----
    let mut module = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    // type section: 3 array types + helper func types + user func types
    let helper_types: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (
            vec![REF_NULL, TY_BYTES as u8, REF_NULL, TY_BYTES as u8],
            vec![REF_NULL, TY_BYTES as u8],
        ), // concat
        (
            vec![REF_NULL, TY_BYTES as u8, REF_NULL, TY_BYTES as u8],
            vec![I32],
        ), // streq
        (
            vec![REF_NULL, TY_BYTES as u8, I64, I64],
            vec![REF_NULL, TY_BYTES as u8],
        ), // substr
        (
            vec![REF_NULL, TY_ARR_I64 as u8, I64],
            vec![REF_NULL, TY_ARR_I64 as u8],
        ), // push_i64
        (
            vec![REF_NULL, TY_ARR_F64 as u8, F64],
            vec![REF_NULL, TY_ARR_F64 as u8],
        ), // push_f64
        (vec![REF_NULL, TY_BYTES as u8], vec![I32]), // str_len
        (vec![REF_NULL, TY_BYTES as u8, I32], vec![I32]), // str_at
    ];
    let mut tsec = Vec::new();
    let n_types = 3 + IMPORTS.len() + helper_types.len() + c.func_types.len();
    uleb(&mut tsec, n_types as u64);
    // 0: bytes (array (mut i8))
    tsec.extend_from_slice(&[0x5e, 0x78, 0x01]);
    // 1: array (mut i64)
    tsec.extend_from_slice(&[0x5e, I64, 0x01]);
    // 2: array (mut f64)
    tsec.extend_from_slice(&[0x5e, F64, 0x01]);
    // import func types (indices 3..)
    let mut import_type_idx = Vec::new();
    for (_, params, results) in IMPORTS {
        import_type_idx.push(3 + import_type_idx.len() as u32);
        tsec.push(0x60);
        uleb(&mut tsec, count_valtypes(params) as u64);
        tsec.extend_from_slice(params);
        uleb(&mut tsec, count_valtypes(results) as u64);
        tsec.extend_from_slice(results);
    }
    // helper func types
    let helper_type_base = 3 + IMPORTS.len() as u32;
    for (params, results) in &helper_types {
        tsec.push(0x60);
        uleb(&mut tsec, count_valtypes(params) as u64);
        tsec.extend_from_slice(params);
        uleb(&mut tsec, count_valtypes(results) as u64);
        tsec.extend_from_slice(results);
    }
    // user func types
    let user_type_base = helper_type_base + helper_types.len() as u32;
    for (params, results) in &c.func_types {
        tsec.push(0x60);
        uleb(&mut tsec, count_valtypes(params) as u64);
        tsec.extend_from_slice(params);
        uleb(&mut tsec, count_valtypes(results) as u64);
        tsec.extend_from_slice(results);
    }
    section(&mut module, 1, &tsec);

    // import section
    let mut isec = Vec::new();
    uleb(&mut isec, IMPORTS.len() as u64);
    for (i, (name, _, _)) in IMPORTS.iter().enumerate() {
        uleb(&mut isec, 3);
        isec.extend_from_slice(b"env");
        uleb(&mut isec, name.len() as u64);
        isec.extend_from_slice(name.as_bytes());
        isec.push(0x00); // func
        uleb(&mut isec, import_type_idx[i] as u64);
    }
    section(&mut module, 2, &isec);

    // function section: helpers then user functions
    let mut fsec = Vec::new();
    uleb(&mut fsec, (N_HELPERS as usize + reachable.len()) as u64);
    for i in 0..N_HELPERS {
        uleb(&mut fsec, (helper_type_base + i) as u64);
    }
    for t in &type_indices {
        uleb(&mut fsec, (user_type_base + t) as u64);
    }
    section(&mut module, 3, &fsec);

    // export section: main + string accessors so the host can decode strings
    let mut esec = Vec::new();
    uleb(&mut esec, 3);
    let main_idx = c.func_index["main"];
    for (name, idx) in [
        ("main", main_idx),
        ("str_len", HELP_STR_LEN),
        ("str_at", HELP_STR_AT),
    ] {
        uleb(&mut esec, name.len() as u64);
        esec.extend_from_slice(name.as_bytes());
        esec.push(0x00);
        uleb(&mut esec, idx as u64);
    }
    section(&mut module, 7, &esec);

    // data count section (required when array.new_data is used)
    let mut dcsec = Vec::new();
    uleb(&mut dcsec, c.data_segments.len() as u64);
    section(&mut module, 12, &dcsec);

    // code section
    let mut csec = Vec::new();
    uleb(&mut csec, (N_HELPERS as usize + bodies.len()) as u64);
    for b in helper_bodies.iter().chain(bodies.iter()) {
        uleb(&mut csec, b.len() as u64);
        csec.extend_from_slice(b);
    }
    section(&mut module, 10, &csec);

    // data section (passive segments for string literals)
    let mut dsec = Vec::new();
    uleb(&mut dsec, c.data_segments.len() as u64);
    for seg in &c.data_segments {
        uleb(&mut dsec, 1); // passive
        uleb(&mut dsec, seg.len() as u64);
        dsec.extend_from_slice(seg);
    }
    section(&mut module, 11, &dsec);

    Ok(module)
}

/// Counts value types in a flat encoding (refs take 2 bytes).
fn count_valtypes(bytes: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < bytes.len() {
        n += 1;
        i += if bytes[i] == REF_NULL { 2 } else { 1 };
    }
    n
}

fn collect_calls_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. }
            | StmtKind::Assign { value, .. }
            | StmtKind::Assert(value)
            | StmtKind::Expr(value)
            | StmtKind::Return(Some(value)) => collect_calls_expr(value, out),
            StmtKind::IndexAssign { base, index, value } => {
                collect_calls_expr(base, out);
                collect_calls_expr(index, out);
                collect_calls_expr(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                collect_calls_expr(base, out);
                collect_calls_expr(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_calls_expr(cond, out);
                collect_calls_stmts(then_body, out);
                collect_calls_stmts(else_body, out);
            }
            StmtKind::While { cond, body } => {
                collect_calls_expr(cond, out);
                collect_calls_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                collect_calls_expr(start, out);
                collect_calls_expr(end, out);
                collect_calls_stmts(body, out);
            }
            _ => {}
        }
    }
}

fn collect_calls_expr(e: &Expr, out: &mut Vec<String>) {
    match &e.kind {
        ExprKind::Call(name, args) => {
            out.push(name.clone());
            for a in args {
                collect_calls_expr(a, out);
            }
        }
        ExprKind::Var(name) => out.push(name.clone()),
        ExprKind::Array(elems) => elems.iter().for_each(|e| collect_calls_expr(e, out)),
        ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
            collect_calls_expr(a, out);
            collect_calls_expr(b, out);
        }
        ExprKind::Field(a, _) | ExprKind::Un(_, a) => collect_calls_expr(a, out),
        ExprKind::Lambda(l) => collect_calls_stmts(&l.body, out),
        ExprKind::Match(m) => {
            collect_calls_expr(&m.scrutinee, out);
            for arm in &m.arms {
                collect_calls_expr(&arm.body, out);
            }
        }
        _ => {}
    }
}

struct Compiler {
    func_index: HashMap<String, u32>,
    signatures: HashMap<String, (Vec<Type>, Type)>,
    /// user function types (params bytes, results bytes), deduped by content
    func_types: Vec<(Vec<u8>, Vec<u8>)>,
    data_segments: Vec<Vec<u8>>,
}

struct Frame {
    kind: FrameKind,
}

enum FrameKind {
    Block,
    Loop,
    If,
    /// target for `break`
    LoopExit,
    /// target for `continue`
    LoopContinue,
    /// function-level block used when ensures clauses are present
    FuncExit,
}

struct Ctx {
    /// scoped name -> (local index, type)
    scopes: Vec<HashMap<String, (u32, Type)>>,
    locals: Vec<Vec<u8>>, // valtype encoding per local (params included)
    n_params: u32,
    frames: Vec<Frame>,
    ret: Type,
    /// local holding the return value when ensures are enforced
    result_local: Option<u32>,
}

impl Ctx {
    fn lookup(&self, name: &str) -> Option<(u32, Type)> {
        self.scopes
            .iter()
            .rev()
            .find_map(|m| m.get(name).cloned())
    }
    fn add_local(&mut self, name: &str, ty: Type, enc: Vec<u8>) -> u32 {
        let idx = self.locals.len() as u32;
        self.locals.push(enc);
        self.scopes
            .last_mut()
            .unwrap()
            .insert(name.to_string(), (idx, ty));
        idx
    }
    fn add_temp(&mut self, enc: Vec<u8>) -> u32 {
        let idx = self.locals.len() as u32;
        self.locals.push(enc);
        idx
    }
    /// br depth from the innermost frame to the given frame predicate
    fn depth_to(&self, pred: impl Fn(&FrameKind) -> bool) -> Option<u32> {
        for (i, f) in self.frames.iter().rev().enumerate() {
            if pred(&f.kind) {
                return Some(i as u32);
            }
        }
        None
    }
}

impl Compiler {
    fn func_type_index(&mut self, params: Vec<u8>, results: Vec<u8>) -> u32 {
        for (i, ft) in self.func_types.iter().enumerate() {
            if ft.0 == params && ft.1 == results {
                return i as u32;
            }
        }
        self.func_types.push((params, results));
        (self.func_types.len() - 1) as u32
    }

    fn intern_string(&mut self, s: &str) -> u32 {
        for (i, seg) in self.data_segments.iter().enumerate() {
            if seg.as_slice() == s.as_bytes() {
                return i as u32;
            }
        }
        self.data_segments.push(s.as_bytes().to_vec());
        (self.data_segments.len() - 1) as u32
    }

    fn compile_function(&mut self, f: &Function) -> Result<(u32, Vec<u8>), Diagnostic> {
        let mut param_enc = Vec::new();
        for p in &f.params {
            let Some(vt) = valtype(&p.ty, p.span)? else {
                return Err(e070("unit-typed parameters", p.span));
            };
            param_enc.extend_from_slice(&vt);
        }
        let ret_enc = match valtype(&f.ret, f.span)? {
            Some(vt) => vt,
            None => Vec::new(),
        };
        let type_idx = self.func_type_index(param_enc, ret_enc.clone());

        let mut ctx = Ctx {
            scopes: vec![HashMap::new()],
            locals: Vec::new(),
            n_params: f.params.len() as u32,
            frames: Vec::new(),
            ret: f.ret.clone(),
            result_local: None,
        };
        for p in &f.params {
            let enc = valtype(&p.ty, p.span)?.unwrap();
            ctx.add_local(&p.name, p.ty.clone(), enc);
        }

        let mut code = Vec::new();

        // requires: trap on violation
        for c in &f.requires {
            self.compile_expr(&mut code, &c.expr, &mut ctx, None)?;
            code.push(OP_I32_EQZ);
            code.push(OP_IF);
            code.push(0x40);
            code.push(OP_UNREACHABLE);
            code.push(OP_END);
        }

        let has_ensures = !f.ensures.is_empty();
        if has_ensures {
            let enc = valtype(&f.ret, f.span)?.unwrap();
            let result_local = ctx.add_temp(enc);
            ctx.result_local = Some(result_local);
            // function body runs inside a block; `return e` sets the result
            // local and branches here, then the ensures run
            code.push(OP_BLOCK);
            code.push(0x40);
            ctx.frames.push(Frame {
                kind: FrameKind::FuncExit,
            });
        }

        let falls_through = self.compile_block(&mut code, &f.body, &mut ctx)?;

        if has_ensures {
            if falls_through {
                // checker guarantees all paths return when ret != Unit
                code.push(OP_UNREACHABLE);
            }
            code.push(OP_END);
            ctx.frames.pop();
            let result_local = ctx.result_local.unwrap();
            // bind `result` for the ensures expressions
            ctx.scopes
                .last_mut()
                .unwrap()
                .insert("result".to_string(), (result_local, f.ret.clone()));
            for c in &f.ensures {
                self.compile_expr(&mut code, &c.expr, &mut ctx, None)?;
                code.push(OP_I32_EQZ);
                code.push(OP_IF);
                code.push(0x40);
                code.push(OP_UNREACHABLE);
                code.push(OP_END);
            }
            code.push(OP_LOCAL_GET);
            uleb(&mut code, result_local as u64);
            code.push(OP_RETURN);
        } else if f.ret != Type::Unit && falls_through {
            code.push(OP_UNREACHABLE);
        }
        code.push(OP_END);

        // assemble: locals declaration + code
        let mut body = Vec::new();
        let extra: Vec<&Vec<u8>> = ctx.locals[ctx.n_params as usize..].iter().collect();
        // group consecutive identical valtypes
        let mut groups: Vec<(u64, Vec<u8>)> = Vec::new();
        for enc in extra {
            match groups.last_mut() {
                Some((n, e)) if e == enc => *n += 1,
                _ => groups.push((1, enc.clone())),
            }
        }
        uleb(&mut body, groups.len() as u64);
        for (n, enc) in groups {
            uleb(&mut body, n);
            body.extend_from_slice(&enc);
        }
        body.extend_from_slice(&code);
        Ok((type_idx, body))
    }

    /// Compiles a block of statements. Returns whether control can fall
    /// through the end.
    fn compile_block(
        &mut self,
        code: &mut Vec<u8>,
        stmts: &[Stmt],
        ctx: &mut Ctx,
    ) -> Result<bool, Diagnostic> {
        ctx.scopes.push(HashMap::new());
        let mut live = true;
        for s in stmts {
            if !live {
                break; // unreachable code was already checked; skip it
            }
            live = self.compile_stmt(code, s, ctx)?;
        }
        ctx.scopes.pop();
        Ok(live)
    }

    /// Returns false if the statement definitely transfers control away.
    fn compile_stmt(
        &mut self,
        code: &mut Vec<u8>,
        stmt: &Stmt,
        ctx: &mut Ctx,
    ) -> Result<bool, Diagnostic> {
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let vty = match ty {
                    Some(t) => t.clone(),
                    None => self.expr_type(value, ctx)?,
                };
                self.compile_expr(code, value, ctx, Some(&vty))?;
                let enc = valtype(&vty, stmt.span)?
                    .ok_or_else(|| e070("unit-typed bindings", stmt.span))?;
                let idx = ctx.add_local(name, vty, enc);
                code.push(OP_LOCAL_SET);
                uleb(code, idx as u64);
                Ok(true)
            }
            StmtKind::Assign { name, value } => {
                let (idx, ty) = ctx
                    .lookup(name)
                    .ok_or_else(|| e070("assignment to an unknown variable", stmt.span))?;
                self.compile_expr(code, value, ctx, Some(&ty))?;
                code.push(OP_LOCAL_SET);
                uleb(code, idx as u64);
                Ok(true)
            }
            StmtKind::IndexAssign { base, index, value } => {
                let bty = self.expr_type(base, ctx)?;
                let Type::Array(elem) = bty else {
                    return Err(e070("index-assignment on this type", stmt.span));
                };
                let arr_ty = array_type_index(&elem, stmt.span)?;
                self.compile_expr(code, base, ctx, None)?;
                self.compile_expr(code, index, ctx, None)?;
                code.push(OP_I32_WRAP_I64);
                self.compile_expr(code, value, ctx, Some(&elem))?;
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_SET);
                uleb(code, arr_ty as u64);
                Ok(true)
            }
            StmtKind::FieldAssign { .. } => Err(e070("structs", stmt.span)),
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                self.compile_expr(code, cond, ctx, None)?;
                code.push(OP_IF);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::If,
                });
                let then_live = self.compile_block(code, then_body, ctx)?;
                let else_live = if !else_body.is_empty() {
                    code.push(OP_ELSE);
                    self.compile_block(code, else_body, ctx)?
                } else {
                    true
                };
                ctx.frames.pop();
                code.push(OP_END);
                Ok(then_live || else_live)
            }
            StmtKind::While { cond, body } => {
                // block $exit { loop $top { !cond -> br $exit; body; br $top } }
                code.push(OP_BLOCK);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::LoopExit,
                });
                code.push(OP_LOOP);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::LoopContinue,
                });
                self.compile_expr(code, cond, ctx, None)?;
                code.push(OP_I32_EQZ);
                code.push(OP_BR_IF);
                uleb(code, 1);
                let live = self.compile_block(code, body, ctx)?;
                if live {
                    code.push(OP_BR);
                    uleb(code, 0);
                }
                ctx.frames.pop();
                code.push(OP_END);
                ctx.frames.pop();
                code.push(OP_END);
                Ok(true)
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                ctx.scopes.push(HashMap::new());
                let ivar = ctx.add_local(var, Type::Int, vec![I64]);
                let end_tmp = ctx.add_temp(vec![I64]);
                self.compile_expr(code, start, ctx, None)?;
                code.push(OP_LOCAL_SET);
                uleb(code, ivar as u64);
                self.compile_expr(code, end, ctx, None)?;
                code.push(OP_LOCAL_SET);
                uleb(code, end_tmp as u64);
                // block $exit { loop $top { i >= end -> br $exit;
                //   block $cont { body } ; i += 1; br $top } }
                code.push(OP_BLOCK);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::LoopExit,
                });
                code.push(OP_LOOP);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::Loop,
                });
                code.push(OP_LOCAL_GET);
                uleb(code, ivar as u64);
                code.push(OP_LOCAL_GET);
                uleb(code, end_tmp as u64);
                code.push(OP_I64_GE_S);
                code.push(OP_BR_IF);
                uleb(code, 1);
                code.push(OP_BLOCK);
                code.push(0x40);
                ctx.frames.push(Frame {
                    kind: FrameKind::LoopContinue,
                });
                let live = self.compile_block(code, body, ctx)?;
                let _ = live;
                ctx.frames.pop();
                code.push(OP_END);
                // increment and loop (depth 0 = the enclosing loop)
                code.push(OP_LOCAL_GET);
                uleb(code, ivar as u64);
                code.push(OP_I64_CONST);
                sleb(code, 1);
                code.push(OP_I64_ADD);
                code.push(OP_LOCAL_SET);
                uleb(code, ivar as u64);
                code.push(OP_BR);
                uleb(code, 0);
                ctx.frames.pop();
                code.push(OP_END);
                ctx.frames.pop();
                code.push(OP_END);
                ctx.scopes.pop();
                Ok(true)
            }
            StmtKind::Break => {
                let depth = ctx
                    .depth_to(|k| matches!(k, FrameKind::LoopExit))
                    .ok_or_else(|| e070("'break' outside a loop", stmt.span))?;
                code.push(OP_BR);
                uleb(code, depth as u64);
                Ok(false)
            }
            StmtKind::Continue => {
                let depth = ctx
                    .depth_to(|k| matches!(k, FrameKind::LoopContinue))
                    .ok_or_else(|| e070("'continue' outside a loop", stmt.span))?;
                code.push(OP_BR);
                uleb(code, depth as u64);
                Ok(false)
            }
            StmtKind::Return(value) => {
                if let Some(result_local) = ctx.result_local {
                    // route through the ensures epilogue
                    if let Some(e) = value {
                        self.compile_expr(code, e, ctx, Some(&ctx.ret.clone()))?;
                        code.push(OP_LOCAL_SET);
                        uleb(code, result_local as u64);
                    }
                    let depth = ctx
                        .depth_to(|k| matches!(k, FrameKind::FuncExit))
                        .expect("ensures block frame");
                    code.push(OP_BR);
                    uleb(code, depth as u64);
                } else {
                    if let Some(e) = value {
                        self.compile_expr(code, e, ctx, Some(&ctx.ret.clone()))?;
                    }
                    code.push(OP_RETURN);
                }
                Ok(false)
            }
            StmtKind::Assert(cond) => {
                self.compile_expr(code, cond, ctx, None)?;
                code.push(OP_I32_EQZ);
                code.push(OP_IF);
                code.push(0x40);
                code.push(OP_UNREACHABLE);
                code.push(OP_END);
                Ok(true)
            }
            StmtKind::Expr(e) => {
                let ty = self.expr_type(e, ctx)?;
                self.compile_expr(code, e, ctx, None)?;
                if ty != Type::Unit {
                    code.push(OP_DROP);
                }
                Ok(true)
            }
        }
    }

    /// Static type of an expression (the program is already fully checked
    /// and monomorphized, so this cannot fail on well-typed input).
    fn expr_type(&self, expr: &Expr, ctx: &Ctx) -> Result<Type, Diagnostic> {
        Ok(match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(_) => Type::Str,
            ExprKind::Var(name) => {
                ctx.lookup(name)
                    .map(|(_, t)| t)
                    .ok_or_else(|| e070("first-class function values", expr.span))?
            }
            ExprKind::Array(elems) => {
                let elem = match elems.first() {
                    Some(e) => self.expr_type(e, ctx)?,
                    None => Type::Int, // refined by the Let annotation
                };
                Type::Array(Box::new(elem))
            }
            ExprKind::Index(base, _) => match self.expr_type(base, ctx)? {
                Type::Array(e) => *e,
                _ => return Err(e070("indexing this type", expr.span)),
            },
            ExprKind::Field(_, _) => return Err(e070("structs", expr.span)),
            ExprKind::Bin(op, lhs, _) => {
                use BinOp::*;
                match op {
                    Add | Sub | Mul | Div | Mod => self.expr_type(lhs, ctx)?,
                    _ => Type::Bool,
                }
            }
            ExprKind::Un(op, inner) => match op {
                UnOp::Neg => self.expr_type(inner, ctx)?,
                UnOp::Not => Type::Bool,
            },
            ExprKind::Call(name, args) => match name.as_str() {
                "print" => Type::Unit,
                "len" | "char_at" | "to_int" => Type::Int,
                "to_float" => Type::Float,
                "substr" | "chr" => Type::Str,
                "push" => self.expr_type(&args[0], ctx)?,
                _ => {
                    let (_, ret) = self
                        .signatures
                        .get(name)
                        .ok_or_else(|| e070(&format!("the call to '{}'", name), expr.span))?;
                    ret.clone()
                }
            },
            ExprKind::Lambda(_) => return Err(e070("closures", expr.span)),
            ExprKind::Match(_) => return Err(e070("match expressions", expr.span)),
        })
    }

    fn compile_expr(
        &mut self,
        code: &mut Vec<u8>,
        expr: &Expr,
        ctx: &mut Ctx,
        expected: Option<&Type>,
    ) -> Result<(), Diagnostic> {
        match &expr.kind {
            ExprKind::Int(n) => {
                code.push(OP_I64_CONST);
                sleb(code, *n);
            }
            ExprKind::Float(v) => {
                code.push(OP_F64_CONST);
                code.extend_from_slice(&v.to_le_bytes());
            }
            ExprKind::Bool(b) => {
                code.push(OP_I32_CONST);
                sleb(code, if *b { 1 } else { 0 });
            }
            ExprKind::Str(s) => {
                let seg = self.intern_string(s);
                code.push(OP_I32_CONST);
                sleb(code, 0); // offset in segment
                code.push(OP_I32_CONST);
                sleb(code, s.len() as i64);
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_NEW_DATA);
                uleb(code, TY_BYTES as u64);
                uleb(code, seg as u64);
            }
            ExprKind::Var(name) => {
                let (idx, _) = ctx
                    .lookup(name)
                    .ok_or_else(|| e070("first-class function values", expr.span))?;
                code.push(OP_LOCAL_GET);
                uleb(code, idx as u64);
            }
            ExprKind::Array(elems) => {
                let elem_ty = if let Some(first) = elems.first() {
                    self.expr_type(first, ctx)?
                } else {
                    match expected {
                        Some(Type::Array(e)) => e.as_ref().clone(),
                        _ => Type::Int,
                    }
                };
                let arr_ty = array_type_index(&elem_ty, expr.span)?;
                for e in elems {
                    self.compile_expr(code, e, ctx, Some(&elem_ty))?;
                }
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_NEW_FIXED);
                uleb(code, arr_ty as u64);
                uleb(code, elems.len() as u64);
            }
            ExprKind::Index(base, index) => {
                let bty = self.expr_type(base, ctx)?;
                match bty {
                    Type::Array(elem) => {
                        let arr_ty = array_type_index(&elem, expr.span)?;
                        self.compile_expr(code, base, ctx, None)?;
                        self.compile_expr(code, index, ctx, None)?;
                        code.push(OP_I32_WRAP_I64);
                        code.push(GC_PREFIX);
                        code.push(GC_ARRAY_GET);
                        uleb(code, arr_ty as u64);
                    }
                    _ => return Err(e070("indexing this type", expr.span)),
                }
            }
            ExprKind::Field(_, _) => return Err(e070("structs", expr.span)),
            ExprKind::Un(op, inner) => match op {
                UnOp::Neg => {
                    let ty = self.expr_type(inner, ctx)?;
                    match ty {
                        Type::Int => {
                            code.push(OP_I64_CONST);
                            sleb(code, 0);
                            self.compile_expr(code, inner, ctx, None)?;
                            code.push(OP_I64_SUB);
                        }
                        Type::Float => {
                            self.compile_expr(code, inner, ctx, None)?;
                            code.push(OP_F64_NEG);
                        }
                        _ => return Err(e070("negating this type", expr.span)),
                    }
                }
                UnOp::Not => {
                    self.compile_expr(code, inner, ctx, None)?;
                    code.push(OP_I32_EQZ);
                }
            },
            ExprKind::Bin(op, lhs, rhs) => {
                use BinOp::*;
                // short-circuit && and ||
                if matches!(op, And | Or) {
                    self.compile_expr(code, lhs, ctx, None)?;
                    code.push(OP_IF);
                    code.push(I32);
                    if matches!(op, And) {
                        self.compile_expr(code, rhs, ctx, None)?;
                        code.push(OP_ELSE);
                        code.push(OP_I32_CONST);
                        sleb(code, 0);
                    } else {
                        code.push(OP_I32_CONST);
                        sleb(code, 1);
                        code.push(OP_ELSE);
                        self.compile_expr(code, rhs, ctx, None)?;
                    }
                    code.push(OP_END);
                    return Ok(());
                }
                let lt = self.expr_type(lhs, ctx)?;
                // string concat / equality use helpers
                if lt == Type::Str {
                    self.compile_expr(code, lhs, ctx, None)?;
                    self.compile_expr(code, rhs, ctx, None)?;
                    match op {
                        Add => {
                            code.push(OP_CALL);
                            uleb(code, HELP_CONCAT as u64);
                        }
                        Eq => {
                            code.push(OP_CALL);
                            uleb(code, HELP_STREQ as u64);
                        }
                        Ne => {
                            code.push(OP_CALL);
                            uleb(code, HELP_STREQ as u64);
                            code.push(OP_I32_EQZ);
                        }
                        _ => return Err(e070("this string operator", expr.span)),
                    }
                    return Ok(());
                }
                self.compile_expr(code, lhs, ctx, None)?;
                self.compile_expr(code, rhs, ctx, None)?;
                let opcode = match (&lt, op) {
                    (Type::Int, Add) => OP_I64_ADD,
                    (Type::Int, Sub) => OP_I64_SUB,
                    (Type::Int, Mul) => OP_I64_MUL,
                    (Type::Int, Div) => OP_I64_DIV_S,
                    (Type::Int, Mod) => OP_I64_REM_S,
                    (Type::Int, Eq) => OP_I64_EQ,
                    (Type::Int, Ne) => OP_I64_NE,
                    (Type::Int, Lt) => OP_I64_LT_S,
                    (Type::Int, Le) => OP_I64_LE_S,
                    (Type::Int, Gt) => OP_I64_GT_S,
                    (Type::Int, Ge) => OP_I64_GE_S,
                    (Type::Float, Add) => OP_F64_ADD,
                    (Type::Float, Sub) => OP_F64_SUB,
                    (Type::Float, Mul) => OP_F64_MUL,
                    (Type::Float, Div) => OP_F64_DIV,
                    (Type::Float, Eq) => OP_F64_EQ,
                    (Type::Float, Ne) => OP_F64_NE,
                    (Type::Float, Lt) => OP_F64_LT,
                    (Type::Float, Le) => OP_F64_LE,
                    (Type::Float, Gt) => OP_F64_GT,
                    (Type::Float, Ge) => OP_F64_GE,
                    (Type::Bool, Eq) => OP_I32_EQ,
                    (Type::Bool, Ne) => OP_I32_NE,
                    _ => return Err(e070("this operator/type combination", expr.span)),
                };
                code.push(opcode);
            }
            ExprKind::Call(name, args) => match name.as_str() {
                "print" => {
                    let aty = self.expr_type(&args[0], ctx)?;
                    self.compile_expr(code, &args[0], ctx, None)?;
                    let import = match aty {
                        Type::Int => IMP_PRINT_I64,
                        Type::Float => IMP_PRINT_F64,
                        Type::Bool => IMP_PRINT_BOOL,
                        Type::Str => IMP_PRINT_STR,
                        other => return Err(e070(&format!("printing '{}'", other), expr.span)),
                    };
                    code.push(OP_CALL);
                    uleb(code, import as u64);
                }
                "len" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_LEN);
                    code.push(OP_I64_EXTEND_I32_S);
                }
                "push" => {
                    let aty = self.expr_type(&args[0], ctx)?;
                    let helper = match &aty {
                        Type::Array(e) if **e == Type::Int => HELP_PUSH_I64,
                        Type::Array(e) if **e == Type::Float => HELP_PUSH_F64,
                        other => return Err(e070(&format!("push on '{}'", other), expr.span)),
                    };
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, helper as u64);
                }
                "to_float" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_F64_CONVERT_I64_S);
                }
                "to_int" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_I64_TRUNC_F64_S);
                }
                "char_at" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, None)?;
                    code.push(OP_I32_WRAP_I64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_GET_U);
                    uleb(code, TY_BYTES as u64);
                    code.push(OP_I64_EXTEND_I32_S);
                }
                "substr" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, None)?;
                    self.compile_expr(code, &args[2], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, HELP_SUBSTR as u64);
                }
                "chr" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_I32_WRAP_I64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_FIXED);
                    uleb(code, TY_BYTES as u64);
                    uleb(code, 1);
                }
                _ => {
                    let idx = *self
                        .func_index
                        .get(name)
                        .ok_or_else(|| e070(&format!("the call to '{}'", name), expr.span))?;
                    let (params, _) = self.signatures.get(name).cloned().unwrap();
                    for (a, p) in args.iter().zip(params.iter()) {
                        self.compile_expr(code, a, ctx, Some(p))?;
                    }
                    code.push(OP_CALL);
                    uleb(code, idx as u64);
                }
            },
            ExprKind::Lambda(_) => return Err(e070("closures", expr.span)),
            ExprKind::Match(_) => return Err(e070("match expressions", expr.span)),
        }
        Ok(())
    }

    /// Hand-assembled helper function bodies (concat, streq, substr, push).
    fn helper_bodies(&self) -> Vec<Vec<u8>> {
        vec![
            self.body_concat(),
            self.body_streq(),
            self.body_substr(),
            self.body_push(TY_ARR_I64, I64),
            self.body_push(TY_ARR_F64, F64),
            self.body_str_len(),
            self.body_str_at(),
        ]
    }

    /// str_len(s: bytes) -> i32 (host accessor)
    fn body_str_len(&self) -> Vec<u8> {
        let mut b = vec![0]; // no extra locals
        b.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_RETURN, OP_END]);
        b
    }

    /// str_at(s: bytes, i: i32) -> i32 (host accessor)
    fn body_str_at(&self) -> Vec<u8> {
        let mut b = vec![0];
        b.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8,
            OP_RETURN, OP_END,
        ]);
        b
    }

    /// concat(a: bytes, b: bytes) -> bytes
    /// locals: 2=la(i32), 3=lb(i32), 4=out(ref bytes)
    fn body_concat(&self) -> Vec<u8> {
        let mut b = Vec::new();
        // locals: 2 x i32, 1 x ref
        b.extend_from_slice(&[2, 2, I32, 1, REF_NULL, TY_BYTES as u8]);
        let mut c = Vec::new();
        // la = len(a); lb = len(b)
        c.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_SET, 2]);
        c.extend_from_slice(&[OP_LOCAL_GET, 1, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_SET, 3]);
        // out = array.new_default bytes (la+lb)
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_LOCAL_GET, 3, OP_I32_ADD]);
        c.extend_from_slice(&[GC_PREFIX, GC_ARRAY_NEW_DEFAULT, TY_BYTES as u8, OP_LOCAL_SET, 4]);
        // array.copy out 0 a 0 la
        c.extend_from_slice(&[
            OP_LOCAL_GET, 4, OP_I32_CONST, 0, OP_LOCAL_GET, 0, OP_I32_CONST, 0, OP_LOCAL_GET, 2,
            GC_PREFIX, GC_ARRAY_COPY, TY_BYTES as u8, TY_BYTES as u8,
        ]);
        // array.copy out la b 0 lb
        c.extend_from_slice(&[
            OP_LOCAL_GET, 4, OP_LOCAL_GET, 2, OP_LOCAL_GET, 1, OP_I32_CONST, 0, OP_LOCAL_GET, 3,
            GC_PREFIX, GC_ARRAY_COPY, TY_BYTES as u8, TY_BYTES as u8,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, 4, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    /// streq(a: bytes, b: bytes) -> i32
    /// locals: 2=la(i32), 3=i(i32)
    fn body_streq(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[1, 2, I32]);
        let mut c = Vec::new();
        // if len(a) != len(b) return 0
        c.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_TEE, 2]);
        c.extend_from_slice(&[OP_LOCAL_GET, 1, GC_PREFIX, GC_ARRAY_LEN, OP_I32_NE]);
        c.extend_from_slice(&[OP_IF, 0x40, OP_I32_CONST, 0, OP_RETURN, OP_END]);
        // i = 0; loop: if i >= la return 1; if a[i] != b[i] return 0; i++
        c.extend_from_slice(&[OP_I32_CONST, 0, OP_LOCAL_SET, 3]);
        c.extend_from_slice(&[OP_LOOP, 0x40]);
        // i >= la -> return 1
        c.extend_from_slice(&[OP_LOCAL_GET, 3, OP_LOCAL_GET, 2, 0x4e /* i32.ge_s */]);
        c.extend_from_slice(&[OP_IF, 0x40, OP_I32_CONST, 1, OP_RETURN, OP_END]);
        // a[i] != b[i] -> return 0
        c.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_LOCAL_GET, 3, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8,
        ]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, 1, OP_LOCAL_GET, 3, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8,
        ]);
        c.extend_from_slice(&[OP_I32_NE, OP_IF, 0x40, OP_I32_CONST, 0, OP_RETURN, OP_END]);
        // i++ ; continue
        c.extend_from_slice(&[
            OP_LOCAL_GET, 3, OP_I32_CONST, 1, OP_I32_ADD, OP_LOCAL_SET, 3, OP_BR, 0, OP_END,
        ]);
        c.extend_from_slice(&[OP_I32_CONST, 1, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    /// substr(s: bytes, start: i64, end: i64) -> bytes
    /// locals: 3=st(i32), 4=n(i32), 5=out(ref bytes)
    fn body_substr(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[2, 2, I32, 1, REF_NULL, TY_BYTES as u8]);
        let mut c = Vec::new();
        // st = wrap(start); n = wrap(end) - st
        c.extend_from_slice(&[OP_LOCAL_GET, 1, OP_I32_WRAP_I64, OP_LOCAL_SET, 3]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, 2, OP_I32_WRAP_I64, OP_LOCAL_GET, 3, 0x6b /* i32.sub */, OP_LOCAL_SET, 4,
        ]);
        // out = array.new_default bytes n
        c.extend_from_slice(&[
            OP_LOCAL_GET, 4, GC_PREFIX, GC_ARRAY_NEW_DEFAULT, TY_BYTES as u8, OP_LOCAL_SET, 5,
        ]);
        // array.copy out 0 s st n  (traps on out-of-bounds)
        c.extend_from_slice(&[
            OP_LOCAL_GET, 5, OP_I32_CONST, 0, OP_LOCAL_GET, 0, OP_LOCAL_GET, 3, OP_LOCAL_GET, 4,
            GC_PREFIX, GC_ARRAY_COPY, TY_BYTES as u8, TY_BYTES as u8,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, 5, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    /// push(arr, v) -> new array with v appended (machino push is copying)
    /// locals: 2=n(i32), 3=out(ref arr)
    fn body_push(&self, arr_ty: u32, elem_vt: u8) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[2, 1, I32, 1, REF_NULL, arr_ty as u8]);
        let mut c = Vec::new();
        // n = len(arr)
        c.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_SET, 2]);
        // out = array.new_default (n + 1)
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I32_CONST, 1, OP_I32_ADD]);
        c.extend_from_slice(&[GC_PREFIX, GC_ARRAY_NEW_DEFAULT, arr_ty as u8, OP_LOCAL_SET, 3]);
        // array.copy out 0 arr 0 n
        c.extend_from_slice(&[
            OP_LOCAL_GET, 3, OP_I32_CONST, 0, OP_LOCAL_GET, 0, OP_I32_CONST, 0, OP_LOCAL_GET, 2,
            GC_PREFIX, GC_ARRAY_COPY, arr_ty as u8, arr_ty as u8,
        ]);
        // out[n] = v
        c.extend_from_slice(&[OP_LOCAL_GET, 3, OP_LOCAL_GET, 2, OP_LOCAL_GET, 1]);
        c.extend_from_slice(&[GC_PREFIX, GC_ARRAY_SET, arr_ty as u8]);
        c.extend_from_slice(&[OP_LOCAL_GET, 3, OP_RETURN, OP_END]);
        let _ = elem_vt;
        b.extend_from_slice(&c);
        b
    }
}
