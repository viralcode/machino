//! Native backend: emit C, then compile with Clang/LLVM into a real host
//! executable (machino ISA → LLVM → native object code).
//!
//! This replaces the old `--native` path that only ran `wasmtime compile` on
//! linear WASM. The emitted C links against `runtime/native/machino_rt.c`.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Result of a successful native build.
pub struct NativeBuild {
    pub exe_path: PathBuf,
    pub c_path: PathBuf,
    pub ll_path: Option<PathBuf>,
}

fn c_ident(name: &str) -> String {
    let mut out = String::from("mno_");
    for c in name.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => out.push(c),
            _ => out.push('_'),
        }
    }
    out
}

fn ret_kind(ty: &Type) -> char {
    match ty {
        Type::Int | Type::Bool => 'i',
        Type::Float => 'f',
        Type::Str => 's',
        _ => 'i',
    }
}

/// Kind tag for `mno_value_clone` / spawn argv deep-copy.
fn value_kind(ty: &Type) -> char {
    match ty {
        Type::Int | Type::Bool => 'i',
        Type::Float => 'f',
        Type::Str => 's',
        Type::Array(_)
        | Type::Struct(_)
        | Type::Enum(_)
        | Type::App(_, _)
        | Type::Fn(_, _) => 'p',
        Type::Unit | Type::TypeVar(_) => 'i',
    }
}

fn is_heap_ty(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Str
            | Type::Array(_)
            | Type::Struct(_)
            | Type::Enum(_)
            | Type::App(_, _)
            | Type::Fn(_, _)
    )
}

fn closure_fn_ptr_type(n_params: usize) -> String {
    let mut s = String::from("mno_i64 (*)(");
    for i in 0..=n_params {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str("mno_i64");
    }
    s.push(')');
    s
}

fn i64_from_typed(name: &str, ty: &Type) -> String {
    if *ty == Type::Float {
        format!("mno_f64_to_bits({})", name)
    } else {
        name.to_string()
    }
}

fn typed_from_i64(name: &str, ty: &Type) -> String {
    if *ty == Type::Float {
        format!("mno_bits_to_f64({})", name)
    } else {
        name.to_string()
    }
}

fn escape_c_string(s: &str) -> String {
    let mut out = String::from("\"");
    for b in s.as_bytes() {
        match *b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(*b as char),
            _ => {
                let _ = write!(out, "\\x{:02x}", b);
            }
        }
    }
    out.push('"');
    out
}

struct Emitter<'a> {
    program: &'a Program,
    out: String,
    structs: HashMap<&'a str, Vec<&'a Param>>,
    enums: HashMap<&'a str, Vec<&'a EnumVariant>>,
    signatures: HashMap<&'a str, (Vec<Type>, Type)>,
    tmp: usize,
    locals: Vec<HashMap<String, Type>>,
    loop_labels: Vec<(String, String)>,
    label_n: usize,
    /// When set, `Var(name)` reads from these C identifiers (ensures shadows).
    ensures_alias: HashMap<String, String>,
    /// Forward declarations for lambdas, wrappers, and spawn entries.
    aux_fwd: String,
    /// Lambda/wrapper/spawn C emitted before user functions.
    aux_code: String,
    lambdas: BTreeMap<usize, Lambda>,
    lambda_captures: HashMap<usize, Vec<(String, Type)>>,
    fn_value_names: Vec<String>,
    spawn_targets: HashSet<String>,
    wrapped_fns: HashSet<String>,
    spawn_entries: HashSet<String>,
    /// When emitting a lambda body, convert returns to i64 ABI.
    in_lambda: bool,
    /// Temps already registered as GC roots in the current frame.
    rooted: HashSet<String>,
}

impl<'a> Emitter<'a> {
    fn new(program: &'a Program) -> Self {
        let mut structs = HashMap::new();
        for s in &program.structs {
            if s.type_params.is_empty() {
                structs.insert(s.name.as_str(), s.fields.iter().collect());
            }
        }
        let mut enums = HashMap::new();
        for e in &program.enums {
            if e.type_params.is_empty() {
                enums.insert(e.name.as_str(), e.variants.iter().collect());
            }
        }
        let mut signatures = HashMap::new();
        for f in &program.functions {
            if f.type_params.is_empty() {
                signatures.insert(
                    f.name.as_str(),
                    (
                        f.params.iter().map(|p| p.ty.clone()).collect(),
                        f.ret.clone(),
                    ),
                );
            }
        }
        Self {
            program,
            out: String::new(),
            structs,
            enums,
            signatures,
            tmp: 0,
            locals: vec![HashMap::new()],
            loop_labels: Vec::new(),
            label_n: 0,
            ensures_alias: HashMap::new(),
            aux_fwd: String::new(),
            aux_code: String::new(),
            lambdas: BTreeMap::new(),
            lambda_captures: HashMap::new(),
            fn_value_names: Vec::new(),
            spawn_targets: HashSet::new(),
            wrapped_fns: HashSet::new(),
            spawn_entries: HashSet::new(),
            in_lambda: false,
            rooted: HashSet::new(),
        }
    }

    fn root_slot(&mut self, cname: &str, ty: &Type) {
        if !is_heap_ty(ty) {
            return;
        }
        if !self.rooted.insert(cname.to_string()) {
            return;
        }
        let _ = writeln!(self.out, "  mno_gc_add_root(&{});", cname);
    }

    fn emit_gc_maybe(&mut self) {
        self.out.push_str("  mno_gc_maybe();\n");
    }

    fn prepare(&mut self, reachable: &HashSet<String>) -> Result<(), Diagnostic> {
        for f in &self.program.functions {
            if f.is_extern || !f.type_params.is_empty() || !reachable.contains(&f.name) {
                continue;
            }
            collect_lambdas_stmts(&f.body, &mut self.lambdas);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                collect_lambdas_expr(&c.expr, &mut self.lambdas);
            }
            collect_fn_value_names_stmts(&f.body, &mut self.fn_value_names);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                let wrapper = [Stmt {
                    kind: StmtKind::Expr(c.expr.clone()),
                    span: c.expr.span,
                }];
                collect_fn_value_names_stmts(&wrapper, &mut self.fn_value_names);
            }
            collect_spawn_targets_stmts(&f.body, &mut self.spawn_targets);
            for c in f.requires.iter().chain(f.ensures.iter()) {
                collect_spawn_targets_expr(&c.expr, &mut self.spawn_targets);
            }
        }
        self.fn_value_names
            .retain(|n| self.signatures.contains_key(n.as_str()));
        self.fn_value_names.sort();
        self.fn_value_names.dedup();
        self.spawn_targets
            .retain(|n| self.signatures.contains_key(n.as_str()));

        for f in &self.program.functions {
            if f.is_extern || !f.type_params.is_empty() || !reachable.contains(&f.name) {
                continue;
            }
            let mut env = vec![HashMap::new()];
            for p in &f.params {
                env.last_mut().unwrap().insert(p.name.clone(), p.ty.clone());
            }
            analyze_lambda_captures_stmts(
                &f.body,
                &mut env,
                &self.signatures,
                &mut self.lambda_captures,
            );
            for c in f.requires.iter().chain(f.ensures.iter()) {
                analyze_lambda_captures_expr(
                    &c.expr,
                    &mut env,
                    &self.signatures,
                    &mut self.lambda_captures,
                );
            }
        }

        for id in self.lambdas.keys().copied().collect::<Vec<_>>() {
            self.emit_lambda_function(id)?;
        }
        for name in self.fn_value_names.clone() {
            self.ensure_wrapper(&name)?;
        }
        for name in self.spawn_targets.clone().into_iter().collect::<Vec<_>>() {
            self.ensure_spawn_entry(&name)?;
        }
        Ok(())
    }

    fn ensure_wrapper(&mut self, name: &str) -> Result<(), Diagnostic> {
        if !self.wrapped_fns.insert(name.to_string()) {
            return Ok(());
        }
        let (params, ret) = self.signatures[name].clone();
        let wrap = format!("mno_wrap_{}", c_ident(name));
        let _ = writeln!(self.aux_fwd, "static mno_i64 {}({});", wrap, self.closure_params(params.len()));
        let mut body = String::new();
        let _ = writeln!(body, "static mno_i64 {}({}) {{", wrap, self.closure_params(params.len()));
        let _ = writeln!(body, "  (void)__env;");
        let mut arg_calls = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let arg = format!("__a{}", i);
            arg_calls.push(typed_from_i64(&arg, p));
        }
        let call = format!("{}({})", c_ident(name), arg_calls.join(", "));
        if ret == Type::Unit {
            let _ = writeln!(body, "  {};", call);
            let _ = writeln!(body, "  return 0LL;");
        } else if ret == Type::Float {
            let _ = writeln!(body, "  return mno_f64_to_bits({});", call);
        } else {
            let _ = writeln!(body, "  return {};", call);
        }
        let _ = writeln!(body, "}}");
        self.aux_code.push_str(&body);
        self.aux_code.push('\n');
        Ok(())
    }

    fn ensure_spawn_entry(&mut self, name: &str) -> Result<(), Diagnostic> {
        if !self.spawn_entries.insert(name.to_string()) {
            return Ok(());
        }
        let entry = format!("__spawn_{}", c_ident(name));
        let (params, ret) = self.signatures[name].clone();
        let _ = writeln!(
            self.aux_fwd,
            "static mno_i64 {}(mno_i64 *argv);",
            entry
        );
        let mut body = String::new();
        let _ = writeln!(body, "static mno_i64 {}(mno_i64 *argv) {{", entry);
        let mut arg_calls = Vec::new();
        for (i, p) in params.iter().enumerate() {
            let raw = format!("argv[{}]", i);
            arg_calls.push(typed_from_i64(&raw, p));
        }
        let call = format!("{}({})", c_ident(name), arg_calls.join(", "));
        if ret == Type::Unit {
            let _ = writeln!(body, "  {};", call);
            let _ = writeln!(body, "  return 0LL;");
        } else if ret == Type::Float {
            let _ = writeln!(body, "  return mno_f64_to_bits({});", call);
        } else {
            let _ = writeln!(body, "  return {};", call);
        }
        let _ = writeln!(body, "}}");
        self.aux_code.push_str(&body);
        self.aux_code.push('\n');
        Ok(())
    }

    fn closure_params(&self, n_params: usize) -> String {
        let mut s = String::new();
        for i in 0..n_params {
            if i > 0 {
                s.push_str(", ");
            }
            let _ = write!(s, "mno_i64 __a{}", i);
        }
        if n_params > 0 {
            s.push_str(", ");
        }
        s.push_str("mno_i64 __env");
        s
    }

    fn emit_lambda_function(&mut self, id: usize) -> Result<(), Diagnostic> {
        let l = self.lambdas[&id].clone();
        let captures = self.lambda_captures.get(&id).cloned().unwrap_or_default();
        let lam = format!("mno_lam_{}", id);
        let n_params = l.params.len();
        let _ = writeln!(
            self.aux_fwd,
            "static mno_i64 {}({});",
            lam,
            self.closure_params(n_params)
        );

        let saved_out = std::mem::take(&mut self.out);
        let saved_rooted = std::mem::take(&mut self.rooted);
        self.in_lambda = true;
        self.out.push_str(&format!(
            "static mno_i64 {}({}) {{\n",
            lam,
            self.closure_params(n_params)
        ));
        self.out.push_str("  mno_gc_push_frame();\n");
        self.rooted.clear();

        self.locals = vec![HashMap::new()];
        for (i, p) in l.params.iter().enumerate() {
            let raw = format!("__a{}", i);
            self.bind(&p.name, p.ty.clone());
            if p.ty == Type::Float {
                let _ = writeln!(
                    self.out,
                    "  mno_f64 {} = mno_bits_to_f64({});",
                    c_ident(&p.name),
                    raw
                );
            } else {
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = {};",
                    c_ident(&p.name),
                    raw
                );
                self.root_slot(&c_ident(&p.name), &p.ty);
            }
        }
        // Env closure itself is a live root for capture loads.
        self.out.push_str("  mno_gc_add_root(&__env);\n");
        let _ = self.rooted.insert("__env".into());
        for (i, (name, ty)) in captures.iter().enumerate() {
            let cap = self.fresh("cap");
            let _ = writeln!(
                self.out,
                "  mno_i64 {} = mno_closure_get(__env, {}LL);",
                cap, i
            );
            if *ty == Type::Float {
                let _ = writeln!(
                    self.out,
                    "  mno_f64 {} = mno_bits_to_f64({});",
                    c_ident(name),
                    cap
                );
            } else {
                let _ = writeln!(self.out, "  mno_i64 {} = {};", c_ident(name), cap);
                self.root_slot(&c_ident(name), ty);
            }
            self.bind(name, ty.clone());
        }

        let result_tmp = if l.ret != Type::Unit {
            Some(self.fresh("lam_ret"))
        } else {
            None
        };
        if let Some(ref r) = result_tmp {
            if l.ret == Type::Float {
                let _ = writeln!(self.out, "  mno_f64 {} = 0;", r);
            } else {
                let _ = writeln!(self.out, "  mno_i64 {} = 0;", r);
                self.root_slot(r, &l.ret);
            }
        }
        let exit_lbl = self.fresh_label("lam_exit");
        self.emit_block(&l.body, result_tmp.as_deref(), &l.ret, &exit_lbl)?;

        let _ = writeln!(self.out, "  {}:;", exit_lbl);
        self.out.push_str("  mno_gc_pop_frame();\n");
        if let Some(r) = result_tmp {
            if l.ret == Type::Float {
                let _ = writeln!(self.out, "  return mno_f64_to_bits({});", r);
            } else if l.ret == Type::Unit {
                let _ = writeln!(self.out, "  return 0LL;");
            } else {
                let _ = writeln!(self.out, "  return {};", r);
            }
        } else {
            let _ = writeln!(self.out, "  return 0LL;");
        }
        self.out.push_str("}\n\n");
        self.aux_code.push_str(&self.out);
        self.out = saved_out;
        self.rooted = saved_rooted;
        self.in_lambda = false;
        Ok(())
    }

    fn emit_fn_value(&mut self, name: &str, span: Span) -> Result<String, Diagnostic> {
        self.ensure_wrapper(name)?;
        let t = self.fresh("clos");
        let wrap = format!("mno_wrap_{}", c_ident(name));
        let _ = writeln!(
            self.out,
            "  mno_i64 {} = mno_closure_new((void *)&{}, 0LL);",
            t, wrap
        );
        self.root_slot(&t, &Type::Fn(vec![], Box::new(Type::Unit)));
        let _ = span;
        Ok(t)
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.tmp += 1;
        format!("{}_{}", prefix, self.tmp)
    }

    fn fresh_label(&mut self, prefix: &str) -> String {
        self.label_n += 1;
        format!("{}_{}", prefix, self.label_n)
    }

    fn push_scope(&mut self) {
        self.locals.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.locals.pop();
    }

    fn bind(&mut self, name: &str, ty: Type) {
        self.locals.last_mut().unwrap().insert(name.to_string(), ty);
    }

    fn lookup_ty(&self, name: &str) -> Option<Type> {
        for scope in self.locals.iter().rev() {
            if let Some(t) = scope.get(name) {
                return Some(t.clone());
            }
        }
        None
    }

    fn emit_program(&mut self) -> Result<(), Diagnostic> {
        self.out.push_str("/* generated by machino native (Clang/LLVM) backend */\n");
        self.out.push_str("#include \"machino_rt.h\"\n");
        self.out.push_str("#include <stdint.h>\n\n");

        let reachable = reachable_from_main(self.program);
        self.prepare(&reachable)?;

        // Forward declarations
        for f in &self.program.functions {
            if f.is_extern || !f.type_params.is_empty() || !reachable.contains(&f.name) {
                continue;
            }
            let name = c_ident(&f.name);
            let ret = if f.ret == Type::Unit {
                "void".to_string()
            } else if f.ret == Type::Float {
                "mno_f64".to_string()
            } else {
                "mno_i64".to_string()
            };
            let mut params = String::new();
            for (i, p) in f.params.iter().enumerate() {
                if i > 0 {
                    params.push_str(", ");
                }
                let pt = if p.ty == Type::Float {
                    "mno_f64"
                } else {
                    "mno_i64"
                };
                let _ = write!(params, "{} {}", pt, c_ident(&p.name));
            }
            if f.params.is_empty() {
                params.push_str("void");
            }
            let _ = writeln!(self.out, "{} {}({});", ret, name, params);
        }
        if !self.aux_fwd.is_empty() {
            self.out.push_str(&self.aux_fwd);
        }
        self.out.push('\n');
        if !self.aux_code.is_empty() {
            self.out.push_str(&self.aux_code);
            self.out.push('\n');
        }

        let fns: Vec<&Function> = self
            .program
            .functions
            .iter()
            .filter(|f| {
                !f.is_extern && f.type_params.is_empty() && reachable.contains(&f.name)
            })
            .collect();
        for f in fns {
            self.emit_function(f)?;
        }

        self.out.push_str("int main(int argc, char **argv) {\n");
        self.out.push_str("  mno_init(argc, argv);\n");
        self.out.push_str("  mno_main();\n");
        self.out.push_str("  return 0;\n");
        self.out.push_str("}\n");
        Ok(())
    }

    fn emit_function(&mut self, f: &Function) -> Result<(), Diagnostic> {
        let name = c_ident(&f.name);
        let ret = if f.ret == Type::Unit {
            "void"
        } else if f.ret == Type::Float {
            "mno_f64"
        } else {
            "mno_i64"
        };
        let _ = write!(self.out, "{} {}(", ret, name);
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            let pt = if p.ty == Type::Float {
                "mno_f64"
            } else {
                "mno_i64"
            };
            let _ = write!(self.out, "{} {}", pt, c_ident(&p.name));
        }
        self.out.push_str(") {\n");
        self.out.push_str("  mno_gc_push_frame();\n");
        self.rooted.clear();

        self.locals = vec![HashMap::new()];
        for p in &f.params {
            self.bind(&p.name, p.ty.clone());
            self.root_slot(&c_ident(&p.name), &p.ty);
        }

        // shadow params for ensures
        let mut shadows: Vec<(String, Type)> = Vec::new();
        if !f.ensures.is_empty() {
            for p in &f.params {
                let sh = format!("__sh_{}", c_ident(&p.name));
                let pt = if p.ty == Type::Float {
                    "mno_f64"
                } else {
                    "mno_i64"
                };
                let _ = writeln!(
                    self.out,
                    "  {} {} = {};",
                    pt,
                    sh,
                    c_ident(&p.name)
                );
                shadows.push((p.name.clone(), p.ty.clone()));
                self.root_slot(&sh, &p.ty);
            }
        }

        for c in &f.requires {
            let v = self.emit_expr(&c.expr, Some(&Type::Bool))?;
            let msg = format!(
                "runtime error: contract violation: requires '{}' failed when calling '{}'",
                c.text, f.name
            );
            let _ = writeln!(
                self.out,
                "  if (!({})) mno_fail({});",
                v,
                escape_c_string(&msg)
            );
        }

        let result_tmp = if f.ret != Type::Unit {
            let t = self.fresh("result");
            let pt = if f.ret == Type::Float {
                "mno_f64"
            } else {
                "mno_i64"
            };
            let _ = writeln!(self.out, "  {} {} = 0;", pt, t);
            self.root_slot(&t, &f.ret);
            Some(t)
        } else {
            None
        };
        let exit_lbl = self.fresh_label("exit");

        self.emit_block(&f.body, result_tmp.as_deref(), &f.ret, &exit_lbl)?;

        let _ = writeln!(self.out, "  {}:;", exit_lbl);
        if !f.ensures.is_empty() {
            self.ensures_alias.clear();
            for (name, _) in &shadows {
                self.ensures_alias
                    .insert(name.clone(), format!("__sh_{}", c_ident(name)));
            }
            if let Some(ref r) = result_tmp {
                self.bind("result", f.ret.clone());
                self.ensures_alias
                    .insert("result".into(), r.clone());
            }
            for c in &f.ensures {
                let v = self.emit_expr(&c.expr, Some(&Type::Bool))?;
                let msg = format!(
                    "runtime error: contract violation: ensures '{}' failed in '{}'",
                    c.text, f.name
                );
                let _ = writeln!(
                    self.out,
                    "  if (!({})) mno_fail({});",
                    v,
                    escape_c_string(&msg)
                );
            }
            self.ensures_alias.clear();
        }

        self.out.push_str("  mno_gc_pop_frame();\n");
        if let Some(r) = result_tmp {
            let _ = writeln!(self.out, "  return {};", r);
        } else {
            self.out.push_str("  return;\n");
        }
        self.out.push_str("}\n\n");
        Ok(())
    }

    fn emit_block(
        &mut self,
        stmts: &[Stmt],
        result_tmp: Option<&str>,
        ret_ty: &Type,
        exit_lbl: &str,
    ) -> Result<(), Diagnostic> {
        self.push_scope();
        for s in stmts {
            self.emit_stmt(s, result_tmp, ret_ty, exit_lbl)?;
        }
        self.pop_scope();
        Ok(())
    }

    fn emit_stmt(
        &mut self,
        stmt: &Stmt,
        result_tmp: Option<&str>,
        ret_ty: &Type,
        exit_lbl: &str,
    ) -> Result<(), Diagnostic> {
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let inferred = ty.clone().unwrap_or_else(|| self.type_of(value));
                let v = self.emit_expr(value, Some(&inferred))?;
                let cname = c_ident(name);
                let pt = if matches!(inferred, Type::Fn(_, _)) {
                    "mno_i64"
                } else if inferred == Type::Float {
                    "mno_f64"
                } else {
                    "mno_i64"
                };
                let _ = writeln!(self.out, "  {} {} = {};", pt, cname, v);
                self.bind(name, inferred.clone());
                self.root_slot(&cname, &inferred);
                self.emit_gc_maybe();
            }
            StmtKind::Assign { name, value } => {
                let ty = self
                    .lookup_ty(name)
                    .ok_or_else(|| e_native("unknown variable in assignment", stmt.span))?;
                let v = self.emit_expr(value, Some(&ty))?;
                let _ = writeln!(self.out, "  {} = {};", c_ident(name), v);
            }
            StmtKind::IndexAssign { base, index, value } => {
                let b = self.emit_expr(base, None)?;
                let i = self.emit_expr(index, Some(&Type::Int))?;
                let elem_ty = match self.type_of(base) {
                    Type::Array(e) => *e,
                    _ => Type::Int,
                };
                let v = self.emit_expr(value, Some(&elem_ty))?;
                if elem_ty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_f64_to_bits({});",
                        bits, v
                    );
                    let _ = writeln!(self.out, "  mno_arr_set({}, {}, {});", b, i, bits);
                } else {
                    let _ = writeln!(self.out, "  mno_arr_set({}, {}, {});", b, i, v);
                }
            }
            StmtKind::FieldAssign { base, field, value } => {
                let bty = self.type_of(base);
                let Type::Struct(sname) = bty else {
                    return Err(e_native("field assign on non-struct", stmt.span));
                };
                let idx = self.field_index(&sname, field, stmt.span)?;
                let fty = self.field_type(&sname, field);
                let b = self.emit_expr(base, None)?;
                let v = self.emit_expr(value, Some(&fty))?;
                if fty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_f64_to_bits({});",
                        bits, v
                    );
                    let _ = writeln!(
                        self.out,
                        "  mno_struct_set({}, {}, {});",
                        b, idx, bits
                    );
                } else {
                    let _ = writeln!(self.out, "  mno_struct_set({}, {}, {});", b, idx, v);
                }
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let c = self.emit_expr(cond, Some(&Type::Bool))?;
                let _ = writeln!(self.out, "  if ({}) {{", c);
                self.emit_block(then_body, result_tmp, ret_ty, exit_lbl)?;
                self.out.push_str("  } else {\n");
                self.emit_block(else_body, result_tmp, ret_ty, exit_lbl)?;
                self.out.push_str("  }\n");
            }
            StmtKind::While {
                cond,
                invariant: _,
                body,
            } => {
                let cont = self.fresh_label("cont");
                let brk = self.fresh_label("brk");
                self.loop_labels.push((cont.clone(), brk.clone()));
                let _ = writeln!(self.out, "  while (1) {{");
                let c = self.emit_expr(cond, Some(&Type::Bool))?;
                let _ = writeln!(self.out, "    if (!({})) break;", c);
                self.emit_block(body, result_tmp, ret_ty, exit_lbl)?;
                let _ = writeln!(self.out, "    {}:;", cont);
                self.emit_gc_maybe();
                self.out.push_str("  }\n");
                let _ = writeln!(self.out, "  {}:;", brk);
                self.loop_labels.pop();
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                let cont = self.fresh_label("cont");
                let brk = self.fresh_label("brk");
                self.loop_labels.push((cont.clone(), brk.clone()));
                let s = self.emit_expr(start, Some(&Type::Int))?;
                let e = self.emit_expr(end, Some(&Type::Int))?;
                let vend = self.fresh("end");
                let _ = writeln!(self.out, "  mno_i64 {} = {};", vend, e);
                let _ = writeln!(
                    self.out,
                    "  for (mno_i64 {} = {}; {} < {}; {} = mno_iadd({}, 1)) {{",
                    c_ident(var),
                    s,
                    c_ident(var),
                    vend,
                    c_ident(var),
                    c_ident(var)
                );
                self.push_scope();
                self.bind(var, Type::Int);
                self.emit_block(body, result_tmp, ret_ty, exit_lbl)?;
                self.pop_scope();
                let _ = writeln!(self.out, "    {}:;", cont);
                self.emit_gc_maybe();
                self.out.push_str("  }\n");
                let _ = writeln!(self.out, "  {}:;", brk);
                self.loop_labels.pop();
            }
            StmtKind::Break => {
                let brk = self
                    .loop_labels
                    .last()
                    .map(|(_, b)| b.clone())
                    .ok_or_else(|| e_native("break outside loop", stmt.span))?;
                let _ = writeln!(self.out, "  goto {};", brk);
            }
            StmtKind::Continue => {
                let cont = self
                    .loop_labels
                    .last()
                    .map(|(c, _)| c.clone())
                    .ok_or_else(|| e_native("continue outside loop", stmt.span))?;
                let _ = writeln!(self.out, "  goto {};", cont);
            }
            StmtKind::Return(None) => {
                let _ = writeln!(self.out, "  goto {};", exit_lbl);
            }
            StmtKind::Return(Some(e)) => {
                let v = self.emit_expr(e, Some(ret_ty))?;
                if let Some(r) = result_tmp {
                    let _ = writeln!(self.out, "  {} = {};", r, v);
                }
                let _ = writeln!(self.out, "  goto {};", exit_lbl);
            }
            StmtKind::Assert(e) => {
                let v = self.emit_expr(e, Some(&Type::Bool))?;
                let _ = writeln!(
                    self.out,
                    "  if (!({})) mno_fail(\"runtime error: assertion failed\");",
                    v
                );
            }
            StmtKind::Expr(e) => {
                let _ = self.emit_expr(e, None)?;
            }
        }
        Ok(())
    }

    fn emit_expr(&mut self, expr: &Expr, expected: Option<&Type>) -> Result<String, Diagnostic> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(format!("{}LL", n)),
            ExprKind::Float(f) => Ok(format!("{:?}", f)),
            ExprKind::Bool(b) => Ok(if *b { "1LL".into() } else { "0LL".into() }),
            ExprKind::Str(s) => {
                let lit = escape_c_string(s);
                let t = self.fresh("str");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_str_from_lit({}, {}LL);",
                    t,
                    lit,
                    s.len()
                );
                self.root_slot(&t, &Type::Str);
                Ok(t)
            }
            ExprKind::Var(name) => {
                if let Some(alias) = self.ensures_alias.get(name) {
                    return Ok(alias.clone());
                }
                if let Some(ty) = self.lookup_ty(name) {
                    if matches!(ty, Type::Fn(_, _)) {
                        return Ok(c_ident(name));
                    }
                    let _ = ty;
                    return Ok(c_ident(name));
                }
                // unit enum variant
                if let Some(colon) = name.rfind("::") {
                    let en = &name[..colon];
                    let vn = &name[colon + 2..];
                    if let Some(variants) = self.enums.get(en) {
                        let tag = variants
                            .iter()
                            .position(|v| v.name == vn)
                            .ok_or_else(|| e_native("unknown enum variant", expr.span))?;
                        let t = self.fresh("en");
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_enum_new({}LL, 0LL);",
                            t, tag
                        );
                        self.root_slot(&t, &Type::Enum(en.to_string()));
                        return Ok(t);
                    }
                }
                if self.signatures.contains_key(name.as_str()) {
                    return self.emit_fn_value(name, expr.span);
                }
                Err(e_native(
                    &format!("unknown variable '{}'", name),
                    expr.span,
                ))
            }
            ExprKind::Array(elems) => {
                let t = self.fresh("arr");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_arr_new({}LL);",
                    t,
                    elems.len()
                );
                let elem_ty = elems
                    .first()
                    .map(|e| self.type_of(e))
                    .unwrap_or(Type::Int);
                self.root_slot(&t, &Type::Array(Box::new(elem_ty.clone())));
                for (i, e) in elems.iter().enumerate() {
                    let v = self.emit_expr(e, Some(&elem_ty))?;
                    if elem_ty == Type::Float {
                        let bits = self.fresh("bits");
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_f64_to_bits({});",
                            bits, v
                        );
                        let _ = writeln!(self.out, "  mno_arr_set({}, {}LL, {});", t, i, bits);
                    } else {
                        let _ = writeln!(self.out, "  mno_arr_set({}, {}LL, {});", t, i, v);
                    }
                }
                Ok(t)
            }
            ExprKind::Index(base, idx) => {
                let b = self.emit_expr(base, None)?;
                let i = self.emit_expr(idx, Some(&Type::Int))?;
                let t = self.fresh("idx");
                let elem_ty = match self.type_of(base) {
                    Type::Array(e) => *e,
                    Type::Str => Type::Int,
                    _ => Type::Int,
                };
                if matches!(self.type_of(base), Type::Str) {
                    let _ = writeln!(self.out, "  mno_i64 {} = mno_str_at({}, {});", t, b, i);
                } else if elem_ty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_arr_get({}, {});",
                        bits, b, i
                    );
                    let _ = writeln!(
                        self.out,
                        "  mno_f64 {} = mno_bits_to_f64({});",
                        t, bits
                    );
                } else {
                    let _ = writeln!(self.out, "  mno_i64 {} = mno_arr_get({}, {});", t, b, i);
                }
                Ok(t)
            }
            ExprKind::Field(base, field) => {
                let Type::Struct(sname) = self.type_of(base) else {
                    return Err(e_native("field access on non-struct", expr.span));
                };
                let idx = self.field_index(&sname, field, expr.span)?;
                let fty = self.field_type(&sname, field);
                let b = self.emit_expr(base, None)?;
                let t = self.fresh("fld");
                if fty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_struct_get({}, {}LL);",
                        bits, b, idx
                    );
                    let _ = writeln!(
                        self.out,
                        "  mno_f64 {} = mno_bits_to_f64({});",
                        t, bits
                    );
                } else {
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_struct_get({}, {}LL);",
                        t, b, idx
                    );
                }
                Ok(t)
            }
            ExprKind::Un(op, inner) => {
                let ty = self.type_of(inner);
                let v = self.emit_expr(inner, Some(&ty))?;
                let t = self.fresh("un");
                match op {
                    UnOp::Neg if ty == Type::Float => {
                        let _ = writeln!(self.out, "  mno_f64 {} = -({});", t, v);
                    }
                    UnOp::Neg => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_isub(0LL, {});",
                            t, v
                        );
                    }
                    UnOp::Not => {
                        let _ = writeln!(self.out, "  mno_i64 {} = !({});", t, v);
                    }
                }
                Ok(t)
            }
            ExprKind::Bin(op, lhs, rhs) => self.emit_bin(*op, lhs, rhs, expr.span),
            ExprKind::Call(name, _ta, args) => self.emit_call(name, args, expected, expr.span),
            ExprKind::Lambda(l) => {
                let captures = self
                    .lambda_captures
                    .get(&l.id)
                    .cloned()
                    .unwrap_or_default();
                let lam = format!("mno_lam_{}", l.id);
                let t = self.fresh("clos");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_closure_new((void *)&{}, {}LL);",
                    t,
                    lam,
                    captures.len()
                );
                self.root_slot(
                    &t,
                    &Type::Fn(
                        l.params.iter().map(|p| p.ty.clone()).collect(),
                        Box::new(l.ret.clone()),
                    ),
                );
                for (i, (cap_name, cap_ty)) in captures.iter().enumerate() {
                    let v = self.emit_expr(
                        &Expr {
                            kind: ExprKind::Var(cap_name.clone()),
                            span: expr.span,
                        },
                        Some(cap_ty),
                    )?;
                    let bits = i64_from_typed(&v, cap_ty);
                    let _ = writeln!(self.out, "  mno_closure_set({}, {}LL, {});", t, i, bits);
                }
                Ok(t)
            }
            ExprKind::Match(m) => self.emit_match(m, expected, expr.span),
        }
    }

    fn emit_bin(
        &mut self,
        op: BinOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
    ) -> Result<String, Diagnostic> {
        let lt = self.type_of(lhs);
        let rt = self.type_of(rhs);
        let l = self.emit_expr(lhs, Some(&lt))?;
        let r = self.emit_expr(rhs, Some(&rt))?;
        let t = self.fresh("bin");
        use BinOp::*;
        match (&lt, &rt, op) {
            (Type::Str, Type::Str, Add) => {
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_str_concat({}, {});",
                    t, l, r
                );
                self.root_slot(&t, &Type::Str);
            }
            (Type::Str, Type::Str, Eq) => {
                let _ = writeln!(self.out, "  mno_i64 {} = mno_str_eq({}, {});", t, l, r);
            }
            (Type::Str, Type::Str, Ne) => {
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = !mno_str_eq({}, {});",
                    t, l, r
                );
            }
            (Type::Float, Type::Float, _) => {
                let fop = match op {
                    Add => "mno_fadd",
                    Sub => "mno_fsub",
                    Mul => "mno_fmul",
                    Div => "mno_fdiv",
                    Eq => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} == {});", t, l, r);
                        return Ok(t);
                    }
                    Ne => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} != {});", t, l, r);
                        return Ok(t);
                    }
                    Lt => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} < {});", t, l, r);
                        return Ok(t);
                    }
                    Le => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} <= {});", t, l, r);
                        return Ok(t);
                    }
                    Gt => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} > {});", t, l, r);
                        return Ok(t);
                    }
                    Ge => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} >= {});", t, l, r);
                        return Ok(t);
                    }
                    _ => {
                        return Err(e_native("unsupported float operator", span));
                    }
                };
                let _ = writeln!(self.out, "  mno_f64 {} = {}({}, {});", t, fop, l, r);
            }
            (Type::Int, Type::Int, _) | (Type::Bool, Type::Bool, _) => {
                match op {
                    Add => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_iadd({}, {});",
                            t, l, r
                        );
                    }
                    Sub => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_isub({}, {});",
                            t, l, r
                        );
                    }
                    Mul => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_imul({}, {});",
                            t, l, r
                        );
                    }
                    Div => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_idiv({}, {});",
                            t, l, r
                        );
                    }
                    Mod => {
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_irem({}, {});",
                            t, l, r
                        );
                    }
                    Eq => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} == {});", t, l, r);
                    }
                    Ne => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} != {});", t, l, r);
                    }
                    Lt => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} < {});", t, l, r);
                    }
                    Le => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} <= {});", t, l, r);
                    }
                    Gt => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} > {});", t, l, r);
                    }
                    Ge => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} >= {});", t, l, r);
                    }
                    And => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} && {});", t, l, r);
                    }
                    Or => {
                        let _ = writeln!(self.out, "  mno_i64 {} = ({} || {});", t, l, r);
                    }
                }
            }
            _ => {
                return Err(e_native(
                    &format!("native backend: unsupported operands for '{:?}'", op),
                    span,
                ));
            }
        }
        Ok(t)
    }

    fn emit_call(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: Option<&Type>,
        span: Span,
    ) -> Result<String, Diagnostic> {
        // Indirect call through a local function value.
        if let Some(Type::Fn(params, ret)) = self.lookup_ty(name) {
            let clos = c_ident(name);
            let mut i64_args = Vec::new();
            for (a, p) in args.iter().zip(params.iter()) {
                let v = self.emit_expr(a, Some(p))?;
                i64_args.push(i64_from_typed(&v, p));
            }
            let cast = closure_fn_ptr_type(params.len());
            let mut call_args = i64_args.join(", ");
            if !call_args.is_empty() {
                call_args.push_str(", ");
            }
            call_args.push_str(&clos);
            let t = self.fresh("icall");
            if *ret == Type::Float {
                let raw = self.fresh("icall_raw");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = (({})mno_closure_fn({}))({});",
                    raw, cast, clos, call_args
                );
                let _ = writeln!(
                    self.out,
                    "  mno_f64 {} = mno_bits_to_f64({});",
                    t, raw
                );
            } else {
                let pt = "mno_i64";
                let _ = writeln!(
                    self.out,
                    "  {} {} = (({})mno_closure_fn({}))({});",
                    pt, t, cast, clos, call_args
                );
            }
            let _ = expected;
            return Ok(t);
        }

        // Host / builtin surface
        match name {
            "print" => {
                let ty = self.type_of(&args[0]);
                let v = self.emit_expr(&args[0], Some(&ty))?;
                match ty {
                    Type::Int => {
                        let _ = writeln!(self.out, "  mno_print_i64({});", v);
                    }
                    Type::Float => {
                        let _ = writeln!(self.out, "  mno_print_f64({});", v);
                    }
                    Type::Bool => {
                        let _ = writeln!(self.out, "  mno_print_bool({});", v);
                    }
                    Type::Str => {
                        let _ = writeln!(self.out, "  mno_print_str({});", v);
                    }
                    _ => {
                        return Err(e_native(
                            "native print only supports int/float/bool/str",
                            span,
                        ));
                    }
                }
                return Ok("0LL".into());
            }
            "len" => {
                let ty = self.type_of(&args[0]);
                let v = self.emit_expr(&args[0], Some(&ty))?;
                let t = self.fresh("len");
                match ty {
                    Type::Str => {
                        let _ = writeln!(self.out, "  mno_i64 {} = mno_str_len({});", t, v);
                    }
                    Type::Array(_) => {
                        let _ = writeln!(self.out, "  mno_i64 {} = mno_arr_len({});", t, v);
                    }
                    _ => return Err(e_native("len expects str or array", span)),
                }
                return Ok(t);
            }
            "push" => {
                let a = self.emit_expr(&args[0], None)?;
                let elem_ty = match self.type_of(&args[0]) {
                    Type::Array(e) => *e,
                    _ => Type::Int,
                };
                let v = self.emit_expr(&args[1], Some(&elem_ty))?;
                let t = self.fresh("push");
                if elem_ty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_f64_to_bits({});",
                        bits, v
                    );
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_arr_push({}, {});",
                        t, a, bits
                    );
                } else {
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_arr_push({}, {});",
                        t, a, v
                    );
                }
                self.root_slot(&t, &Type::Array(Box::new(elem_ty)));
                return Ok(t);
            }
            "char_at" => {
                let s = self.emit_expr(&args[0], Some(&Type::Str))?;
                let i = self.emit_expr(&args[1], Some(&Type::Int))?;
                let t = self.fresh("ch");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_str_at({}, {});", t, s, i);
                return Ok(t);
            }
            "substr" => {
                let s = self.emit_expr(&args[0], Some(&Type::Str))?;
                let a = self.emit_expr(&args[1], Some(&Type::Int))?;
                let b = self.emit_expr(&args[2], Some(&Type::Int))?;
                let t = self.fresh("sub");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_substr({}, {}, {});",
                    t, s, a, b
                );
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            "chr" => {
                let c = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("chr");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_chr({});", t, c);
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            "len_cp" => {
                let s = self.emit_expr(&args[0], Some(&Type::Str))?;
                let t = self.fresh("lcp");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_len_cp({});", t, s);
                return Ok(t);
            }
            "char_at_cp" => {
                let s = self.emit_expr(&args[0], Some(&Type::Str))?;
                let i = self.emit_expr(&args[1], Some(&Type::Int))?;
                let t = self.fresh("ccp");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_char_at_cp({}, {});",
                    t, s, i
                );
                return Ok(t);
            }
            "substr_cp" => {
                let s = self.emit_expr(&args[0], Some(&Type::Str))?;
                let a = self.emit_expr(&args[1], Some(&Type::Int))?;
                let b = self.emit_expr(&args[2], Some(&Type::Int))?;
                let t = self.fresh("scp");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_substr_cp({}, {}, {});",
                    t, s, a, b
                );
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            "chr_cp" => {
                let c = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("chrp");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_chr_cp({});", t, c);
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            "to_int" => {
                let v = self.emit_expr(&args[0], Some(&Type::Float))?;
                let t = self.fresh("toi");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_to_int({});", t, v);
                return Ok(t);
            }
            "to_float" => {
                let v = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("tof");
                let _ = writeln!(self.out, "  mno_f64 {} = mno_to_float({});", t, v);
                return Ok(t);
            }
            "hash" => {
                let ty = self.type_of(&args[0]);
                let v = self.emit_expr(&args[0], Some(&ty))?;
                let t = self.fresh("h");
                if ty == Type::Str {
                    let _ = writeln!(self.out, "  mno_i64 {} = mno_hash_str({});", t, v);
                } else {
                    let _ = writeln!(self.out, "  mno_i64 {} = {};", t, v);
                }
                return Ok(t);
            }
            "spawn" => {
                let ExprKind::Var(target) = &args[0].kind else {
                    return Err(e_native("spawn of a non-named function", span));
                };
                let (params, ret) = self
                    .signatures
                    .get(target.as_str())
                    .cloned()
                    .ok_or_else(|| e_native(&format!("unknown spawn target '{}'", target), span))?;
                self.ensure_spawn_entry(target)?;
                let n = params.len();
                // Emit argument expressions first (they may introduce statements),
                // then pack them into the argv array.
                let mut arg_words = Vec::new();
                for (a, p) in args[1..].iter().zip(params.iter()) {
                    let v = self.emit_expr(a, Some(p))?;
                    arg_words.push(i64_from_typed(&v, p));
                }
                let argv = self.fresh("spawn_argv");
                if n > 0 {
                    let _ = writeln!(self.out, "  mno_i64 {}[{}] = {{", argv, n);
                    for w in &arg_words {
                        let _ = writeln!(self.out, "    {},", w);
                    }
                    let _ = writeln!(self.out, "  }};");
                } else {
                    let _ = writeln!(self.out, "  mno_i64 *{} = NULL;", argv);
                }
                let entry = format!("__spawn_{}", c_ident(target));
                let t = self.fresh("spawn");
                let rk = ret_kind(&ret);
                let kinds = params.iter().map(value_kind).collect::<String>();
                let kinds_lit = if n == 0 {
                    "NULL".to_string()
                } else {
                    let k = self.fresh("spawn_kinds");
                    let _ = writeln!(
                        self.out,
                        "  static const char {}[] = {};",
                        k,
                        escape_c_string(&kinds)
                    );
                    k
                };
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_task_spawn(&{}, {}, {}LL, '{}', {});",
                    t, entry, argv, n, rk, kinds_lit
                );
                return Ok(t);
            }
            "join_int" | "join_bool" => {
                let h = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("join");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_task_join_i64({});", t, h);
                return Ok(t);
            }
            "join_float" => {
                let h = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("join");
                let _ = writeln!(self.out, "  mno_f64 {} = mno_task_join_f64({});", t, h);
                return Ok(t);
            }
            "join_str" => {
                let h = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("join");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_task_join_str({});", t, h);
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            "chan_new" => {
                let t = self.fresh("chan");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_chan_new();", t);
                return Ok(t);
            }
            "chan_close" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let _ = writeln!(self.out, "  mno_chan_close({});", ch);
                return Ok("0LL".into());
            }
            "chan_send_int" | "chan_send_bool" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let v = self.emit_expr(&args[1], Some(&Type::Int))?;
                let _ = writeln!(self.out, "  mno_chan_send_i64({}, {});", ch, v);
                return Ok("0LL".into());
            }
            "chan_send_float" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let v = self.emit_expr(&args[1], Some(&Type::Float))?;
                let _ = writeln!(self.out, "  mno_chan_send_f64({}, {});", ch, v);
                return Ok("0LL".into());
            }
            "chan_send_str" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let v = self.emit_expr(&args[1], Some(&Type::Str))?;
                let _ = writeln!(self.out, "  mno_chan_send_str({}, {});", ch, v);
                return Ok("0LL".into());
            }
            "chan_recv_int" | "chan_recv_bool" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("recv");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_chan_recv_i64({});", t, ch);
                return Ok(t);
            }
            "chan_recv_float" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("recv");
                let _ = writeln!(self.out, "  mno_f64 {} = mno_chan_recv_f64({});", t, ch);
                return Ok(t);
            }
            "chan_recv_str" => {
                let ch = self.emit_expr(&args[0], Some(&Type::Int))?;
                let t = self.fresh("recv");
                let _ = writeln!(self.out, "  mno_i64 {} = mno_chan_recv_str({});", t, ch);
                self.root_slot(&t, &Type::Str);
                return Ok(t);
            }
            _ => {}
        }

        // Host externs mapped to runtime
        let host = match name {
            "args" => Some(("mno_args", true, Type::Array(Box::new(Type::Str)))),
            "getenv" => Some(("mno_getenv", true, Type::Str)),
            "clock_ms" => Some(("mno_clock_ms", true, Type::Int)),
            "sleep_ms" => Some(("mno_sleep_ms", false, Type::Unit)),
            "read_file" => Some(("mno_read_file", true, Type::Str)),
            "write_file" => Some(("mno_write_file", true, Type::Int)),
            "file_exists" => Some(("mno_file_exists", true, Type::Bool)),
            "read_line" => Some(("mno_read_line", true, Type::Str)),
            "exit" => Some(("mno_exit", false, Type::Unit)),
            "http_get" => Some(("mno_http_get", true, Type::Str)),
            "tcp_listen" => Some(("mno_tcp_listen", true, Type::Int)),
            "tcp_accept" => Some(("mno_tcp_accept", true, Type::Int)),
            "tcp_read" => Some(("mno_tcp_read", true, Type::Str)),
            "tcp_write" => Some(("mno_tcp_write", true, Type::Int)),
            "tcp_close" => Some(("mno_tcp_close", false, Type::Unit)),
            "gc_collect" => Some(("mno_gc_collect", false, Type::Unit)),
            "heap_live_count" => Some(("mno_heap_live_count", true, Type::Int)),
            _ => None,
        };
        if let Some((fname, has_ret, ret)) = host {
            let mut arg_ss = Vec::new();
            let param_tys: Option<Vec<Type>> =
                self.signatures.get(name).map(|(ps, _)| ps.clone());
            if let Some(params) = param_tys {
                for (a, p) in args.iter().zip(params.iter()) {
                    arg_ss.push(self.emit_expr(a, Some(p))?);
                }
            } else {
                for a in args {
                    arg_ss.push(self.emit_expr(a, None)?);
                }
            }
            if has_ret {
                let t = self.fresh("call");
                let pt = if ret == Type::Float {
                    "mno_f64"
                } else {
                    "mno_i64"
                };
                let _ = write!(self.out, "  {} {} = {}(", pt, t, fname);
                for (i, a) in arg_ss.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(a);
                }
                self.out.push_str(");\n");
                self.root_slot(&t, &ret);
                return Ok(t);
            } else {
                let _ = write!(self.out, "  {}(", fname);
                for (i, a) in arg_ss.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(a);
                }
                self.out.push_str(");\n");
                return Ok("0LL".into());
            }
        }

        // Enum variant constructor
        if let Some(colon) = name.rfind("::") {
            let en = &name[..colon];
            let vn = &name[colon + 2..];
            if let Some(variants) = self.enums.get(en).cloned() {
                let (tag, variant) = variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.name == vn)
                    .map(|(i, v)| (i, *v))
                    .ok_or_else(|| e_native("unknown variant", span))?;
                let t = self.fresh("en");
                let _ = writeln!(
                    self.out,
                    "  mno_i64 {} = mno_enum_new({}LL, {}LL);",
                    t,
                    tag,
                    variant.payloads.len()
                );
                self.root_slot(&t, &Type::Enum(en.to_string()));
                for (i, (a, pty)) in args.iter().zip(variant.payloads.iter()).enumerate() {
                    let v = self.emit_expr(a, Some(pty))?;
                    if *pty == Type::Float {
                        let bits = self.fresh("bits");
                        let _ = writeln!(
                            self.out,
                            "  mno_i64 {} = mno_f64_to_bits({});",
                            bits, v
                        );
                        let _ = writeln!(
                            self.out,
                            "  mno_enum_set_payload({}, {}LL, {});",
                            t, i, bits
                        );
                    } else {
                        let _ = writeln!(
                            self.out,
                            "  mno_enum_set_payload({}, {}LL, {});",
                            t, i, v
                        );
                    }
                }
                return Ok(t);
            }
        }

        // Struct constructor
        if let Some(fields) = self.structs.get(name).cloned() {
            let t = self.fresh("st");
            let _ = writeln!(
                self.out,
                "  mno_i64 {} = mno_struct_new({}LL);",
                t,
                fields.len()
            );
            self.root_slot(&t, &Type::Struct(name.to_string()));
            for (i, (fld, a)) in fields.iter().zip(args.iter()).enumerate() {
                let v = self.emit_expr(a, Some(&fld.ty))?;
                if fld.ty == Type::Float {
                    let bits = self.fresh("bits");
                    let _ = writeln!(
                        self.out,
                        "  mno_i64 {} = mno_f64_to_bits({});",
                        bits, v
                    );
                    let _ = writeln!(
                        self.out,
                        "  mno_struct_set({}, {}LL, {});",
                        t, i, bits
                    );
                } else {
                    let _ = writeln!(self.out, "  mno_struct_set({}, {}LL, {});", t, i, v);
                }
            }
            return Ok(t);
        }

        // User / std function
        let (params, ret) = self
            .signatures
            .get(name)
            .cloned()
            .ok_or_else(|| e_native(&format!("unknown function '{}'", name), span))?;
        let mut arg_ss = Vec::new();
        for (a, p) in args.iter().zip(params.iter()) {
            arg_ss.push(self.emit_expr(a, Some(p))?);
        }
        if ret == Type::Unit {
            let _ = write!(self.out, "  {}(", c_ident(name));
            for (i, a) in arg_ss.iter().enumerate() {
                if i > 0 {
                    self.out.push_str(", ");
                }
                self.out.push_str(a);
            }
            self.out.push_str(");\n");
            Ok("0LL".into())
        } else {
            let t = self.fresh("call");
            let pt = if ret == Type::Float {
                "mno_f64"
            } else {
                "mno_i64"
            };
            let _ = write!(self.out, "  {} {} = {}(", pt, t, c_ident(name));
            for (i, a) in arg_ss.iter().enumerate() {
                if i > 0 {
                    self.out.push_str(", ");
                }
                self.out.push_str(a);
            }
            self.out.push_str(");\n");
            self.root_slot(&t, &ret);
            Ok(t)
        }
    }

    fn emit_match(
        &mut self,
        m: &Match,
        expected: Option<&Type>,
        span: Span,
    ) -> Result<String, Diagnostic> {
        let sty = self.type_of(&m.scrutinee);
        let s = self.emit_expr(&m.scrutinee, Some(&sty))?;
        let result_ty = expected.cloned().unwrap_or_else(|| {
            m.arms
                .first()
                .map(|a| self.type_of(&a.body))
                .unwrap_or(Type::Int)
        });
        let t = self.fresh("match");
        let pt = if result_ty == Type::Float {
            "mno_f64"
        } else {
            "mno_i64"
        };
        let _ = writeln!(self.out, "  {} {} = 0;", pt, t);
        let end = self.fresh_label("match_end");
        for (i, arm) in m.arms.iter().enumerate() {
            let lbl = self.fresh_label(&format!("arm{}", i));
            let next = self.fresh_label(&format!("next{}", i));
            let _ = writeln!(self.out, "  {{\n    /* match arm */");
            self.push_scope();
            self.emit_pattern_test(&arm.pattern, &s, &sty, &lbl, &next)?;
            let _ = writeln!(self.out, "    {}:;", lbl);
            let v = self.emit_expr(&arm.body, Some(&result_ty))?;
            let _ = writeln!(self.out, "    {} = {};", t, v);
            let _ = writeln!(self.out, "    goto {};", end);
            let _ = writeln!(self.out, "    {}:;", next);
            self.pop_scope();
            self.out.push_str("  }\n");
        }
        let _ = writeln!(
            self.out,
            "  mno_fail(\"runtime error: non-exhaustive match\");"
        );
        let _ = writeln!(self.out, "  {}:;", end);
        let _ = span;
        Ok(t)
    }

    fn emit_pattern_test(
        &mut self,
        pat: &Pattern,
        scrut: &str,
        sty: &Type,
        ok: &str,
        fail: &str,
    ) -> Result<(), Diagnostic> {
        match pat {
            Pattern::Wildcard => {
                let _ = writeln!(self.out, "    goto {};", ok);
            }
            Pattern::Var(name) => {
                let pt = if *sty == Type::Float {
                    "mno_f64"
                } else {
                    "mno_i64"
                };
                let _ = writeln!(self.out, "    {} {} = {};", pt, c_ident(name), scrut);
                self.bind(name, sty.clone());
                let _ = writeln!(self.out, "    goto {};", ok);
            }
            Pattern::Int(n) => {
                let _ = writeln!(
                    self.out,
                    "    if ({} == {}LL) goto {}; else goto {};",
                    scrut, n, ok, fail
                );
            }
            Pattern::Bool(b) => {
                let v = if *b { 1 } else { 0 };
                let _ = writeln!(
                    self.out,
                    "    if ({} == {}LL) goto {}; else goto {};",
                    scrut, v, ok, fail
                );
            }
            Pattern::Str(s) => {
                let lit = self.fresh("plit");
                let _ = writeln!(
                    self.out,
                    "    mno_i64 {} = mno_str_from_lit({}, {}LL);",
                    lit,
                    escape_c_string(s),
                    s.len()
                );
                let _ = writeln!(
                    self.out,
                    "    if (mno_str_eq({}, {})) goto {}; else goto {};",
                    scrut, lit, ok, fail
                );
            }
            Pattern::Variant(en, vn) => {
                let tag = self.variant_tag(en, vn)?;
                let _ = writeln!(
                    self.out,
                    "    if (mno_enum_tag({}) == {}LL) goto {}; else goto {};",
                    scrut, tag, ok, fail
                );
            }
            Pattern::VariantPayload(en, vn, binds) => {
                let tag = self.variant_tag(en, vn)?;
                let variants = self.enums.get(en.as_str()).cloned().unwrap_or_default();
                let variant = variants.iter().find(|v| v.name == *vn).copied();
                let Some(variant) = variant else {
                    return Err(e_native("unknown variant in pattern", Span::new(0, 0)));
                };
                let _ = writeln!(
                    self.out,
                    "    if (mno_enum_tag({}) != {}LL) goto {};",
                    scrut, tag, fail
                );
                for (i, bp) in binds.iter().enumerate() {
                    let pty = variant
                        .payloads
                        .get(i)
                        .cloned()
                        .unwrap_or(Type::Int);
                    let pv = self.fresh("pay");
                    if pty == Type::Float {
                        let bits = self.fresh("bits");
                        let _ = writeln!(
                            self.out,
                            "    mno_i64 {} = mno_enum_payload({}, {}LL);",
                            bits, scrut, i
                        );
                        let _ = writeln!(
                            self.out,
                            "    mno_f64 {} = mno_bits_to_f64({});",
                            pv, bits
                        );
                    } else {
                        let _ = writeln!(
                            self.out,
                            "    mno_i64 {} = mno_enum_payload({}, {}LL);",
                            pv, scrut, i
                        );
                    }
                    match bp {
                        Pattern::Var(name) => {
                            let pt = if pty == Type::Float {
                                "mno_f64"
                            } else {
                                "mno_i64"
                            };
                            let _ = writeln!(
                                self.out,
                                "    {} {} = {};",
                                pt,
                                c_ident(name),
                                pv
                            );
                            self.bind(name, pty);
                        }
                        Pattern::Wildcard => {}
                        _ => {
                            return Err(e_native(
                                "native backend: nested patterns in enum payloads not supported",
                                Span::new(0, 0),
                            ));
                        }
                    }
                }
                let _ = writeln!(self.out, "    goto {};", ok);
            }
        }
        Ok(())
    }

    fn variant_tag(&self, en: &str, vn: &str) -> Result<usize, Diagnostic> {
        let variants = self
            .enums
            .get(en)
            .ok_or_else(|| e_native("unknown enum in pattern", Span::new(0, 0)))?;
        variants
            .iter()
            .position(|v| v.name == vn)
            .ok_or_else(|| e_native("unknown variant", Span::new(0, 0)))
    }

    fn field_index(&self, sname: &str, field: &str, span: Span) -> Result<usize, Diagnostic> {
        let fields = self
            .structs
            .get(sname)
            .ok_or_else(|| e_native("unknown struct", span))?;
        fields
            .iter()
            .position(|f| f.name == field)
            .ok_or_else(|| e_native("unknown field", span))
    }

    fn field_type(&self, sname: &str, field: &str) -> Type {
        self.structs
            .get(sname)
            .and_then(|fs| fs.iter().find(|f| f.name == field))
            .map(|f| f.ty.clone())
            .unwrap_or(Type::Int)
    }

    fn type_of(&self, expr: &Expr) -> Type {
        match &expr.kind {
            ExprKind::Int(_) => Type::Int,
            ExprKind::Float(_) => Type::Float,
            ExprKind::Bool(_) => Type::Bool,
            ExprKind::Str(_) => Type::Str,
            ExprKind::Var(name) => {
                if let Some(t) = self.lookup_ty(name) {
                    return t;
                }
                if let Some(colon) = name.rfind("::") {
                    let en = &name[..colon];
                    if self.enums.contains_key(en) {
                        return Type::Enum(en.to_string());
                    }
                }
                if let Some((params, ret)) = self.signatures.get(name.as_str()) {
                    return Type::Fn(params.clone(), Box::new(ret.clone()));
                }
                Type::Int
            }
            ExprKind::Array(elems) => {
                let e = elems
                    .first()
                    .map(|e| self.type_of(e))
                    .unwrap_or(Type::Int);
                Type::Array(Box::new(e))
            }
            ExprKind::Index(base, _) => match self.type_of(base) {
                Type::Array(e) => *e,
                Type::Str => Type::Int,
                t => t,
            },
            ExprKind::Field(base, field) => match self.type_of(base) {
                Type::Struct(s) => self.field_type(&s, field),
                _ => Type::Int,
            },
            ExprKind::Bin(op, l, r) => {
                use BinOp::*;
                match op {
                    Eq | Ne | Lt | Le | Gt | Ge | And | Or => Type::Bool,
                    Add if matches!(self.type_of(l), Type::Str) => Type::Str,
                    _ => {
                        let lt = self.type_of(l);
                        if lt == Type::Float || self.type_of(r) == Type::Float {
                            Type::Float
                        } else {
                            Type::Int
                        }
                    }
                }
            }
            ExprKind::Un(UnOp::Not, _) => Type::Bool,
            ExprKind::Un(_, inner) => self.type_of(inner),
            ExprKind::Call(name, _, args) => {
                if name == "print" {
                    return Type::Unit;
                }
                if name == "len" || name == "char_at" || name == "to_int" || name == "hash" {
                    return Type::Int;
                }
                if name == "to_float" {
                    return Type::Float;
                }
                if matches!(
                    name.as_str(),
                    "spawn" | "join_int" | "join_bool" | "chan_new" | "chan_recv_int"
                        | "chan_recv_bool"
                ) {
                    return Type::Int;
                }
                if name == "join_float" || name == "chan_recv_float" {
                    return Type::Float;
                }
                if name == "join_str" || name == "chan_recv_str" {
                    return Type::Str;
                }
                if matches!(
                    name.as_str(),
                    "chan_close" | "chan_send_int" | "chan_send_bool" | "chan_send_float"
                        | "chan_send_str" | "print"
                ) {
                    return Type::Unit;
                }
                if name == "push" {
                    return self.type_of(&args[0]);
                }
                if name == "substr" || name == "chr" || name == "substr_cp" || name == "chr_cp"
                {
                    return Type::Str;
                }
                if let Some((_, ret)) = self.signatures.get(name.as_str()) {
                    return ret.clone();
                }
                if let Some(colon) = name.rfind("::") {
                    return Type::Enum(name[..colon].to_string());
                }
                if self.structs.contains_key(name as &str) {
                    return Type::Struct(name.to_string());
                }
                Type::Int
            }
            ExprKind::Lambda(l) => Type::Fn(
                l.params.iter().map(|p| p.ty.clone()).collect(),
                Box::new(l.ret.clone()),
            ),
            ExprKind::Match(m) => m
                .arms
                .first()
                .map(|a| self.type_of(&a.body))
                .unwrap_or(Type::Int),
        }
    }
}

fn e_native(msg: &str, span: Span) -> Diagnostic {
    Diagnostic::new("E080", msg, span)
        .with_help("the native Clang/LLVM backend does not support this construct yet; use machino run or machino build (WASM)")
}

/// Emit C source for a monomorphized program.
pub fn emit_c(program: &Program) -> Result<String, Diagnostic> {
    // Reject remaining generics
    for f in &program.functions {
        if !f.type_params.is_empty() && !f.is_extern {
            return Err(e_native(
                "internal: generic functions must be monomorphized before native codegen",
                f.span,
            ));
        }
    }
    let mut em = Emitter::new(program);
    em.emit_program()?;
    Ok(em.out)
}

fn runtime_dir() -> PathBuf {
    // Prefer next to the executable, then CARGO_MANIFEST_DIR, then cwd.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join("../runtime/native");
            if cand.join("machino_rt.c").is_file() {
                return cand.canonicalize().unwrap_or(cand);
            }
            let cand = dir.join("runtime/native");
            if cand.join("machino_rt.c").is_file() {
                return cand.canonicalize().unwrap_or(cand);
            }
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/native");
    if manifest.join("machino_rt.c").is_file() {
        return manifest;
    }
    PathBuf::from("runtime/native")
}

/// Compile a monomorphized program to a native executable via Clang/LLVM.
pub fn compile_native(
    program: &Program,
    out_exe: &Path,
    target: Option<&str>,
) -> Result<NativeBuild, Diagnostic> {
    let c_source = emit_c(program)?;
    let rt = runtime_dir();
    let rt_c = rt.join("machino_rt.c");
    let rt_h = rt.join("machino_rt.h");
    if !rt_c.is_file() || !rt_h.is_file() {
        return Err(Diagnostic::new(
            "E080",
            format!(
                "native runtime not found at {} (expected machino_rt.c/h)",
                rt.display()
            ),
            Span::new(0, 0),
        ));
    }

    let work = out_exe
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            ".machino-native-{}",
            out_exe
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("out")
        ));
    let _ = std::fs::create_dir_all(&work);
    let c_path = work.join("program.c");
    std::fs::write(&c_path, &c_source).map_err(|e| {
        Diagnostic::new(
            "E080",
            format!("cannot write '{}': {}", c_path.display(), e),
            Span::new(0, 0),
        )
    })?;
    // copy header next to program.c so #include "machino_rt.h" works
    let hdr_dst = work.join("machino_rt.h");
    let _ = std::fs::copy(&rt_h, &hdr_dst);

    let clang = std::env::var("MACHINO_CC").unwrap_or_else(|_| "clang".into());
    let mut compile_args = vec![
        "-O3".to_string(),
        "-flto".to_string(),
        "-ffunction-sections".to_string(),
        "-fdata-sections".to_string(),
        "-pthread".to_string(),
        "-std=c11".to_string(),
        "-o".to_string(),
        out_exe.to_str().unwrap_or("a.out").to_string(),
        c_path.to_str().unwrap_or("program.c").to_string(),
        rt_c.to_str().unwrap_or("machino_rt.c").to_string(),
    ];
    if let Some(triple) = target {
        compile_args.insert(0, "-target".to_string());
        compile_args.insert(1, triple.to_string());
    } else {
        // Host builds: tune for the CPU running the compiler.
        compile_args.insert(0, "-march=native".to_string());
    }
    // Dead-strip unused sections on common linkers (best-effort).
    if cfg!(target_os = "macos") {
        compile_args.push("-Wl,-dead_strip".to_string());
    } else if cfg!(target_os = "linux") {
        compile_args.push("-Wl,--gc-sections".to_string());
    }
    let status = Command::new(&clang)
        .args(&compile_args)
        .status()
        .map_err(|e| {
            Diagnostic::new(
                "E080",
                format!(
                    "failed to invoke '{}' ({}); install Clang/LLVM or set MACHINO_CC",
                    clang, e
                ),
                Span::new(0, 0),
            )
        })?;
    if !status.success() {
        return Err(Diagnostic::new(
            "E080",
            format!(
                "Clang/LLVM native link failed (exit {}); see compiler diagnostics above",
                status.code().unwrap_or(-1)
            ),
            Span::new(0, 0),
        ));
    }

    // Also emit LLVM IR for the program TU (inspection / tooling).
    let ll_path = work.join("program.ll");
    let ll_status = Command::new(&clang)
        .args([
            "-O3",
            "-flto",
            "-pthread",
            "-std=c11",
            "-S",
            "-emit-llvm",
            "-o",
            ll_path.to_str().unwrap_or("program.ll"),
            c_path.to_str().unwrap_or("program.c"),
            "-I",
            work.to_str().unwrap_or("."),
        ])
        .status();
    let ll_path = match ll_status {
        Ok(s) if s.success() => Some(ll_path),
        _ => None,
    };

    Ok(NativeBuild {
        exe_path: out_exe.to_path_buf(),
        c_path,
        ll_path,
    })
}

/// Build a macOS universal (arm64 + x86_64) binary via `lipo`.
pub fn compile_native_universal(
    program: &Program,
    out_exe: &Path,
) -> Result<NativeBuild, Diagnostic> {
    if !cfg!(target_os = "macos") {
        return Err(Diagnostic::new(
            "E080",
            "--universal is only supported when building on macOS (lipo)",
            Span::new(0, 0),
        )
        .with_help("on other hosts use --target <triple> for cross-compilation, or ship .wasm for portable deploy"));
    }
    let arm_out = out_exe.with_extension("native-arm64");
    let x64_out = out_exe.with_extension("native-x86_64");
    let arm = compile_native(program, &arm_out, Some("arm64-apple-macosx"))?;
    let x64_build = compile_native(program, &x64_out, Some("x86_64-apple-macosx")).map_err(|e| {
        e.with_help(
            "could not build the x86_64 slice; install an Xcode clang that supports -target x86_64-apple-macosx, or omit --universal",
        )
    })?;
    let status = Command::new("lipo")
        .args([
            "-create",
            "-output",
            out_exe.to_str().unwrap_or("a.out"),
            arm_out.to_str().unwrap_or("a.arm64"),
            x64_out.to_str().unwrap_or("a.x64"),
        ])
        .status()
        .map_err(|e| {
            Diagnostic::new(
                "E080",
                format!("lipo failed ({e}); --universal requires macOS lipo"),
                Span::new(0, 0),
            )
        })?;
    if !status.success() {
        return Err(Diagnostic::new(
            "E080",
            "lipo -create failed while building universal binary",
            Span::new(0, 0),
        ));
    }
    let _ = std::fs::remove_file(&arm_out);
    let _ = std::fs::remove_file(&x64_out);
    Ok(NativeBuild {
        exe_path: out_exe.to_path_buf(),
        c_path: arm.c_path,
        ll_path: arm.ll_path.or(x64_build.ll_path),
    })
}

fn collect_calls(stmts: &[Stmt], out: &mut HashSet<String>) {
    fn expr(e: &Expr, out: &mut HashSet<String>) {
        match &e.kind {
            ExprKind::Call(name, _, args) => {
                out.insert(name.clone());
                if name == "spawn" {
                    if let Some(ExprKind::Var(target)) = args.first().map(|a| &a.kind) {
                        out.insert(target.clone());
                    }
                }
                for a in args {
                    expr(a, out);
                }
            }
            ExprKind::Array(xs) => xs.iter().for_each(|x| expr(x, out)),
            ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
                expr(a, out);
                expr(b, out);
            }
            ExprKind::Field(a, _) | ExprKind::Un(_, a) => expr(a, out),
            ExprKind::Match(m) => {
                expr(&m.scrutinee, out);
                for arm in &m.arms {
                    expr(&arm.body, out);
                }
            }
            ExprKind::Lambda(l) => walk_stmts_calls(&l.body, out),
            // bare function name used as a value (e.g. map(xs, double))
            ExprKind::Var(name) => {
                out.insert(name.clone());
            }
            _ => {}
        }
    }
    fn walk_stmts_calls(stmts: &[Stmt], out: &mut HashSet<String>) {
        for s in stmts {
            match &s.kind {
                StmtKind::Let { value, .. }
                | StmtKind::Assign { value, .. }
                | StmtKind::Assert(value)
                | StmtKind::Expr(value)
                | StmtKind::Return(Some(value)) => expr(value, out),
                StmtKind::IndexAssign {
                    base, index, value, ..
                } => {
                    expr(base, out);
                    expr(index, out);
                    expr(value, out);
                }
                StmtKind::FieldAssign { base, value, .. } => {
                    expr(base, out);
                    expr(value, out);
                }
                StmtKind::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    expr(cond, out);
                    walk_stmts_calls(then_body, out);
                    walk_stmts_calls(else_body, out);
                }
                StmtKind::While {
                    cond,
                    invariant,
                    body,
                } => {
                    expr(cond, out);
                    if let Some(inv) = invariant {
                        expr(inv, out);
                    }
                    walk_stmts_calls(body, out);
                }
                StmtKind::For {
                    start, end, body, ..
                } => {
                    expr(start, out);
                    expr(end, out);
                    walk_stmts_calls(body, out);
                }
                _ => {}
            }
        }
    }
    walk_stmts_calls(stmts, out);
}

fn reachable_from_main(program: &Program) -> HashSet<String> {
    let mut reachable: HashSet<String> = HashSet::new();
    reachable.insert("main".into());
    let mut queue = vec!["main".to_string()];
    let by_name: HashMap<&str, &Function> = program
        .functions
        .iter()
        .map(|f| (f.name.as_str(), f))
        .collect();
    while let Some(name) = queue.pop() {
        let Some(f) = by_name.get(name.as_str()) else {
            continue;
        };
        let mut calls = HashSet::new();
        collect_calls(&f.body, &mut calls);
        for c in f.requires.iter().chain(f.ensures.iter()) {
            let wrapper = [Stmt {
                kind: StmtKind::Expr(c.expr.clone()),
                span: c.expr.span,
            }];
            collect_calls(&wrapper, &mut calls);
        }
        for c in calls {
            if reachable.insert(c.clone()) {
                queue.push(c);
            }
        }
    }
    reachable
}

fn lookup_env(env: &[HashMap<String, Type>], name: &str) -> Option<Type> {
    for scope in env.iter().rev() {
        if let Some(t) = scope.get(name) {
            return Some(t.clone());
        }
    }
    None
}

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

fn collect_spawn_targets_stmts(stmts: &[Stmt], out: &mut HashSet<String>) {
    for s in stmts {
        match &s.kind {
            StmtKind::Let { value, .. }
            | StmtKind::Assign { value, .. }
            | StmtKind::Assert(value)
            | StmtKind::Expr(value)
            | StmtKind::Return(Some(value)) => collect_spawn_targets_expr(value, out),
            StmtKind::IndexAssign { base, index, value } => {
                collect_spawn_targets_expr(base, out);
                collect_spawn_targets_expr(index, out);
                collect_spawn_targets_expr(value, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                collect_spawn_targets_expr(base, out);
                collect_spawn_targets_expr(value, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                collect_spawn_targets_expr(cond, out);
                collect_spawn_targets_stmts(then_body, out);
                collect_spawn_targets_stmts(else_body, out);
            }
            StmtKind::While { cond, invariant: _, body } => {
                collect_spawn_targets_expr(cond, out);
                collect_spawn_targets_stmts(body, out);
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                collect_spawn_targets_expr(start, out);
                collect_spawn_targets_expr(end, out);
                collect_spawn_targets_stmts(body, out);
            }
            _ => {}
        }
    }
}

fn collect_spawn_targets_expr(e: &Expr, out: &mut HashSet<String>) {
    match &e.kind {
        ExprKind::Call(name, _, args) if name == "spawn" => {
            if let Some(ExprKind::Var(target)) = args.first().map(|a| &a.kind) {
                out.insert(target.clone());
            }
            for a in args {
                collect_spawn_targets_expr(a, out);
            }
        }
        ExprKind::Array(elems) => elems.iter().for_each(|e| collect_spawn_targets_expr(e, out)),
        ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
            collect_spawn_targets_expr(a, out);
            collect_spawn_targets_expr(b, out);
        }
        ExprKind::Field(a, _) | ExprKind::Un(_, a) => collect_spawn_targets_expr(a, out),
        ExprKind::Call(_, _, args) => args.iter().for_each(|a| collect_spawn_targets_expr(a, out)),
        ExprKind::Lambda(l) => collect_spawn_targets_stmts(&l.body, out),
        ExprKind::Match(m) => {
            collect_spawn_targets_expr(&m.scrutinee, out);
            for arm in &m.arms {
                collect_spawn_targets_expr(&arm.body, out);
            }
        }
        _ => {}
    }
}

fn analyze_lambda_captures_stmts(
    stmts: &[Stmt],
    env: &mut Vec<HashMap<String, Type>>,
    signatures: &HashMap<&str, (Vec<Type>, Type)>,
    out: &mut HashMap<usize, Vec<(String, Type)>>,
) {
    env.push(HashMap::new());
    for s in stmts {
        match &s.kind {
            StmtKind::Let { name, ty, value } => {
                analyze_lambda_captures_expr(value, env, signatures, out);
                let inferred = ty.clone().unwrap_or(Type::Int);
                env.last_mut().unwrap().insert(name.clone(), inferred);
            }
            StmtKind::Assign { name, value } => {
                analyze_lambda_captures_expr(value, env, signatures, out);
                let _ = name;
            }
            StmtKind::IndexAssign { base, index, value } => {
                analyze_lambda_captures_expr(base, env, signatures, out);
                analyze_lambda_captures_expr(index, env, signatures, out);
                analyze_lambda_captures_expr(value, env, signatures, out);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                analyze_lambda_captures_expr(base, env, signatures, out);
                analyze_lambda_captures_expr(value, env, signatures, out);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                analyze_lambda_captures_expr(cond, env, signatures, out);
                analyze_lambda_captures_stmts(then_body, env, signatures, out);
                analyze_lambda_captures_stmts(else_body, env, signatures, out);
            }
            StmtKind::While {
                cond,
                invariant: _,
                body,
            } => {
                analyze_lambda_captures_expr(cond, env, signatures, out);
                analyze_lambda_captures_stmts(body, env, signatures, out);
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                analyze_lambda_captures_expr(start, env, signatures, out);
                analyze_lambda_captures_expr(end, env, signatures, out);
                env.push(HashMap::new());
                env.last_mut().unwrap().insert(var.clone(), Type::Int);
                analyze_lambda_captures_stmts(body, env, signatures, out);
                env.pop();
            }
            StmtKind::Assert(e) | StmtKind::Expr(e) | StmtKind::Return(Some(e)) => {
                analyze_lambda_captures_expr(e, env, signatures, out);
            }
            _ => {}
        }
    }
    env.pop();
}

fn analyze_lambda_captures_expr(
    e: &Expr,
    env: &[HashMap<String, Type>],
    signatures: &HashMap<&str, (Vec<Type>, Type)>,
    out: &mut HashMap<usize, Vec<(String, Type)>>,
) {
    match &e.kind {
        ExprKind::Lambda(l) => {
            let mut caps = Vec::new();
            for n in l.free_names() {
                if signatures.contains_key(n.as_str()) {
                    continue;
                }
                if let Some(ty) = lookup_env(env, &n) {
                    caps.push((n, ty));
                }
            }
            out.insert(l.id, caps);
            let mut inner = env.to_vec();
            inner.push(HashMap::new());
            for p in &l.params {
                inner.last_mut().unwrap().insert(p.name.clone(), p.ty.clone());
            }
            analyze_lambda_captures_stmts(&l.body, &mut inner, signatures, out);
        }
        ExprKind::Array(elems) => {
            for x in elems {
                analyze_lambda_captures_expr(x, env, signatures, out);
            }
        }
        ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
            analyze_lambda_captures_expr(a, env, signatures, out);
            analyze_lambda_captures_expr(b, env, signatures, out);
        }
        ExprKind::Field(a, _) | ExprKind::Un(_, a) => {
            analyze_lambda_captures_expr(a, env, signatures, out);
        }
        ExprKind::Call(_, _, args) => {
            for a in args {
                analyze_lambda_captures_expr(a, env, signatures, out);
            }
        }
        ExprKind::Match(m) => {
            analyze_lambda_captures_expr(&m.scrutinee, env, signatures, out);
            for arm in &m.arms {
                analyze_lambda_captures_expr(&arm.body, env, signatures, out);
            }
        }
        _ => {}
    }
}

/// Collect diagnostics for unsupported constructs in code reachable from main.
pub fn unsupported_constructs(program: &Program) -> Vec<Diagnostic> {
    let _ = program;
    Vec::new()
}
