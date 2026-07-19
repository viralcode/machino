//! WASM-GC backend (`machino build --gc`): compiles to the WebAssembly GC
//! proposal, using reference-typed GC objects instead of the linear-memory
//! mark-sweep collector in wasm.rs. The host's collector manages all memory.
//!
//! Value representation:
//!   - int -> i64, float -> f64, bool -> i32
//!   - str -> (ref null $bytes), an immutable GC byte array
//!   - [int]/[float] -> specialized GC arrays of i64/f64
//!   - structs, enums, closures, and arrays of references -> $anyarr, a GC
//!     array of anyref. Scalars stored in $anyarr slots are boxed in $boxi64
//!     / $boxf64 structs; reads cast back with ref.cast. An enum value is
//!     [box(tag), payload-or-null]; a closure is [box(table slot), captures].
//!   - function calls through values use a funcref table + call_indirect,
//!     passing the closure as a hidden trailing parameter.
//!
//! The full language compiles on this backend. Externs, channels, and
//! spawn/join (int/bool spawn args) are supported via host imports
//! (see runners/run-gc.mjs).
//!
//! Run the output with `node runners/run-gc.mjs out.wasm` (Node 22+ / any
//! host with WASM GC).

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::{BTreeMap, HashMap, HashSet};

// value types
const I32: u8 = 0x7f;
const I64: u8 = 0x7e;
const F64: u8 = 0x7c;
const ANYREF: u8 = 0x6e;
const FUNCREF: u8 = 0x70;
const REF_NULL: u8 = 0x63; // (ref null $t) followed by type index

// GC type indices (fixed layout in the type section)
const TY_BYTES: u32 = 0; // array (mut i8)   — strings
const TY_ARR_I64: u32 = 1; // array (mut i64)
const TY_ARR_F64: u32 = 2; // array (mut f64)
const TY_ANYARR: u32 = 3; // array (mut anyref) — structs/enums/closures/ref arrays
const TY_BOXI64: u32 = 4; // struct { i64 } — boxed int/bool
const TY_BOXF64: u32 = 5; // struct { f64 } — boxed float
const N_FIXED_TYPES: u32 = 6;

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
const OP_CALL_INDIRECT: u8 = 0x11;
const OP_REF_NULL: u8 = 0xd0;
const OP_I32_CONST: u8 = 0x41;
const OP_I64_CONST: u8 = 0x42;
const OP_F64_CONST: u8 = 0x44;
const OP_I32_EQZ: u8 = 0x45;
const OP_I32_EQ: u8 = 0x46;
const OP_I32_NE: u8 = 0x47;
const OP_I32_LT_U: u8 = 0x49;
const OP_I32_GE_S: u8 = 0x4e;
const OP_I64_EQ: u8 = 0x51;
const OP_I64_NE: u8 = 0x52;
const OP_I64_LT_S: u8 = 0x53;
const OP_I64_LT_U: u8 = 0x54;
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
const OP_I32_SUB: u8 = 0x6b;
const OP_I32_OR: u8 = 0x72;
const OP_I32_AND: u8 = 0x71;
const OP_I32_SHL: u8 = 0x74;
const OP_I64_ADD: u8 = 0x7c;
const OP_I64_SUB: u8 = 0x7d;
const OP_I64_MUL: u8 = 0x7e;
const OP_I64_DIV_S: u8 = 0x7f;
const OP_I64_REM_U: u8 = 0x80;
const OP_I64_REM_S: u8 = 0x81;
const OP_I64_SHR_U: u8 = 0x88;
const OP_I64_AND: u8 = 0x83;
const OP_I64_XOR: u8 = 0x85;
const OP_F64_NEG: u8 = 0x9a;
const OP_F64_ADD: u8 = 0xa0;
const OP_F64_SUB: u8 = 0xa1;
const OP_F64_MUL: u8 = 0xa2;
const OP_F64_DIV: u8 = 0xa3;
const OP_I32_WRAP_I64: u8 = 0xa7;
const OP_I64_EXTEND_I32_S: u8 = 0xac;
const OP_I64_EXTEND_I32_U: u8 = 0xad;
const OP_I64_TRUNC_F64_S: u8 = 0xb0;
const OP_F64_CONVERT_I64_S: u8 = 0xb9;
const OP_I64_REINTERPRET_F64: u8 = 0xbd;
const GC_PREFIX: u8 = 0xfb;
const GC_STRUCT_NEW: u8 = 0x00;
const GC_STRUCT_GET: u8 = 0x02;
const GC_ARRAY_NEW_DEFAULT: u8 = 0x07;
const GC_ARRAY_NEW_FIXED: u8 = 0x08;
const GC_ARRAY_NEW_DATA: u8 = 0x09;
const GC_ARRAY_GET: u8 = 0x0b;
const GC_ARRAY_GET_U: u8 = 0x0d;
const GC_ARRAY_SET: u8 = 0x0e;
const GC_ARRAY_LEN: u8 = 0x0f;
const GC_ARRAY_COPY: u8 = 0x11;
const GC_REF_CAST_NULL: u8 = 0x17;

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

fn append_i64_const(out: &mut Vec<u8>, v: i64) {
    out.push(OP_I64_CONST);
    sleb(out, v);
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
    let _ = span;
    Ok(match ty {
        Type::Int => Some(vec![I64]),
        Type::Float => Some(vec![F64]),
        Type::Bool => Some(vec![I32]),
        Type::Str => Some(vec![REF_NULL, TY_BYTES as u8]),
        Type::Array(inner) => match inner.as_ref() {
            Type::Int => Some(vec![REF_NULL, TY_ARR_I64 as u8]),
            Type::Float => Some(vec![REF_NULL, TY_ARR_F64 as u8]),
            _ => Some(vec![REF_NULL, TY_ANYARR as u8]),
        },
        Type::Struct(_) | Type::Enum(_) | Type::App(_, _) | Type::Fn(_, _) => {
            Some(vec![REF_NULL, TY_ANYARR as u8])
        }
        Type::Unit => None,
        Type::TypeVar(_) => unreachable!("monomorphized before codegen"),
    })
}

fn array_type_index(elem: &Type) -> u32 {
    match elem {
        Type::Int => TY_ARR_I64,
        Type::Float => TY_ARR_F64,
        _ => TY_ANYARR,
    }
}

/// Wraps the value on top of the stack (of machino type `ty`) as an anyref
/// suitable for an $anyarr slot.
fn box_value(code: &mut Vec<u8>, ty: &Type) {
    match ty {
        Type::Int => {
            code.push(GC_PREFIX);
            code.push(GC_STRUCT_NEW);
            uleb(code, TY_BOXI64 as u64);
        }
        Type::Bool => {
            code.push(OP_I64_EXTEND_I32_U);
            code.push(GC_PREFIX);
            code.push(GC_STRUCT_NEW);
            uleb(code, TY_BOXI64 as u64);
        }
        Type::Float => {
            code.push(GC_PREFIX);
            code.push(GC_STRUCT_NEW);
            uleb(code, TY_BOXF64 as u64);
        }
        // reference types are already anyref-compatible
        _ => {}
    }
}

/// Casts/unboxes the anyref on top of the stack back to machino type `ty`.
fn unbox_value(code: &mut Vec<u8>, ty: &Type) {
    match ty {
        Type::Int | Type::Bool => {
            code.push(GC_PREFIX);
            code.push(GC_REF_CAST_NULL);
            sleb(code, TY_BOXI64 as i64);
            code.push(GC_PREFIX);
            code.push(GC_STRUCT_GET);
            uleb(code, TY_BOXI64 as u64);
            uleb(code, 0);
            if *ty == Type::Bool {
                code.push(OP_I32_WRAP_I64);
            }
        }
        Type::Float => {
            code.push(GC_PREFIX);
            code.push(GC_REF_CAST_NULL);
            sleb(code, TY_BOXF64 as i64);
            code.push(GC_PREFIX);
            code.push(GC_STRUCT_GET);
            uleb(code, TY_BOXF64 as u64);
            uleb(code, 0);
        }
        Type::Str => {
            code.push(GC_PREFIX);
            code.push(GC_REF_CAST_NULL);
            sleb(code, TY_BYTES as i64);
        }
        Type::Array(inner) => {
            code.push(GC_PREFIX);
            code.push(GC_REF_CAST_NULL);
            sleb(code, array_type_index(inner) as i64);
        }
        _ => {
            code.push(GC_PREFIX);
            code.push(GC_REF_CAST_NULL);
            sleb(code, TY_ANYARR as i64);
        }
    }
}

/// (import index space) host imports, in order.
const IMPORTS: &[(&str, &[u8], &[u8])] = &[
    // (name, param valtypes, result valtypes) — refs written as 2 bytes
    ("fail", &[REF_NULL, TY_BYTES as u8], &[]),
    ("print_i64", &[I64], &[]),
    ("print_f64", &[F64], &[]),
    ("print_bool", &[I32], &[]),
    ("print_str", &[REF_NULL, TY_BYTES as u8], &[]),
];
const IMP_FAIL: u32 = 0;
const IMP_PRINT_I64: u32 = 1;
const IMP_PRINT_F64: u32 = 2;
const IMP_PRINT_BOOL: u32 = 3;
const IMP_PRINT_STR: u32 = 4;
/// Fixed print/fail imports; concurrency + extern imports follow these.
const N_BASE_IMPORTS: u32 = 5;

// helper function offsets (added to Compiler.n_imports)
const N_HELPERS: u32 = 22; // + make_str/str_set/i64arr_len/i64arr_get
const OFF_CONCAT: u32 = 0;
const OFF_STREQ: u32 = 1;
const OFF_SUBSTR: u32 = 2;
const OFF_PUSH_I64: u32 = 3;
const OFF_PUSH_F64: u32 = 4;
const OFF_PUSH_ANY: u32 = 5;
const OFF_STR_LEN: u32 = 6;
const OFF_STR_AT: u32 = 7;
const OFF_IADD: u32 = 8;
const OFF_ISUB: u32 = 9;
const OFF_IMUL: u32 = 10;
const OFF_IREM: u32 = 11;
const OFF_IDIV: u32 = 12;
const OFF_LEN_CP: u32 = 13;
const OFF_CHAR_AT_CP: u32 = 14;
const OFF_SUBSTR_CP: u32 = 15;
const OFF_CHR_CP: u32 = 16;
const OFF_HASH_STR: u32 = 17;
const OFF_MAKE_STR: u32 = 18; // (i64 len) -> bytes
const OFF_STR_SET: u32 = 19; // (bytes, i32 idx, i32 byte) -> ()
const OFF_I64ARR_LEN: u32 = 20; // (arr_i64) -> i32
const OFF_I64ARR_GET: u32 = 21; // (arr_i64, i32) -> i64
const HASH_MUL: i64 = -7030230956028963701; // 11400714819323198485u64 as i64
const HASH_MOD: i64 = 1_000_000_007;

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

    let mut uses_channels = false;
    let mut uses_spawn = false;
    {
        let mut names: Vec<String> = Vec::new();
        for f in &reachable {
            if !f.is_extern {
                collect_calls_stmts(&f.body, &mut names);
            }
        }
        for n in &names {
            match n.as_str() {
                "chan_new" | "chan_close"
                | "chan_send_int" | "chan_send_float" | "chan_send_bool" | "chan_send_str"
                | "chan_recv_int" | "chan_recv_float" | "chan_recv_bool" | "chan_recv_str" => {
                    uses_channels = true;
                }
                "spawn" | "join_int" | "join_float" | "join_bool" | "join_str" => {
                    uses_spawn = true;
                }
                _ => {}
            }
        }
    }
    let externs: Vec<&Function> = reachable.iter().copied().filter(|f| f.is_extern).collect();
    let user_fns: Vec<&Function> = reachable.iter().copied().filter(|f| !f.is_extern).collect();

    // import layout: base(5) + optional channels(8) + optional spawn(4) + externs
    let mut n_imports = N_BASE_IMPORTS;
    let mut imp_chan_new = 0u32;
    let mut imp_chan_close = 0u32;
    let mut imp_chan_send_i64 = 0u32;
    let mut imp_chan_send_f64 = 0u32;
    let mut imp_chan_send_str = 0u32;
    let mut imp_chan_recv_i64 = 0u32;
    let mut imp_chan_recv_f64 = 0u32;
    let mut imp_chan_recv_str = 0u32;
    let mut imp_spawn_pack_str = 0u32;
    let mut imp_spawn = 0u32;
    let mut imp_join_i64 = 0u32;
    let mut imp_join_f64 = 0u32;
    let mut imp_join_str = 0u32;
    if uses_channels {
        imp_chan_new = n_imports;
        imp_chan_close = n_imports + 1;
        imp_chan_send_i64 = n_imports + 2;
        imp_chan_send_f64 = n_imports + 3;
        imp_chan_send_str = n_imports + 4;
        imp_chan_recv_i64 = n_imports + 5;
        imp_chan_recv_f64 = n_imports + 6;
        imp_chan_recv_str = n_imports + 7;
        n_imports += 8;
    }
    if uses_spawn {
        imp_spawn_pack_str = n_imports;
        imp_spawn = n_imports + 1;
        imp_join_i64 = n_imports + 2;
        imp_join_f64 = n_imports + 3;
        imp_join_str = n_imports + 4;
        n_imports += 5;
    }
    let extern_base = n_imports;
    n_imports += externs.len() as u32;
    let user_type_base = N_FIXED_TYPES + n_imports + N_HELPERS;

    let mut c = Compiler {
        func_index: HashMap::new(),
        signatures: HashMap::new(),
        func_types: Vec::new(),
        data_segments: Vec::new(),
        structs: HashMap::new(),
        enums: HashMap::new(),
        lambdas: BTreeMap::new(),
        lambda_slot: HashMap::new(),
        lambda_captures: HashMap::new(),
        wrapper_slot: HashMap::new(),
        n_imports,
        user_type_base,
        imp_chan_new,
        imp_chan_close,
        imp_chan_send_i64,
        imp_chan_send_f64,
        imp_chan_send_str,
        imp_chan_recv_i64,
        imp_chan_recv_f64,
        imp_chan_recv_str,
        imp_spawn_pack_str,
        imp_spawn,
        imp_join_i64,
        imp_join_f64,
        imp_join_str,
        extern_index: HashMap::new(),
        spawn_targets: std::collections::BTreeSet::new(),
    };
    for s in &program.structs {
        c.structs.insert(s.name.clone(), s.fields.clone());
    }
    for e in &program.enums {
        c.enums.insert(e.name.clone(), e.variants.clone());
    }
    let contracted_externs: Vec<&Function> = externs
        .iter()
        .copied()
        .filter(|f| !f.requires.is_empty() || !f.ensures.is_empty())
        .collect();
    for (i, f) in externs.iter().enumerate() {
        c.extern_index
            .insert(f.name.clone(), extern_base + i as u32);
        c.signatures.insert(
            f.name.clone(),
            (
                f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(),
                f.ret.clone(),
            ),
        );
    }
    let n_ext_wrap = contracted_externs.len() as u32;
    let ext_wrap_base = n_imports + N_HELPERS;
    for (i, f) in contracted_externs.iter().enumerate() {
        // wrappers sit in front of user functions so calls hit contracts first
        c.func_index
            .insert(f.name.clone(), ext_wrap_base + i as u32);
    }
    for (i, f) in user_fns.iter().enumerate() {
        c.func_index
            .insert(f.name.clone(), ext_wrap_base + n_ext_wrap + i as u32);
        c.signatures.insert(
            f.name.clone(),
            (
                f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(),
                f.ret.clone(),
            ),
        );
    }
    // keep `reachable` as user functions only for body compilation
    let reachable = user_fns;

    // ---- closures: collect lambdas and named-function values upfront ----
    let mut fn_value_names: Vec<String> = Vec::new();
    for f in &reachable {
        collect_lambdas_stmts(&f.body, &mut c.lambdas);
        for ct in f.requires.iter().chain(f.ensures.iter()) {
            collect_lambdas_expr(&ct.expr, &mut c.lambdas);
        }
        collect_fn_value_names_stmts(&f.body, &mut fn_value_names);
        for ct in f.requires.iter().chain(f.ensures.iter()) {
            let wrapper = [Stmt {
                kind: StmtKind::Expr(ct.expr.clone()),
                span: ct.expr.span,
            }];
            collect_fn_value_names_stmts(&wrapper, &mut fn_value_names);
        }
    }
    fn_value_names.retain(|n| c.signatures.contains_key(n));
    fn_value_names.sort();
    fn_value_names.dedup();

    // function indices: imports, helpers, extern wrappers, user fns, fn wrappers, lambdas
    let wrapper_base = ext_wrap_base + n_ext_wrap + reachable.len() as u32;
    let lambda_base = wrapper_base + fn_value_names.len() as u32;
    let lambda_ids: Vec<usize> = c.lambdas.keys().copied().collect();
    // table slots: wrappers first, then lambdas
    let mut slot = 0u32;
    for name in &fn_value_names {
        c.wrapper_slot.insert(name.clone(), slot);
        slot += 1;
    }
    for id in &lambda_ids {
        c.lambda_slot.insert(*id, slot);
        slot += 1;
    }
    let n_slots = slot;

    // ---- compile function bodies first (they register func types/data) ----
    let helper_bodies = c.helper_bodies();
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let mut type_indices: Vec<u32> = Vec::new();
    for f in &contracted_externs {
        let host = c.extern_index[&f.name];
        let (type_idx, body) = c.compile_extern_wrapper(f, host)?;
        type_indices.push(type_idx);
        bodies.push(body);
    }
    for f in &reachable {
        let (type_idx, body) = c.compile_function(f)?;
        type_indices.push(type_idx);
        bodies.push(body);
    }
    // wrappers: adapt a named function to the closure calling convention by
    // dropping the trailing env parameter
    let mut wrapper_type_indices: Vec<u32> = Vec::new();
    let mut wrapper_bodies: Vec<Vec<u8>> = Vec::new();
    for name in &fn_value_names {
        let (params, ret) = c.signatures[name].clone();
        let tidx = c.env_type_index(&params, &ret)?;
        wrapper_type_indices.push(tidx);
        let mut body = vec![0]; // no extra locals
        for i in 0..params.len() as u32 {
            body.push(OP_LOCAL_GET);
            uleb(&mut body, i as u64);
        }
        body.push(OP_CALL);
        uleb(&mut body, c.func_index[name] as u64);
        body.push(OP_END);
        wrapper_bodies.push(body);
    }
    // lambda bodies, parents before children so nested captures resolve
    let mut lambda_type_indices: Vec<u32> = Vec::new();
    let mut lambda_bodies: Vec<Vec<u8>> = Vec::new();
    for id in &lambda_ids {
        let l = c.lambdas[id].clone();
        let (tidx, body) = c.compile_lambda(&l)?;
        lambda_type_indices.push(tidx);
        lambda_bodies.push(body);
    }

    // ---- assemble the module ----
    let mut module = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    // type section: fixed GC types + import/helper/user func types
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
        (
            vec![REF_NULL, TY_ANYARR as u8, ANYREF],
            vec![REF_NULL, TY_ANYARR as u8],
        ), // push_any
        (vec![REF_NULL, TY_BYTES as u8], vec![I32]), // str_len
        (vec![REF_NULL, TY_BYTES as u8, I32], vec![I32]), // str_at
        (vec![I64, I64], vec![I64]), // iadd
        (vec![I64, I64], vec![I64]), // isub
        (vec![I64, I64], vec![I64]), // imul
        (vec![I64, I64], vec![I64]), // irem
        (vec![I64, I64], vec![I64]), // idiv
        (vec![REF_NULL, TY_BYTES as u8], vec![I64]), // len_cp
        (vec![REF_NULL, TY_BYTES as u8, I64], vec![I64]), // char_at_cp
        (
            vec![REF_NULL, TY_BYTES as u8, I64, I64],
            vec![REF_NULL, TY_BYTES as u8],
        ), // substr_cp
        (vec![I64], vec![REF_NULL, TY_BYTES as u8]), // chr_cp
        (vec![REF_NULL, TY_BYTES as u8], vec![I64]), // hash_str
        (vec![I64], vec![REF_NULL, TY_BYTES as u8]), // make_str
        (vec![REF_NULL, TY_BYTES as u8, I32, I32], vec![]), // str_set
        (vec![REF_NULL, TY_ARR_I64 as u8], vec![I32]), // i64arr_len
        (vec![REF_NULL, TY_ARR_I64 as u8, I32], vec![I64]), // i64arr_get
    ];
    // Build the full import descriptor list (name, params, results).
    let mut import_descs: Vec<(String, Vec<u8>, Vec<u8>)> = IMPORTS
        .iter()
        .map(|(n, p, r)| (n.to_string(), p.to_vec(), r.to_vec()))
        .collect();
    if uses_channels {
        let bytes = vec![REF_NULL, TY_BYTES as u8];
        import_descs.push(("chan_new".into(), vec![], vec![I64]));
        import_descs.push(("chan_close".into(), vec![I64], vec![]));
        import_descs.push(("chan_send_i64".into(), vec![I64, I64], vec![]));
        import_descs.push(("chan_send_f64".into(), vec![I64, F64], vec![]));
        import_descs.push(("chan_send_str".into(), {
            let mut p = vec![I64];
            p.extend_from_slice(&bytes);
            p
        }, vec![]));
        import_descs.push(("chan_recv_i64".into(), vec![I64], vec![I64]));
        import_descs.push(("chan_recv_f64".into(), vec![I64], vec![F64]));
        import_descs.push(("chan_recv_str".into(), vec![I64], bytes));
    }
    if uses_spawn {
        let bytes = vec![REF_NULL, TY_BYTES as u8];
        let i64arr = vec![REF_NULL, TY_ARR_I64 as u8];
        import_descs.push(("spawn_pack_str".into(), bytes.clone(), vec![I64]));
        let mut spawn_params = bytes.clone();
        spawn_params.extend_from_slice(&bytes);
        spawn_params.extend_from_slice(&i64arr);
        import_descs.push(("task_spawn".into(), spawn_params, vec![I64]));
        import_descs.push(("task_join_i64".into(), vec![I64], vec![I64]));
        import_descs.push(("task_join_f64".into(), vec![I64], vec![F64]));
        import_descs.push(("task_join_str".into(), vec![I64], bytes));
    }
    for f in &externs {
        let mut params = Vec::new();
        for p in &f.params {
            if let Some(vt) = valtype(&p.ty, f.span)? {
                params.extend(vt);
            }
        }
        let mut results = Vec::new();
        if let Some(vt) = valtype(&f.ret, f.span)? {
            results.extend(vt);
        }
        import_descs.push((f.name.clone(), params, results));
    }
    debug_assert_eq!(import_descs.len() as u32, n_imports);

    let mut tsec = Vec::new();
    let n_types =
        N_FIXED_TYPES as usize + import_descs.len() + helper_types.len() + c.func_types.len();
    uleb(&mut tsec, n_types as u64);
    // 0: bytes (array (mut i8))
    tsec.extend_from_slice(&[0x5e, 0x78, 0x01]);
    // 1: array (mut i64)
    tsec.extend_from_slice(&[0x5e, I64, 0x01]);
    // 2: array (mut f64)
    tsec.extend_from_slice(&[0x5e, F64, 0x01]);
    // 3: array (mut anyref)
    tsec.extend_from_slice(&[0x5e, ANYREF, 0x01]);
    // 4: struct { i64 } (immutable field)
    tsec.extend_from_slice(&[0x5f, 0x01, I64, 0x00]);
    // 5: struct { f64 }
    tsec.extend_from_slice(&[0x5f, 0x01, F64, 0x00]);
    // import func types
    let mut import_type_idx = Vec::new();
    for (_, params, results) in &import_descs {
        import_type_idx.push(N_FIXED_TYPES + import_type_idx.len() as u32);
        tsec.push(0x60);
        uleb(&mut tsec, count_valtypes(params) as u64);
        tsec.extend_from_slice(params);
        uleb(&mut tsec, count_valtypes(results) as u64);
        tsec.extend_from_slice(results);
    }
    // helper func types
    let helper_type_base = N_FIXED_TYPES + import_descs.len() as u32;
    for (params, results) in &helper_types {
        tsec.push(0x60);
        uleb(&mut tsec, count_valtypes(params) as u64);
        tsec.extend_from_slice(params);
        uleb(&mut tsec, count_valtypes(results) as u64);
        tsec.extend_from_slice(results);
    }
    debug_assert_eq!(helper_type_base + helper_types.len() as u32, c.user_type_base);
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
    uleb(&mut isec, import_descs.len() as u64);
    for (i, (name, _, _)) in import_descs.iter().enumerate() {
        uleb(&mut isec, 3);
        isec.extend_from_slice(b"env");
        uleb(&mut isec, name.len() as u64);
        isec.extend_from_slice(name.as_bytes());
        isec.push(0x00); // func
        uleb(&mut isec, import_type_idx[i] as u64);
    }
    section(&mut module, 2, &isec);

    // function section: helpers, user functions, wrappers, lambdas
    let n_funcs = N_HELPERS as usize
        + contracted_externs.len()
        + reachable.len()
        + wrapper_bodies.len()
        + lambda_bodies.len();
    let mut fsec = Vec::new();
    uleb(&mut fsec, n_funcs as u64);
    for i in 0..N_HELPERS {
        uleb(&mut fsec, (helper_type_base + i) as u64);
    }
    for t in type_indices
        .iter()
        .chain(wrapper_type_indices.iter())
        .chain(lambda_type_indices.iter())
    {
        uleb(&mut fsec, (c.user_type_base + t) as u64);
    }
    section(&mut module, 3, &fsec);

    // table section (for closures / function values)
    if n_slots > 0 {
        let mut tabsec = Vec::new();
        uleb(&mut tabsec, 1);
        tabsec.push(FUNCREF);
        tabsec.push(0x00); // min only
        uleb(&mut tabsec, n_slots as u64);
        section(&mut module, 4, &tabsec);
    }

    // export section: main + string accessors (+ spawn targets)
    let mut exports: Vec<(String, u32)> = vec![
        ("main".into(), c.func_index["main"]),
        ("str_len".into(), c.help(OFF_STR_LEN)),
        ("str_at".into(), c.help(OFF_STR_AT)),
        ("make_str".into(), c.help(OFF_MAKE_STR)),
        ("str_set".into(), c.help(OFF_STR_SET)),
        ("i64arr_len".into(), c.help(OFF_I64ARR_LEN)),
        ("i64arr_get".into(), c.help(OFF_I64ARR_GET)),
    ];
    for name in &c.spawn_targets {
        if let Some(&idx) = c.func_index.get(name) {
            exports.push((name.clone(), idx));
        }
    }
    let mut esec = Vec::new();
    uleb(&mut esec, exports.len() as u64);
    for (name, idx) in &exports {
        uleb(&mut esec, name.len() as u64);
        esec.extend_from_slice(name.as_bytes());
        esec.push(0x00);
        uleb(&mut esec, *idx as u64);
    }
    section(&mut module, 7, &esec);

    // element section: fill the table, wrappers first then lambdas
    if n_slots > 0 {
        let mut elsec = Vec::new();
        uleb(&mut elsec, 1); // one segment
        uleb(&mut elsec, 0); // active, table 0, funcref, expr offset
        elsec.push(OP_I32_CONST);
        sleb(&mut elsec, 0);
        elsec.push(OP_END);
        uleb(&mut elsec, n_slots as u64);
        // in slot order: wrappers (sorted names), then lambdas (ascending id)
        for (i, _) in fn_value_names.iter().enumerate() {
            uleb(&mut elsec, (wrapper_base + i as u32) as u64);
        }
        for (i, _) in lambda_ids.iter().enumerate() {
            uleb(&mut elsec, (lambda_base + i as u32) as u64);
        }
        section(&mut module, 9, &elsec);
    }

    // data count section (required when array.new_data is used)
    let mut dcsec = Vec::new();
    uleb(&mut dcsec, c.data_segments.len() as u64);
    section(&mut module, 12, &dcsec);

    // code section
    let mut csec = Vec::new();
    uleb(&mut csec, n_funcs as u64);
    for b in helper_bodies
        .iter()
        .chain(bodies.iter())
        .chain(wrapper_bodies.iter())
        .chain(lambda_bodies.iter())
    {
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

/// Absolute type index of the first user function type for the *base*
/// import layout (print/fail only). Dynamic import counts adjust via
/// `Compiler::user_type_base`.

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
            StmtKind::While { cond, invariant: _, body } => {
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
        ExprKind::Call(name, _type_args, args) => {
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

/// Walks statements collecting every lambda (including nested ones).
fn collect_lambdas_stmts(stmts: &[Stmt], out: &mut BTreeMap<usize, Lambda>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. }
            | StmtKind::Assign { value, .. }
            | StmtKind::Assert(value)
            | StmtKind::Expr(value)
            | StmtKind::Return(Some(value)) => collect_lambdas_expr(value, out),
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
            _ => {}
        }
    }
}

fn collect_lambdas_expr(e: &Expr, out: &mut BTreeMap<usize, Lambda>) {
    match &e.kind {
        ExprKind::Lambda(l) => {
            out.insert(l.id, (**l).clone());
            collect_lambdas_stmts(&l.body, out);
        }
        ExprKind::Array(elems) => elems.iter().for_each(|e| collect_lambdas_expr(e, out)),
        ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
            collect_lambdas_expr(a, out);
            collect_lambdas_expr(b, out);
        }
        ExprKind::Field(a, _) | ExprKind::Un(_, a) => collect_lambdas_expr(a, out),
        ExprKind::Call(_, _, args) => args.iter().for_each(|a| collect_lambdas_expr(a, out)),
        ExprKind::Match(m) => {
            collect_lambdas_expr(&m.scrutinee, out);
            for arm in &m.arms {
                collect_lambdas_expr(&arm.body, out);
            }
        }
        _ => {}
    }
}

/// Collects names that appear in value position (ExprKind::Var); those that
/// resolve to top-level functions become table wrappers.
fn collect_fn_value_names_stmts(stmts: &[Stmt], out: &mut Vec<String>) {
    fn in_expr(e: &Expr, out: &mut Vec<String>) {
        match &e.kind {
            ExprKind::Var(name) => out.push(name.clone()),
            ExprKind::Array(elems) => elems.iter().for_each(|e| in_expr(e, out)),
            ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
                in_expr(a, out);
                in_expr(b, out);
            }
            ExprKind::Field(a, _) | ExprKind::Un(_, a) => in_expr(a, out),
            ExprKind::Call(_, _, args) => args.iter().for_each(|a| in_expr(a, out)),
            ExprKind::Lambda(l) => collect_fn_value_names_stmts(&l.body, out),
            ExprKind::Match(m) => {
                in_expr(&m.scrutinee, out);
                for arm in &m.arms {
                    in_expr(&arm.body, out);
                }
            }
            _ => {}
        }
    }
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. }
            | StmtKind::Assign { value, .. }
            | StmtKind::Assert(value)
            | StmtKind::Expr(value)
            | StmtKind::Return(Some(value)) => in_expr(value, out),
            StmtKind::IndexAssign { base, index, value } => {
                in_expr(base, out);
                in_expr(index, out);
                in_expr(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                in_expr(base, out);
                in_expr(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                in_expr(cond, out);
                collect_fn_value_names_stmts(then_body, out);
                collect_fn_value_names_stmts(else_body, out);
            }
            StmtKind::While { cond, invariant: _, body } => {
                in_expr(cond, out);
                collect_fn_value_names_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                in_expr(start, out);
                in_expr(end, out);
                collect_fn_value_names_stmts(body, out);
            }
            _ => {}
        }
    }
}

struct Compiler {
    func_index: HashMap<String, u32>,
    signatures: HashMap<String, (Vec<Type>, Type)>,
    /// user function types (params bytes, results bytes), deduped by content
    func_types: Vec<(Vec<u8>, Vec<u8>)>,
    data_segments: Vec<Vec<u8>>,
    structs: HashMap<String, Vec<Param>>,
    enums: HashMap<String, Vec<EnumVariant>>,
    lambdas: BTreeMap<usize, Lambda>,
    /// lambda id -> table slot
    lambda_slot: HashMap<usize, u32>,
    /// lambda id -> captured (name, type), recorded at the creation site
    lambda_captures: HashMap<usize, Vec<(String, Type)>>,
    /// named function value -> table slot (wrapper drops the env argument)
    wrapper_slot: HashMap<String, u32>,
    /// total import count (base + concurrency + externs)
    n_imports: u32,
    /// type-section index of first user func type
    user_type_base: u32,
    imp_chan_new: u32,
    imp_chan_close: u32,
    imp_chan_send_i64: u32,
    imp_chan_send_f64: u32,
    imp_chan_send_str: u32,
    imp_chan_recv_i64: u32,
    imp_chan_recv_f64: u32,
    imp_chan_recv_str: u32,
    imp_spawn_pack_str: u32,
    imp_spawn: u32,
    imp_join_i64: u32,
    imp_join_f64: u32,
    imp_join_str: u32,
    /// extern name -> import func index
    extern_index: HashMap<String, u32>,
    spawn_targets: std::collections::BTreeSet<String>,
}

struct Frame {
    kind: FrameKind,
}

enum FrameKind {
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

/// Locals declaration (grouping consecutive identical valtypes) + code.
fn assemble_body(ctx: &Ctx, code: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    let extra: Vec<&Vec<u8>> = ctx.locals[ctx.n_params as usize..].iter().collect();
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
    body.extend_from_slice(code);
    body
}

impl Compiler {
    fn help(&self, off: u32) -> u32 {
        self.n_imports + off
    }

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

    /// Push a GC string literal and call the host `fail` import, then trap.
    fn append_fail(&mut self, code: &mut Vec<u8>, message: &str) {
        let seg = self.intern_string(message);
        code.push(OP_I32_CONST);
        sleb(code, 0);
        code.push(OP_I32_CONST);
        sleb(code, message.len() as i64);
        code.push(GC_PREFIX);
        code.push(GC_ARRAY_NEW_DATA);
        uleb(code, TY_BYTES as u64);
        uleb(code, seg as u64);
        code.push(OP_CALL);
        uleb(code, IMP_FAIL as u64);
        code.push(OP_UNREACHABLE);
    }

    /// Type index (relative to user_type_base) of the closure calling
    /// convention for this signature: params plus a hidden env parameter.
    fn env_type_index(&mut self, params: &[Type], ret: &Type) -> Result<u32, Diagnostic> {
        let span = Span::new(0, 0);
        let mut p = Vec::new();
        for t in params {
            let vt = valtype(t, span)?.ok_or_else(|| e070("unit parameters", span))?;
            p.extend_from_slice(&vt);
        }
        p.extend_from_slice(&[REF_NULL, TY_ANYARR as u8]);
        let r = valtype(ret, span)?.unwrap_or_default();
        Ok(self.func_type_index(p, r))
    }

    /// Compiles a lambda as a function taking its params plus the closure
    /// (env) reference; captures load from env slots 1.. at entry.
    fn compile_lambda(&mut self, l: &Lambda) -> Result<(u32, Vec<u8>), Diagnostic> {
        let params: Vec<Type> = l.params.iter().map(|p| p.ty.clone()).collect();
        let type_idx = self.env_type_index(&params, &l.ret)?;
        let mut ctx = Ctx {
            scopes: vec![HashMap::new()],
            locals: Vec::new(),
            n_params: l.params.len() as u32 + 1,
            frames: Vec::new(),
            ret: l.ret.clone(),
            result_local: None,
        };
        for p in &l.params {
            let enc = valtype(&p.ty, p.span)?.unwrap();
            ctx.add_local(&p.name, p.ty.clone(), enc);
        }
        let env_idx = ctx.add_temp(vec![REF_NULL, TY_ANYARR as u8]);
        let mut code = Vec::new();
        let captures = self.lambda_captures.get(&l.id).cloned().unwrap_or_default();
        for (i, (name, ty)) in captures.iter().enumerate() {
            code.push(OP_LOCAL_GET);
            uleb(&mut code, env_idx as u64);
            code.push(OP_I32_CONST);
            sleb(&mut code, (i + 1) as i64);
            code.push(GC_PREFIX);
            code.push(GC_ARRAY_GET);
            uleb(&mut code, TY_ANYARR as u64);
            unbox_value(&mut code, ty);
            let enc = valtype(ty, Span::new(0, 0))?.unwrap();
            let idx = ctx.add_local(name, ty.clone(), enc);
            code.push(OP_LOCAL_SET);
            uleb(&mut code, idx as u64);
        }
        let falls_through = self.compile_block(&mut code, &l.body, &mut ctx)?;
        if l.ret != Type::Unit && falls_through {
            code.push(OP_UNREACHABLE);
        }
        code.push(OP_END);
        Ok((type_idx, assemble_body(&ctx, &code)))
    }

    /// Wrapper around a host import that enforces requires/ensures.
    fn compile_extern_wrapper(
        &mut self,
        f: &Function,
        host_idx: u32,
    ) -> Result<(u32, Vec<u8>), Diagnostic> {
        let mut param_enc = Vec::new();
        for p in &f.params {
            let Some(vt) = valtype(&p.ty, p.span)? else {
                return Err(e070("extern wrapper parameter type", p.span));
            };
            param_enc.extend_from_slice(&vt);
        }
        let ret_enc = valtype(&f.ret, f.span)?.unwrap_or_default();
        let type_idx = self.func_type_index(param_enc, ret_enc);

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
        for c in &f.requires {
            self.compile_expr(&mut code, &c.expr, &mut ctx, None)?;
            code.push(OP_I32_EQZ);
            code.push(OP_IF);
            code.push(0x40);
            code.push(OP_UNREACHABLE);
            code.push(OP_END);
        }

        for i in 0..f.params.len() as u32 {
            code.push(OP_LOCAL_GET);
            uleb(&mut code, i as u64);
        }
        code.push(OP_CALL);
        uleb(&mut code, host_idx as u64);

        if f.ret != Type::Unit {
            let enc = valtype(&f.ret, f.span)?.unwrap();
            let result_local = ctx.add_temp(enc);
            code.push(OP_LOCAL_SET);
            uleb(&mut code, result_local as u64);
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
        } else {
            for c in &f.ensures {
                self.compile_expr(&mut code, &c.expr, &mut ctx, None)?;
                code.push(OP_I32_EQZ);
                code.push(OP_IF);
                code.push(0x40);
                code.push(OP_UNREACHABLE);
                code.push(OP_END);
            }
        }
        code.push(OP_END);
        Ok((type_idx, assemble_body(&ctx, &code)))
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
        Ok((type_idx, assemble_body(&ctx, &code)))
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
                let arr_ty = array_type_index(&elem);
                self.compile_expr(code, base, ctx, None)?;
                self.compile_expr(code, index, ctx, None)?;
                code.push(OP_I32_WRAP_I64);
                self.compile_expr(code, value, ctx, Some(&elem))?;
                if arr_ty == TY_ANYARR {
                    box_value(code, &elem);
                }
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_SET);
                uleb(code, arr_ty as u64);
                Ok(true)
            }
            StmtKind::FieldAssign { base, field, value } => {
                let Type::Struct(sname) = self.expr_type(base, ctx)? else {
                    return Err(e070("field assignment on this type", stmt.span));
                };
                let fields = self.structs[&sname].clone();
                let idx = fields
                    .iter()
                    .position(|f| f.name == *field)
                    .expect("checked field");
                let fty = fields[idx].ty.clone();
                self.compile_expr(code, base, ctx, None)?;
                code.push(OP_I32_CONST);
                sleb(code, idx as i64);
                self.compile_expr(code, value, ctx, Some(&fty))?;
                box_value(code, &fty);
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_SET);
                uleb(code, TY_ANYARR as u64);
                Ok(true)
            }
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
            StmtKind::While { cond, invariant: _, body } => {
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

    /// The parser writes every named type as Type::Struct; rewrite to
    /// Type::Enum when the name is an enum (matches the checker's norm()).
    fn norm(&self, t: Type) -> Type {
        match t {
            Type::Struct(n) if self.enums.contains_key(&n) => Type::Enum(n),
            Type::Array(inner) => Type::Array(Box::new(self.norm(*inner))),
            Type::Fn(params, ret) => Type::Fn(
                params.into_iter().map(|p| self.norm(p)).collect(),
                Box::new(self.norm(*ret)),
            ),
            other => other,
        }
    }

    /// Static type of an expression (the program is already fully checked
    /// and monomorphized, so this cannot fail on well-typed input).
    fn expr_type(&self, expr: &Expr, ctx: &mut Ctx) -> Result<Type, Diagnostic> {
        let t = self.expr_type_raw(expr, ctx)?;
        Ok(self.norm(t))
    }

    fn expr_type_raw(&self, expr: &Expr, ctx: &mut Ctx) -> Result<Type, Diagnostic> {
        Ok(match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(_) => Type::Str,
            ExprKind::Var(name) => {
                if let Some((_, t)) = ctx.lookup(name) {
                    t
                } else if let Some(colon) = name.rfind("::") {
                    let enum_name = &name[..colon];
                    if self.enums.contains_key(enum_name) {
                        Type::Enum(enum_name.to_string())
                    } else {
                        return Err(e070(&format!("the name '{}'", name), expr.span));
                    }
                } else if let Some((params, ret)) = self.signatures.get(name) {
                    Type::Fn(params.clone(), Box::new(ret.clone()))
                } else {
                    return Err(e070(&format!("the name '{}'", name), expr.span));
                }
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
            ExprKind::Field(base, field) => {
                let Type::Struct(sname) = self.expr_type(base, ctx)? else {
                    return Err(e070("field access on this type", expr.span));
                };
                self.structs[&sname]
                    .iter()
                    .find(|f| f.name == *field)
                    .expect("checked field")
                    .ty
                    .clone()
            }
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
            ExprKind::Call(name, _type_args, args) => match name.as_str() {
                "print" | "chan_close"
                | "chan_send_int" | "chan_send_float" | "chan_send_bool" | "chan_send_str" => {
                    Type::Unit
                }
                "len" | "char_at" | "to_int" | "len_cp" | "char_at_cp" | "hash"
                | "chan_new" | "chan_recv_int" | "chan_recv_bool" | "spawn"
                | "join_int" | "join_bool" => Type::Int,
                "to_float" | "chan_recv_float" | "join_float" => Type::Float,
                "substr" | "chr" | "substr_cp" | "chr_cp" | "chan_recv_str" | "join_str" => {
                    Type::Str
                }
                "push" => self.expr_type(&args[0], ctx)?,
                _ => {
                    if let Some((_, Type::Fn(_, ret))) = ctx.lookup(name) {
                        return Ok(*ret);
                    }
                    if let Some(colon) = name.rfind("::") {
                        let enum_name = &name[..colon];
                        if self.enums.contains_key(enum_name) {
                            return Ok(Type::Enum(enum_name.to_string()));
                        }
                    }
                    if self.structs.contains_key(name) {
                        return Ok(Type::Struct(name.clone()));
                    }
                    let (_, ret) = self
                        .signatures
                        .get(name)
                        .ok_or_else(|| e070(&format!("the call to '{}'", name), expr.span))?;
                    ret.clone()
                }
            },
            ExprKind::Lambda(l) => Type::Fn(
                l.params.iter().map(|p| p.ty.clone()).collect(),
                Box::new(l.ret.clone()),
            ),
            ExprKind::Match(m) => {
                let scrut_ty = self.expr_type(&m.scrutinee, ctx)?;
                let arm = m
                    .arms
                    .first()
                    .ok_or_else(|| e070("empty match expressions", expr.span))?;
                let mut frame = HashMap::new();
                self.pattern_type_frame(&arm.pattern, &scrut_ty, &mut frame);
                ctx.scopes.push(frame);
                let t = self.expr_type(&arm.body, ctx);
                ctx.scopes.pop();
                t?
            }
        })
    }

    /// Records pattern binding types (with dummy local indices) so arm
    /// bodies can be typed before their binding code is emitted.
    fn pattern_type_frame(
        &self,
        pattern: &Pattern,
        scrut_ty: &Type,
        frame: &mut HashMap<String, (u32, Type)>,
    ) {
        match pattern {
            Pattern::Var(name) => {
                frame.insert(name.clone(), (u32::MAX, scrut_ty.clone()));
            }
            Pattern::VariantPayload(enum_name, variant_name, inners) => {
                if let Some(variants) = self.enums.get(enum_name) {
                    if let Some(v) = variants.iter().find(|v| v.name == *variant_name) {
                        for (inner, payload_ty) in inners.iter().zip(v.payloads.iter()) {
                            self.pattern_type_frame(inner, payload_ty, frame);
                        }
                    }
                }
            }
            _ => {}
        }
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
                if let Some((idx, _)) = ctx.lookup(name) {
                    code.push(OP_LOCAL_GET);
                    uleb(code, idx as u64);
                } else if let Some(colon) = name.rfind("::") {
                    // payload-less enum variant value: [box(tag)]
                    let enum_name = &name[..colon];
                    let variant_name = &name[colon + 2..];
                    let variants = self
                        .enums
                        .get(enum_name)
                        .ok_or_else(|| e070(&format!("the name '{}'", name), expr.span))?;
                    let tag = variants
                        .iter()
                        .position(|v| v.name == variant_name)
                        .expect("checked variant");
                    code.push(OP_I64_CONST);
                    sleb(code, tag as i64);
                    code.push(GC_PREFIX);
                    code.push(GC_STRUCT_NEW);
                    uleb(code, TY_BOXI64 as u64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_FIXED);
                    uleb(code, TY_ANYARR as u64);
                    uleb(code, 1);
                } else if let Some(&slot) = self.wrapper_slot.get(name) {
                    // named function as a value: singleton closure [box(slot)]
                    code.push(OP_I64_CONST);
                    sleb(code, slot as i64);
                    code.push(GC_PREFIX);
                    code.push(GC_STRUCT_NEW);
                    uleb(code, TY_BOXI64 as u64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_FIXED);
                    uleb(code, TY_ANYARR as u64);
                    uleb(code, 1);
                } else {
                    return Err(e070(&format!("the name '{}'", name), expr.span));
                }
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
                let arr_ty = array_type_index(&elem_ty);
                for e in elems {
                    self.compile_expr(code, e, ctx, Some(&elem_ty))?;
                    if arr_ty == TY_ANYARR {
                        box_value(code, &elem_ty);
                    }
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
                        let arr_ty = array_type_index(&elem);
                        self.compile_expr(code, base, ctx, None)?;
                        self.compile_expr(code, index, ctx, None)?;
                        code.push(OP_I32_WRAP_I64);
                        code.push(GC_PREFIX);
                        code.push(GC_ARRAY_GET);
                        uleb(code, arr_ty as u64);
                        if arr_ty == TY_ANYARR {
                            unbox_value(code, &elem);
                        }
                    }
                    _ => return Err(e070("indexing this type", expr.span)),
                }
            }
            ExprKind::Field(base, field) => {
                let Type::Struct(sname) = self.expr_type(base, ctx)? else {
                    return Err(e070("field access on this type", expr.span));
                };
                let fields = self.structs[&sname].clone();
                let idx = fields
                    .iter()
                    .position(|f| f.name == *field)
                    .expect("checked field");
                let fty = fields[idx].ty.clone();
                self.compile_expr(code, base, ctx, None)?;
                code.push(OP_I32_CONST);
                sleb(code, idx as i64);
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_GET);
                uleb(code, TY_ANYARR as u64);
                unbox_value(code, &fty);
            }
            ExprKind::Un(op, inner) => match op {
                UnOp::Neg => {
                    let ty = self.expr_type(inner, ctx)?;
                    match ty {
                        Type::Int => {
                            // checked: 0 - i64::MIN overflows and must trap
                            code.push(OP_I64_CONST);
                            sleb(code, 0);
                            self.compile_expr(code, inner, ctx, None)?;
                            code.push(OP_CALL);
                            uleb(code, self.help(OFF_ISUB) as u64);
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
                            uleb(code, self.help(OFF_CONCAT) as u64);
                        }
                        Eq => {
                            code.push(OP_CALL);
                            uleb(code, self.help(OFF_STREQ) as u64);
                        }
                        Ne => {
                            code.push(OP_CALL);
                            uleb(code, self.help(OFF_STREQ) as u64);
                            code.push(OP_I32_EQZ);
                        }
                        _ => return Err(e070("this string operator", expr.span)),
                    }
                    return Ok(());
                }
                self.compile_expr(code, lhs, ctx, None)?;
                self.compile_expr(code, rhs, ctx, None)?;
                // checked int arithmetic: traps on overflow, matching the
                // linear backend and the interpreter
                if lt == Type::Int {
                    if let Some(helper) = match op {
                        Add => Some(OFF_IADD),
                        Sub => Some(OFF_ISUB),
                        Mul => Some(OFF_IMUL),
                        Div => Some(OFF_IDIV),
                        Mod => Some(OFF_IREM),
                        _ => None,
                    } {
                        code.push(OP_CALL);
                        uleb(code, self.help(helper) as u64);
                        return Ok(());
                    }
                }
                let opcode = match (&lt, op) {
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
            ExprKind::Call(name, _type_args, args) => match name.as_str() {
                // a variable holding a function value: indirect call
                _ if matches!(ctx.lookup(name), Some((_, Type::Fn(_, _)))) => {
                    let Some((idx, Type::Fn(params, ret))) = ctx.lookup(name) else {
                        unreachable!()
                    };
                    for (a, p) in args.iter().zip(params.iter()) {
                        self.compile_expr(code, a, ctx, Some(p))?;
                    }
                    // hidden env argument (the closure itself)
                    code.push(OP_LOCAL_GET);
                    uleb(code, idx as u64);
                    // table slot from closure[0]
                    code.push(OP_LOCAL_GET);
                    uleb(code, idx as u64);
                    code.push(OP_I32_CONST);
                    sleb(code, 0);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_GET);
                    uleb(code, TY_ANYARR as u64);
                    unbox_value(code, &Type::Int);
                    code.push(OP_I32_WRAP_I64);
                    let tidx = self.env_type_index(&params, &ret)?;
                    code.push(OP_CALL_INDIRECT);
                    uleb(code, (self.user_type_base + tidx) as u64);
                    uleb(code, 0); // table 0
                }
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
                    let Type::Array(elem) = &aty else {
                        return Err(e070(&format!("push on '{}'", aty), expr.span));
                    };
                    let (helper, boxed) = match elem.as_ref() {
                        Type::Int => (OFF_PUSH_I64, false),
                        Type::Float => (OFF_PUSH_F64, false),
                        _ => (OFF_PUSH_ANY, true),
                    };
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, Some(elem))?;
                    if boxed {
                        box_value(code, elem);
                    }
                    code.push(OP_CALL);
                    uleb(code, self.help(helper) as u64);
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
                    uleb(code, self.help(OFF_SUBSTR) as u64);
                }
                "chr" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_I32_WRAP_I64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_FIXED);
                    uleb(code, TY_BYTES as u64);
                    uleb(code, 1);
                }
                "len_cp" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, self.help(OFF_LEN_CP) as u64);
                }
                "char_at_cp" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, self.help(OFF_CHAR_AT_CP) as u64);
                }
                "substr_cp" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    self.compile_expr(code, &args[1], ctx, None)?;
                    self.compile_expr(code, &args[2], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, self.help(OFF_SUBSTR_CP) as u64);
                }
                "chr_cp" => {
                    self.compile_expr(code, &args[0], ctx, None)?;
                    code.push(OP_CALL);
                    uleb(code, self.help(OFF_CHR_CP) as u64);
                }
                "hash" => {
                    let ty = self.expr_type(&args[0], ctx)?;
                    match ty {
                        Type::Int => {
                            self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                            append_i64_const(code, HASH_MUL);
                            code.push(OP_I64_MUL);
                            append_i64_const(code, HASH_MOD);
                            code.push(OP_I64_REM_U);
                        }
                        Type::Bool => {
                            self.compile_expr(code, &args[0], ctx, Some(&Type::Bool))?;
                        }
                        Type::Str => {
                            self.compile_expr(code, &args[0], ctx, Some(&Type::Str))?;
                            code.push(OP_CALL);
                            uleb(code, self.help(OFF_HASH_STR) as u64);
                        }
                        _ => unreachable!("checked"),
                    }
                }
                "chan_new" => {
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_new as u64);
                }
                "chan_close" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_close as u64);
                }
                "chan_send_int" | "chan_send_bool" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    self.compile_expr(code, &args[1], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_send_i64 as u64);
                }
                "chan_send_float" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    self.compile_expr(code, &args[1], ctx, Some(&Type::Float))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_send_f64 as u64);
                }
                "chan_send_str" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    self.compile_expr(code, &args[1], ctx, Some(&Type::Str))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_send_str as u64);
                }
                "chan_recv_int" | "chan_recv_bool" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_recv_i64 as u64);
                }
                "chan_recv_float" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_recv_f64 as u64);
                }
                "chan_recv_str" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_chan_recv_str as u64);
                }
                "spawn" => {
                    let ExprKind::Var(target) = &args[0].kind else {
                        return Err(e070("spawn of a non-named function", expr.span));
                    };
                    let target = target.clone();
                    let (params, ret) = self.signatures.get(&target).cloned().ok_or_else(|| {
                        e070(&format!("unknown spawn target '{}'", target), expr.span)
                    })?;
                    for p in &params {
                        if !matches!(p, Type::Int | Type::Bool | Type::Float | Type::Str) {
                            return Err(e070(
                                "GC spawn arguments other than int/bool/float/str",
                                expr.span,
                            )
                            .with_help(
                                "under --gc, spawn args must be int, bool, float, or str; use the linear backend for struct/array graphs",
                            ));
                        }
                    }
                    self.spawn_targets.insert(target.clone());
                    let mut sig: String = params
                        .iter()
                        .map(|p| match p {
                            Type::Int | Type::Bool => 'i',
                            Type::Float => 'f',
                            Type::Str => 's',
                            _ => 'i',
                        })
                        .collect();
                    sig.push(':');
                    sig.push(match &ret {
                        Type::Int | Type::Bool => 'i',
                        Type::Float => 'f',
                        Type::Str => 's',
                        _ => {
                            return Err(e070("GC spawn return type", expr.span));
                        }
                    });
                    let name_seg = self.intern_string(&target);
                    let sig_seg = self.intern_string(&sig);
                    // name
                    code.push(OP_I32_CONST);
                    sleb(code, 0);
                    code.push(OP_I32_CONST);
                    sleb(code, target.len() as i64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_DATA);
                    uleb(code, TY_BYTES as u64);
                    uleb(code, name_seg as u64);
                    // sig
                    code.push(OP_I32_CONST);
                    sleb(code, 0);
                    code.push(OP_I32_CONST);
                    sleb(code, sig.len() as i64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_DATA);
                    uleb(code, TY_BYTES as u64);
                    uleb(code, sig_seg as u64);
                    // argv i64 words: int/bool as-is, float bitcast, str via spawn_pack_str
                    for (a, p) in args[1..].iter().zip(params.iter()) {
                        self.compile_expr(code, a, ctx, Some(p))?;
                        match p {
                            Type::Float => code.push(OP_I64_REINTERPRET_F64),
                            Type::Str => {
                                code.push(OP_CALL);
                                uleb(code, self.imp_spawn_pack_str as u64);
                            }
                            _ => {}
                        }
                    }
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_NEW_FIXED);
                    uleb(code, TY_ARR_I64 as u64);
                    uleb(code, params.len() as u64);
                    code.push(OP_CALL);
                    uleb(code, self.imp_spawn as u64);
                }
                "join_int" | "join_bool" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_join_i64 as u64);
                }
                "join_float" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_join_f64 as u64);
                }
                "join_str" => {
                    self.compile_expr(code, &args[0], ctx, Some(&Type::Int))?;
                    code.push(OP_CALL);
                    uleb(code, self.imp_join_str as u64);
                }
                _ => {
                    // enum variant constructor with payload(s): [box(tag), ...payloads]
                    if let Some(colon) = name.rfind("::") {
                        let enum_name = &name[..colon];
                        let variant_name = &name[colon + 2..];
                        if let Some(variants) = self.enums.get(enum_name).cloned() {
                            let tag = variants
                                .iter()
                                .position(|v| v.name == variant_name)
                                .expect("checked variant");
                            let payload_tys = variants[tag].payloads.clone();
                            code.push(OP_I64_CONST);
                            sleb(code, tag as i64);
                            code.push(GC_PREFIX);
                            code.push(GC_STRUCT_NEW);
                            uleb(code, TY_BOXI64 as u64);
                            for (arg, payload_ty) in args.iter().zip(payload_tys.iter()) {
                                self.compile_expr(code, arg, ctx, Some(payload_ty))?;
                                box_value(code, payload_ty);
                            }
                            code.push(GC_PREFIX);
                            code.push(GC_ARRAY_NEW_FIXED);
                            uleb(code, TY_ANYARR as u64);
                            uleb(code, 1 + payload_tys.len() as u64);
                            return Ok(());
                        }
                    }
                    // struct constructor: fields boxed into an $anyarr
                    if let Some(fields) = self.structs.get(name).cloned() {
                        for (fld, a) in fields.iter().zip(args.iter()) {
                            self.compile_expr(code, a, ctx, Some(&fld.ty))?;
                            box_value(code, &fld.ty);
                        }
                        code.push(GC_PREFIX);
                        code.push(GC_ARRAY_NEW_FIXED);
                        uleb(code, TY_ANYARR as u64);
                        uleb(code, fields.len() as u64);
                        return Ok(());
                    }
                    let idx = self
                        .func_index
                        .get(name)
                        .copied()
                        .or_else(|| self.extern_index.get(name).copied())
                        .ok_or_else(|| e070(&format!("the call to '{}'", name), expr.span))?;
                    let (params, _) = self.signatures.get(name).cloned().unwrap();
                    for (a, p) in args.iter().zip(params.iter()) {
                        self.compile_expr(code, a, ctx, Some(p))?;
                    }
                    code.push(OP_CALL);
                    uleb(code, idx as u64);
                }
            },
            ExprKind::Lambda(l) => {
                // build the closure: [box(table slot), boxed captures...]
                let mut captures: Vec<(String, Type)> = Vec::new();
                for n in l.free_names() {
                    if let Some((_, ty)) = ctx.lookup(&n) {
                        captures.push((n, ty));
                    }
                }
                self.lambda_captures.insert(l.id, captures.clone());
                let slot = self.lambda_slot[&l.id];
                code.push(OP_I64_CONST);
                sleb(code, slot as i64);
                code.push(GC_PREFIX);
                code.push(GC_STRUCT_NEW);
                uleb(code, TY_BOXI64 as u64);
                for (name, ty) in &captures {
                    let (idx, _) = ctx.lookup(name).expect("capture in scope");
                    code.push(OP_LOCAL_GET);
                    uleb(code, idx as u64);
                    box_value(code, ty);
                }
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_NEW_FIXED);
                uleb(code, TY_ANYARR as u64);
                uleb(code, (1 + captures.len()) as u64);
            }
            ExprKind::Match(m) => {
                // scrutinee into a temp, nested ifs set a result local
                let scrut_ty = self.expr_type(&m.scrutinee, ctx)?;
                let scrut_enc = valtype(&scrut_ty, expr.span)?
                    .ok_or_else(|| e070("matching on unit", expr.span))?;
                self.compile_expr(code, &m.scrutinee, ctx, None)?;
                let scrut_local = ctx.add_temp(scrut_enc);
                code.push(OP_LOCAL_SET);
                uleb(code, scrut_local as u64);

                let result_ty = match expected {
                    Some(t) => t.clone(),
                    None => self.expr_type(expr, ctx)?,
                };
                let result_local = match valtype(&result_ty, expr.span)? {
                    Some(enc) => Some(ctx.add_temp(enc)),
                    None => None,
                };

                let mut opened_ifs = 0;
                for (arm_idx, arm) in m.arms.iter().enumerate() {
                    let is_last = arm_idx == m.arms.len() - 1;
                    let has_check =
                        !matches!(arm.pattern, Pattern::Wildcard | Pattern::Var(_));
                    if has_check && !is_last {
                        self.compile_pattern_check(
                            code,
                            &arm.pattern,
                            scrut_local,
                            &scrut_ty,
                            ctx,
                        )?;
                        code.push(OP_IF);
                        code.push(0x40);
                        opened_ifs += 1;
                    }
                    ctx.scopes.push(HashMap::new());
                    self.bind_pattern_vars(code, &arm.pattern, scrut_local, &scrut_ty, ctx)?;
                    self.compile_expr(code, &arm.body, ctx, Some(&result_ty))?;
                    ctx.scopes.pop();
                    if let Some(r) = result_local {
                        code.push(OP_LOCAL_SET);
                        uleb(code, r as u64);
                    }
                    if has_check && !is_last {
                        code.push(OP_ELSE);
                    }
                }
                for _ in 0..opened_ifs {
                    code.push(OP_END);
                }
                if let Some(r) = result_local {
                    code.push(OP_LOCAL_GET);
                    uleb(code, r as u64);
                }
            }
        }
        Ok(())
    }

    /// Leaves an i32 (0/1) on the stack: does the scrutinee match?
    fn compile_pattern_check(
        &mut self,
        code: &mut Vec<u8>,
        pattern: &Pattern,
        scrut_local: u32,
        scrut_ty: &Type,
        ctx: &mut Ctx,
    ) -> Result<(), Diagnostic> {
        let _ = ctx;
        match pattern {
            Pattern::Wildcard | Pattern::Var(_) => {
                code.push(OP_I32_CONST);
                sleb(code, 1);
            }
            Pattern::Int(v) => {
                code.push(OP_LOCAL_GET);
                uleb(code, scrut_local as u64);
                code.push(OP_I64_CONST);
                sleb(code, *v);
                code.push(OP_I64_EQ);
            }
            Pattern::Bool(v) => {
                code.push(OP_LOCAL_GET);
                uleb(code, scrut_local as u64);
                code.push(OP_I32_CONST);
                sleb(code, if *v { 1 } else { 0 });
                code.push(OP_I32_EQ);
            }
            Pattern::Str(s) => {
                code.push(OP_LOCAL_GET);
                uleb(code, scrut_local as u64);
                let seg = self.intern_string(s);
                code.push(OP_I32_CONST);
                sleb(code, 0);
                code.push(OP_I32_CONST);
                sleb(code, s.len() as i64);
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_NEW_DATA);
                uleb(code, TY_BYTES as u64);
                uleb(code, seg as u64);
                code.push(OP_CALL);
                uleb(code, self.help(OFF_STREQ) as u64);
            }
            Pattern::Variant(_, variant_name)
            | Pattern::VariantPayload(_, variant_name, _) => {
                let Type::Enum(enum_name) = scrut_ty else {
                    unreachable!("checked: variant pattern on enum scrutinee");
                };
                let tag = self.enums[enum_name]
                    .iter()
                    .position(|v| v.name == *variant_name)
                    .expect("checked variant");
                code.push(OP_LOCAL_GET);
                uleb(code, scrut_local as u64);
                code.push(OP_I32_CONST);
                sleb(code, 0);
                code.push(GC_PREFIX);
                code.push(GC_ARRAY_GET);
                uleb(code, TY_ANYARR as u64);
                unbox_value(code, &Type::Int);
                code.push(OP_I64_CONST);
                sleb(code, tag as i64);
                code.push(OP_I64_EQ);
            }
        }
        Ok(())
    }

    /// Binds pattern variables as fresh locals in the current scope frame.
    fn bind_pattern_vars(
        &mut self,
        code: &mut Vec<u8>,
        pattern: &Pattern,
        scrut_local: u32,
        scrut_ty: &Type,
        ctx: &mut Ctx,
    ) -> Result<(), Diagnostic> {
        match pattern {
            Pattern::Var(name) => {
                let enc = valtype(scrut_ty, Span::new(0, 0))?.unwrap();
                let idx = ctx.add_local(name, scrut_ty.clone(), enc);
                code.push(OP_LOCAL_GET);
                uleb(code, scrut_local as u64);
                code.push(OP_LOCAL_SET);
                uleb(code, idx as u64);
            }
            Pattern::VariantPayload(enum_name, variant_name, inners) => {
                let variants = self.enums[enum_name].clone();
                let variant = variants
                    .iter()
                    .find(|v| v.name == *variant_name)
                    .expect("checked variant");
                for (i, (inner, payload_ty)) in inners.iter().zip(variant.payloads.iter()).enumerate()
                {
                    let enc = valtype(payload_ty, Span::new(0, 0))?.unwrap();
                    let payload_local = ctx.add_temp(enc);
                    code.push(OP_LOCAL_GET);
                    uleb(code, scrut_local as u64);
                    code.push(OP_I32_CONST);
                    sleb(code, (i + 1) as i64);
                    code.push(GC_PREFIX);
                    code.push(GC_ARRAY_GET);
                    uleb(code, TY_ANYARR as u64);
                    unbox_value(code, payload_ty);
                    code.push(OP_LOCAL_SET);
                    uleb(code, payload_local as u64);
                    self.bind_pattern_vars(code, inner, payload_local, payload_ty, ctx)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Hand-assembled helper function bodies (concat, streq, substr, push).
    fn helper_bodies(&mut self) -> Vec<Vec<u8>> {
        vec![
            self.body_concat(),
            self.body_streq(),
            self.body_substr(),
            self.body_push(TY_ARR_I64, I64),
            self.body_push(TY_ARR_F64, F64),
            self.body_push(TY_ANYARR, ANYREF),
            self.body_str_len(),
            self.body_str_at(),
            self.body_iadd(),
            self.body_isub(),
            self.body_imul(),
            self.body_irem(),
            self.body_idiv(),
            self.body_len_cp(),
            self.body_char_at_cp(),
            self.body_substr_cp(),
            self.body_chr_cp(),
            self.body_hash_str(),
            self.body_make_str(),
            self.body_str_set(),
            self.body_i64arr_len(),
            self.body_i64arr_get(),
        ]
    }

    /// make_str(len: i64) -> bytes
    fn body_make_str(&self) -> Vec<u8> {
        let mut b = vec![0];
        b.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_I32_WRAP_I64, GC_PREFIX, GC_ARRAY_NEW_DEFAULT, TY_BYTES as u8,
            OP_RETURN, OP_END,
        ]);
        b
    }

    /// str_set(s: bytes, i: i32, b: i32)
    fn body_str_set(&self) -> Vec<u8> {
        let mut b = vec![0];
        b.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_LOCAL_GET, 2, GC_PREFIX, GC_ARRAY_SET,
            TY_BYTES as u8, OP_RETURN, OP_END,
        ]);
        b
    }

    fn body_i64arr_len(&self) -> Vec<u8> {
        let mut b = vec![0];
        b.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_RETURN, OP_END]);
        b
    }

    fn body_i64arr_get(&self) -> Vec<u8> {
        let mut b = vec![0];
        b.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, GC_PREFIX, GC_ARRAY_GET, TY_ARR_I64 as u8,
            OP_RETURN, OP_END,
        ]);
        b
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

    /// iadd(a, b) -> a + b, trapping on overflow: ((a^c) & (b^c)) < 0.
    /// locals: 2 = c (i64)
    fn body_iadd(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[1, 1, I64]);
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_ADD, OP_LOCAL_TEE, 2]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_I64_XOR]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_LOCAL_GET, 1, OP_I64_XOR, OP_I64_AND]);
        c.extend_from_slice(&[OP_I64_CONST, 0, OP_I64_LT_S]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '+'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn body_isub(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[1, 1, I64]);
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_SUB, OP_LOCAL_SET, 2]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_XOR]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 2, OP_I64_XOR, OP_I64_AND]);
        c.extend_from_slice(&[OP_I64_CONST, 0, OP_I64_LT_S]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '-'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn body_imul(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[1, 1, I64]);
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0, 0x50 /* i64.eqz */]);
        c.extend_from_slice(&[OP_IF, 0x40, OP_I64_CONST, 0, OP_RETURN, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_I64_CONST, 0x7f /* -1 */, OP_I64_EQ]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 1]);
        c.push(OP_I64_CONST);
        sleb(&mut c, i64::MIN);
        c.extend_from_slice(&[OP_I64_EQ, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '*'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_I64_CONST, 0, OP_LOCAL_GET, 1, OP_I64_SUB, OP_RETURN, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_MUL, OP_LOCAL_TEE, 2]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_I64_DIV_S, OP_LOCAL_GET, 1, OP_I64_NE]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '*'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn body_irem(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0]);
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 1, 0x50 /* i64.eqz */]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: modulo by zero");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        c.push(OP_I64_CONST);
        sleb(&mut c, i64::MIN);
        c.extend_from_slice(&[OP_I64_EQ, OP_LOCAL_GET, 1, OP_I64_CONST, 0x7f /* -1 */, OP_I64_EQ]);
        c.extend_from_slice(&[0x71 /* i32.and */, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '%'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_REM_S, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn body_idiv(&mut self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0]);
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 1, 0x50 /* i64.eqz */]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: division by zero");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        c.push(OP_I64_CONST);
        sleb(&mut c, i64::MIN);
        c.extend_from_slice(&[OP_I64_EQ, OP_LOCAL_GET, 1, OP_I64_CONST, 0x7f /* -1 */, OP_I64_EQ]);
        c.extend_from_slice(&[0x71 /* i32.and */, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: integer overflow in '/'");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_LOCAL_GET, 1, OP_I64_DIV_S, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    /// len_cp(s) -> i64: count UTF-8 leading bytes.
    fn body_len_cp(&mut self) -> Vec<u8> {
        // param 0 = s; locals 1=la, 2=i, 3=count, 4=b
        let mut b = vec![1, 4, I32];
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_SET, 1]);
        c.extend_from_slice(&[OP_I32_CONST, 0, OP_LOCAL_SET, 2, OP_I32_CONST, 0, OP_LOCAL_SET, 3]);
        c.extend_from_slice(&[OP_BLOCK, 0x40, OP_LOOP, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_LOCAL_GET, 1, 0x4e, OP_BR_IF, 1]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, 0, OP_LOCAL_GET, 2, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_SET, 4,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, 4, OP_I32_CONST, 0x80, 0x71, OP_I32_CONST, 0x80, OP_I32_NE]);
        c.extend_from_slice(&[OP_IF, 0x40, OP_LOCAL_GET, 3, OP_I32_CONST, 1, OP_I32_ADD, OP_LOCAL_SET, 3, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I32_CONST, 1, OP_I32_ADD, OP_LOCAL_SET, 2, OP_BR, 0]);
        c.extend_from_slice(&[OP_END, OP_END, OP_LOCAL_GET, 3, OP_I64_EXTEND_I32_S, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn append_cp_byte_offset_gc(
        &mut self,
        c: &mut Vec<u8>,
        str: u8,
        target: u8,
        off: u8,
        found: u8,
        la: u8,
        i: u8,
        cp: u8,
        byte: u8,
    ) {
        c.extend_from_slice(&[OP_I32_CONST, 0, OP_LOCAL_SET, found]);
        c.extend_from_slice(&[OP_LOCAL_GET, str, GC_PREFIX, GC_ARRAY_LEN, OP_LOCAL_SET, la]);
        c.extend_from_slice(&[OP_I32_CONST, 0, OP_LOCAL_SET, i, OP_I32_CONST, 0, OP_LOCAL_SET, cp]);
        c.extend_from_slice(&[OP_BLOCK, 0x40, OP_LOOP, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, i, OP_LOCAL_GET, la, 0x4e, OP_BR_IF, 1]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, str, OP_LOCAL_GET, i, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_SET, byte,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, byte, OP_I32_CONST, 0x80, 0x71, OP_I32_CONST, 0x80, OP_I32_NE]);
        c.extend_from_slice(&[OP_IF, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, cp, OP_I64_EXTEND_I32_S, OP_LOCAL_GET, target, OP_I64_EQ]);
        c.extend_from_slice(&[
            OP_IF, 0x40, OP_LOCAL_GET, i, OP_LOCAL_SET, off, OP_I32_CONST, 1, OP_LOCAL_SET, found, OP_BR, 1, OP_END,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, cp, OP_I32_CONST, 1, OP_I32_ADD, OP_LOCAL_SET, cp, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, i, OP_I32_CONST, 1, OP_I32_ADD, OP_LOCAL_SET, i, OP_BR, 0]);
        c.extend_from_slice(&[OP_END, OP_END]);
    }

    fn append_utf8_decode_at_gc(
        &self,
        c: &mut Vec<u8>,
        str: u8,
        off: u8,
        b0: u8,
        b1: u8,
        b2: u8,
        b3: u8,
    ) {
        c.extend_from_slice(&[
            OP_LOCAL_GET, str, OP_LOCAL_GET, off, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_TEE, b0,
        ]);
        c.extend_from_slice(&[OP_I32_CONST, 0x80, 0x49, OP_IF, 0x40, OP_LOCAL_GET, b0, OP_I64_EXTEND_I32_S, OP_RETURN, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, b0, OP_I32_CONST, 0xE0, 0x49, OP_IF, 0x40]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, str, OP_LOCAL_GET, off, OP_I32_CONST, 1, OP_I32_ADD, GC_PREFIX, GC_ARRAY_GET_U,
            TY_BYTES as u8, OP_LOCAL_SET, b1,
        ]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, b0, OP_I32_CONST, 0x1F, 0x71, OP_I32_CONST, 6, 0x74, OP_LOCAL_GET, b1, OP_I32_CONST, 0x3F,
            0x71, OP_I32_OR, OP_I64_EXTEND_I32_S, OP_RETURN, OP_END,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, b0, OP_I32_CONST, 0xF0, 0x49, OP_IF, 0x40]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, str, OP_LOCAL_GET, off, OP_I32_CONST, 1, OP_I32_ADD, GC_PREFIX, GC_ARRAY_GET_U,
            TY_BYTES as u8, OP_LOCAL_SET, b1, OP_LOCAL_GET, str, OP_LOCAL_GET, off, OP_I32_CONST, 2, OP_I32_ADD,
            GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_SET, b2,
        ]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, b0, OP_I32_CONST, 0x0F, 0x71, OP_I32_CONST, 12, 0x74, OP_LOCAL_GET, b1, OP_I32_CONST, 0x3F,
            0x71, OP_I32_CONST, 6, 0x74, OP_I32_OR, OP_LOCAL_GET, b2, OP_I32_CONST, 0x3F, 0x71, OP_I32_OR,
            OP_I64_EXTEND_I32_S, OP_RETURN, OP_END,
        ]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, str, OP_LOCAL_GET, off, OP_I32_CONST, 1, OP_I32_ADD, GC_PREFIX, GC_ARRAY_GET_U,
            TY_BYTES as u8, OP_LOCAL_SET, b1, OP_LOCAL_GET, str, OP_LOCAL_GET, off, OP_I32_CONST, 2, OP_I32_ADD,
            GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_SET, b2, OP_LOCAL_GET, str, OP_LOCAL_GET, off,
            OP_I32_CONST, 3, OP_I32_ADD, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8, OP_LOCAL_SET, b3,
        ]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, b0, OP_I32_CONST, 7, 0x71, OP_I32_CONST, 18, 0x74, OP_LOCAL_GET, b1, OP_I32_CONST, 0x3F, 0x71,
            OP_I32_CONST, 12, 0x74, OP_I32_OR, OP_LOCAL_GET, b2, OP_I32_CONST, 0x3F, 0x71, OP_I32_CONST, 6, 0x74,
            OP_I32_OR, OP_LOCAL_GET, b3, OP_I32_CONST, 0x3F, 0x71, OP_I32_OR, OP_I64_EXTEND_I32_S, OP_RETURN,
        ]);
    }

    fn body_char_at_cp(&mut self) -> Vec<u8> {
        // params 0=s(ref),1=idx(i64); locals 2=total(i64),3=off,4=found,5=la,6=i,7=cp,8=byte,9..12=decode
        let mut b = vec![2, 1, I64, 12, I32];
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 1, OP_I64_CONST, 0, OP_I64_LT_S, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_CALL]);
        uleb(&mut c, self.help(OFF_LEN_CP) as u64);
        c.extend_from_slice(&[OP_LOCAL_TEE, 2, OP_LOCAL_GET, 1, OP_I64_GE_S, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        self.append_cp_byte_offset_gc(&mut c, 0, 1, 3, 4, 5, 6, 7, 8);
        c.extend_from_slice(&[OP_LOCAL_GET, 4, OP_I32_EQZ, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        self.append_utf8_decode_at_gc(&mut c, 0, 3, 9, 10, 11, 12);
        c.push(OP_END);
        b.extend_from_slice(&c);
        b
    }

    fn body_substr_cp(&mut self) -> Vec<u8> {
        // params 0=s,1=start,2=end; locals 3=total(i64),4=byte_start,5=byte_end,6=found,7=la,8=i,9=cp,10=byte,11=n,12=out
        let mut b = vec![3, 1, I64, 8, I32, 1, REF_NULL, TY_BYTES as u8];
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 1, OP_I64_CONST, 0, OP_I64_LT_S]);
        c.extend_from_slice(&[OP_LOCAL_GET, 1, OP_LOCAL_GET, 2, OP_I64_GT_S, OP_I32_OR, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0, OP_CALL]);
        uleb(&mut c, self.help(OFF_LEN_CP) as u64);
        c.extend_from_slice(&[OP_LOCAL_SET, 3, OP_LOCAL_GET, 2, OP_LOCAL_GET, 3, OP_I64_GT_S, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        self.append_cp_byte_offset_gc(&mut c, 0, 1, 4, 6, 7, 8, 9, 10);
        c.extend_from_slice(&[OP_LOCAL_GET, 6, OP_I32_EQZ, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        self.append_cp_byte_offset_gc(&mut c, 0, 2, 5, 6, 7, 8, 9, 10);
        c.extend_from_slice(&[OP_LOCAL_GET, 6, OP_I32_EQZ, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: codepoint index out of bounds");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 5, OP_LOCAL_GET, 4, OP_I32_SUB, OP_LOCAL_SET, 11]);
        c.extend_from_slice(&[OP_LOCAL_GET, 11, GC_PREFIX, GC_ARRAY_NEW_DEFAULT, TY_BYTES as u8, OP_LOCAL_SET, 12]);
        c.extend_from_slice(&[
            OP_LOCAL_GET, 12, OP_I32_CONST, 0, OP_LOCAL_GET, 0, OP_LOCAL_GET, 4, OP_LOCAL_GET, 11,
            GC_PREFIX, GC_ARRAY_COPY, TY_BYTES as u8, TY_BYTES as u8,
        ]);
        c.extend_from_slice(&[OP_LOCAL_GET, 12, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    fn append_chr_cp_encode_gc(&self, c: &mut Vec<u8>) {
        c.extend_from_slice(&[OP_BLOCK, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x80);
        c.extend_from_slice(&[OP_I64_LT_U, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_I32_CONST, 1, OP_LOCAL_SET, 2, OP_LOCAL_GET, 0, OP_I32_WRAP_I64, OP_LOCAL_SET, 3, OP_BR, 0, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x800);
        c.extend_from_slice(&[OP_I64_LT_U, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_I32_CONST, 2, OP_LOCAL_SET, 2, OP_I32_CONST, 0xC0, OP_LOCAL_GET, 0]);
        append_i64_const(c, 6);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 3, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x3F);
        c.extend_from_slice(&[OP_I64_AND, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 4, OP_BR, 0, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x10000);
        c.extend_from_slice(&[OP_I64_LT_U, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_I32_CONST, 3, OP_LOCAL_SET, 2, OP_I32_CONST, 0xE0, OP_LOCAL_GET, 0]);
        append_i64_const(c, 12);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 3, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 6);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_CONST, 0x3F, OP_I32_AND, OP_I32_OR, OP_LOCAL_SET, 4, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x3F);
        c.extend_from_slice(&[OP_I64_AND, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 5, OP_BR, 0, OP_END]);
        c.extend_from_slice(&[OP_I32_CONST, 4, OP_LOCAL_SET, 2, OP_I32_CONST, 0xF0, OP_LOCAL_GET, 0]);
        append_i64_const(c, 18);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 3, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 12);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_CONST, 0x3F, OP_I32_AND, OP_I32_OR, OP_LOCAL_SET, 4, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 6);
        c.extend_from_slice(&[OP_I64_SHR_U, OP_I32_WRAP_I64, OP_I32_CONST, 0x3F, OP_I32_AND, OP_I32_OR, OP_LOCAL_SET, 5, OP_I32_CONST, 0x80, OP_LOCAL_GET, 0]);
        append_i64_const(c, 0x3F);
        c.extend_from_slice(&[OP_I64_AND, OP_I32_WRAP_I64, OP_I32_OR, OP_LOCAL_SET, 6]);
        c.push(OP_END);
    }

    fn body_chr_cp(&mut self) -> Vec<u8> {
        let mut b = vec![2, 6, I32, 1, REF_NULL, TY_BYTES as u8];
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(&mut c, 0);
        c.extend_from_slice(&[OP_I64_LT_S]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(&mut c, 0x10FFFF);
        c.extend_from_slice(&[OP_I64_GT_S, OP_I32_OR, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: invalid Unicode scalar value");
        c.extend_from_slice(&[OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 0]);
        append_i64_const(&mut c, 0xD800);
        c.extend_from_slice(&[OP_I64_GE_S, OP_LOCAL_GET, 0]);
        append_i64_const(&mut c, 0xDFFF);
        c.extend_from_slice(&[OP_I64_LE_S, OP_I32_AND, OP_IF, 0x40]);
        self.append_fail(&mut c, "runtime error: invalid Unicode scalar value");
        c.extend_from_slice(&[OP_END]);
        self.append_chr_cp_encode_gc(&mut c);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, GC_PREFIX, GC_ARRAY_NEW_DEFAULT, TY_BYTES as u8, OP_LOCAL_SET, 7]);
        c.extend_from_slice(&[OP_LOCAL_GET, 7, OP_I32_CONST, 0, OP_LOCAL_GET, 3, GC_PREFIX, GC_ARRAY_SET, TY_BYTES as u8]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I32_CONST, 2, 0x4e, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 7, OP_I32_CONST, 1, OP_LOCAL_GET, 4, GC_PREFIX, GC_ARRAY_SET, TY_BYTES as u8, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I32_CONST, 3, 0x4e, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 7, OP_I32_CONST, 2, OP_LOCAL_GET, 5, GC_PREFIX, GC_ARRAY_SET, TY_BYTES as u8, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I32_CONST, 4, 0x4e, OP_IF, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 7, OP_I32_CONST, 3, OP_LOCAL_GET, 6, GC_PREFIX, GC_ARRAY_SET, TY_BYTES as u8, OP_END]);
        c.extend_from_slice(&[OP_LOCAL_GET, 7, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }

    /// hash_str(s: bytes) -> i64: polynomial hash over UTF-8 bytes.
    fn body_hash_str(&mut self) -> Vec<u8> {
        // param 0 = s; locals 1=la, 2=i, 3=h (all i64)
        let mut b = vec![1, 3, I64];
        let mut c = Vec::new();
        c.extend_from_slice(&[OP_LOCAL_GET, 0, GC_PREFIX, GC_ARRAY_LEN, OP_I64_EXTEND_I32_S, OP_LOCAL_SET, 1]);
        append_i64_const(&mut c, 0);
        c.extend_from_slice(&[OP_LOCAL_SET, 2]);
        append_i64_const(&mut c, 0);
        c.extend_from_slice(&[OP_LOCAL_SET, 3]);
        c.extend_from_slice(&[OP_BLOCK, 0x40, OP_LOOP, 0x40]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_LOCAL_GET, 1, OP_I64_GE_S, OP_BR_IF, 1]);
        c.extend_from_slice(&[OP_LOCAL_GET, 3]);
        append_i64_const(&mut c, 31);
        c.extend_from_slice(&[OP_I64_MUL, OP_LOCAL_GET, 0, OP_LOCAL_GET, 2]);
        c.extend_from_slice(&[OP_I32_WRAP_I64, GC_PREFIX, GC_ARRAY_GET_U, TY_BYTES as u8]);
        c.extend_from_slice(&[OP_I64_EXTEND_I32_U, OP_I64_ADD]);
        append_i64_const(&mut c, HASH_MOD);
        c.extend_from_slice(&[OP_I64_REM_S, OP_LOCAL_SET, 3]);
        c.extend_from_slice(&[OP_LOCAL_GET, 2, OP_I64_CONST, 1, OP_I64_ADD, OP_LOCAL_SET, 2, OP_BR, 0]);
        c.extend_from_slice(&[OP_END, OP_END, OP_LOCAL_GET, 3, OP_RETURN, OP_END]);
        b.extend_from_slice(&c);
        b
    }
}
