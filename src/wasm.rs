//! WebAssembly backend. Emits a standard, self-contained .wasm binary
//! (version 1 + bulk-memory) with no external toolchain.
//!
//! Value representation (one wasm value per machino value):
//!   int    -> i64
//!   bool   -> i64 (0 or 1)
//!   float  -> f64
//!   str    -> i64 pointer into linear memory: [len: i64][bytes]
//!   [T]    -> i64 pointer into linear memory: [len: i64][elements, 8 bytes each]
//!   struct -> i64 pointer into linear memory: [fields, 8 bytes each]
//!   fn     -> i64 index into the module's function table (call_indirect)
//!
//! Integer arithmetic is checked: overflow, division by zero, and modulo by
//! zero call the host `fail` import with a message and trap — identical
//! behavior to the reference interpreter.
//!
//! Memory is managed by a bump allocator (mutable global 0 holds the heap
//! pointer). Arrays are immutable-length; push copies. There is no free/GC —
//! fine for short-lived programs, documented in SPEC.md.
//!
//! Host interface (module "env"):
//!   fail(msg: i64)        called before trapping on contract/bounds failures
//!   print_i64 / print_f64 / print_bool / print_str
//!   ...plus every `extern fn` the program declares.
//! The module exports "memory", "alloc" (so hosts can pass strings/arrays
//! in), and every non-std user function.

use crate::ast::*;
use crate::diag::line_col;
use std::collections::{HashMap, HashSet};

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

fn valtype(ty: &Type) -> u8 {
    match ty {
        Type::Float => VT_F64,
        _ => VT_I64,
    }
}

// ---- function body builder ----

struct FnBuilder {
    n_params: u32,
    locals: Vec<u8>, // valtypes of locals beyond params
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
        self.op(0x00); // table 0
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
    fn i64_le_s(&mut self) {
        self.op(0x57);
    }
    fn i64_ge_s(&mut self) {
        self.op(0x59);
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

    /// Final encoding for the code section entry.
    fn finish(self) -> Vec<u8> {
        let mut body = Vec::new();
        // compress consecutive identical local valtypes
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
        body.push(0x0B); // end
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
const N_HELPERS: u32 = 17;

pub struct WasmCompiler<'a> {
    program: &'a Program,
    source: &'a str,
    signatures: HashMap<&'a str, (Vec<Type>, Type)>,
    struct_fields: HashMap<&'a str, &'a [Param]>,
    // type section
    types: Vec<Vec<u8>>, // encoded functype
    type_index: HashMap<Vec<u8>, u32>,
    // static data blob; lives at address 8 in linear memory
    data: Vec<u8>,
    string_addrs: HashMap<String, u32>,
    // function indexing
    func_index: HashMap<&'a str, u32>,
    table_slot: HashMap<&'a str, u32>,
    n_imports: u32,
    // helper function indices
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
}

type Scope<'s> = Vec<HashMap<String, (u32, Type)>>;

struct FnCtx {
    b: FnBuilder,
    /// number of blocks entered since the function's exit block
    nesting: u32,
    /// stack of (break_target, continue_target) nesting levels
    loops: Vec<(u32, u32)>,
    result_local: Option<u32>,
    ret: Type,
}

pub fn compile(program: &Program, source: &str) -> Vec<u8> {
    WasmCompiler::new(program, source).compile()
}

/// Collects the names of all functions reachable from the roots (all
/// non-std, non-extern functions). Unreachable std-prelude functions are
/// not compiled into the module.
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
            types: Vec::new(),
            type_index: HashMap::new(),
            data: Vec::new(),
            string_addrs: HashMap::new(),
            func_index: HashMap::new(),
            table_slot: HashMap::new(),
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

    /// Interns a string constant, returning its address in linear memory.
    fn intern(&mut self, text: &str) -> u32 {
        if let Some(&addr) = self.string_addrs.get(text) {
            return addr;
        }
        while self.data.len() % 8 != 0 {
            self.data.push(0);
        }
        let addr = 8 + self.data.len() as u32;
        self.data
            .extend_from_slice(&(text.len() as i64).to_le_bytes());
        self.data.extend_from_slice(text.as_bytes());
        self.string_addrs.insert(text.to_string(), addr);
        addr
    }

    fn compile(mut self) -> Vec<u8> {
        for f in &self.program.functions {
            self.signatures.insert(
                f.name.as_str(),
                (f.params.iter().map(|p| p.ty.clone()).collect(), f.ret.clone()),
            );
        }
        for s in &self.program.structs {
            self.struct_fields.insert(s.name.as_str(), &s.fields);
        }

        let reachable = reachable_functions(self.program);

        // -- imports: 5 runtime imports + user externs --
        let t_i64_void = self.get_type(&[VT_I64], &[]);
        let t_f64_void = self.get_type(&[VT_F64], &[]);
        let mut imports: Vec<(String, u32)> = vec![
            ("fail".to_string(), t_i64_void),
            ("print_i64".to_string(), t_i64_void),
            ("print_f64".to_string(), t_f64_void),
            ("print_bool".to_string(), t_i64_void),
            ("print_str".to_string(), t_i64_void),
        ];
        let externs: Vec<&Function> =
            self.program.functions.iter().filter(|f| f.is_extern).collect();
        for f in &externs {
            let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
            let tidx = self.sig_type(&params, &f.ret);
            imports.push((f.name.clone(), tidx));
        }
        self.n_imports = imports.len() as u32;
        for (i, f) in externs.iter().enumerate() {
            self.func_index
                .insert(f.name.as_str(), N_RUNTIME_IMPORTS + i as u32);
        }

        // -- assign indices: helpers, then reachable defined functions --
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

        // -- function table (for first-class function values) --
        // externs first, then defined functions, in stable order
        {
            let mut slot = 0u32;
            for f in &externs {
                self.table_slot.insert(f.name.as_str(), slot);
                slot += 1;
            }
            for f in &user_fns {
                self.table_slot.insert(f.name.as_str(), slot);
                slot += 1;
            }
        }

        // -- helper types --
        let t_alloc = self.get_type(&[VT_I64], &[VT_I64]);
        let t_bin_i64 = self.get_type(&[VT_I64, VT_I64], &[VT_I64]);
        let t_push_f64 = self.get_type(&[VT_I64, VT_F64], &[VT_I64]);
        let t_get_f64 = self.get_type(&[VT_I64, VT_I64], &[VT_F64]);
        let t_set_i64 = self.get_type(&[VT_I64, VT_I64, VT_I64], &[]);
        let t_set_f64 = self.get_type(&[VT_I64, VT_I64, VT_F64], &[]);
        let t_substr = self.get_type(&[VT_I64, VT_I64, VT_I64], &[VT_I64]);

        // -- compile bodies --
        let mut func_types: Vec<u32> = vec![
            t_alloc,   // alloc
            t_bin_i64, // concat
            t_bin_i64, // str_eq
            t_bin_i64, // push_i64
            t_push_f64,
            t_bin_i64, // get_i64
            t_get_f64,
            t_set_i64,
            t_set_f64,
            t_bin_i64, // iadd
            t_bin_i64, // isub
            t_bin_i64, // imul
            t_bin_i64, // idiv
            t_bin_i64, // irem
            t_bin_i64, // char_at
            t_substr,  // substr
            t_alloc,   // chr
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
        ];
        for f in &user_fns {
            let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
            func_types.push(self.sig_type(&params, &f.ret));
            bodies.push(self.emit_function(f));
        }

        // -- layout --
        let data_end = {
            let mut end = 8 + self.data.len() as u32;
            end = (end + 7) & !7;
            end
        };
        let min_pages = (data_end as u64 / 65536) + 17; // ~1MB headroom, grows on demand

        // -- assemble module --
        let mut module: Vec<u8> = vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00];

        // type section (1)
        {
            let mut payload = Vec::new();
            uleb(&mut payload, self.types.len() as u64);
            for t in &self.types {
                payload.extend_from_slice(t);
            }
            section(&mut module, 1, &payload);
        }
        // import section (2)
        {
            let mut payload = Vec::new();
            uleb(&mut payload, imports.len() as u64);
            for (name, tidx) in &imports {
                write_name(&mut payload, "env");
                write_name(&mut payload, name);
                payload.push(0x00); // func import
                uleb(&mut payload, *tidx as u64);
            }
            section(&mut module, 2, &payload);
        }
        // function section (3)
        {
            let mut payload = Vec::new();
            uleb(&mut payload, func_types.len() as u64);
            for t in &func_types {
                uleb(&mut payload, *t as u64);
            }
            section(&mut module, 3, &payload);
        }
        // table section (4): funcref table for call_indirect
        let n_slots = self.table_slot.len() as u64;
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x70); // funcref
            payload.push(0x00); // min only
            uleb(&mut payload, n_slots);
            section(&mut module, 4, &payload);
        }
        // memory section (5)
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x00); // min only
            uleb(&mut payload, min_pages);
            section(&mut module, 5, &payload);
        }
        // global section (6): heap pointer
        {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(VT_I32);
            payload.push(0x01); // mutable
            payload.push(0x41); // i32.const
            sleb(&mut payload, data_end as i64);
            payload.push(0x0B);
            section(&mut module, 6, &payload);
        }
        // export section (7): memory + alloc + non-std user functions
        {
            let exported: Vec<&&Function> = user_fns.iter().filter(|f| !f.is_std).collect();
            let mut payload = Vec::new();
            uleb(&mut payload, 2 + exported.len() as u64);
            write_name(&mut payload, "memory");
            payload.push(0x02); // memory export
            uleb(&mut payload, 0);
            write_name(&mut payload, "alloc");
            payload.push(0x00);
            uleb(&mut payload, self.h_alloc as u64);
            for f in exported {
                write_name(&mut payload, &f.name);
                payload.push(0x00); // func export
                uleb(&mut payload, self.func_index[f.name.as_str()] as u64);
            }
            section(&mut module, 7, &payload);
        }
        // element section (9): populate the function table
        if n_slots > 0 {
            let mut slots: Vec<(&str, u32)> =
                self.table_slot.iter().map(|(n, s)| (*n, *s)).collect();
            slots.sort_by_key(|(_, s)| *s);
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x00); // active, table 0, offset expr
            payload.push(0x41); // i32.const 0
            sleb(&mut payload, 0);
            payload.push(0x0B);
            uleb(&mut payload, n_slots);
            for (name, _) in slots {
                uleb(&mut payload, self.func_index[name] as u64);
            }
            section(&mut module, 9, &payload);
        }
        // code section (10)
        {
            let mut payload = Vec::new();
            uleb(&mut payload, bodies.len() as u64);
            for b in &bodies {
                payload.extend_from_slice(b);
            }
            section(&mut module, 10, &payload);
        }
        // data section (11)
        if !self.data.is_empty() {
            let mut payload = Vec::new();
            uleb(&mut payload, 1);
            payload.push(0x00); // active, memory 0
            payload.push(0x41); // i32.const 8
            sleb(&mut payload, 8);
            payload.push(0x0B);
            uleb(&mut payload, self.data.len() as u64);
            payload.extend_from_slice(&self.data);
            section(&mut module, 11, &payload);
        }

        module
    }

    // ---- runtime helper functions (hand-assembled) ----

    fn emit_fail(&mut self, b: &mut FnBuilder, message: &str) {
        let addr = self.intern(message);
        b.i64_const(addr as i64);
        b.call(IMP_FAIL);
        b.unreachable();
    }

    /// fn alloc(size: i64) -> i64  (bump allocator, grows memory on demand)
    fn emit_alloc(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(1);
        let ptr = b.new_local(VT_I64);
        let new_hp = b.new_local(VT_I64);
        // size = (size + 7) & ~7
        b.local_get(0);
        b.i64_const(7);
        b.i64_add();
        b.i64_const(-8);
        b.i64_and();
        b.local_set(0);
        // ptr = hp
        b.global_get(0);
        b.i64_extend_i32_u();
        b.local_set(ptr);
        // new_hp = ptr + size
        b.local_get(ptr);
        b.local_get(0);
        b.i64_add();
        b.local_set(new_hp);
        // if memory_bytes < new_hp: grow
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
            b.unreachable();
            b.end();
        }
        b.end();
        // hp = new_hp
        b.local_get(new_hp);
        b.i32_wrap_i64();
        b.global_set(0);
        b.local_get(ptr);
        b.finish()
    }

    /// fn concat(a: i64, b: i64) -> i64
    fn emit_concat(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let la = b.new_local(VT_I64);
        let lb = b.new_local(VT_I64);
        let out = b.new_local(VT_I64);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.local_set(la);
        b.local_get(1);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.local_set(lb);
        // out = alloc(8 + la + lb)
        b.i64_const(8);
        b.local_get(la);
        b.i64_add();
        b.local_get(lb);
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        // *out = la + lb
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(la);
        b.local_get(lb);
        b.i64_add();
        b.i64_store(0);
        // copy a's bytes
        b.local_get(out);
        b.i64_const(8);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(8);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(la);
        b.i32_wrap_i64();
        b.memory_copy();
        // copy b's bytes
        b.local_get(out);
        b.i64_const(8);
        b.i64_add();
        b.local_get(la);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(1);
        b.i64_const(8);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(lb);
        b.i32_wrap_i64();
        b.memory_copy();
        b.local_get(out);
        b.finish()
    }

    /// fn str_eq(a: i64, b: i64) -> i64
    fn emit_str_eq(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let la = b.new_local(VT_I64);
        let i = b.new_local(VT_I64);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.local_set(la);
        // if len(a) != len(b) return 0
        b.local_get(la);
        b.local_get(1);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_ne();
        b.if_void();
        b.i64_const(0);
        b.ret();
        b.end();
        // byte-by-byte compare
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
            b.i64_load8_u(8);
            b.local_get(1);
            b.local_get(i);
            b.i64_add();
            b.i32_wrap_i64();
            b.i64_load8_u(8);
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

    /// fn push(arr: i64, v) -> i64   (copies into a new, longer array)
    fn emit_push(&mut self, float_elem: bool) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let n = b.new_local(VT_I64);
        let out = b.new_local(VT_I64);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.local_set(n);
        // out = alloc(16 + 8n)
        b.i64_const(16);
        b.local_get(n);
        b.i64_const(3);
        b.i64_shl();
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        // *out = n + 1
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(n);
        b.i64_const(1);
        b.i64_add();
        b.i64_store(0);
        // copy old elements
        b.local_get(out);
        b.i64_const(8);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(8);
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
            b.f64_store(8);
        } else {
            b.i64_store(8);
        }
        b.local_get(out);
        b.finish()
    }

    fn emit_bounds_check(&mut self, b: &mut FnBuilder) {
        // idx < 0 || idx >= len
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_ge_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(b, "runtime error: array index out of bounds");
        b.end();
    }

    /// fn arr_get(ptr: i64, idx: i64) -> elem
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
            b.f64_load(8);
        } else {
            b.i64_load(8);
        }
        b.finish()
    }

    /// fn arr_set(ptr: i64, idx: i64, v)
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
            b.f64_store(8);
        } else {
            b.i64_store(8);
        }
        b.finish()
    }

    /// fn iadd(a, b) -> i64, trapping on overflow.
    fn emit_iadd(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        b.local_get(0);
        b.local_get(1);
        b.i64_add();
        b.local_set(c);
        // overflow iff (a^c) & (b^c) < 0
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

    /// fn isub(a, b) -> i64, trapping on overflow.
    fn emit_isub(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        b.local_get(0);
        b.local_get(1);
        b.i64_sub();
        b.local_set(c);
        // overflow iff (a^b) & (a^c) < 0
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

    /// fn imul(a, b) -> i64, trapping on overflow.
    fn emit_imul(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        let c = b.new_local(VT_I64);
        // a == 0 -> 0
        b.local_get(0);
        b.i64_eqz();
        b.if_void();
        b.i64_const(0);
        b.ret();
        b.end();
        // a == -1: result is -b; only i64::MIN overflows
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
        // general case: c = a*b; overflow iff c/a != b (a != 0, a != -1)
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

    /// fn idiv(a, b) -> i64, trapping on b == 0 and MIN / -1.
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

    /// fn irem(a, b) -> i64, trapping on b == 0 and MIN % -1.
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

    /// fn char_at(str: i64, idx: i64) -> i64 (byte value)
    fn emit_char_at(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(2);
        // idx < 0 || idx >= len
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_ge_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: char_at out of bounds");
        b.end();
        b.local_get(0);
        b.local_get(1);
        b.i64_add();
        b.i32_wrap_i64();
        b.i64_load8_u(8);
        b.finish()
    }

    /// fn substr(str: i64, a: i64, b: i64) -> i64
    fn emit_substr(&mut self) -> Vec<u8> {
        let mut b = FnBuilder::new(3);
        let out = b.new_local(VT_I64);
        let n = b.new_local(VT_I64);
        // a < 0 || a > b || b > len
        b.local_get(1);
        b.i64_const(0);
        b.i64_lt_s();
        b.local_get(1);
        b.local_get(2);
        b.i64_gt_s();
        b.i32_or();
        b.local_get(2);
        b.local_get(0);
        b.i32_wrap_i64();
        b.i64_load(0);
        b.i64_gt_s();
        b.i32_or();
        b.if_void();
        self.emit_fail(&mut b, "runtime error: substr out of range");
        b.end();
        // n = b - a; out = alloc(8 + n)
        b.local_get(2);
        b.local_get(1);
        b.i64_sub();
        b.local_set(n);
        b.i64_const(8);
        b.local_get(n);
        b.i64_add();
        b.call(self.h_alloc);
        b.local_set(out);
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(n);
        b.i64_store(0);
        // copy bytes
        b.local_get(out);
        b.i64_const(8);
        b.i64_add();
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_const(8);
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

    /// fn chr(byte: i64) -> i64 (a one-byte string)
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
        b.i64_const(9);
        b.call(self.h_alloc);
        b.local_set(out);
        b.local_get(out);
        b.i32_wrap_i64();
        b.i64_const(1);
        b.i64_store(0);
        b.local_get(out);
        b.i32_wrap_i64();
        b.local_get(0);
        b.i64_store8(8);
        b.local_get(out);
        b.finish()
    }

    // ---- user function codegen ----

    fn emit_function(&mut self, f: &Function) -> Vec<u8> {
        let mut ctx = FnCtx {
            b: FnBuilder::new(f.params.len() as u32),
            nesting: 0,
            loops: Vec::new(),
            result_local: None,
            ret: f.ret.clone(),
        };
        let mut scope: Scope = vec![HashMap::new()];
        for (i, p) in f.params.iter().enumerate() {
            scope[0].insert(p.name.clone(), (i as u32, p.ty.clone()));
        }

        // requires clauses run first, against the incoming arguments
        for c in &f.requires {
            let msg = format!(
                "runtime error: contract violation: requires '{}' failed when calling '{}'",
                c.text, f.name
            );
            let addr = self.intern(&msg);
            self.expr(&mut ctx, &scope, &c.expr, None);
            ctx.b.i32_wrap_i64();
            ctx.b.i32_eqz();
            ctx.b.if_void();
            ctx.b.i64_const(addr as i64);
            ctx.b.call(IMP_FAIL);
            ctx.b.unreachable();
            ctx.b.end();
        }

        // shadow-copy params so ensures sees the original argument values
        let mut shadows: Vec<(String, u32, Type)> = Vec::new();
        if !f.ensures.is_empty() {
            for (i, p) in f.params.iter().enumerate() {
                let sh = ctx.b.new_local(valtype(&p.ty));
                ctx.b.local_get(i as u32);
                ctx.b.local_set(sh);
                shadows.push((p.name.clone(), sh, p.ty.clone()));
            }
        }

        if f.ret != Type::Unit {
            ctx.result_local = Some(ctx.b.new_local(valtype(&f.ret)));
        }

        // function body inside an exit block; 'return' branches to its end
        ctx.b.block_void();
        ctx.nesting = 0;
        self.stmts(&mut ctx, &mut scope, &f.body);
        ctx.b.end();

        // ensures clauses check 'result' against the original arguments
        if !f.ensures.is_empty() {
            let mut ens_scope: Scope = vec![HashMap::new()];
            for (name, idx, ty) in &shadows {
                ens_scope[0].insert(name.clone(), (*idx, ty.clone()));
            }
            if let Some(r) = ctx.result_local {
                ens_scope[0].insert("result".to_string(), (r, f.ret.clone()));
            }
            for c in &f.ensures {
                let msg = format!(
                    "runtime error: contract violation: ensures '{}' failed in '{}'",
                    c.text, f.name
                );
                let addr = self.intern(&msg);
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

        if let Some(r) = ctx.result_local {
            ctx.b.local_get(r);
        }
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
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let vty = match ty {
                    Some(t) => t.clone(),
                    None => self.type_of(value, scope),
                };
                self.expr(ctx, scope, value, Some(&vty));
                let idx = ctx.b.new_local(valtype(&vty));
                ctx.b.local_set(idx);
                scope.last_mut().unwrap().insert(name.clone(), (idx, vty));
            }
            StmtKind::Assign { name, value } => {
                let (idx, ty) = lookup(scope, name).expect("checked");
                self.expr(ctx, scope, value, Some(&ty));
                ctx.b.local_set(idx);
            }
            StmtKind::IndexAssign { base, index, value } => {
                let elem = match self.type_of(base, scope) {
                    Type::Array(e) => *e,
                    _ => unreachable!(),
                };
                self.expr(ctx, scope, base, None);
                self.expr(ctx, scope, index, Some(&Type::Int));
                self.expr(ctx, scope, value, Some(&elem));
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
                self.expr(ctx, scope, base, None);
                ctx.b.i32_wrap_i64();
                self.expr(ctx, scope, value, Some(&fty));
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
                self.expr(ctx, scope, cond, Some(&Type::Bool));
                ctx.b.i32_wrap_i64();
                ctx.b.i32_eqz();
                ctx.b.br_if(ctx.nesting - t_break);
                ctx.loops.push((t_break, t_continue));
                self.stmts(ctx, scope, body);
                ctx.loops.pop();
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
                // end is evaluated once, before the loop
                let e_local = ctx.b.new_local(VT_I64);
                self.expr(ctx, scope, end, Some(&Type::Int));
                ctx.b.local_set(e_local);
                let i_local = ctx.b.new_local(VT_I64);
                self.expr(ctx, scope, start, Some(&Type::Int));
                ctx.b.local_set(i_local);
                scope.push(HashMap::new());
                scope
                    .last_mut()
                    .unwrap()
                    .insert(var.clone(), (i_local, Type::Int));

                ctx.b.block_void(); // A: break target
                ctx.nesting += 1;
                let t_break = ctx.nesting;
                ctx.b.loop_void(); // L
                ctx.nesting += 1;
                let t_loop = ctx.nesting;
                // if i >= e, exit
                ctx.b.local_get(i_local);
                ctx.b.local_get(e_local);
                ctx.b.i64_ge_s();
                ctx.b.br_if(ctx.nesting - t_break);
                ctx.b.block_void(); // C: continue target (increment still runs)
                ctx.nesting += 1;
                let t_continue = ctx.nesting;
                ctx.loops.push((t_break, t_continue));
                self.stmts(ctx, scope, body);
                ctx.loops.pop();
                ctx.nesting -= 1;
                ctx.b.end(); // C
                // i = i + 1 (checked, same as the interpreter)
                ctx.b.local_get(i_local);
                ctx.b.i64_const(1);
                ctx.b.call(self.h_iadd);
                ctx.b.local_set(i_local);
                ctx.b.br(ctx.nesting - t_loop);
                ctx.nesting -= 2;
                ctx.b.end(); // L
                ctx.b.end(); // A
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
        (8 * idx as u32, fields[idx].ty.clone())
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
                } else {
                    // a bare function name is a function-table slot
                    let slot = self.table_slot[name.as_str()];
                    ctx.b.i64_const(slot as i64);
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
                ctx.b.i64_const(8 + 8 * n);
                ctx.b.call(self.h_alloc);
                let tmp = ctx.b.new_local(VT_I64);
                ctx.b.local_set(tmp);
                ctx.b.local_get(tmp);
                ctx.b.i32_wrap_i64();
                ctx.b.i64_const(n);
                ctx.b.i64_store(0);
                for (i, e) in elems.iter().enumerate() {
                    ctx.b.local_get(tmp);
                    ctx.b.i32_wrap_i64();
                    self.expr(ctx, scope, e, Some(&elem_ty));
                    let off = 8 + 8 * i as u32;
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
                let elem = match base_ty {
                    Type::Array(e) => *e,
                    _ => unreachable!(),
                };
                self.expr(ctx, scope, base, None);
                self.expr(ctx, scope, index, Some(&Type::Int));
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
        }
    }

    fn binop(&mut self, ctx: &mut FnCtx, scope: &Scope, op: BinOp, lhs: &Expr, rhs: &Expr) {
        use BinOp::*;
        // short-circuit logic ops compile to if-expressions
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
        self.expr(ctx, scope, lhs, None);
        self.expr(ctx, scope, rhs, Some(&lt));

        match (&lt, op) {
            (Type::Str, Add) => ctx.b.call(self.h_concat),
            (Type::Str, Eq) => ctx.b.call(self.h_str_eq),
            (Type::Str, Ne) => {
                ctx.b.call(self.h_str_eq);
                ctx.b.i64_eqz();
                ctx.b.i64_extend_i32_u();
            }
            (Type::Float, _) => {
                match op {
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
                }
            }
            // int (checked arithmetic) and bool (comparisons only)
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
            for (a, p) in args.iter().zip(params.iter()) {
                self.expr(ctx, scope, a, Some(p));
            }
            ctx.b.local_get(idx);
            ctx.b.i32_wrap_i64();
            let type_idx = self.sig_type(&params, &ret);
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
                ctx.b.i32_wrap_i64();
                ctx.b.i64_load(0);
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
                self.expr(ctx, scope, &args[0], Some(&arr_ty));
                self.expr(ctx, scope, &args[1], Some(&elem));
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
                // guard the trunc range so out-of-range values fail with a
                // message instead of a bare wasm trap
                self.expr(ctx, scope, &args[0], Some(&Type::Float));
                let v = ctx.b.new_local(VT_F64);
                ctx.b.local_set(v);
                ctx.b.local_get(v);
                ctx.b.f64_const(-9223372036854775808.0); // -(2^63), exact
                ctx.b.f64_ge();
                ctx.b.local_get(v);
                ctx.b.f64_const(9223372036854775808.0); // 2^63, exact
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
                self.expr(ctx, scope, &args[0], Some(&Type::Str));
                self.expr(ctx, scope, &args[1], Some(&Type::Int));
                ctx.b.call(self.h_char_at);
            }
            "substr" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Str));
                self.expr(ctx, scope, &args[1], Some(&Type::Int));
                self.expr(ctx, scope, &args[2], Some(&Type::Int));
                ctx.b.call(self.h_substr);
            }
            "chr" => {
                self.expr(ctx, scope, &args[0], Some(&Type::Int));
                ctx.b.call(self.h_chr);
            }
            _ => {
                // struct constructor: allocate and fill fields
                if let Some(fields) = self.struct_fields.get(name).map(|f| f.to_vec()) {
                    let n = fields.len() as i64;
                    ctx.b.i64_const(8 * n);
                    ctx.b.call(self.h_alloc);
                    let tmp = ctx.b.new_local(VT_I64);
                    ctx.b.local_set(tmp);
                    for (i, (fld, arg)) in fields.iter().zip(args).enumerate() {
                        ctx.b.local_get(tmp);
                        ctx.b.i32_wrap_i64();
                        self.expr(ctx, scope, arg, Some(&fld.ty));
                        let off = 8 * i as u32;
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
                for (a, p) in args.iter().zip(params.iter()) {
                    self.expr(ctx, scope, a, Some(p));
                }
                let idx = self.func_index[name];
                ctx.b.call(idx);
            }
        }
    }

    /// Type of an already-checked expression (cannot fail).
    fn type_of(&self, expr: &Expr, scope: &Scope) -> Type {
        match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(_) => Type::Str,
            ExprKind::Var(name) => match lookup(scope, name) {
                Some((_, t)) => t,
                None => {
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
            ExprKind::Call(name, args) => {
                if let Some((_, Type::Fn(_, ret))) = lookup(scope, name) {
                    return *ret;
                }
                match name.as_str() {
                    "print" => Type::Unit,
                    "len" | "to_int" | "char_at" => Type::Int,
                    "to_float" => Type::Float,
                    "substr" | "chr" => Type::Str,
                    "push" => match self.type_of(&args[0], scope) {
                        t @ Type::Array(_) => t,
                        _ => Type::Array(Box::new(Type::Int)),
                    },
                    other => {
                        if self.struct_fields.contains_key(other) {
                            Type::Struct(other.to_string())
                        } else {
                            self.signatures[other].1.clone()
                        }
                    }
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
