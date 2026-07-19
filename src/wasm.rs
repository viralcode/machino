//! WebAssembly backend. Emits a standard, self-contained .wasm binary
//! (version 1 + bulk-memory) with no external toolchain.
//!
//! Value representation (one wasm value per machino value):
//!   int    -> i64
//!   bool   -> i64 (0 or 1)
//!   float  -> f64
//!   str / [T] / struct / fn -> i64 pointer to a heap object
//!
//! Every heap object has a uniform 16-byte header for the garbage collector:
//!   word 0 (meta):   bits 0..2 tag, bit 3 mark, bits 4.. count
//!   word 1 (bitmap): for TAG_STRUCT, bit i set if payload word i is a pointer;
//!   for TAG_BIGSTRUCT (>60 payload words) it holds the address of a static
//!   multi-word bitmap in the data segment instead
//!   payload at +16
//! Tags: 0 = bytes (str, count = byte length), 1 = array of scalars,
//! 2 = array of pointers (count = elements), 3 = struct/closure (count =
//! payload words), 6 = free block (count = total block size in bytes).
//!
//! Function values are closure objects: [header][table_slot][captures...].
//! Named functions used as values get a static singleton closure plus a
//! wrapper in the function table that drops the environment argument.
//! Indirect calls pass the closure pointer as a hidden trailing parameter.
//!
//! Memory management is a precise mark-sweep garbage collector:
//! - Pointer-typed variables are mirrored into shadow-stack frames in linear
//!   memory; pointer-typed temporaries that must survive a potentially
//!   GC-ing evaluation are rooted in per-statement frame slots. Objects
//!   never move, so operand-stack copies stay valid.
//! - GC runs only at safepoints (function entry and loop back-edges), when
//!   allocation since the last collection exceeds an adaptive threshold.
//! - The allocator serves from a free list built by the sweep phase, then
//!   bumps, then grows memory (capped at 1 GiB).
//!
//! Integer arithmetic is checked: overflow, division by zero, and modulo by
//! zero call the host `fail` import with a message and trap — identical
//! behavior to the reference interpreter.
//!
//! Host interface (module "env"):
//!   fail(msg: i64)        called before trapping on contract/bounds failures
//!   print_i64 / print_f64 / print_bool / print_str
//!   ...plus every `extern fn` the program declares.
//! The module exports "memory", "alloc" (so hosts can pass strings/arrays
//! in; hosts must write the 16-byte header), and every non-std user function.

use crate::ast::*;
use crate::diag::line_col;
use std::collections::{BTreeMap, HashMap, HashSet};

// ---- encoding primitives ----

fn uleb(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        buf.push(b);
        if v == 0 {
            break;
        }
    }
}

fn sleb(buf: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        let sign_clear = b & 0x40 == 0;
        if (v == 0 && sign_clear) || (v == -1 && !sign_clear) {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

const VT_I32: u8 = 0x7F;
const VT_I64: u8 = 0x7E;
const VT_F64: u8 = 0x7C;
const BLOCK_VOID: u8 = 0x40;

// object tags
const TAG_BYTES: i64 = 0;
const TAG_ARR: i64 = 1; // array of scalars
const TAG_ARRP: i64 = 2; // array of pointers
const TAG_STRUCT: i64 = 3; // struct or closure, inline pointer bitmap
/// Struct/closure with more than [`INLINE_BITMAP_WORDS`] payload words: header
/// word 1 holds the address of a static multi-word pointer bitmap in the data
/// segment instead of an inline bitmap. Same size/layout otherwise.
const TAG_BIGSTRUCT: i64 = 4;
const TAG_FREE: i64 = 6; // free block (count = total size in bytes)

/// Payload-word limit for the inline (single-word) pointer bitmap.
const INLINE_BITMAP_WORDS: usize = 60;
const MARK: i64 = 8;
const HDR: i64 = 16;

// mutable globals
const G_HP: u32 = 0; // i32 heap bump pointer
const G_SSP: u32 = 1; // i32 shadow stack pointer
const G_FREELIST: u32 = 2; // i64 free-list head (0 = empty)
const G_ALLOC: u32 = 3; // i64 bytes allocated since last GC
const G_THRESH: u32 = 4; // i64 GC trigger threshold
const G_MSBASE: u32 = 5; // i64 mark-stack base (set during GC)
const G_MSTOP: u32 = 6; // i64 mark-stack element count
// immutable globals (values patched in at assembly time)
const G_HEAP_BASE: u32 = 7; // i32
const G_SHADOW_BASE: u32 = 8; // i32

/// Default shadow-stack size; `machino build --stack-mib N` overrides.
const DEFAULT_SHADOW_BYTES: u32 = 16 * 1024 * 1024;
const MAX_PAGES: u64 = 65536; // 4 GiB (the wasm32 address-space maximum)

fn valtype(ty: &Type) -> u8 {
    match ty {
        Type::Float => VT_F64,
        _ => VT_I64,
    }
}

fn is_ptr(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Str | Type::Array(_) | Type::Struct(_) | Type::Fn(_, _)
    )
}

// ---- function body builder ----

struct FnBuilder {
    n_params: u32,
    locals: Vec<u8>,
    code: Vec<u8>,
}

#[allow(dead_code)]
impl FnBuilder {
    fn new(n_params: u32) -> Self {
        FnBuilder {
            n_params,
            locals: Vec::new(),
            code: Vec::new(),
        }
    }

    fn new_local(&mut self, vt: u8) -> u32 {
        self.locals.push(vt);
        self.n_params + self.locals.len() as u32 - 1
    }

    fn op(&mut self, b: u8) {
        self.code.push(b);
    }
    fn unreachable(&mut self) {
        self.op(0x00);
    }
    fn block_void(&mut self) {
        self.op(0x02);
        self.op(BLOCK_VOID);
    }
    fn block_typed(&mut self, vt: u8) {
        self.op(0x02);
        self.op(vt);
    }
    fn loop_void(&mut self) {
        self.op(0x03);
        self.op(BLOCK_VOID);
    }
    fn if_void(&mut self) {
        self.op(0x04);
        self.op(BLOCK_VOID);
    }
    fn if_typed(&mut self, vt: u8) {
        self.op(0x04);
        self.op(vt);
    }
    fn else_(&mut self) {
        self.op(0x05);
    }
    fn end(&mut self) {
        self.op(0x0B);
    }
    fn br(&mut self, depth: u32) {
        self.op(0x0C);
        uleb(&mut self.code, depth as u64);
    }
    fn br_if(&mut self, depth: u32) {
        self.op(0x0D);
        uleb(&mut self.code, depth as u64);
    }
    fn ret(&mut self) {
        self.op(0x0F);
    }
    fn call(&mut self, idx: u32) {
        self.op(0x10);
        uleb(&mut self.code, idx as u64);
    }
    fn call_indirect(&mut self, type_idx: u32) {
        self.op(0x11);
        uleb(&mut self.code, type_idx as u64);
        self.op(0x00);
    }
    fn drop_(&mut self) {
        self.op(0x1A);
    }
    fn local_get(&mut self, idx: u32) {
        self.op(0x20);
        uleb(&mut self.code, idx as u64);
    }
    fn local_set(&mut self, idx: u32) {
        self.op(0x21);
        uleb(&mut self.code, idx as u64);
    }
    fn local_tee(&mut self, idx: u32) {
        self.op(0x22);
        uleb(&mut self.code, idx as u64);
    }
    fn global_get(&mut self, idx: u32) {
        self.op(0x23);
        uleb(&mut self.code, idx as u64);
    }
    fn global_set(&mut self, idx: u32) {
        self.op(0x24);
        uleb(&mut self.code, idx as u64);
    }
    fn memarg(&mut self, align: u32, offset: u32) {
        uleb(&mut self.code, align as u64);
        uleb(&mut self.code, offset as u64);
    }
    fn i64_load(&mut self, offset: u32) {
        self.op(0x29);
        self.memarg(3, offset);
    }
    fn f64_load(&mut self, offset: u32) {
        self.op(0x2B);
        self.memarg(3, offset);
    }
    fn i64_load8_u(&mut self, offset: u32) {
        self.op(0x31);
        self.memarg(0, offset);
    }
    fn i64_store(&mut self, offset: u32) {
        self.op(0x37);
        self.memarg(3, offset);
    }
    fn f64_store(&mut self, offset: u32) {
        self.op(0x39);
        self.memarg(3, offset);
    }
    fn i64_store8(&mut self, offset: u32) {
        self.op(0x3C);
        self.memarg(0, offset);
    }
    fn memory_size(&mut self) {
        self.op(0x3F);
        self.op(0x00);
    }
    fn memory_grow(&mut self) {
        self.op(0x40);
        self.op(0x00);
    }
    fn i32_const(&mut self, v: i32) {
        self.op(0x41);
        sleb(&mut self.code, v as i64);
    }
    /// Emits an i32.const with a fixed 5-byte payload that can be patched
    /// later. Returns the payload position within `code`.
    fn i32_const_patchable(&mut self) -> usize {
        self.op(0x41);
        let pos = self.code.len();
        self.code
            .extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x00]);
        pos
    }
    fn patch_i32(&mut self, pos: usize, v: i32) {
        let v = v as u32;
        self.code[pos] = (v & 0x7F) as u8 | 0x80;
        self.code[pos + 1] = ((v >> 7) & 0x7F) as u8 | 0x80;
        self.code[pos + 2] = ((v >> 14) & 0x7F) as u8 | 0x80;
        self.code[pos + 3] = ((v >> 21) & 0x7F) as u8 | 0x80;
        self.code[pos + 4] = ((v >> 28) & 0x0F) as u8;
    }
    fn i64_const(&mut self, v: i64) {
        self.op(0x42);
        sleb(&mut self.code, v);
    }
    fn f64_const(&mut self, v: f64) {
        self.op(0x44);
        self.code.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn i32_eqz(&mut self) {
        self.op(0x45);
    }
    fn i32_eq(&mut self) {
        self.op(0x46);
    }
    fn i32_lt_u(&mut self) {
        self.op(0x49);
    }
    fn i32_gt_u(&mut self) {
        self.op(0x4B);
    }
    fn i32_add(&mut self) {
        self.op(0x6A);
    }
    fn i32_and(&mut self) {
        self.op(0x71);
    }
    fn i32_or(&mut self) {
        self.op(0x72);
    }
    fn i64_eqz(&mut self) {
        self.op(0x50);
    }
    fn i64_eq(&mut self) {
        self.op(0x51);
    }
    fn i64_ne(&mut self) {
        self.op(0x52);
    }
    fn i64_lt_s(&mut self) {
        self.op(0x53);
    }
    fn i64_lt_u(&mut self) {
        self.op(0x54);
    }
    fn i64_gt_s(&mut self) {
        self.op(0x55);
    }
    fn i64_gt_u(&mut self) {
        self.op(0x56);
    }
    fn i64_le_s(&mut self) {
        self.op(0x57);
    }
    fn i64_ge_s(&mut self) {
        self.op(0x59);
    }
    fn i64_ge_u(&mut self) {
        self.op(0x5A);
    }
    fn f64_eq(&mut self) {
        self.op(0x61);
    }
    fn f64_ne(&mut self) {
        self.op(0x62);
    }
    fn f64_lt(&mut self) {
        self.op(0x63);
    }
    fn f64_gt(&mut self) {
        self.op(0x64);
    }
    fn f64_le(&mut self) {
        self.op(0x65);
    }
    fn f64_ge(&mut self) {
        self.op(0x66);
    }
    fn i64_add(&mut self) {
        self.op(0x7C);
    }
    fn i64_sub(&mut self) {
        self.op(0x7D);
    }
    fn i64_mul(&mut self) {
        self.op(0x7E);
    }
    fn i64_div_s(&mut self) {
        self.op(0x7F);
    }
    fn i64_rem_s(&mut self) {
        self.op(0x81);
    }
    fn i64_and(&mut self) {
        self.op(0x83);
    }
    fn i64_or(&mut self) {
        self.op(0x84);
    }
    fn i64_xor(&mut self) {
        self.op(0x85);
    }
    fn i64_shl(&mut self) {
        self.op(0x86);
    }
    fn i64_shr_u(&mut self) {
        self.op(0x88);
    }
    fn f64_neg(&mut self) {
        self.op(0x9A);
    }
    fn f64_add(&mut self) {
        self.op(0xA0);
    }
    fn f64_sub(&mut self) {
        self.op(0xA1);
    }
    fn f64_mul(&mut self) {
        self.op(0xA2);
    }
    fn f64_div(&mut self) {
        self.op(0xA3);
    }
    fn i32_wrap_i64(&mut self) {
        self.op(0xA7);
    }
    fn i64_extend_i32_u(&mut self) {
        self.op(0xAD);
    }
    fn i64_trunc_f64_s(&mut self) {
        self.op(0xB0);
    }
    fn f64_convert_i64_s(&mut self) {
        self.op(0xB9);
    }
    fn memory_copy(&mut self) {
        self.op(0xFC);
        uleb(&mut self.code, 0x0A);
        self.op(0x00);
        self.op(0x00);
    }
    fn memory_fill(&mut self) {
        self.op(0xFC);
        uleb(&mut self.code, 0x0B);
        self.op(0x00);
    }

    fn finish(self) -> Vec<u8> {
        let mut body = Vec::new();
        let mut groups: Vec<(u32, u8)> = Vec::new();
        for vt in &self.locals {
            match groups.last_mut() {
                Some((n, t)) if t == vt => *n += 1,
                _ => groups.push((1, *vt)),
            }
        }
        uleb(&mut body, groups.len() as u64);
        for (n, vt) in groups {
            uleb(&mut body, n as u64);
            body.push(vt);
        }
        body.extend_from_slice(&self.code);
        body.push(0x0B);
        let mut out = Vec::new();
        uleb(&mut out, body.len() as u64);
        out.extend_from_slice(&body);
        out
    }
}

// ---- compiler ----

const IMP_FAIL: u32 = 0;
const IMP_PRINT_I64: u32 = 1;
const IMP_PRINT_F64: u32 = 2;
const IMP_PRINT_BOOL: u32 = 3;
const IMP_PRINT_STR: u32 = 4;
const N_RUNTIME_IMPORTS: u32 = 5;
const N_HELPERS: u32 = 21;

pub struct WasmCompiler<'a> {
    program: &'a Program,
    source: &'a str,
    signatures: HashMap<&'a str, (Vec<Type>, Type)>,
    struct_fields: HashMap<&'a str, &'a [Param]>,
    struct_bitmap: HashMap<&'a str, Vec<u64>>,
    enum_variants: HashMap<&'a str, &'a [EnumVariant]>,
    types: Vec<Vec<u8>>,
    type_index: HashMap<Vec<u8>, u32>,
    data: Vec<u8>,
    string_addrs: HashMap<String, u32>,
    singleton_addrs: HashMap<String, u32>,
    func_index: HashMap<&'a str, u32>,
    // closures
    lambdas: BTreeMap<usize, &'a Lambda>,
    lambda_index: HashMap<usize, u32>,
    lambda_slot: HashMap<usize, u32>,
    lambda_captures: HashMap<usize, Vec<(String, Type)>>,
    wrapper_slot: HashMap<&'a str, u32>,
    n_imports: u32,
    // helpers
    h_alloc: u32,
    h_concat: u32,
    h_str_eq: u32,
    h_push_i64: u32,
    h_push_f64: u32,
    h_get_i64: u32,
    h_get_f64: u32,
    h_set_i64: u32,
    h_set_f64: u32,
    h_iadd: u32,
    h_isub: u32,
    h_imul: u32,
    h_idiv: u32,
    h_irem: u32,
    h_char_at: u32,
    h_substr: u32,
    h_chr: u32,
    h_obj_size: u32,
    h_mark_push: u32,
    h_gc_collect: u32,
    h_gc_maybe: u32,
    shadow_bytes: u32,
    // concurrency imports (present only when the program uses spawn/join)
    imp_spawn: u32,
    imp_join_i64: u32,
    imp_join_f64: u32,
    imp_join_str: u32,
    /// Functions passed to spawn(); exported so worker instances can call
    /// them by name.
    spawn_targets: std::collections::BTreeSet<String>,
}

type Scope = Vec<HashMap<String, (u32, Type)>>;

struct FnCtx {
    b: FnBuilder,
    nesting: u32,
    loops: Vec<(u32, u32)>,
    result_local: Option<u32>,
    ret: Type,
    /// i32 local holding the shadow-stack frame base.
    fb: u32,
    /// scratch i64 local for pointer plumbing.
    scratch: u32,
    named_next: u32,
    temp_next: u32,
    frame_high: u32,
}

impl FnCtx {
    fn alloc_named(&mut self) -> u32 {
        let s = self.named_next;
        self.named_next += 1;
        self.temp_next = self.temp_next.max(self.named_next);
        self.frame_high = self.frame_high.max(self.named_next);
        s
    }
    fn alloc_temp(&mut self) -> u32 {
        let s = self.temp_next;
        self.temp_next += 1;
        self.frame_high = self.frame_high.max(self.temp_next);
        s
    }
}

enum Saved {
    Scalar(u32),
    Ptr(u32),
}

pub fn compile(program: &Program, source: &str) -> Vec<u8> {
    compile_with_stack(program, source, DEFAULT_SHADOW_BYTES)
}

/// Compile with an explicit shadow-stack size (for `--stack-mib`).
pub fn compile_with_stack(program: &Program, source: &str, shadow_bytes: u32) -> Vec<u8> {
    let mut c = WasmCompiler::new(program, source);
    c.shadow_bytes = shadow_bytes;
    c.compile()
}

/// Conservative: an expression "can GC" if evaluating it may reach a
/// safepoint — i.e. it contains any call. Lambda bodies don't run at
/// creation, so they are not descended into.
fn can_gc(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Call(_, _) => true,
        ExprKind::Array(elems) => elems.iter().any(can_gc),
        ExprKind::Index(a, b) => can_gc(a) || can_gc(b),
        ExprKind::Field(a, _) => can_gc(a),
        ExprKind::Bin(_, a, b) => can_gc(a) || can_gc(b),
        ExprKind::Un(_, a) => can_gc(a),
        ExprKind::Match(m) => {
            can_gc(&m.scrutinee) || m.arms.iter().any(|arm| can_gc(&arm.body))
        }
        _ => false,
    }
}

/// One-character marshalling tag for the task_spawn host protocol.
fn sig_char(ty: &Type) -> char {
    match ty {
        Type::Int => 'i',
        Type::Bool => 'b',
        Type::Float => 'f',
        Type::Str => 's',
        _ => 'p',
    }
}

/// True if any reachable function (or test) calls spawn/join.
fn program_uses_concurrency(program: &Program, reachable: &HashSet<String>) -> bool {
    const CONC: &[&str] = &["spawn", "join_int", "join_float", "join_bool", "join_str"];
    let mut names: Vec<&str> = Vec::new();
    for f in &program.functions {
        if f.is_extern || !reachable.contains(&f.name) {
            continue;
        }
        collect_fn_names_stmts(&f.body, &mut names);
    }
    for t in &program.tests {
        collect_fn_names_stmts(&t.body, &mut names);
    }
    names.iter().any(|n| CONC.contains(n))
}

/// Pointer bitmap for a struct's payload, one u64 per 64 fields.
fn pointer_bitmap_words(fields: &[Param]) -> Vec<u64> {
    let mut words = vec![0u64; fields.len().div_ceil(64).max(1)];
    for (i, fld) in fields.iter().enumerate() {
        if is_ptr(&fld.ty) {
            words[i / 64] |= 1 << (i % 64);
        }
    }
    words
}

fn reachable_functions(program: &Program) -> HashSet<String> {
    let by_name: HashMap<&str, &Function> = program
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut work: Vec<&str> = Vec::new();
    for f in &program.functions {
        if !f.is_std {
            seen.insert(f.name.clone());
            work.push(f.name.as_str());
        }
    }
    while let Some(name) = work.pop() {
        let Some(f) = by_name.get(name) else { continue };
        let mut names: Vec<&str> = Vec::new();
        for c in f.requires.iter().chain(f.ensures.iter()) {
            collect_fn_names(&c.expr, &mut names);
        }
        collect_fn_names_stmts(&f.body, &mut names);
        for n in names {
            if by_name.contains_key(n) && seen.insert(n.to_string()) {
                work.push(by_name.get_key_value(n).unwrap().0);
            }
        }
    }
    seen
}

fn collect_fn_names_stmts<'e>(stmts: &'e [Stmt], out: &mut Vec<&'e str>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Assign { value, .. } => {
                collect_fn_names(value, out)
            }
            StmtKind::IndexAssign { base, index, value } => {
                collect_fn_names(base, out);
                collect_fn_names(index, out);
                collect_fn_names(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                collect_fn_names(base, out);
                collect_fn_names(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_fn_names(cond, out);
                collect_fn_names_stmts(then_body, out);
                collect_fn_names_stmts(else_body, out);
            }
            StmtKind::While { cond, body } => {
                collect_fn_names(cond, out);
                collect_fn_names_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                collect_fn_names(start, out);
                collect_fn_names(end, out);
                collect_fn_names_stmts(body, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                collect_fn_names(e, out)
            }
            _ => {}
        }
    }
}

fn collect_fn_names<'e>(expr: &'e Expr, out: &mut Vec<&'e str>) {
    match &expr.kind {
        ExprKind::Var(name) => out.push(name),
        ExprKind::Array(elems) => {
            for e in elems {
                collect_fn_names(e, out);
            }
        }
        ExprKind::Index(a, b) => {
            collect_fn_names(a, out);
            collect_fn_names(b, out);
        }
        ExprKind::Field(a, _) => collect_fn_names(a, out),
        ExprKind::Bin(_, a, b) => {
            collect_fn_names(a, out);
            collect_fn_names(b, out);
        }
        ExprKind::Un(_, a) => collect_fn_names(a, out),
        ExprKind::Call(name, args) => {
            out.push(name);
            for a in args {
                collect_fn_names(a, out);
            }
        }
        ExprKind::Lambda(l) => collect_fn_names_stmts(&l.body, out),
        ExprKind::Match(m) => {
            collect_fn_names(&m.scrutinee, out);
            for arm in &m.arms {
                collect_fn_names(&arm.body, out);
            }
        }
        _ => {}
    }
}

fn collect_lambdas_stmts<'e>(stmts: &'e [Stmt], out: &mut BTreeMap<usize, &'e Lambda>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Assign { value, .. } => {
                collect_lambdas(value, out)
            }
            StmtKind::IndexAssign { base, index, value } => {
                collect_lambdas(base, out);
                collect_lambdas(index, out);
                collect_lambdas(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                collect_lambdas(base, out);
                collect_lambdas(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_lambdas(cond, out);
                collect_lambdas_stmts(then_body, out);
                collect_lambdas_stmts(else_body, out);
            }
            StmtKind::While { cond, body } => {
                collect_lambdas(cond, out);
                collect_lambdas_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                collect_lambdas(start, out);
                collect_lambdas(end, out);
                collect_lambdas_stmts(body, out);
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                collect_lambdas(e, out)
            }
            _ => {}
        }
    }
}

fn collect_lambdas<'e>(expr: &'e Expr, out: &mut BTreeMap<usize, &'e Lambda>) {
    match &expr.kind {
        ExprKind::Array(elems) => {
            for e in elems {
                collect_lambdas(e, out);
            }
        }
        ExprKind::Index(a, b) => {
            collect_lambdas(a, out);
            collect_lambdas(b, out);
        }
        ExprKind::Field(a, _) => collect_lambdas(a, out),
        ExprKind::Bin(_, a, b) => {
            collect_lambdas(a, out);
            collect_lambdas(b, out);
        }
        ExprKind::Un(_, a) => collect_lambdas(a, out),
        ExprKind::Call(_, args) => {
            for a in args {
                collect_lambdas(a, out);
            }
        }
        ExprKind::Lambda(l) => {
            out.insert(l.id, l);
            collect_lambdas_stmts(&l.body, out);
        }
        ExprKind::Match(m) => {
            collect_lambdas(&m.scrutinee, out);
            for arm in &m.arms {
                collect_lambdas(&arm.body, out);
            }
        }
        _ => {}
    }
}

impl<'a> WasmCompiler<'a> {
    fn new(program: &'a Program, source: &'a str) -> Self {
        WasmCompiler {
            program,
            source,
            signatures: HashMap::new(),
            struct_fields: HashMap::new(),
            struct_bitmap: HashMap::new(),
            enum_variants: HashMap::new(),
            types: Vec::new(),
            type_index: HashMap::new(),
            data: Vec::new(),
            string_addrs: HashMap::new(),
            singleton_addrs: HashMap::new(),
            func_index: HashMap::new(),
            lambdas: BTreeMap::new(),
            lambda_index: HashMap::new(),
            lambda_slot: HashMap::new(),
            lambda_captures: HashMap::new(),
            wrapper_slot: HashMap::new(),
            n_imports: 0,
            h_alloc: 0,
            h_concat: 0,
            h_str_eq: 0,
            h_push_i64: 0,
            h_push_f64: 0,
            h_get_i64: 0,
            h_get_f64: 0,
            h_set_i64: 0,
            h_set_f64: 0,
            h_iadd: 0,
            h_isub: 0,
            h_imul: 0,
            h_idiv: 0,
            h_irem: 0,
            h_char_at: 0,
            h_substr: 0,
            h_chr: 0,
            h_obj_size: 0,
            h_mark_push: 0,
            h_gc_collect: 0,
            h_gc_maybe: 0,
            shadow_bytes: DEFAULT_SHADOW_BYTES,
            imp_spawn: 0,
            imp_join_i64: 0,
            imp_join_f64: 0,
            imp_join_str: 0,
            spawn_targets: std::collections::BTreeSet::new(),
        }
    }

    fn get_type(&mut self, params: &[u8], results: &[u8]) -> u32 {
        let mut enc = vec![0x60u8];
        uleb(&mut enc, params.len() as u64);
        enc.extend_from_slice(params);
        uleb(&mut enc, results.len() as u64);
        enc.extend_from_slice(results);
        if let Some(&idx) = self.type_index.get(&enc) {
            return idx;
        }
        let idx = self.types.len() as u32;
        self.type_index.insert(enc.clone(), idx);
        self.types.push(enc);
        idx
    }

    fn sig_type(&mut self, params: &[Type], ret: &Type) -> u32 {
        let p: Vec<u8> = params.iter().map(valtype).collect();
        let r: Vec<u8> = match ret {
            Type::Unit => vec![],
            t => vec![valtype(t)],
        };
        self.get_type(&p, &r)
    }

    /// Signature for indirect calls: params plus the hidden i64 env pointer.
    fn sig_type_env(&mut self, params: &[Type], ret: &Type) -> u32 {
        let mut p: Vec<u8> = params.iter().map(valtype).collect();
        p.push(VT_I64);
        let r: Vec<u8> = match ret {
            Type::Unit => vec![],
            t => vec![valtype(t)],
        };
        self.get_type(&p, &r)
    }

    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&addr) = self.string_addrs.get(text) {
            return addr;
        }
        while self.data.len() % 8 != 0 {
            self.data.push(0);
        }
        let addr = 8 + self.data.len() as u32;
        let meta = (text.len() as i64) << 4 | TAG_BYTES;
        self.data.extend_from_slice(&meta.to_le_bytes());
        self.data.extend_from_slice(&0i64.to_le_bytes());
        self.data.extend_from_slice(text.as_bytes());
        self.string_addrs.insert(text.to_string(), addr);
        addr
    }

    /// Static multi-word pointer bitmap for a TAG_BIGSTRUCT object; returns
    /// its address in the data segment (word i/64, bit i%64 <=> payload word
    /// i is a pointer).
    fn intern_bitmap(&mut self, words: &[u64]) -> u32 {
        while self.data.len() % 8 != 0 {
            self.data.push(0);
        }
        let addr = 8 + self.data.len() as u32;
        for w in words {
            self.data.extend_from_slice(&w.to_le_bytes());
        }
        addr
    }

    /// A static closure object for a named function: [meta][0][table_slot].
    fn intern_singleton(&mut self, name: &str, slot: u32) -> u32 {
        if let Some(&addr) = self.singleton_addrs.get(name) {
            return addr;
        }
        while self.data.len() % 8 != 0 {
            self.data.push(0);
        }
        let addr = 8 + self.data.len() as u32;
        let meta = 1i64 << 4 | TAG_STRUCT;
        self.data.extend_from_slice(&meta.to_le_bytes());
        self.data.extend_from_slice(&0i64.to_le_bytes());
        self.data.extend_from_slice(&(slot as i64).to_le_bytes());
        self.singleton_addrs.insert(name.to_string(), addr);
        addr
    }

    fn compile(mut self) -> Vec<u8> {
        for f in &self.program.functions {
            self.signatures.insert(
                f.name.as_str(),
                (
                    f.params.iter().map(|p| p.ty.clone()).collect(),
                    f.ret.clone(),
                ),
            );
        }
        for s in &self.program.structs {
            self.struct_fields.insert(s.name.as_str(), &s.fields);
            self.struct_bitmap
                .insert(s.name.as_str(), pointer_bitmap_words(&s.fields));
        }
        for e in &self.program.enums {
            self.enum_variants.insert(e.name.as_str(), &e.variants);
        }

        let reachable = reachable_functions(self.program);

        // -- imports --
        let t_i64_void = self.get_type(&[VT_I64], &[]);
        let t_f64_void = self.get_type(&[VT_F64], &[]);
        let mut imports: Vec<(String, u32)> = vec![
            ("fail".to_string(), t_i64_void),
            ("print_i64".to_string(), t_i64_void),
            ("print_f64".to_string(), t_f64_void),
            ("print_bool".to_string(), t_i64_void),
            ("print_str".to_string(), t_i64_void),
        ];
        // task imports are added only when the program actually uses
        // spawn/join, so ordinary modules run on hosts without thread support
        let uses_concurrency = program_uses_concurrency(self.program, &reachable);
        if uses_concurrency {
            let t_spawn = self.get_type(&[VT_I64, VT_I64, VT_I64], &[VT_I64]);
            let t_i64_i64 = self.get_type(&[VT_I64], &[VT_I64]);
            let t_i64_f64 = self.get_type(&[VT_I64], &[VT_F64]);
            self.imp_spawn = imports.len() as u32;
            imports.push(("task_spawn".to_string(), t_spawn));
            self.imp_join_i64 = imports.len() as u32;
            imports.push(("task_join_i64".to_string(), t_i64_i64));
            self.imp_join_f64 = imports.len() as u32;
            imports.push(("task_join_f64".to_string(), t_i64_f64));
            self.imp_join_str = imports.len() as u32;
            imports.push(("task_join_str".to_string(), t_i64_i64));
        }
        let extern_base = imports.len() as u32;
        let externs: Vec<&Function> = self
            .program
            .functions
            .iter()
            .filter(|f| f.is_extern)
            .collect();
        for f in &externs {
            let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
            let tidx = self.sig_type(&params, &f.ret);
            imports.push((f.name.clone(), tidx));
        }
        self.n_imports = imports.len() as u32;
        for (i, f) in externs.iter().enumerate() {
            self.func_index.insert(f.name.as_str(), extern_base + i as u32);
        }

        // -- lambdas and named-function values in reachable code --
        let mut fn_value_names: Vec<&str> = Vec::new();
        for f in &self.program.functions {
            if f.is_extern || !reachable.contains(&f.name) {
                continue;
            }
            collect_lambdas_stmts(&f.body, &mut self.lambdas);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                collect_lambdas(&c.expr, &mut self.lambdas);
            }
            let mut names = Vec::new();
            collect_fn_names_stmts(&f.body, &mut names);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                collect_fn_names(&c.expr, &mut names);
            }
            for n in names {
                if self.signatures.contains_key(n) {
                    fn_value_names.push(n);
                }
            }
        }
        fn_value_names.sort();
        fn_value_names.dedup();

        // -- function indices: helpers, user fns, wrappers, lambdas --
        let base = self.n_imports;
        self.h_alloc = base;
        self.h_concat = base + 1;
        self.h_str_eq = base + 2;
        self.h_push_i64 = base + 3;
        self.h_push_f64 = base + 4;
        self.h_get_i64 = base + 5;
        self.h_get_f64 = base + 6;
        self.h_set_i64 = base + 7;
        self.h_set_f64 = base + 8;
        self.h_iadd = base + 9;
        self.h_isub = base + 10;
        self.h_imul = base + 11;
        self.h_idiv = base + 12;
        self.h_irem = base + 13;
        self.h_char_at = base + 14;
        self.h_substr = base + 15;
        self.h_chr = base + 16;
        self.h_obj_size = base + 17;
        self.h_mark_push = base + 18;
        self.h_gc_collect = base + 19;
        self.h_gc_maybe = base + 20;

        let user_fns: Vec<&Function> = self
            .program
            .functions
            .iter()
            .filter(|f| !f.is_extern && reachable.contains(&f.name))
            .collect();
        for (i, f) in user_fns.iter().enumerate() {
            self.func_index
                .insert(f.name.as_str(), base + N_HELPERS + i as u32);
        }
        let wrapper_base = base + N_HELPERS + user_fns.len() as u32;
        let lambda_base = wrapper_base + fn_value_names.len() as u32;
        for (i, l) in self.lambdas.values().enumerate() {
            self.lambda_index.insert(l.id, lambda_base + i as u32);
        }

        // -- table slots: wrappers first, then lambdas --
        let mut wrapper_func_index: HashMap<&str, u32> = HashMap::new();
        {
            let mut slot = 0u32;
            for (i, name) in fn_value_names.iter().enumerate() {
                self.wrapper_slot.insert(name, slot);
                wrapper_func_index.insert(name, wrapper_base + i as u32);
                slot += 1;
            }
            let ids: Vec<usize> = self.lambdas.keys().copied().collect();
            for id in ids {
                self.lambda_slot.insert(id, slot);
                slot += 1;
            }
        }
        // singletons for named function values
        for name in &fn_value_names {
            let slot = self.wrapper_slot[name];
            self.intern_singleton(name, slot);
        }

        // -- helper types --
        let t_alloc = self.get_type(&[VT_I64], &[VT_I64]);
        let t_bin_i64 = self.get_type(&[VT_I64, VT_I64], &[VT_I64]);
        let t_push_f64 = self.get_type(&[VT_I64, VT_F64], &[VT_I64]);
        let t_get_f64 = self.get_type(&[VT_I64, VT_I64], &[VT_F64]);
        let t_set_i64 = self.get_type(&[VT_I64, VT_I64, VT_I64], &[]);
        let t_set_f64 = self.get_type(&[VT_I64, VT_I64, VT_F64], &[]);
        let t_substr = self.get_type(&[VT_I64, VT_I64, VT_I64], &[VT_I64]);
        let t_void_i64 = self.get_type(&[VT_I64], &[]);
        let t_void_void = self.get_type(&[], &[]);

        let mut func_types: Vec<u32> = vec![
            t_alloc,    // alloc
            t_bin_i64,  // concat
            t_bin_i64,  // str_eq
            t_bin_i64,  // push_i64
            t_push_f64, // push_f64
            t_bin_i64,  // get_i64
            t_get_f64,  // get_f64
            t_set_i64,  // set_i64
            t_set_f64,  // set_f64
            t_bin_i64,  // iadd
            t_bin_i64,  // isub
            t_bin_i64,  // imul
            t_bin_i64,  // idiv
            t_bin_i64,  // irem
            t_bin_i64,  // char_at
            t_substr,   // substr
            t_alloc,    // chr
            t_alloc,    // obj_size
            t_void_i64, // mark_push
            t_void_void, // gc_collect
            t_void_void, // gc_maybe
        ];
        let mut bodies: Vec<Vec<u8>> = vec![
            self.emit_alloc(),
            self.emit_concat(),
            self.emit_str_eq(),
            self.emit_push(false),
            self.emit_push(true),
            self.emit_arr_get(false),
            self.emit_arr_get(true),
            self.emit_arr_set(false),
            self.emit_arr_set(true),
            self.emit_iadd(),
            self.emit_isub(),
            self.emit_imul(),
            self.emit_idiv(),
            self.emit_irem(),
            self.emit_char_at(),
            self.emit_substr(),
            self.emit_chr(),
            self.emit_obj_size(),
            self.emit_mark_push(),
            self.emit_gc_collect(),
            self.emit_gc_maybe(),
        ];

        // user functions
        for f in &user_fns {
            let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
            func_types.push(self.sig_type(&params, &f.ret));
            bodies.push(self.emit_function(f));
        }
        // wrappers
        for name in &fn_value_names {
            let (params, ret) = self.signatures[name].clone();
            func_types.push(self.sig_type_env(&params, &ret));
            let target = self.func_index[name];
            let mut b = FnBuilder::new(params.len() as u32 + 1);
            for i in 0..params.len() as u32 {
                b.local_get(i);
            }
            b.call(target);
            bodies.push(b.finish());
        }
        // lambdas, parents before children (ids ascend outward-in)
        let lambda_list: Vec<&Lambda> = self.lambdas.values().copied().collect();
        for l in &lambda_list {
            let params: Vec<Type> = l.params.iter().map(|p| p.ty.clone()).collect();
            func_types.push(self.sig_type_env(&params, &l.ret));
        }
        for l in &lambda_list {
            bodies.push(self.emit_lambda(l));
        }

        // -- memory layout --
        let data_end = {
            let mut end = 8 + self.data.len() as u32;
            end = (end + 7) & !7;
            end
        };
        let shadow_base = data_end;
        let heap_base = shadow_base + self.shadow_bytes;
        let min_pages = (heap_base as u64 / 65536) + 17;

        // -- assemble --
        let mut module: Vec<u8> = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];

        {
            let mut payload = Vec::new();
            uleb(&mut payload, self.types.len() as u64);
            for t in &self.types {
                payload.extend_from_slice(t);
            }
            section(&mut module, 1, &payload);
        }
        {
            let mut payload = Vec::new();
            uleb(&mut payload, imports.len() as u64);
            for (name, tidx) in &imports {
                write_name(&mut payload, "env");
                write_name(&mut payload, name);
                payload.push(0x00);
                uleb(&mut payload, *tidx as u64);
            }
            section(&mut module, 2, &payload);
        }
        {
            let mut payload = Vec::new();
            uleb(&mut payload, func_types.len() as u64);
            for t in &func_types {
                uleb(&mut payload, *t as u64);
            }
            section(&mut module, 3, &payload);
        }
        let n_slots = (self.wrapper_slot.len() + self.lambda_slot.len()) as u64;
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x70);
            payload.push(0x00);
            uleb(&mut payload, n_slots);
            section(&mut module, 4, &payload);
        }
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x01); // min and max
            uleb(&mut payload, min_pages);
            uleb(&mut payload, MAX_PAGES.max(min_pages + 1));
            section(&mut module, 5, &payload);
        }
        // globals
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 9);
            let g_i32 = |payload: &mut Vec<u8>, mutable: bool, v: i64| {
                payload.push(VT_I32);
                payload.push(if mutable { 0x01 } else { 0x00 });
                payload.push(0x41);
                sleb(payload, v);
                payload.push(0x0B);
            };
            let g_i64 = |payload: &mut Vec<u8>, v: i64| {
                payload.push(VT_I64);
                payload.push(0x01);
                payload.push(0x42);
                sleb(payload, v);
                payload.push(0x0B);
            };
            g_i32(&mut payload, true, heap_base as i64); // hp
            g_i32(&mut payload, true, shadow_base as i64); // ssp
            g_i64(&mut payload, 0); // free list
            g_i64(&mut payload, 0); // alloc since gc
            g_i64(&mut payload, 1 << 20); // threshold
            g_i64(&mut payload, 0); // mark stack base
            g_i64(&mut payload, 0); // mark stack top
            g_i32(&mut payload, false, heap_base as i64);
            g_i32(&mut payload, false, shadow_base as i64);
            section(&mut module, 6, &payload);
        }
        // exports
        {
            let exported: Vec<&&Function> = user_fns.iter().filter(|f| !f.is_std).collect();
            // spawn targets not already exported (std functions, for example)
            // must be callable by name from worker instances
            let extra: Vec<&str> = self
                .spawn_targets
                .iter()
                .map(|s| s.as_str())
                .filter(|n| !exported.iter().any(|f| f.name == *n))
                .collect();
            let mut payload = Vec::new();
            uleb(&mut payload, 3 + (exported.len() + extra.len()) as u64);
            write_name(&mut payload, "memory");
            payload.push(0x02);
            uleb(&mut payload, 0);
            write_name(&mut payload, "alloc");
            payload.push(0x00);
            uleb(&mut payload, self.h_alloc as u64);
            // hosts use heap_base to tell static objects from heap objects
            // when deep-copying spawn arguments
            write_name(&mut payload, "heap_base");
            payload.push(0x03);
            uleb(&mut payload, G_HEAP_BASE as u64);
            for f in exported {
                write_name(&mut payload, &f.name);
                payload.push(0x00);
                uleb(&mut payload, self.func_index[f.name.as_str()] as u64);
            }
            for n in extra {
                write_name(&mut payload, n);
                payload.push(0x00);
                uleb(&mut payload, self.func_index[n] as u64);
            }
            section(&mut module, 7, &payload);
        }
        // elements
        if n_slots > 0 {
            let mut in_slot_order: Vec<(u32, u32)> = Vec::new(); // (slot, func idx)
            for (name, slot) in &self.wrapper_slot {
                in_slot_order.push((*slot, wrapper_func_index[name]));
            }
            for (id, slot) in &self.lambda_slot {
                in_slot_order.push((*slot, self.lambda_index[id]));
            }
            in_slot_order.sort();
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x00);
            payload.push(0x41);
            sleb(&mut payload, 0);
            payload.push(0x0B);
            uleb(&mut payload, n_slots);
            for (_, fidx) in in_slot_order {
                uleb(&mut payload, fidx as u64);
            }
            section(&mut module, 9, &payload);
        }
        {
            let mut payload = Vec::new();
            uleb(&mut payload, bodies.len() as u64);
            for b in &bodies {
                payload.extend_from_slice(b);
            }
            section(&mut module, 10, &payload);
        }
        if !self.data.is_empty() {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x00);
            payload.push(0x41);
            sleb(&mut payload, 8);
            payload.push(0x0B);
            uleb(&mut payload, self.data.len() as u64);
            payload.extend_from_slice(&self.data);
            section(&mut module, 11, &payload);
        }

        module
    }

    // ---- runtime helpers ----

    fn emit_fail(&mut self, b: &mut FnBuilder, message: &str) {
        let addr = self.intern(message);
        b.i64_const(addr as i64);
        b.call(IMP_FAIL);
        b.unreachable();
    }

    /// fn alloc(total_bytes: i64) -> i64
    /// First-fit from the free list, else bump, else grow. Zeroes the block.
    /// Never triggers GC (collection happens only at safepoints).
    fn emit_alloc(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(1);
        let prev = b.new_local(VT_I64);
        let cur = b.new_local(VT_I64);
        let bsize = b.new_local(VT_I64);
        let ptr = b.new_local(VT_I64);
        let new_hp = b.new_local(VT_I64);
        // total = (total + 7) & ~7
        b.local_get(0);
        b.i64_const(7);
        b.i64_add();
        b.i64_const(-8);
        b.i64_and();
        b.local_set(0);
        // alloc_since += total
        b.global_get(G_ALLOC);
        b.local_get(0);
        b.i64_add();
        b.global_set(G_ALLOC);
        // free-list first fit
        b.i64_const(0);
        b.local_set(prev);
        b.global_get(G_FREELIST);
        b.local_set(cur);
        b.block_void();
        b.loop_void();
        {
            b.local_get(cur);
            b.i64_eqz();
            b.br_if(1);
            // bsize = meta(cur) >> 4
            b.local_get(cur);
            b.i32_wrap_i64();
            b.i64_load(0);
            b.i64_const(4);
            b.i64_shr_u();
            b.local_set(bsize);
            // usable if bsize == total or bsize >= total + 16
            b.local_get(bsize);
            b.local_get(0);
            b.i64_eq();
            b.local_get(bsize);
            b.local_get(0);
            b.i64_const(16);
            b.i64_add();
            b.i64_ge_s();
            b.i32_or();
            b.if_void();
            {
                // split?
                b.local_get(bsize);
                b.local_get(0);
                b.i64_ne();
                b.if_void();
                {
                    // remainder block at cur + total
                    b.local_get(cur);
                    b.local_get(0);
                    b.i64_add();
                    b.local_set(ptr); // reuse ptr as remainder addr
                    b.local_get(ptr);
                    b.i32_wrap_i64();
                    b.local_get(bsize);
                    b.local_get(0);
                    b.i64_sub();
                    b.i64_const(4);
                    b.i64_shl();
                    b.i64_const(TAG_FREE);
                    b.i64_or();
                    b.i64_store(0);
                    b.local_get(ptr);
                    b.i32_wrap_i64();
                    b.local_get(cur);
                    b.i32_wrap_i64();
                    b.i64_load(8);
                    b.i64_store(8);
                    // link remainder where cur was
                    b.local_get(prev);
                    b.i64_eqz();
                    b.if_void();
                    b.local_get(ptr);
                    b.global_set(G_FREELIST);
                    b.else_();
                    b.local_get(prev);
                    b.i32_wrap_i64();
                    b.local_get(ptr);
                    b.i64_store(8);
                    b.end();
                }
                b.else_();
                {
                    // take the whole block: unlink cur
                    b.local_get(prev);
                    b.i64_eqz();
                    b.if_void();
                    b.local_get(cur);
                    b.i32_wrap_i64();
                    b.i64_load(8);
                    b.global_set(G_FREELIST);
                    b.else_();
                    b.local_get(prev);
                    b.i32_wrap_i64();
                    b.local_get(cur);
                    b.i32_wrap_i64();
                    b.i64_load(8);
                    b.i64_store(8);
                    b.end();
                }
                b.end();
                // zero and return
                b.local_get(cur);
                b.i32_wrap_i64();
                b.i32_const(0);
                b.local_get(0);
                b.i32_wrap_i64();
                b.memory_fill();
                b.local_get(cur);
                b.ret();
            }
            b.end();
            b.local_get(cur);
            b.local_set(prev);
            b.local_get(cur);
            b.i32_wrap_i64();
            b.i64_load(8);
            b.local_set(cur);
            b.br(0);
        }
        b.end();
        b.end();
        // bump path
        b.global_get(G_HP);
        b.i64_extend_i32_u();
        b.local_set(ptr);
        b.local_get(ptr);
        b.local_get(0);
        b.i64_add();
        b.local_set(new_hp);
        b.memory_size();
        b.i64_extend_i32_u();
        b.i64_const(16);
        b.i64_shl();
        b.local_get(new_hp);
        b.i64_lt_u();
        b.if_void();
        {
            b.local_get(0);
            b.i64_const(16);
            b.i64_shr_u();
            b.i32_wrap_i64();
            b.i32_const(1);
            b.i32_add();
            b.memory_grow();
            b.i32_const(-1);
            b.i32_eq();
            b.if_void();
            self.emit_fail(&mut b, "runtime error: out of memory");
            b.end();
        }
        b.end();
        b.local_get(new_hp);
        b.i32_wrap_i64();
        b.global_set(G_HP);
        b.local_get(ptr);
        b.i32_wrap_i64();
        b.i32_const(0);
        b.local_get(0);
        b.i32_wrap_i64();
        b.memory_fill();
        b.local_get(ptr);
        b.finish()
    }

    /// Reads an object's byte length (meta >> 4) from a pointer on the stack.
    fn emit_len_read(&self, b: &mut FnBuilder) {
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_const(4);
        b.i64_shr_u();
    }

    /// fn concat(a, b) -> new str
    fn emit_concat(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let la = b.new_local(VT_I64);
        let lb = b.new_local(VT_I64);
        let out = b.new_local(VT_I64);
        b.local_get(0);
        self.emit_len_read(&mut b);
        b.local_set(la);
        b.local_get(1);
        self.emit_len_read(&mut b);
        b.local_set(lb);
        // out = alloc(16 + la + lb)
        b.i64_const(HDR);
        b.local_get(la);
        b.i64_add();
        b.local_get(lb);
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        // meta = (la+lb) << 4 | TAG_BYTES
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(la);
        b.local_get(lb);
        b.i64_add();
        b.i64_const(4);
        b.i64_shl();
        b.i64_store(0);
        // copy a
        b.local_get(out);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(la);
        b.i32_wrap_i64();
        b.memory_copy();
        // copy b
        b.local_get(out);
        b.i64_const(HDR);
        b.i64_add();
        b.local_get(la);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(1);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(lb);
        b.i32_wrap_i64();
        b.memory_copy();
        b.local_get(out);
        b.finish()
    }

    /// fn str_eq(a, b) -> i64
    fn emit_str_eq(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let la = b.new_local(VT_I64);
        let i = b.new_local(VT_I64);
        b.local_get(0);
        self.emit_len_read(&mut b);
        b.local_set(la);
        b.local_get(la);
        b.local_get(1);
        self.emit_len_read(&mut b);
        b.i64_ne();
        b.if_void();
        b.i64_const(0);
        b.ret();
        b.end();
        b.i64_const(0);
        b.local_set(i);
        b.block_void();
        b.loop_void();
        {
            b.local_get(i);
            b.local_get(la);
            b.i64_ge_s();
            b.br_if(1);
            b.local_get(0);
            b.local_get(i);
            b.i64_add();
            b.i32_wrap_i64();
            b.i64_load8_u(HDR as u32);
            b.local_get(1);
            b.local_get(i);
            b.i64_add();
            b.i32_wrap_i64();
            b.i64_load8_u(HDR as u32);
            b.i64_ne();
            b.if_void();
            b.i64_const(0);
            b.ret();
            b.end();
            b.local_get(i);
            b.i64_const(1);
            b.i64_add();
            b.local_set(i);
            b.br(0);
        }
        b.end();
        b.end();
        b.i64_const(1);
        b.finish()
    }

    /// fn push(arr, v) -> new array (one element longer)
    fn emit_push(&mut self, float_elem: bool) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let n = b.new_local(VT_I64);
        let out = b.new_local(VT_I64);
        b.local_get(0);
        self.emit_len_read(&mut b);
        b.local_set(n);
        // out = alloc(16 + 8(n+1))
        b.i64_const(HDR + 8);
        b.local_get(n);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        // meta = old meta + (1 << 4)  (same tag, count + 1)
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_const(16);
        b.i64_add();
        b.i64_store(0);
        // copy old elements
        b.local_get(out);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(n);
        b.i64_const(3);
        b.i64_shl();
        b.i32_wrap_i64();
        b.memory_copy();
        // out[n] = v
        b.local_get(out);
        b.local_get(n);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(1);
        if float_elem {
            b.f64_store(HDR as u32);
        } else {
            b.i64_store(HDR as u32);
        }
        b.local_get(out);
        b.finish()
    }

    fn emit_bounds_check(&mut self, b: &mut FnBuilder) {
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(0);
        self.emit_len_read(b);
        b.i64_ge_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(b, "runtime error: array index out of bounds");
        b.end();
    }

    fn emit_arr_get(&mut self, float_elem: bool) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        self.emit_bounds_check(&mut b);
        b.local_get(0);
        b.local_get(1);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.i32_wrap_i64();
        if float_elem {
            b.f64_load(HDR as u32);
        } else {
            b.i64_load(HDR as u32);
        }
        b.finish()
    }

    fn emit_arr_set(&mut self, float_elem: bool) -> Vec<u8> {
        let mut b = FnBuilder::new(3);
        self.emit_bounds_check(&mut b);
        b.local_get(0);
        b.local_get(1);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(2);
        if float_elem {
            b.f64_store(HDR as u32);
        } else {
            b.i64_store(HDR as u32);
        }
        b.finish()
    }

    fn emit_iadd(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        b.local_get(0);
        b.local_get(1);
        b.i64_add();
        b.local_set(c);
        b.local_get(0);
        b.local_get(c);
        b.i64_xor();
        b.local_get(1);
        b.local_get(c);
        b.i64_xor();
        b.i64_and();
        b.i64_const(0);
        b.i64_lt_s();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: integer overflow in '+'");
        b.end();
        b.local_get(c);
        b.finish()
    }

    fn emit_isub(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        b.local_get(0);
        b.local_get(1);
        b.i64_sub();
        b.local_set(c);
        b.local_get(0);
        b.local_get(1);
        b.i64_xor();
        b.local_get(0);
        b.local_get(c);
        b.i64_xor();
        b.i64_and();
        b.i64_const(0);
        b.i64_lt_s();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: integer overflow in '-'");
        b.end();
        b.local_get(c);
        b.finish()
    }

    fn emit_imul(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        b.local_get(0);
        b.i64_eqz();
        b.if_void();
        b.i64_const(0);
        b.ret();
        b.end();
        b.local_get(0);
        b.i64_const(-1);
        b.i64_eq();
        b.if_void();
        {
            b.local_get(1);
            b.i64_const(i64::MIN);
            b.i64_eq();
            b.if_void();
            self.emit_fail(&mut b, "runtime error: integer overflow in '*'");
            b.end();
            b.i64_const(0);
            b.local_get(1);
            b.i64_sub();
            b.ret();
        }
        b.end();
        b.local_get(0);
        b.local_get(1);
        b.i64_mul();
        b.local_set(c);
        b.local_get(c);
        b.local_get(0);
        b.i64_div_s();
        b.local_get(1);
        b.i64_ne();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: integer overflow in '*'");
        b.end();
        b.local_get(c);
        b.finish()
    }

    fn emit_idiv(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        b.local_get(1);
        b.i64_eqz();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: division by zero");
        b.end();
        b.local_get(0);
        b.i64_const(i64::MIN);
        b.i64_eq();
        b.local_get(1);
        b.i64_const(-1);
        b.i64_eq();
        b.i32_and();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: integer overflow in '/'");
        b.end();
        b.local_get(0);
        b.local_get(1);
        b.i64_div_s();
        b.finish()
    }

    fn emit_irem(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        b.local_get(1);
        b.i64_eqz();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: modulo by zero");
        b.end();
        b.local_get(0);
        b.i64_const(i64::MIN);
        b.i64_eq();
        b.local_get(1);
        b.i64_const(-1);
        b.i64_eq();
        b.i32_and();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: integer overflow in '%'");
        b.end();
        b.local_get(0);
        b.local_get(1);
        b.i64_rem_s();
        b.finish()
    }

    fn emit_char_at(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(0);
        self.emit_len_read(&mut b);
        b.i64_ge_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: char_at out of bounds");
        b.end();
        b.local_get(0);
        b.local_get(1);
        b.i64_add();
        b.i32_wrap_i64();
        b.i64_load8_u(HDR as u32);
        b.finish()
    }

    fn emit_substr(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(3);
        let out = b.new_local(VT_I64);
        let n = b.new_local(VT_I64);
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(2);
        b.i64_gt_s();
        b.i32_or();
        b.local_get(2);
        b.local_get(0);
        self.emit_len_read(&mut b);
        b.i64_gt_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: substr out of range");
        b.end();
        b.local_get(2);
        b.local_get(1);
        b.i64_sub();
        b.local_set(n);
        b.i64_const(HDR);
        b.local_get(n);
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(n);
        b.i64_const(4);
        b.i64_shl();
        b.i64_store(0);
        b.local_get(out);
        b.i64_const(HDR);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(HDR);
        b.i64_add();
        b.local_get(1);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(n);
        b.i32_wrap_i64();
        b.memory_copy();
        b.local_get(out);
        b.finish()
    }

    fn emit_chr(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(1);
        let out = b.new_local(VT_I64);
        b.local_get(0);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(0);
        b.i64_const(255);
        b.i64_gt_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: chr byte value out of range 0..=255");
        b.end();
        b.i64_const(HDR + 1);
        b.call(self.h_alloc);
        b.local_set(out);
        b.local_get(out);
        b.i32_wrap_i64();
        b.i64_const(1 << 4);
        b.i64_store(0);
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_store8(HDR as u32);
        b.local_get(out);
        b.finish()
    }

    // ---- garbage collector ----

    /// fn obj_size(meta: i64) -> i64 (total block bytes, 8-aligned)
    fn emit_obj_size(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(1);
        let tag = b.new_local(VT_I64);
        let count = b.new_local(VT_I64);
        b.local_get(0);
        b.i64_const(7);
        b.i64_and();
        b.local_set(tag);
        b.local_get(0);
        b.i64_const(4);
        b.i64_shr_u();
        b.local_set(count);
        // bytes object: 16 + align8(count)
        b.local_get(tag);
        b.i64_const(TAG_BYTES);
        b.i64_eq();
        b.if_void();
        b.i64_const(HDR);
        b.local_get(count);
        b.i64_const(7);
        b.i64_add();
        b.i64_const(-8);
        b.i64_and();
        b.i64_add();
        b.ret();
        b.end();
        // free block: count = total size
        b.local_get(tag);
        b.i64_const(TAG_FREE);
        b.i64_eq();
        b.if_void();
        b.local_get(count);
        b.ret();
        b.end();
        // arrays and structs: 16 + 8 * count
        b.i64_const(HDR);
        b.local_get(count);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.finish()
    }

    /// fn mark_push(p: i64): mark an object and push it on the mark stack.
    fn emit_mark_push(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(1);
        let meta = b.new_local(VT_I64);
        let addr = b.new_local(VT_I64);
        // null or static: ignore
        b.local_get(0);
        b.i64_eqz();
        b.if_void();
        b.ret();
        b.end();
        b.local_get(0);
        b.global_get(G_HEAP_BASE);
        b.i64_extend_i32_u();
        b.i64_lt_u();
        b.if_void();
        b.ret();
        b.end();
        // already marked?
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.local_set(meta);
        b.local_get(meta);
        b.i64_const(MARK);
        b.i64_and();
        b.i64_eqz();
        b.i32_eqz();
        b.if_void();
        b.ret();
        b.end();
        // set mark bit
        b.local_get(0);
        b.i32_wrap_i64();
        b.local_get(meta);
        b.i64_const(MARK);
        b.i64_or();
        b.i64_store(0);
        // push: addr = ms_base + ms_top * 8
        b.global_get(G_MSBASE);
        b.global_get(G_MSTOP);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.local_set(addr);
        // grow memory if the mark stack ran past the end
        b.local_get(addr);
        b.i64_const(8);
        b.i64_add();
        b.memory_size();
        b.i64_extend_i32_u();
        b.i64_const(16);
        b.i64_shl();
        b.i64_gt_u();
        b.if_void();
        {
            b.i32_const(1);
            b.memory_grow();
            b.i32_const(-1);
            b.i32_eq();
            b.if_void();
            self.emit_fail(&mut b, "runtime error: out of memory (gc mark stack)");
            b.end();
        }
        b.end();
        b.local_get(addr);
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_store(0);
        b.global_get(G_MSTOP);
        b.i64_const(1);
        b.i64_add();
        b.global_set(G_MSTOP);
        b.finish()
    }

    /// fn gc_collect(): mark from the shadow stack, then sweep the heap into
    /// a free list, updating the adaptive threshold.
    fn emit_gc_collect(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(0);
        let p = b.new_local(VT_I64);
        let limit = b.new_local(VT_I64);
        let obj = b.new_local(VT_I64);
        let meta = b.new_local(VT_I64);
        let tag = b.new_local(VT_I64);
        let count = b.new_local(VT_I64);
        let i = b.new_local(VT_I64);
        let bm = b.new_local(VT_I64);
        let size = b.new_local(VT_I64);
        let live = b.new_local(VT_I64);
        let prevfree = b.new_local(VT_I64);
        let tmp = b.new_local(VT_I64);

        // the mark stack lives in the slack between the bump pointer and the
        // end of memory (growing if it runs out); no heap allocation happens
        // during collection, so the slack is free to use
        b.global_get(G_HP);
        b.i64_extend_i32_u();
        b.global_set(G_MSBASE);
        b.i64_const(0);
        b.global_set(G_MSTOP);

        // --- roots: every slot of every shadow frame ---
        b.global_get(G_SHADOW_BASE);
        b.i64_extend_i32_u();
        b.local_set(p);
        b.global_get(G_SSP);
        b.i64_extend_i32_u();
        b.local_set(limit);
        b.block_void();
        b.loop_void();
        {
            b.local_get(p);
            b.local_get(limit);
            b.i64_ge_u();
            b.br_if(1);
            b.local_get(p);
            b.i32_wrap_i64();
            b.i64_load(0);
            b.call(self.h_mark_push);
            b.local_get(p);
            b.i64_const(8);
            b.i64_add();
            b.local_set(p);
            b.br(0);
        }
        b.end();
        b.end();

        // --- drain the mark stack, tracing pointer fields ---
        b.block_void();
        b.loop_void();
        {
            b.global_get(G_MSTOP);
            b.i64_eqz();
            b.br_if(1);
            b.global_get(G_MSTOP);
            b.i64_const(1);
            b.i64_sub();
            b.global_set(G_MSTOP);
            b.global_get(G_MSBASE);
            b.global_get(G_MSTOP);
            b.i64_const(3);
            b.i64_shl();
            b.i64_add();
            b.i32_wrap_i64();
            b.i64_load(0);
            b.local_set(obj);
            b.local_get(obj);
            b.i32_wrap_i64();
            b.i64_load(0);
            b.local_set(meta);
            b.local_get(meta);
            b.i64_const(7);
            b.i64_and();
            b.local_set(tag);
            b.local_get(meta);
            b.i64_const(4);
            b.i64_shr_u();
            b.local_set(count);
            // array of pointers: trace every element
            b.local_get(tag);
            b.i64_const(TAG_ARRP);
            b.i64_eq();
            b.if_void();
            {
                b.i64_const(0);
                b.local_set(i);
                b.block_void();
                b.loop_void();
                {
                    b.local_get(i);
                    b.local_get(count);
                    b.i64_ge_s();
                    b.br_if(1);
                    b.local_get(obj);
                    b.local_get(i);
                    b.i64_const(3);
                    b.i64_shl();
                    b.i64_add();
                    b.i32_wrap_i64();
                    b.i64_load(HDR as u32);
                    b.call(self.h_mark_push);
                    b.local_get(i);
                    b.i64_const(1);
                    b.i64_add();
                    b.local_set(i);
                    b.br(0);
                }
                b.end();
                b.end();
            }
            b.end();
            // struct/closure: trace fields flagged in the inline bitmap
            b.local_get(tag);
            b.i64_const(TAG_STRUCT);
            b.i64_eq();
            b.if_void();
            {
                b.local_get(obj);
                b.i32_wrap_i64();
                b.i64_load(8);
                b.local_set(bm);
                b.i64_const(0);
                b.local_set(i);
                b.block_void();
                b.loop_void();
                {
                    b.local_get(i);
                    b.local_get(count);
                    b.i64_ge_s();
                    b.br_if(1);
                    b.local_get(bm);
                    b.local_get(i);
                    b.i64_shr_u();
                    b.i64_const(1);
                    b.i64_and();
                    b.i64_eqz();
                    b.i32_eqz();
                    b.if_void();
                    {
                        b.local_get(obj);
                        b.local_get(i);
                        b.i64_const(3);
                        b.i64_shl();
                        b.i64_add();
                        b.i32_wrap_i64();
                        b.i64_load(HDR as u32);
                        b.call(self.h_mark_push);
                    }
                    b.end();
                    b.local_get(i);
                    b.i64_const(1);
                    b.i64_add();
                    b.local_set(i);
                    b.br(0);
                }
                b.end();
                b.end();
            }
            b.end();
            // big struct: header word 1 is the address of a static multi-word
            // bitmap (word i/64, bit i%64) in the data segment
            b.local_get(tag);
            b.i64_const(TAG_BIGSTRUCT);
            b.i64_eq();
            b.if_void();
            {
                b.local_get(obj);
                b.i32_wrap_i64();
                b.i64_load(8);
                b.local_set(bm); // bm = bitmap base address
                b.i64_const(0);
                b.local_set(i);
                b.block_void();
                b.loop_void();
                {
                    b.local_get(i);
                    b.local_get(count);
                    b.i64_ge_s();
                    b.br_if(1);
                    // bit = (load(bm + (i>>6)*8) >> (i & 63)) & 1
                    b.local_get(bm);
                    b.local_get(i);
                    b.i64_const(6);
                    b.i64_shr_u();
                    b.i64_const(3);
                    b.i64_shl();
                    b.i64_add();
                    b.i32_wrap_i64();
                    b.i64_load(0);
                    b.local_get(i);
                    b.i64_const(63);
                    b.i64_and();
                    b.i64_shr_u();
                    b.i64_const(1);
                    b.i64_and();
                    b.i64_eqz();
                    b.i32_eqz();
                    b.if_void();
                    {
                        b.local_get(obj);
                        b.local_get(i);
                        b.i64_const(3);
                        b.i64_shl();
                        b.i64_add();
                        b.i32_wrap_i64();
                        b.i64_load(HDR as u32);
                        b.call(self.h_mark_push);
                    }
                    b.end();
                    b.local_get(i);
                    b.i64_const(1);
                    b.i64_add();
                    b.local_set(i);
                    b.br(0);
                }
                b.end();
                b.end();
            }
            b.end();
            b.br(0);
        }
        b.end();
        b.end();

        // --- sweep: walk the heap, freeing unmarked blocks (coalescing) ---
        b.global_get(G_HEAP_BASE);
        b.i64_extend_i32_u();
        b.local_set(p);
        b.global_get(G_HP);
        b.i64_extend_i32_u();
        b.local_set(limit);
        b.i64_const(0);
        b.global_set(G_FREELIST);
        b.i64_const(0);
        b.local_set(live);
        b.i64_const(0);
        b.local_set(prevfree);
        b.block_void();
        b.loop_void();
        {
            b.local_get(p);
            b.local_get(limit);
            b.i64_ge_u();
            b.br_if(1);
            b.local_get(p);
            b.i32_wrap_i64();
            b.i64_load(0);
            b.local_set(meta);
            b.local_get(meta);
            b.call(self.h_obj_size);
            b.local_set(size);
            b.local_get(meta);
            b.i64_const(MARK);
            b.i64_and();
            b.i64_eqz();
            b.i32_eqz();
            b.if_void();
            {
                // live: clear the mark
                b.local_get(p);
                b.i32_wrap_i64();
                b.local_get(meta);
                b.i64_const(!MARK);
                b.i64_and();
                b.i64_store(0);
                b.local_get(live);
                b.local_get(size);
                b.i64_add();
                b.local_set(live);
                b.i64_const(0);
                b.local_set(prevfree);
            }
            b.else_();
            {
                // dead: free block, coalescing with the previous free block
                b.local_get(prevfree);
                b.i64_eqz();
                b.if_void();
                {
                    b.local_get(p);
                    b.i32_wrap_i64();
                    b.local_get(size);
                    b.i64_const(4);
                    b.i64_shl();
                    b.i64_const(TAG_FREE);
                    b.i64_or();
                    b.i64_store(0);
                    b.local_get(p);
                    b.i32_wrap_i64();
                    b.global_get(G_FREELIST);
                    b.i64_store(8);
                    b.local_get(p);
                    b.global_set(G_FREELIST);
                    b.local_get(p);
                    b.local_set(prevfree);
                }
                b.else_();
                {
                    // grow prevfree by size
                    b.local_get(prevfree);
                    b.i32_wrap_i64();
                    b.i64_load(0);
                    b.i64_const(4);
                    b.i64_shr_u();
                    b.local_get(size);
                    b.i64_add();
                    b.local_set(tmp);
                    b.local_get(prevfree);
                    b.i32_wrap_i64();
                    b.local_get(tmp);
                    b.i64_const(4);
                    b.i64_shl();
                    b.i64_const(TAG_FREE);
                    b.i64_or();
                    b.i64_store(0);
                }
                b.end();
            }
            b.end();
            b.local_get(p);
            b.local_get(size);
            b.i64_add();
            b.local_set(p);
            b.br(0);
        }
        b.end();
        b.end();
        // threshold = max(live, 1 MiB); reset the allocation counter
        b.local_get(live);
        b.i64_const(1 << 20);
        b.i64_lt_s();
        b.if_void();
        b.i64_const(1 << 20);
        b.local_set(live);
        b.end();
        b.local_get(live);
        b.global_set(G_THRESH);
        b.i64_const(0);
        b.global_set(G_ALLOC);
        b.finish()
    }

    /// fn gc_maybe(): the safepoint check.
    fn emit_gc_maybe(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(0);
        b.global_get(G_ALLOC);
        b.global_get(G_THRESH);
        b.i64_gt_s();
        b.if_void();
        b.call(self.h_gc_collect);
        b.end();
        b.finish()
    }

    // ---- user function codegen ----

    fn frame_addr(&self, ctx: &mut FnCtx, slot: u32) {
        ctx.b.local_get(ctx.fb);
        ctx.b.i32_const((slot * 8) as i32);
        ctx.b.i32_add();
    }

    /// Stores the i64 on top of the stack into a frame slot (via scratch),
    /// consuming it.
    fn frame_store(&self, ctx: &mut FnCtx, slot: u32) {
        ctx.b.local_set(ctx.scratch);
        self.frame_addr(ctx, slot);
        ctx.b.local_get(ctx.scratch);
        ctx.b.i64_store(0);
    }

    fn frame_load(&self, ctx: &mut FnCtx, slot: u32) {
        self.frame_addr(ctx, slot);
        ctx.b.i64_load(0);
    }

    /// Mirrors a pointer-typed local into its frame slot (roots it).
    fn mirror_local(&self, ctx: &mut FnCtx, local: u32, slot: u32) {
        self.frame_addr(ctx, slot);
        ctx.b.local_get(local);
        ctx.b.i64_store(0);
    }

    fn emit_function(&mut self, f: &Function) -> Vec<u8> {
        let params: Vec<(String, Type)> = f
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        self.emit_body(
            &params,
            f.ret.clone(),
            &f.requires,
            &f.ensures,
            &f.body,
            &f.name,
            None,
        )
    }

    fn emit_lambda(&mut self, l: &Lambda) -> Vec<u8> {
        let params: Vec<(String, Type)> = l
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone()))
            .collect();
        let captures = self
            .lambda_captures
            .get(&l.id)
            .cloned()
            .unwrap_or_default();
        self.emit_body(
            &params,
            l.ret.clone(),
            &[],
            &[],
            &l.body,
            "<lambda>",
            Some(captures),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_body(
        &mut self,
        params: &[(String, Type)],
        ret: Type,
        requires: &[Contract],
        ensures: &[Contract],
        body: &[Stmt],
        fn_name: &str,
        captures: Option<Vec<(String, Type)>>,
    ) -> Vec<u8> {
        let is_lambda = captures.is_some();
        let n_params = params.len() as u32 + if is_lambda { 1 } else { 0 };
        let mut b = FnBuilder::new(n_params);
        let fb = b.new_local(VT_I32);
        let scratch = b.new_local(VT_I64);
        let mut ctx = FnCtx {
            b,
            nesting: 0,
            loops: Vec::new(),
            result_local: None,
            ret: ret.clone(),
            fb,
            scratch,
            named_next: 0,
            temp_next: 0,
            frame_high: 0,
        };
        let mut scope: Scope = vec![HashMap::new()];
        for (i, (name, ty)) in params.iter().enumerate() {
            scope[0].insert(name.clone(), (i as u32, ty.clone()));
        }

        // --- frame prologue ---
        ctx.b.global_get(G_SSP);
        ctx.b.local_set(ctx.fb);
        ctx.b.local_get(ctx.fb);
        let patch_size = ctx.b.i32_const_patchable();
        ctx.b.i32_add();
        ctx.b.global_set(G_SSP);
        ctx.b.global_get(G_SSP);
        ctx.b.global_get(G_HEAP_BASE);
        ctx.b.i32_gt_u();
        ctx.b.if_void();
        self.emit_fail(&mut ctx.b, "runtime error: stack overflow");
        ctx.b.end();
        ctx.b.local_get(ctx.fb);
        ctx.b.i32_const(0);
        let patch_fill = ctx.b.i32_const_patchable();
        ctx.b.memory_fill();

        // pointer params (including the env pointer of a lambda) get slots
        for (i, (name, ty)) in params.iter().enumerate() {
            if is_ptr(ty) {
                let slot = ctx.alloc_named();
                self.mirror_local(&mut ctx, i as u32, slot);
                scope[0].insert(format!("{}\u{0}slot", name), (slot, Type::Int));
            }
        }
        if is_lambda {
            let env = params.len() as u32;
            let slot = ctx.alloc_named();
            self.mirror_local(&mut ctx, env, slot);
            // load captures from the closure object into locals; they stay
            // alive through the rooted env pointer
            for (i, (name, ty)) in captures.as_ref().unwrap().iter().enumerate() {
                let l = ctx.b.new_local(valtype(ty));
                ctx.b.local_get(env);
                ctx.b.i32_wrap_i64();
                let off = HDR as u32 + 8 + 8 * i as u32;
                if *ty == Type::Float {
                    ctx.b.f64_load(off);
                } else {
                    ctx.b.i64_load(off);
                }
                ctx.b.local_set(l);
                scope[0].insert(name.clone(), (l, ty.clone()));
            }
        }

        // shadow copies of params for ensures (original argument values)
        let mut shadows: Vec<(String, u32, Type)> = Vec::new();
        if !ensures.is_empty() {
            for (i, (name, ty)) in params.iter().enumerate() {
                let sh = ctx.b.new_local(valtype(ty));
                ctx.b.local_get(i as u32);
                ctx.b.local_set(sh);
                if is_ptr(ty) {
                    let slot = ctx.alloc_named();
                    self.mirror_local(&mut ctx, sh, slot);
                }
                shadows.push((name.clone(), sh, ty.clone()));
            }
        }

        // entry safepoint
        ctx.b.call(self.h_gc_maybe);

        for c in requires {
            let msg = format!(
                "runtime error: contract violation: requires '{}' failed when calling '{}'",
                c.text, fn_name
            );
            let addr = self.intern(&msg);
            ctx.temp_next = ctx.named_next;
            self.expr(&mut ctx, &scope, &c.expr, None);
            ctx.b.i32_wrap_i64();
            ctx.b.i32_eqz();
            ctx.b.if_void();
            ctx.b.i64_const(addr as i64);
            ctx.b.call(IMP_FAIL);
            ctx.b.unreachable();
            ctx.b.end();
        }

        if ret != Type::Unit {
            ctx.result_local = Some(ctx.b.new_local(valtype(&ret)));
        }

        ctx.b.block_void();
        ctx.nesting = 0;
        self.stmts(&mut ctx, &mut scope, body);
        ctx.b.end();

        if !ensures.is_empty() {
            let mut ens_scope: Scope = vec![HashMap::new()];
            for (name, idx, ty) in &shadows {
                ens_scope[0].insert(name.clone(), (*idx, ty.clone()));
            }
            if let Some(r) = ctx.result_local {
                ens_scope[0].insert("result".to_string(), (r, ret.clone()));
            }
            // the result must survive GC during ensures evaluation
            if is_ptr(&ret) {
                if let Some(r) = ctx.result_local {
                    let slot = ctx.alloc_named();
                    self.mirror_local(&mut ctx, r, slot);
                }
            }
            for c in ensures {
                let msg = format!(
                    "runtime error: contract violation: ensures '{}' failed in '{}'",
                    c.text, fn_name
                );
                let addr = self.intern(&msg);
                ctx.temp_next = ctx.named_next;
                self.expr(&mut ctx, &ens_scope, &c.expr, None);
                ctx.b.i32_wrap_i64();
                ctx.b.i32_eqz();
                ctx.b.if_void();
                ctx.b.i64_const(addr as i64);
                ctx.b.call(IMP_FAIL);
                ctx.b.unreachable();
                ctx.b.end();
            }
        }

        // frame epilogue
        ctx.b.local_get(ctx.fb);
        ctx.b.global_set(G_SSP);
        if let Some(r) = ctx.result_local {
            ctx.b.local_get(r);
        }
        let frame_bytes = (ctx.frame_high * 8) as i32;
        ctx.b.patch_i32(patch_size, frame_bytes);
        ctx.b.patch_i32(patch_fill, frame_bytes);
        ctx.b.finish()
    }

    fn stmts(&mut self, ctx: &mut FnCtx, scope: &mut Scope, body: &[Stmt]) {
        scope.push(HashMap::new());
        for s in body {
            self.stmt(ctx, scope, s);
        }
        scope.pop();
    }

    fn stmt(&mut self, ctx: &mut FnCtx, scope: &mut Scope, stmt: &Stmt) {
        // temp roots are per-statement
        ctx.temp_next = ctx.named_next;
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let vty = match ty {
                    Some(t) => t.clone(),
                    None => self.type_of(value, scope),
                };
                self.expr(ctx, scope, value, Some(&vty));
                let idx = ctx.b.new_local(valtype(&vty));
                ctx.b.local_set(idx);
                if is_ptr(&vty) {
                    let slot = ctx.alloc_named();
                    self.mirror_local(ctx, idx, slot);
                    scope
                        .last_mut()
                        .unwrap()
                        .insert(format!("{}\u{0}slot", name), (slot, Type::Int));
                }
                scope.last_mut().unwrap().insert(name.clone(), (idx, vty));
            }
            StmtKind::Assign { name, value } => {
                let (idx, ty) = lookup(scope, name).expect("checked");
                self.expr(ctx, scope, value, Some(&ty));
                ctx.b.local_set(idx);
                if is_ptr(&ty) {
                    let (slot, _) = lookup(scope, &format!("{}\u{0}slot", name))
                        .expect("every pointer variable has a frame slot");
                    self.mirror_local(ctx, idx, slot);
                }
            }
            StmtKind::IndexAssign { base, index, value } => {
                let elem = match self.type_of(base, scope) {
                    Type::Array(e) => *e,
                    _ => unreachable!(),
                };
                let bt = self.type_of(base, scope);
                self.eval_operands(
                    ctx,
                    scope,
                    &[
                        (base, Some(bt)),
                        (index, Some(Type::Int)),
                        (value, Some(elem.clone())),
                    ],
                );
                let helper = if elem == Type::Float {
                    self.h_set_f64
                } else {
                    self.h_set_i64
                };
                ctx.b.call(helper);
            }
            StmtKind::FieldAssign { base, field, value } => {
                let sname = match self.type_of(base, scope) {
                    Type::Struct(s) => s,
                    _ => unreachable!(),
                };
                let (offset, fty) = self.field_slot(&sname, field);
                // evaluate to temps so the base survives value's evaluation,
                // then interleave: addr, value, store
                let saved = self.eval_to_temps(
                    ctx,
                    scope,
                    &[
                        (base, Some(Type::Struct(sname.clone()))),
                        (value, Some(fty.clone())),
                    ],
                );
                self.reload(ctx, &saved[0]);
                ctx.b.i32_wrap_i64();
                self.reload(ctx, &saved[1]);
                if fty == Type::Float {
                    ctx.b.f64_store(offset);
                } else {
                    ctx.b.i64_store(offset);
                }
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                self.expr(ctx, scope, cond, Some(&Type::Bool));
                ctx.b.i32_wrap_i64();
                ctx.b.if_void();
                ctx.nesting += 1;
                self.stmts(ctx, scope, then_body);
                if !else_body.is_empty() {
                    ctx.b.else_();
                    self.stmts(ctx, scope, else_body);
                }
                ctx.nesting -= 1;
                ctx.b.end();
            }
            StmtKind::While { cond, body } => {
                ctx.b.block_void();
                ctx.nesting += 1;
                let t_break = ctx.nesting;
                ctx.b.loop_void();
                ctx.nesting += 1;
                let t_continue = ctx.nesting;
                ctx.temp_next = ctx.named_next;
                self.expr(ctx, scope, cond, Some(&Type::Bool));
                ctx.b.i32_wrap_i64();
                ctx.b.i32_eqz();
                ctx.b.br_if(ctx.nesting - t_break);
                ctx.loops.push((t_break, t_continue));
                self.stmts(ctx, scope, body);
                ctx.loops.pop();
                // loop back-edge safepoint
                ctx.b.call(self.h_gc_maybe);
                ctx.b.br(ctx.nesting - t_continue);
                ctx.nesting -= 2;
                ctx.b.end();
                ctx.b.end();
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                let e_local = ctx.b.new_local(VT_I64);
                self.expr(ctx, scope, end, Some(&Type::Int));
                ctx.b.local_set(e_local);
                let i_local = ctx.b.new_local(VT_I64);
                ctx.temp_next = ctx.named_next;
                self.expr(ctx, scope, start, Some(&Type::Int));
                ctx.b.local_set(i_local);
                scope.push(HashMap::new());
                scope
                    .last_mut()
                    .unwrap()
                    .insert(var.clone(), (i_local, Type::Int));

                ctx.b.block_void();
                ctx.nesting += 1;
                let t_break = ctx.nesting;
                ctx.b.loop_void();
                ctx.nesting += 1;
                let t_loop = ctx.nesting;
                ctx.b.local_get(i_local);
                ctx.b.local_get(e_local);
                ctx.b.i64_ge_s();
                ctx.b.br_if(ctx.nesting - t_break);
                ctx.b.block_void();
                ctx.nesting += 1;
                let t_continue = ctx.nesting;
                ctx.loops.push((t_break, t_continue));
                self.stmts(ctx, scope, body);
                ctx.loops.pop();
                ctx.nesting -= 1;
                ctx.b.end();
                ctx.b.local_get(i_local);
                ctx.b.i64_const(1);
                ctx.b.call(self.h_iadd);
                ctx.b.local_set(i_local);
                // loop back-edge safepoint
                ctx.b.call(self.h_gc_maybe);
                ctx.b.br(ctx.nesting - t_loop);
                ctx.nesting -= 2;
                ctx.b.end();
                ctx.b.end();
                scope.pop();
            }
            StmtKind::Break => {
                let (t_break, _) = *ctx.loops.last().expect("checked");
                ctx.b.br(ctx.nesting - t_break);
            }
            StmtKind::Continue => {
                let (_, t_continue) = *ctx.loops.last().expect("checked");
                ctx.b.br(ctx.nesting - t_continue);
            }
            StmtKind::Return(value) => {
                if let Some(e) = value {
                    let ret = ctx.ret.clone();
                    self.expr(ctx, scope, e, Some(&ret));
                    let r = ctx.result_local.expect("checked");
                    ctx.b.local_set(r);
                }
                let depth = ctx.nesting;
                ctx.b.br(depth);
            }
            StmtKind::Assert(expr) => {
                let (line, _) = line_col(self.source, expr.span.start);
                let text = self
                    .source
                    .get(expr.span.start as usize..expr.span.end as usize)
                    .unwrap_or("<expr>");
                let msg = format!(
                    "runtime error: assertion failed: '{}' (line {})",
                    text.trim(),
                    line
                );
                let addr = self.intern(&msg);
                self.expr(ctx, scope, expr, Some(&Type::Bool));
                ctx.b.i32_wrap_i64();
                ctx.b.i32_eqz();
                ctx.b.if_void();
                ctx.b.i64_const(addr as i64);
                ctx.b.call(IMP_FAIL);
                ctx.b.unreachable();
                ctx.b.end();
            }
            StmtKind::Expr(expr) => {
                let ty = self.type_of(expr, scope);
                self.expr(ctx, scope, expr, None);
                if ty != Type::Unit {
                    ctx.b.drop_();
                }
            }
        }
    }

    fn field_slot(&self, struct_name: &str, field: &str) -> (u32, Type) {
        let fields = self.struct_fields[struct_name];
        let idx = fields
            .iter()
            .position(|f| f.name == field)
            .expect("checked");
        (HDR as u32 + 8 * idx as u32, fields[idx].ty.clone())
    }

    /// Evaluates operands to temps (frame slots for pointers, locals for
    /// scalars) when needed for GC safety, then reloads them in order onto
    /// the stack. When no operand needs rooting, evaluates directly.
    fn eval_operands(&mut self, ctx: &mut FnCtx, scope: &Scope, ops: &[(&Expr, Option<Type>)]) {
        let saved = self.eval_to_temps(ctx, scope, ops);
        for s in &saved {
            self.reload(ctx, s);
        }
    }

    /// Like eval_operands but leaves values in temps, returning handles.
    fn eval_to_temps(
        &mut self,
        ctx: &mut FnCtx,
        scope: &Scope,
        ops: &[(&Expr, Option<Type>)],
    ) -> Vec<Saved> {
        let mut out = Vec::with_capacity(ops.len());
        for (e, expected) in ops {
            let ty = expected
                .clone()
                .unwrap_or_else(|| self.type_of(e, scope));
            self.expr(ctx, scope, e, expected.as_ref());
            if is_ptr(&ty) {
                let slot = ctx.alloc_temp();
                self.frame_store(ctx, slot);
                out.push(Saved::Ptr(slot));
            } else {
                let l = ctx.b.new_local(valtype(&ty));
                ctx.b.local_set(l);
                out.push(Saved::Scalar(l));
            }
        }
        out
    }

    fn reload(&self, ctx: &mut FnCtx, s: &Saved) {
        match s {
            Saved::Scalar(l) => ctx.b.local_get(*l),
            Saved::Ptr(slot) => self.frame_load(ctx, *slot),
        }
    }

    /// True if any of `ops` after the first pointer-producing one can GC —
    /// used to decide between direct stack evaluation and temping.
    fn needs_temps(&self, scope: &Scope, ops: &[(&Expr, Option<Type>)]) -> bool {
        for (i, (e, expected)) in ops.iter().enumerate() {
            let ty = expected
                .clone()
                .unwrap_or_else(|| self.type_of(e, scope));
            if is_ptr(&ty) && ops[i + 1..].iter().any(|(later, _)| can_gc(later)) {
                return true;
            }
        }
        false
    }

    /// Evaluates a list of operands onto the stack, temping only when a
    /// pointer would otherwise sit on the operand stack across a safepoint.
    fn eval_operands_smart(
        &mut self,
        ctx: &mut FnCtx,
        scope: &Scope,
        ops: &[(&Expr, Option<Type>)],
    ) {
        if self.needs_temps(scope, ops) {
            self.eval_operands(ctx, scope, ops);
        } else {
            for (e, expected) in ops {
                self.expr(ctx, scope, e, expected.as_ref());
            }
        }
    }

    fn expr(&mut self, ctx: &mut FnCtx, scope: &Scope, expr: &Expr, expected: Option<&Type>) {
        match &expr.kind {
            ExprKind::Int(v) => ctx.b.i64_const(*v),
            ExprKind::Float(v) => ctx.b.f64_const(*v),
            ExprKind::Bool(v) => ctx.b.i64_const(if *v { 1 } else { 0 }),
            ExprKind::Str(s) => {
                let addr = self.intern(s);
                ctx.b.i64_const(addr as i64);
            }
            ExprKind::Var(name) => {
                if let Some((idx, _)) = lookup(scope, name) {
                    ctx.b.local_get(idx);
                } else if let Some(colon_pos) = name.rfind("::") {
                    // enum variant without payload (Enum::Variant)
                    let enum_name = &name[..colon_pos];
                    let variant_name = &name[colon_pos + 2..];
                    if let Some(variants) = self.enum_variants.get(enum_name) {
                        let variant_idx = variants
                            .iter()
                            .position(|v| v.name == variant_name)
                            .expect("variant exists (checked)");
                        
                        // allocate enum object: header + tag + unused payload
                        ctx.b.i64_const(HDR + 16);
                        ctx.b.call(self.h_alloc);
                        let tmp = ctx.b.new_local(VT_I64);
                        ctx.b.local_tee(tmp);
                        // root it (consumes the teed copy from the stack)
                        let root = ctx.alloc_temp();
                        self.frame_store(ctx, root);

                        // write header
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(2 << 4 | TAG_STRUCT);
                        ctx.b.i64_store(0);
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(0); // bitmap: no pointers
                        ctx.b.i64_store(8);
                        
                        // write tag
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(variant_idx as i64);
                        ctx.b.i64_store(HDR as u32);
                        
                        ctx.b.local_get(tmp);
                    } else {
                        // a bare function name: its static singleton closure
                        let addr = self.singleton_addrs[name.as_str()];
                        ctx.b.i64_const(addr as i64);
                    }
                } else {
                    // a bare function name: its static singleton closure
                    let addr = self.singleton_addrs[name.as_str()];
                    ctx.b.i64_const(addr as i64);
                }
            }
            ExprKind::Array(elems) => {
                let elem_ty = match expected {
                    Some(Type::Array(e)) => (**e).clone(),
                    _ => elems
                        .first()
                        .map(|e| self.type_of(e, scope))
                        .unwrap_or(Type::Int),
                };
                let n = elems.len() as i64;
                let tag = if is_ptr(&elem_ty) { TAG_ARRP } else { TAG_ARR };
                ctx.b.i64_const(HDR + 8 * n);
                ctx.b.call(self.h_alloc);
                // root the fresh array: element evaluation may trigger GC
                let tmp = ctx.b.new_local(VT_I64);
                ctx.b.local_tee(tmp);
                let root = ctx.alloc_temp();
                self.frame_store(ctx, root);
                ctx.b.local_get(tmp);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(n << 4 | tag);
                ctx.b.i64_store(0);
                for (i, e) in elems.iter().enumerate() {
                    ctx.b.local_get(tmp);
                    ctx.b.i32_wrap_i64();
                    self.expr(ctx, scope, e, Some(&elem_ty));
                    let off = HDR as u32 + 8 * i as u32;
                    if elem_ty == Type::Float {
                        ctx.b.f64_store(off);
                    } else {
                        ctx.b.i64_store(off);
                    }
                }
                ctx.b.local_get(tmp);
            }
            ExprKind::Index(base, index) => {
                let base_ty = self.type_of(base, scope);
                let elem = match &base_ty {
                    Type::Array(e) => (**e).clone(),
                    _ => unreachable!(),
                };
                self.eval_operands_smart(
                    ctx,
                    scope,
                    &[(base, Some(base_ty)), (index, Some(Type::Int))],
                );
                let helper = if elem == Type::Float {
                    self.h_get_f64
                } else {
                    self.h_get_i64
                };
                ctx.b.call(helper);
            }
            ExprKind::Field(base, field) => {
                let sname = match self.type_of(base, scope) {
                    Type::Struct(s) => s,
                    _ => unreachable!(),
                };
                let (offset, fty) = self.field_slot(&sname, field);
                self.expr(ctx, scope, base, None);
                ctx.b.i32_wrap_i64();
                if fty == Type::Float {
                    ctx.b.f64_load(offset);
                } else {
                    ctx.b.i64_load(offset);
                }
            }
            ExprKind::Un(op, inner) => match op {
                UnOp::Neg => {
                    let ty = self.type_of(inner, scope);
                    if ty == Type::Float {
                        self.expr(ctx, scope, inner, None);
                        ctx.b.f64_neg();
                    } else {
                        ctx.b.i64_const(0);
                        self.expr(ctx, scope, inner, None);
                        ctx.b.call(self.h_isub);
                    }
                }
                UnOp::Not => {
                    self.expr(ctx, scope, inner, Some(&Type::Bool));
                    ctx.b.i64_eqz();
                    ctx.b.i64_extend_i32_u();
                }
            },
            ExprKind::Bin(op, lhs, rhs) => self.binop(ctx, scope, *op, lhs, rhs),
            ExprKind::Call(name, args) => self.call(ctx, scope, name, args, expected),
            ExprKind::Lambda(l) => {
                // resolve captures against the current scope, record their
                // types for the lifted function, and build the closure object
                let mut captures: Vec<(String, Type)> = Vec::new();
                for n in l.free_names() {
                    if let Some((_, ty)) = lookup(scope, &n) {
                        captures.push((n, ty));
                    }
                }
                self.lambda_captures.insert(l.id, captures.clone());
                let ncaps = captures.len() as i64;
                let words = 1 + captures.len(); // table slot + captures
                let mut bm_words = vec![0u64; words.div_ceil(64)];
                for (i, (_, ty)) in captures.iter().enumerate() {
                    if is_ptr(ty) {
                        bm_words[(i + 1) / 64] |= 1 << ((i + 1) % 64);
                    }
                }
                let (tag, word1) = if words <= INLINE_BITMAP_WORDS {
                    (TAG_STRUCT, bm_words[0] as i64)
                } else {
                    (TAG_BIGSTRUCT, self.intern_bitmap(&bm_words) as i64)
                };
                ctx.b.i64_const(HDR + 8 * (1 + ncaps));
                ctx.b.call(self.h_alloc);
                let tmp = ctx.b.new_local(VT_I64);
                ctx.b.local_tee(tmp);
                let root = ctx.alloc_temp();
                self.frame_store(ctx, root);
                ctx.b.local_get(tmp);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const((1 + ncaps) << 4 | tag);
                ctx.b.i64_store(0);
                ctx.b.local_get(tmp);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(word1);
                ctx.b.i64_store(8);
                ctx.b.local_get(tmp);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(self.lambda_slot[&l.id] as i64);
                ctx.b.i64_store(HDR as u32);
                for (i, (name, ty)) in captures.iter().enumerate() {
                    ctx.b.local_get(tmp);
                    ctx.b.i32_wrap_i64();
                    let (idx, _) = lookup(scope, name).expect("capture in scope");
                    ctx.b.local_get(idx);
                    let off = HDR as u32 + 8 + 8 * i as u32;
                    if *ty == Type::Float {
                        ctx.b.f64_store(off);
                    } else {
                        ctx.b.i64_store(off);
                    }
                }
                ctx.b.local_get(tmp);
            }
            ExprKind::Match(m) => {
                // Compile match expression as nested if-else
                self.expr(ctx, scope, &m.scrutinee, None);
                let scrut_ty = self.type_of(&m.scrutinee, scope);
                let scrut_local = ctx.b.new_local(valtype(&scrut_ty));
                ctx.b.local_set(scrut_local);
                
                let result_ty = expected.cloned().or_else(|| {
                    m.arms.first().map(|arm| {
                        // arm bodies may reference pattern bindings; give the
                        // type computation a scope that knows their types
                        let mut probe = scope.clone();
                        let mut frame = HashMap::new();
                        self.pattern_types(&arm.pattern, &scrut_ty, &mut frame);
                        probe.push(frame);
                        self.type_of(&arm.body, &probe)
                    })
                }).unwrap_or(Type::Unit);
                
                // Generate nested if-else for each arm
                let mut opened_ifs = 0;
                for (arm_idx, arm) in m.arms.iter().enumerate() {
                    let is_last = arm_idx == m.arms.len() - 1;
                    let has_check = !matches!(arm.pattern, Pattern::Wildcard | Pattern::Var(_));
                    
                    if has_check && !is_last {
                        // Generate pattern check
                        self.compile_pattern_check(ctx, scope, &arm.pattern, scrut_local, &scrut_ty);
                        
                        if result_ty == Type::Unit {
                            ctx.b.if_void();
                        } else {
                            ctx.b.if_typed(valtype(&result_ty));
                        }
                        ctx.nesting += 1;
                        opened_ifs += 1;
                    }
                    
                    // Bind pattern variables and evaluate body
                    let mut arm_scope = scope.clone();
                    arm_scope.push(HashMap::new());
                    self.bind_pattern_vars(ctx, &mut arm_scope, &arm.pattern, scrut_local, &scrut_ty);
                    self.expr(ctx, &arm_scope, &arm.body, Some(&result_ty));
                    
                    if has_check && !is_last {
                        ctx.b.else_();
                    }
                }
                
                // Close all opened if blocks
                for _ in 0..opened_ifs {
                    ctx.nesting -= 1;
                    ctx.b.end();
                }
            }
        }
    }

    /// Compile a pattern check, leaving an i32 (0 or 1) on the stack.
    fn compile_pattern_check(
        &mut self,
        ctx: &mut FnCtx,
        _scope: &Scope,
        pattern: &Pattern,
        scrut_local: u32,
        _scrut_ty: &Type,
    ) {
        match pattern {
            Pattern::Wildcard | Pattern::Var(_) => {
                // Always matches
                ctx.b.i32_const(1);
            }
            Pattern::Int(v) => {
                ctx.b.local_get(scrut_local);
                ctx.b.i64_const(*v);
                ctx.b.i64_eq();
            }
            Pattern::Bool(v) => {
                ctx.b.local_get(scrut_local);
                ctx.b.i64_const(if *v { 1 } else { 0 });
                ctx.b.i64_eq();
            }
            Pattern::Str(s) => {
                ctx.b.local_get(scrut_local);
                let addr = self.intern(s);
                ctx.b.i64_const(addr as i64);
                ctx.b.call(self.h_str_eq);
                ctx.b.i64_const(1);
                ctx.b.i64_eq();
            }
            Pattern::Variant(enum_name, variant_name) | Pattern::VariantPayload(enum_name, variant_name, _) => {
                let variants = self.enum_variants[enum_name.as_str()];
                let variant_idx = variants
                    .iter()
                    .position(|v| v.name == *variant_name)
                    .expect("variant exists");
                
                // Check tag
                ctx.b.local_get(scrut_local);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_load(HDR as u32);
                ctx.b.i64_const(variant_idx as i64);
                ctx.b.i64_eq();
            }
        }
    }

    /// Bind pattern variables to the scope
    /// Records the types of a pattern's bindings into `frame` (with a dummy
    /// local index). Used to type-check arm bodies before code for the
    /// bindings is emitted.
    fn pattern_types(
        &self,
        pattern: &Pattern,
        scrut_ty: &Type,
        frame: &mut HashMap<String, (u32, Type)>,
    ) {
        match pattern {
            Pattern::Var(name) => {
                frame.insert(name.clone(), (u32::MAX, scrut_ty.clone()));
            }
            Pattern::VariantPayload(enum_name, variant_name, inner) => {
                if let Some(variants) = self.enum_variants.get(enum_name.as_str()) {
                    if let Some(v) = variants.iter().find(|v| v.name == *variant_name) {
                        if let Some(ref payload_ty) = v.payload {
                            self.pattern_types(inner, payload_ty, frame);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn bind_pattern_vars(
        &mut self,
        ctx: &mut FnCtx,
        scope: &mut Scope,
        pattern: &Pattern,
        scrut_local: u32,
        scrut_ty: &Type,
    ) {
        match pattern {
            Pattern::Wildcard => {
                // No bindings
            }
            Pattern::Var(name) => {
                // Bind the whole scrutinee
                let idx = ctx.b.new_local(valtype(scrut_ty));
                ctx.b.local_get(scrut_local);
                ctx.b.local_set(idx);
                scope.last_mut().unwrap().insert(name.clone(), (idx, scrut_ty.clone()));
            }
            Pattern::Int(_) | Pattern::Bool(_) | Pattern::Str(_) => {
                // No bindings
            }
            Pattern::Variant(_, _) => {
                // Unit variant, no bindings
            }
            Pattern::VariantPayload(enum_name, variant_name, inner) => {
                let variants = self.enum_variants[enum_name.as_str()];
                let variant = variants
                    .iter()
                    .find(|v| v.name == *variant_name)
                    .expect("variant exists");
                
                if let Some(ref payload_ty) = variant.payload {
                    // Extract payload from enum object
                    let payload_local = ctx.b.new_local(valtype(payload_ty));
                    ctx.b.local_get(scrut_local);
                    ctx.b.i32_wrap_i64();
                    if *payload_ty == Type::Float {
                        ctx.b.f64_load(HDR as u32 + 8);
                    } else {
                        ctx.b.i64_load(HDR as u32 + 8);
                    }
                    ctx.b.local_set(payload_local);
                    
                    // Recursively bind inner pattern
                    self.bind_pattern_vars(ctx, scope, inner, payload_local, payload_ty);
                }
            }
        }
    }

    fn binop(&mut self, ctx: &mut FnCtx, scope: &Scope, op: BinOp, lhs: &Expr, rhs: &Expr) {
        use BinOp::*;
        if matches!(op, And | Or) {
            self.expr(ctx, scope, lhs, Some(&Type::Bool));
            ctx.b.i32_wrap_i64();
            ctx.b.if_typed(VT_I64);
            ctx.nesting += 1;
            if op == And {
                self.expr(ctx, scope, rhs, Some(&Type::Bool));
                ctx.b.else_();
                ctx.b.i64_const(0);
            } else {
                ctx.b.i64_const(1);
                ctx.b.else_();
                self.expr(ctx, scope, rhs, Some(&Type::Bool));
            }
            ctx.nesting -= 1;
            ctx.b.end();
            return;
        }

        let lt = self.type_of(lhs, scope);
        self.eval_operands_smart(
            ctx,
            scope,
            &[(lhs, Some(lt.clone())), (rhs, Some(lt.clone()))],
        );

        match (&lt, op) {
            (Type::Str, Add) => ctx.b.call(self.h_concat),
            (Type::Str, Eq) => ctx.b.call(self.h_str_eq),
            (Type::Str, Ne) => {
                ctx.b.call(self.h_str_eq);
                ctx.b.i64_eqz();
                ctx.b.i64_extend_i32_u();
            }
            (Type::Float, _) => match op {
                Add => ctx.b.f64_add(),
                Sub => ctx.b.f64_sub(),
                Mul => ctx.b.f64_mul(),
                Div => ctx.b.f64_div(),
                Eq => {
                    ctx.b.f64_eq();
                    ctx.b.i64_extend_i32_u();
                }
                Ne => {
                    ctx.b.f64_ne();
                    ctx.b.i64_extend_i32_u();
                }
                Lt => {
                    ctx.b.f64_lt();
                    ctx.b.i64_extend_i32_u();
                }
                Le => {
                    ctx.b.f64_le();
                    ctx.b.i64_extend_i32_u();
                }
                Gt => {
                    ctx.b.f64_gt();
                    ctx.b.i64_extend_i32_u();
                }
                Ge => {
                    ctx.b.f64_ge();
                    ctx.b.i64_extend_i32_u();
                }
                Mod | And | Or => unreachable!("checked"),
            },
            _ => match op {
                Add => ctx.b.call(self.h_iadd),
                Sub => ctx.b.call(self.h_isub),
                Mul => ctx.b.call(self.h_imul),
                Div => ctx.b.call(self.h_idiv),
                Mod => ctx.b.call(self.h_irem),
                Eq => {
                    ctx.b.i64_eq();
                    ctx.b.i64_extend_i32_u();
                }
                Ne => {
                    ctx.b.i64_ne();
                    ctx.b.i64_extend_i32_u();
                }
                Lt => {
                    ctx.b.i64_lt_s();
                    ctx.b.i64_extend_i32_u();
                }
                Le => {
                    ctx.b.i64_le_s();
                    ctx.b.i64_extend_i32_u();
                }
                Gt => {
                    ctx.b.i64_gt_s();
                    ctx.b.i64_extend_i32_u();
                }
                Ge => {
                    ctx.b.i64_ge_s();
                    ctx.b.i64_extend_i32_u();
                }
                And | Or => unreachable!(),
            },
        }
    }

    fn call(
        &mut self,
        ctx: &mut FnCtx,
        scope: &Scope,
        name: &str,
        args: &[Expr],
        expected: Option<&Type>,
    ) {
        // a local variable holding a function value: indirect call
        if let Some((idx, Type::Fn(params, ret))) = lookup(scope, name) {
            let ops: Vec<(&Expr, Option<Type>)> = args
                .iter()
                .zip(params.iter())
                .map(|(a, p)| (a, Some(p.clone())))
                .collect();
            self.eval_operands_smart(ctx, scope, &ops);
            // hidden env argument, then the table index from the closure
            ctx.b.local_get(idx);
            ctx.b.local_get(idx);
            ctx.b.i32_wrap_i64();
            ctx.b.i64_load(HDR as u32);
            ctx.b.i32_wrap_i64();
            let type_idx = self.sig_type_env(&params, &ret);
            ctx.b.call_indirect(type_idx);
            return;
        }
        match name {
            "print" => {
                let ty = self.type_of(&args[0], scope);
                self.expr(ctx, scope, &args[0], None);
                let imp = match ty {
                    Type::Int => IMP_PRINT_I64,
                    Type::Float => IMP_PRINT_F64,
                    Type::Bool => IMP_PRINT_BOOL,
                    Type::Str => IMP_PRINT_STR,
                    _ => unreachable!("checked"),
                };
                ctx.b.call(imp);
            }
            "len" => {
                self.expr(ctx, scope, &args[0], None);
                self.emit_len_read(&mut ctx.b);
            }
            "push" => {
                let arr_ty = match expected {
                    Some(t @ Type::Array(_)) => t.clone(),
                    _ => self.type_of(&args[0], scope),
                };
                let elem = match &arr_ty {
                    Type::Array(e) => (**e).clone(),
                    _ => Type::Int,
                };
                self.eval_operands_smart(
                    ctx,
                    scope,
                    &[(&args[0], Some(arr_ty)), (&args[1], Some(elem.clone()))],
                );
                let helper = if elem == Type::Float {
                    self.h_push_f64
                } else {
                    self.h_push_i64
                };
                ctx.b.call(helper);
            }
            "to_float" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.f64_convert_i64_s();
            }
            "to_int" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Float));
                let v = ctx.b.new_local(VT_F64);
                ctx.b.local_set(v);
                ctx.b.local_get(v);
                ctx.b.f64_const(-9223372036854775808.0);
                ctx.b.f64_ge();
                ctx.b.local_get(v);
                ctx.b.f64_const(9223372036854775808.0);
                ctx.b.f64_lt();
                ctx.b.i32_and();
                ctx.b.i32_eqz();
                ctx.b.if_void();
                let addr = self.intern("runtime error: to_int: value is out of int range");
                ctx.b.i64_const(addr as i64);
                ctx.b.call(IMP_FAIL);
                ctx.b.unreachable();
                ctx.b.end();
                ctx.b.local_get(v);
                ctx.b.i64_trunc_f64_s();
            }
            "char_at" => {
                self.eval_operands_smart(
                    ctx,
                    scope,
                    &[(&args[0], Some(Type::Str)), (&args[1], Some(Type::Int))],
                );
                ctx.b.call(self.h_char_at);
            }
            "substr" => {
                self.eval_operands_smart(
                    ctx,
                    scope,
                    &[
                        (&args[0], Some(Type::Str)),
                        (&args[1], Some(Type::Int)),
                        (&args[2], Some(Type::Int)),
                    ],
                );
                ctx.b.call(self.h_substr);
            }
            "chr" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.call(self.h_chr);
            }
            "spawn" => {
                // spawn(f, args...) compiles to the task_spawn host import:
                // the host deep-copies the argument graph out of this
                // instance's memory and runs `f` in a fresh instance of the
                // same module on another thread (shared-nothing).
                let ExprKind::Var(target) = &args[0].kind else {
                    unreachable!("validated by cmd_build: spawn target is a named function")
                };
                let target = target.clone();
                let (params, ret) = self.signatures[target.as_str()].clone();
                self.spawn_targets.insert(target.clone());
                // signature string tells the host how to read the argv words
                // and how to ship the result back: i/b/f scalars, s/p pointers
                let mut sig: String = params.iter().map(sig_char).collect();
                sig.push(':');
                sig.push(sig_char(&ret));
                let name_addr = self.intern(&target);
                let sig_addr = self.intern(&sig);
                // evaluate arguments into GC-safe temps
                let ops: Vec<(&Expr, Option<Type>)> = args[1..]
                    .iter()
                    .zip(params.iter())
                    .map(|(a, p)| (a, Some(p.clone())))
                    .collect();
                let saved = self.eval_to_temps(ctx, scope, &ops);
                // pack them into a scalar array; no safepoint can run between
                // filling it and the import call, so GC tracing is not needed
                let n = saved.len() as i64;
                ctx.b.i64_const(HDR + 8 * n);
                ctx.b.call(self.h_alloc);
                let argv = ctx.b.new_local(VT_I64);
                ctx.b.local_set(argv);
                ctx.b.local_get(argv);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(n << 4 | TAG_ARR);
                ctx.b.i64_store(0);
                ctx.b.local_get(argv);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(0);
                ctx.b.i64_store(8);
                for (i, (s, p)) in saved.iter().zip(params.iter()).enumerate() {
                    ctx.b.local_get(argv);
                    ctx.b.i32_wrap_i64();
                    self.reload(ctx, s);
                    let off = HDR as u32 + 8 * i as u32;
                    if *p == Type::Float {
                        ctx.b.f64_store(off);
                    } else {
                        ctx.b.i64_store(off);
                    }
                }
                ctx.b.i64_const(name_addr as i64);
                ctx.b.i64_const(sig_addr as i64);
                ctx.b.local_get(argv);
                ctx.b.call(self.imp_spawn);
            }
            "join_int" | "join_bool" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.call(self.imp_join_i64);
            }
            "join_float" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.call(self.imp_join_f64);
            }
            "join_str" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.call(self.imp_join_str);
            }
            _ => {
                // check if this is an enum variant constructor (Enum::Variant)
                if let Some(colon_pos) = name.rfind("::") {
                    let enum_name = &name[..colon_pos];
                    let variant_name = &name[colon_pos + 2..];
                    if let Some(variants) = self.enum_variants.get(enum_name) {
                        let variant_idx = variants
                            .iter()
                            .position(|v| v.name == variant_name)
                            .expect("variant exists (checked)");
                        let variant = &variants[variant_idx];
                        
                        // allocate enum object: header + tag + payload
                        ctx.b.i64_const(HDR + 16);
                        ctx.b.call(self.h_alloc);
                        let tmp = ctx.b.new_local(VT_I64);
                        ctx.b.local_tee(tmp);
                        let root = ctx.alloc_temp();
                        self.frame_store(ctx, root);
                        
                        // write header: count=2 (tag+payload), tag=STRUCT
                        // bitmap: bit 0 = 1 if payload is pointer
                        let bitmap = if variant.payload.as_ref().map(is_ptr).unwrap_or(false) {
                            1i64
                        } else {
                            0i64
                        };
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(2 << 4 | TAG_STRUCT);
                        ctx.b.i64_store(0);
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(bitmap);
                        ctx.b.i64_store(8);
                        
                        // write tag
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        ctx.b.i64_const(variant_idx as i64);
                        ctx.b.i64_store(HDR as u32);
                        
                        // write payload
                        if let Some(ref payload_ty) = variant.payload {
                            ctx.b.local_get(tmp);
                            ctx.b.i32_wrap_i64();
                            self.expr(ctx, scope, &args[0], Some(payload_ty));
                            if *payload_ty == Type::Float {
                                ctx.b.f64_store(HDR as u32 + 8);
                            } else {
                                ctx.b.i64_store(HDR as u32 + 8);
                            }
                        }
                        
                        ctx.b.local_get(tmp);
                        return;
                    }
                }
                // struct constructor
                if let Some(fields) = self.struct_fields.get(name).map(|f| f.to_vec()) {
                    let bm_words = self.struct_bitmap[name].clone();
                    let n = fields.len() as i64;
                    // small structs keep the pointer bitmap inline in header
                    // word 1; larger ones point at a static multi-word bitmap
                    let (tag, word1) = if fields.len() <= INLINE_BITMAP_WORDS {
                        (TAG_STRUCT, bm_words[0] as i64)
                    } else {
                        (TAG_BIGSTRUCT, self.intern_bitmap(&bm_words) as i64)
                    };
                    ctx.b.i64_const(HDR + 8 * n);
                    ctx.b.call(self.h_alloc);
                    let tmp = ctx.b.new_local(VT_I64);
                    ctx.b.local_tee(tmp);
                    let root = ctx.alloc_temp();
                    self.frame_store(ctx, root);
                    ctx.b.local_get(tmp);
                    ctx.b.i32_wrap_i64();
                    ctx.b.i64_const(n << 4 | tag);
                    ctx.b.i64_store(0);
                    ctx.b.local_get(tmp);
                    ctx.b.i32_wrap_i64();
                    ctx.b.i64_const(word1);
                    ctx.b.i64_store(8);
                    for (i, (fld, arg)) in fields.iter().zip(args).enumerate() {
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        self.expr(ctx, scope, arg, Some(&fld.ty));
                        let off = HDR as u32 + 8 * i as u32;
                        if fld.ty == Type::Float {
                            ctx.b.f64_store(off);
                        } else {
                            ctx.b.i64_store(off);
                        }
                    }
                    ctx.b.local_get(tmp);
                    return;
                }
                let (params, _) = self.signatures[name].clone();
                let ops: Vec<(&Expr, Option<Type>)> = args
                    .iter()
                    .zip(params.iter())
                    .map(|(a, p)| (a, Some(p.clone())))
                    .collect();
                self.eval_operands_smart(ctx, scope, &ops);
                let idx = self.func_index[name];
                ctx.b.call(idx);
            }
        }
    }

    fn type_of(&self, expr: &Expr, scope: &Scope) -> Type {
        match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(_) => Type::Str,
            ExprKind::Var(name) => match lookup(scope, name) {
                Some((_, t)) => t,
                None => {
                    if let Some(colon_pos) = name.rfind("::") {
                        let enum_name = &name[..colon_pos];
                        if self.enum_variants.contains_key(enum_name) {
                            return Type::Enum(enum_name.to_string());
                        }
                    }
                    let (params, ret) = self.signatures[name.as_str()].clone();
                    Type::Fn(params, Box::new(ret))
                }
            },
            ExprKind::Array(elems) => {
                let elem = elems
                    .first()
                    .map(|e| self.type_of(e, scope))
                    .unwrap_or(Type::Int);
                Type::Array(Box::new(elem))
            }
            ExprKind::Index(base, _) => match self.type_of(base, scope) {
                Type::Array(e) => *e,
                _ => unreachable!("checked"),
            },
            ExprKind::Field(base, field) => match self.type_of(base, scope) {
                Type::Struct(sname) => self.field_slot(&sname, field).1,
                _ => unreachable!("checked"),
            },
            ExprKind::Un(UnOp::Neg, inner) => self.type_of(inner, scope),
            ExprKind::Un(UnOp::Not, _) => Type::Bool,
            ExprKind::Bin(op, lhs, _) => {
                use BinOp::*;
                match op {
                    Eq | Ne | Lt | Le | Gt | Ge | And | Or => Type::Bool,
                    _ => self.type_of(lhs, scope),
                }
            }
            ExprKind::Lambda(l) => Type::Fn(
                l.params.iter().map(|p| p.ty.clone()).collect(),
                Box::new(l.ret.clone()),
            ),
            ExprKind::Call(name, args) => {
                if let Some((_, Type::Fn(_, ret))) = lookup(scope, name) {
                    return *ret;
                }
                match name.as_str() {
                    "print" => Type::Unit,
                    "len" | "to_int" | "char_at" => Type::Int,
                    "spawn" | "join_int" => Type::Int,
                    "join_bool" => Type::Bool,
                    "to_float" => Type::Float,
                    "join_float" => Type::Float,
                    "substr" | "chr" => Type::Str,
                    "join_str" => Type::Str,
                    "push" => match self.type_of(&args[0], scope) {
                        t @ Type::Array(_) => t,
                        _ => Type::Array(Box::new(Type::Int)),
                    },
                    other => {
                        if let Some(colon_pos) = other.rfind("::") {
                            let enum_name = &other[..colon_pos];
                            if self.enum_variants.contains_key(enum_name) {
                                return Type::Enum(enum_name.to_string());
                            }
                        }
                        if self.struct_fields.contains_key(other) {
                            Type::Struct(other.to_string())
                        } else {
                            self.signatures[other].1.clone()
                        }
                    }
                }
            }
            ExprKind::Match(m) => {
                // match expression type is the type of the first arm body
                // (checker ensures all arms have the same type)
                if let Some(arm) = m.arms.first() {
                    let scrut_ty = self.type_of(&m.scrutinee, scope);
                    let mut probe = scope.clone();
                    let mut frame = HashMap::new();
                    self.pattern_types(&arm.pattern, &scrut_ty, &mut frame);
                    probe.push(frame);
                    self.type_of(&arm.body, &probe)
                } else {
                    Type::Unit
                }
            }
        }
    }
}

fn lookup(scope: &Scope, name: &str) -> Option<(u32, Type)> {
    for frame in scope.iter().rev() {
        if let Some((idx, ty)) = frame.get(name) {
            return Some((*idx, ty.clone()));
        }
    }
    None
}

fn section(module: &mut Vec<u8>, id: u8, payload: &[u8]) {
    module.push(id);
    uleb(module, payload.len() as u64);
    module.extend_from_slice(payload);
}

fn write_name(buf: &mut Vec<u8>, name: &str) {
    uleb(buf, name.len() as u64);
    buf.extend_from_slice(name.as_bytes());
}
