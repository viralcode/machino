//! Monomorphization. The checker infers type arguments for every call to a
//! generic function/struct/enum and records them keyed by call span. This pass
//! stamps out a concrete copy of each generic item per distinct type-argument
//! list, rewrites the call sites (and enum variant names) to target the
//! concrete copies, and drops the generic templates. The result is a fully
//! concrete program that the interpreter and both WASM backends run unchanged.

use crate::ast::*;
use crate::checker::apply_subst;
use crate::diag::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

/// The checker's record of one generic call: (call span, callee, type args).
/// Type args recorded inside a generic body may themselves contain the
/// enclosing template's type variables; they are resolved when that body is
/// instantiated.
pub type Instantiation = (Span, String, Vec<Type>);

struct Mono<'a> {
    /// call span -> (callee, recorded type args)
    call_map: HashMap<(u32, u32), (String, Vec<Type>)>,
    fn_templates: HashMap<&'a str, &'a Function>,
    struct_templates: HashMap<&'a str, &'a StructDef>,
    enum_templates: HashMap<&'a str, &'a EnumDef>,
    /// (name, concrete type args) still to instantiate
    queue: Vec<(String, Vec<Type>)>,
    /// mangled names already emitted
    done: HashSet<String>,
    /// fresh lambda ids for instantiated bodies (ids must stay unique
    /// program-wide; clones of a template would otherwise collide)
    next_lambda_id: usize,
}

/// Canonical mangled name for one instantiation, e.g. `max2$int`.
pub fn mangle(name: &str, type_args: &[Type]) -> String {
    let mut out = String::from(name);
    for ta in type_args {
        out.push('$');
        mangle_type(ta, &mut out);
    }
    out
}

/// True if `ty` mentions any type variable (polymorphic / not yet concrete).
pub fn contains_typevar(ty: &Type) -> bool {
    match ty {
        Type::TypeVar(_) => true,
        Type::Array(inner) => contains_typevar(inner),
        Type::App(_, args) => args.iter().any(contains_typevar),
        Type::Fn(ps, r) => ps.iter().any(contains_typevar) || contains_typevar(r),
        _ => false,
    }
}

fn mangle_type(ty: &Type, out: &mut String) {
    match ty {
        Type::Int => out.push_str("int"),
        Type::Float => out.push_str("float"),
        Type::Bool => out.push_str("bool"),
        Type::Str => out.push_str("str"),
        Type::Unit => out.push_str("unit"),
        Type::Array(inner) => {
            out.push_str("arr_");
            mangle_type(inner, out);
        }
        Type::App(name, args) => {
            // Prefer the same mangling as a resolved instance.
            out.push_str(&mangle(name, args));
        }
        Type::Struct(n) | Type::Enum(n) | Type::TypeVar(n) => out.push_str(n),
        Type::Fn(params, ret) => {
            out.push_str("fn");
            for p in params {
                out.push('_');
                mangle_type(p, out);
            }
            out.push_str("_to_");
            mangle_type(ret, out);
        }
    }
}

/// Apply subst, then mangle any generic struct/enum whose type params are all
/// bound — so `HashMap` under `{K:str, V:int}` becomes `HashMap$str$int`.
fn apply_subst_full(
    ty: &Type,
    subst: &HashMap<String, Type>,
    struct_tps: &HashMap<&str, Vec<&str>>,
    enum_tps: &HashMap<&str, Vec<&str>>,
    queue: &mut Vec<(String, Vec<Type>)>,
) -> Type {
    match ty {
        Type::TypeVar(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Type::Array(inner) => Type::Array(Box::new(apply_subst_full(
            inner, subst, struct_tps, enum_tps, queue,
        ))),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter()
                .map(|p| apply_subst_full(p, subst, struct_tps, enum_tps, queue))
                .collect(),
            Box::new(apply_subst_full(r, subst, struct_tps, enum_tps, queue)),
        ),
        Type::Struct(name) => {
            if let Some(tps) = struct_tps.get(name.as_str()) {
                let mut args = Vec::new();
                for tp in tps {
                    match subst.get(*tp) {
                        Some(t) => args.push(apply_subst_full(
                            t, subst, struct_tps, enum_tps, queue,
                        )),
                        None => return Type::Struct(name.clone()),
                    }
                }
                let mangled = mangle(name, &args);
                if !args.iter().any(contains_typevar) {
                    queue.push((name.clone(), args));
                }
                Type::Struct(mangled)
            } else {
                Type::Struct(name.clone())
            }
        }
        Type::Enum(name) => {
            if let Some(tps) = enum_tps.get(name.as_str()) {
                let mut args = Vec::new();
                for tp in tps {
                    match subst.get(*tp) {
                        Some(t) => args.push(apply_subst_full(
                            t, subst, struct_tps, enum_tps, queue,
                        )),
                        None => return Type::Enum(name.clone()),
                    }
                }
                let mangled = mangle(name, &args);
                if !args.iter().any(contains_typevar) {
                    queue.push((name.clone(), args));
                }
                Type::Enum(mangled)
            } else {
                Type::Enum(name.clone())
            }
        }
        Type::App(name, args) => {
            let args: Vec<Type> = args
                .iter()
                .map(|a| apply_subst_full(a, subst, struct_tps, enum_tps, queue))
                .collect();
            let mangled = mangle(name, &args);
            if struct_tps.contains_key(name.as_str()) {
                if !args.iter().any(contains_typevar) {
                    queue.push((name.clone(), args));
                }
                Type::Struct(mangled)
            } else if enum_tps.contains_key(name.as_str()) {
                if !args.iter().any(contains_typevar) {
                    queue.push((name.clone(), args));
                }
                Type::Enum(mangled)
            } else {
                Type::App(name.clone(), args)
            }
        }
        _ => ty.clone(),
    }
}

/// Monomorphizes `program` given the checker-recorded instantiations.
pub fn monomorphize_with(
    program: &Program,
    instantiations: &[Instantiation],
) -> Result<Program, Diagnostic> {
    let call_map: HashMap<(u32, u32), (String, Vec<Type>)> = instantiations
        .iter()
        .map(|(sp, name, tas)| ((sp.start, sp.end), (name.clone(), tas.clone())))
        .collect();
    let fn_templates: HashMap<&str, &Function> = program
        .functions
        .iter()
        .filter(|f| !f.type_params.is_empty())
        .map(|f| (f.name.as_str(), f))
        .collect();
    let struct_templates: HashMap<&str, &StructDef> = program
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty())
        .map(|s| (s.name.as_str(), s))
        .collect();
    let enum_templates: HashMap<&str, &EnumDef> = program
        .enums
        .iter()
        .filter(|e| !e.type_params.is_empty())
        .map(|e| (e.name.as_str(), e))
        .collect();
    let next_lambda_id = max_lambda_id(program) + 1;

    let mut mono = Mono {
        call_map,
        fn_templates,
        struct_templates,
        enum_templates,
        queue: Vec::new(),
        done: HashSet::new(),
        next_lambda_id,
    };

    let mut out = Program {
        functions: Vec::new(),
        structs: program
            .structs
            .iter()
            .filter(|s| s.type_params.is_empty())
            .cloned()
            .collect(),
        enums: program
            .enums
            .iter()
            .filter(|e| e.type_params.is_empty())
            .cloned()
            .collect(),
        tests: Vec::new(),
        imports: program.imports.clone(),
    };

    // seed the queue from top-level instantiations (skip polymorphic ones
    // recorded while checking generic bodies — those still contain TypeVars)
    for (_, name, tas) in instantiations {
        if tas.iter().any(contains_typevar) {
            continue;
        }
        mono.queue.push((name.clone(), tas.clone()));
    }

    // rewrite the concrete world; generic calls found there seed the worklist
    let empty: HashMap<String, Type> = HashMap::new();
    for f in program.functions.iter().filter(|f| f.type_params.is_empty()) {
        let mut f2 = f.clone();
        mono.rewrite_function(&mut f2, &empty);
        out.functions.push(f2);
    }
    for t in &program.tests {
        let mut t2 = t.clone();
        for s in &mut t2.body {
            mono.rewrite_stmt(s, &empty);
        }
        out.tests.push(t2);
    }

    // instantiate templates transitively
    while let Some((name, type_args)) = mono.queue.pop() {
        let mangled = mangle(&name, &type_args);
        if !mono.done.insert(mangled.clone()) {
            continue;
        }
        if let Some(&template) = mono.fn_templates.get(name.as_str()) {
            let subst: HashMap<String, Type> = template
                .type_params
                .iter()
                .map(|tp| tp.name.clone())
                .zip(type_args.iter().cloned())
                .collect();
            let mut inst = template.clone();
            inst.name = mangled;
            inst.type_params = Vec::new();
            for p in &mut inst.params {
                p.ty = mono.subst_ty(&p.ty, &subst);
            }
            inst.ret = mono.subst_ty(&inst.ret, &subst);
            mono.rewrite_function(&mut inst, &subst);
            out.functions.push(inst);
        } else if let Some(&template) = mono.struct_templates.get(name.as_str()) {
            let subst: HashMap<String, Type> = template
                .type_params
                .iter()
                .map(|tp| tp.name.clone())
                .zip(type_args.iter().cloned())
                .collect();
            let mut inst = template.clone();
            inst.name = mangled;
            inst.type_params = Vec::new();
            for f in &mut inst.fields {
                f.ty = mono.subst_ty(&f.ty, &subst);
            }
            out.structs.push(inst);
        } else if let Some(&template) = mono.enum_templates.get(name.as_str()) {
            let subst: HashMap<String, Type> = template
                .type_params
                .iter()
                .map(|tp| tp.name.clone())
                .zip(type_args.iter().cloned())
                .collect();
            let mut inst = template.clone();
            inst.name = mangled;
            inst.type_params = Vec::new();
            for v in &mut inst.variants {
                if let Some(p) = &v.payload {
                    v.payload = Some(mono.subst_ty(p, &subst));
                }
            }
            out.enums.push(inst);
        } else {
            return Err(Diagnostic::new(
                "E064",
                format!("internal error: no generic template named '{}'", name),
                Span::new(0, 0),
            ));
        }
    }

    Ok(out)
}

fn max_lambda_id(program: &Program) -> usize {
    let mut max = 0usize;
    fn walk_stmts(stmts: &[Stmt], max: &mut usize) {
        for s in stmts {
            match &s.kind {
                StmtKind::Let { value, .. }
                | StmtKind::Assign { value, .. }
                | StmtKind::Assert(value)
                | StmtKind::Expr(value)
                | StmtKind::Return(Some(value)) => walk_expr(value, max),
                StmtKind::IndexAssign { base, index, value } => {
                    walk_expr(base, max);
                    walk_expr(index, max);
                    walk_expr(value, max);
                }
                StmtKind::FieldAssign { base, value, .. } => {
                    walk_expr(base, max);
                    walk_expr(value, max);
                }
                StmtKind::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    walk_expr(cond, max);
                    walk_stmts(then_body, max);
                    walk_stmts(else_body, max);
                }
                StmtKind::While { cond, body } => {
                    walk_expr(cond, max);
                    walk_stmts(body, max);
                }
                StmtKind::For {
                    start, end, body, ..
                } => {
                    walk_expr(start, max);
                    walk_expr(end, max);
                    walk_stmts(body, max);
                }
                _ => {}
            }
        }
    }
    fn walk_expr(e: &Expr, max: &mut usize) {
        match &e.kind {
            ExprKind::Array(elems) => elems.iter().for_each(|e| walk_expr(e, max)),
            ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
                walk_expr(a, max);
                walk_expr(b, max);
            }
            ExprKind::Field(a, _) | ExprKind::Un(_, a) => walk_expr(a, max),
            ExprKind::Call(_, args) => args.iter().for_each(|a| walk_expr(a, max)),
            ExprKind::Lambda(l) => {
                *max = (*max).max(l.id);
                walk_stmts(&l.body, max);
            }
            ExprKind::Match(m) => {
                walk_expr(&m.scrutinee, max);
                for arm in &m.arms {
                    walk_expr(&arm.body, max);
                }
            }
            _ => {}
        }
    }
    for f in &program.functions {
        walk_stmts(&f.body, &mut max);
        for c in f.requires.iter().chain(f.ensures.iter()) {
            walk_expr(&c.expr, &mut max);
        }
    }
    for t in &program.tests {
        walk_stmts(&t.body, &mut max);
    }
    max
}

impl<'a> Mono<'a> {
    fn struct_tps(&self) -> HashMap<&str, Vec<&str>> {
        self.struct_templates
            .iter()
            .map(|(n, s)| {
                (
                    *n,
                    s.type_params.iter().map(|tp| tp.name.as_str()).collect(),
                )
            })
            .collect()
    }

    fn enum_tps(&self) -> HashMap<&str, Vec<&str>> {
        self.enum_templates
            .iter()
            .map(|(n, e)| {
                (
                    *n,
                    e.type_params.iter().map(|tp| tp.name.as_str()).collect(),
                )
            })
            .collect()
    }

    fn subst_ty(&mut self, ty: &Type, subst: &HashMap<String, Type>) -> Type {
        let st = self.struct_tps();
        let et = self.enum_tps();
        let mut extra: Vec<(String, Vec<Type>)> = Vec::new();
        let out = apply_subst_full(ty, subst, &st, &et, &mut extra);
        self.queue.extend(extra);
        out
    }

    fn rewrite_function(&mut self, f: &mut Function, subst: &HashMap<String, Type>) {
        for c in f.requires.iter_mut().chain(f.ensures.iter_mut()) {
            self.rewrite_expr(&mut c.expr, subst);
        }
        for s in &mut f.body {
            self.rewrite_stmt(s, subst);
        }
    }

    fn rewrite_stmt(&mut self, stmt: &mut Stmt, subst: &HashMap<String, Type>) {
        match &mut stmt.kind {
            StmtKind::Let { ty, value, .. } => {
                if let Some(t) = ty {
                    *t = self.subst_ty(t, subst);
                }
                self.rewrite_expr(value, subst);
            }
            StmtKind::Assign { value, .. } => self.rewrite_expr(value, subst),
            StmtKind::IndexAssign { base, index, value } => {
                self.rewrite_expr(base, subst);
                self.rewrite_expr(index, subst);
                self.rewrite_expr(value, subst);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                self.rewrite_expr(base, subst);
                self.rewrite_expr(value, subst);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                self.rewrite_expr(cond, subst);
                for s in then_body {
                    self.rewrite_stmt(s, subst);
                }
                for s in else_body {
                    self.rewrite_stmt(s, subst);
                }
            }
            StmtKind::While { cond, body } => {
                self.rewrite_expr(cond, subst);
                for s in body {
                    self.rewrite_stmt(s, subst);
                }
            }
            StmtKind::For {
                start, end, body, ..
            } => {
                self.rewrite_expr(start, subst);
                self.rewrite_expr(end, subst);
                for s in body {
                    self.rewrite_stmt(s, subst);
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                self.rewrite_expr(e, subst)
            }
            _ => {}
        }
    }

    fn rewrite_pattern(&mut self, pat: &mut Pattern, subst: &HashMap<String, Type>) {
        match pat {
            Pattern::Variant(enum_name, _) | Pattern::VariantPayload(enum_name, _, _) => {
                if self.enum_templates.contains_key(enum_name.as_str()) {
                    // resolve type args from subst (enclosing generic) — the
                    // enum's type params must be bound in subst by name
                    if let Some(ed) = self.enum_templates.get(enum_name.as_str()) {
                        let mut args = Vec::new();
                        let mut ok = true;
                        for tp in &ed.type_params {
                            match subst.get(&tp.name) {
                                Some(t) => args.push(t.clone()),
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                        if ok {
                            let mangled = mangle(enum_name, &args);
                            self.queue.push((enum_name.clone(), args));
                            *enum_name = mangled;
                        }
                    }
                }
                if let Pattern::VariantPayload(_, _, inner) = pat {
                    self.rewrite_pattern(inner, subst);
                }
            }
            _ => {}
        }
    }

    fn rewrite_expr(&mut self, expr: &mut Expr, subst: &HashMap<String, Type>) {
        // rewrite children first, then this call site
        match &mut expr.kind {
            ExprKind::Array(elems) => {
                for e in elems {
                    self.rewrite_expr(e, subst);
                }
            }
            ExprKind::Index(a, b) | ExprKind::Bin(_, a, b) => {
                self.rewrite_expr(a, subst);
                self.rewrite_expr(b, subst);
            }
            ExprKind::Field(a, _) | ExprKind::Un(_, a) => self.rewrite_expr(a, subst),
            ExprKind::Call(_, args) => {
                for a in args {
                    self.rewrite_expr(a, subst);
                }
            }
            ExprKind::Lambda(l) => {
                l.id = self.next_lambda_id;
                self.next_lambda_id += 1;
                for p in &mut l.params {
                    p.ty = self.subst_ty(&p.ty, subst);
                }
                l.ret = self.subst_ty(&l.ret, subst);
                for s in &mut l.body {
                    self.rewrite_stmt(s, subst);
                }
            }
            ExprKind::Match(m) => {
                self.rewrite_expr(&mut m.scrutinee, subst);
                for arm in &mut m.arms {
                    self.rewrite_pattern(&mut arm.pattern, subst);
                    self.rewrite_expr(&mut arm.body, subst);
                }
            }
            ExprKind::Var(name) => {
                // payload-less generic enum variant: Option::None
                if let Some(colon) = name.rfind("::") {
                    let enum_name = &name[..colon];
                    let variant = &name[colon + 2..];
                    if self.enum_templates.contains_key(enum_name) {
                        if let Some(ed) = self.enum_templates.get(enum_name) {
                            let mut args = Vec::new();
                            let mut ok = true;
                            for tp in &ed.type_params {
                                match subst.get(&tp.name) {
                                    Some(t) => args.push(t.clone()),
                                    None => {
                                        ok = false;
                                        break;
                                    }
                                }
                            }
                            if ok {
                                let mangled = mangle(enum_name, &args);
                                self.queue.push((enum_name.to_string(), args));
                                *name = format!("{}::{}", mangled, variant);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        if let ExprKind::Call(name, _) = &mut expr.kind {
            if let Some((callee, recorded_args)) =
                self.call_map.get(&(expr.span.start, expr.span.end)).cloned()
            {
                let concrete: Vec<Type> = recorded_args
                    .iter()
                    .map(|t| apply_subst(t, subst))
                    .collect();
                if concrete.iter().any(contains_typevar) {
                    // still polymorphic — leave the call for a later concrete
                    // instantiation of this enclosing template
                } else if name == &callee {
                    *name = mangle(&callee, &concrete);
                    self.queue.push((callee, concrete));
                } else if let Some(variant) = name.strip_prefix(&format!("{}::", callee)) {
                    // Enum::Variant call recorded under the enum name
                    let mangled = mangle(&callee, &concrete);
                    *name = format!("{}::{}", mangled, variant);
                    self.queue.push((callee, concrete));
                }
            }
        }
    }
}
