//! Static type checker. Machino has no implicit conversions, no undefined
//! behavior, and no dynamic typing: everything an agent writes is either
//! provably well-typed or rejected with a coded, actionable diagnostic.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::mono::{contains_typevar, mangle};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Signature {
    pub params: Vec<Type>,
    pub ret: Type,
}

pub struct Checker<'a> {
    pub program: &'a Program,
    /// Full bundle source; used to quote operand text in machine-applicable fixes.
    pub source: &'a str,
    pub signatures: HashMap<String, Signature>,
    pub structs: HashMap<String, Vec<Param>>,
    pub enums: HashMap<String, Vec<EnumVariant>>,
    /// Type parameters in scope (for generic functions/structs).
    pub type_params: Vec<String>,
    /// Constraint bounds of the type parameters currently in scope.
    pub type_param_bounds: HashMap<String, Vec<String>>,
    type_params_stack: Vec<Vec<String>>,
    type_param_bounds_stack: Vec<HashMap<String, Vec<String>>>,
    /// Names of generic (templated) functions in the program.
    pub generic_fns: HashMap<String, usize>,
    /// Generic call instantiations discovered during checking:
    /// call span -> (function name, inferred type arguments).
    pub instantiations: std::cell::RefCell<Vec<(Span, String, Vec<Type>)>>,
    /// mangled struct/enum name -> (template name, type arguments)
    mangle_map: std::cell::RefCell<HashMap<String, (String, Vec<Type>)>>,
    diags: Vec<Diagnostic>,
    loop_depth: u32,
}

/// Levenshtein edit distance, used for did-you-mean suggestions.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Substitutes type variables in `ty` using `subst`. Shared with the
/// monomorphizer, which uses it to stamp out concrete instantiations.
pub fn apply_subst(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    match ty {
        Type::TypeVar(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Type::Array(inner) => Type::Array(Box::new(apply_subst(inner, subst))),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter().map(|p| apply_subst(p, subst)).collect(),
            Box::new(apply_subst(r, subst)),
        ),
        Type::App(name, args) => Type::App(
            name.clone(),
            args.iter().map(|a| apply_subst(a, subst)).collect(),
        ),
        _ => ty.clone(),
    }
}

/// One-way unification: matches a parameter type (which may contain the
/// substitutable type variables in `tps`) against a concrete argument type,
/// extending `subst`. Type variables NOT in `tps` are rigid (they belong to
/// an enclosing generic function) and only match themselves.
fn unify_ty(
    param: &Type,
    arg: &Type,
    tps: &[&str],
    subst: &mut HashMap<String, Type>,
    demangle: &HashMap<String, (String, Vec<Type>)>,
    struct_tps: &HashMap<String, Vec<String>>,
    enum_tps: &HashMap<String, Vec<String>>,
) -> bool {
    match (param, arg) {
        (Type::TypeVar(n), a) if tps.contains(&n.as_str()) => match subst.get(n) {
            Some(bound) => bound == a,
            None => {
                subst.insert(n.clone(), a.clone());
                true
            }
        },
        (Type::TypeVar(n), Type::TypeVar(m)) => n == m,
        (Type::Array(p), Type::Array(a)) => unify_ty(p, a, tps, subst, demangle, struct_tps, enum_tps),
        (Type::Fn(pp, pr), Type::Fn(ap, ar)) => {
            pp.len() == ap.len()
                && pp.iter().zip(ap).all(|(p, a)| {
                    unify_ty(p, a, tps, subst, demangle, struct_tps, enum_tps)
                })
                && unify_ty(pr, ar, tps, subst, demangle, struct_tps, enum_tps)
        }
        (Type::Struct(p), Type::Struct(a)) | (Type::Enum(p), Type::Enum(a)) => {
            if p == a {
                return true;
            }
            let nominal_tps = |name: &str| -> Option<&Vec<String>> {
                struct_tps.get(name).or_else(|| enum_tps.get(name))
            };
            let polymorphic_key = |template: &str, tp_list: &[String]| -> String {
                mangle(
                    template,
                    &tp_list
                        .iter()
                        .map(|n| Type::TypeVar(n.clone()))
                        .collect::<Vec<_>>(),
                )
            };
            if let Some((template, args)) = demangle.get(a) {
                if p == template || Some(p.as_str()) == Some(template.as_str()) {
                    if let Some(tp_list) = nominal_tps(template) {
                        if tp_list.len() == args.len() {
                            return tp_list.iter().zip(args).all(|(tp_name, arg_ty)| {
                                unify_ty(
                                    &Type::TypeVar(tp_name.clone()),
                                    arg_ty,
                                    tps,
                                    subst,
                                    demangle,
                                    struct_tps,
                                    enum_tps,
                                )
                            });
                        }
                    }
                }
            }
            for (template, tp_list) in struct_tps.iter().chain(enum_tps.iter()) {
                if tp_list.is_empty() {
                    continue;
                }
                let poly = polymorphic_key(template, tp_list);
                if p == &poly {
                    if let Some((template2, args)) = demangle.get(a) {
                        if template2 == template && tp_list.len() == args.len() {
                            return tp_list.iter().zip(args).all(|(tp_name, arg_ty)| {
                                unify_ty(
                                    &Type::TypeVar(tp_name.clone()),
                                    arg_ty,
                                    tps,
                                    subst,
                                    demangle,
                                    struct_tps,
                                    enum_tps,
                                )
                            });
                        }
                    }
                }
            }
            false
        }
        _ => param == arg,
    }
}

/// Returns the closest candidate within a sane edit distance, for suggestions.
fn closest<'x>(name: &str, candidates: impl Iterator<Item = &'x str>) -> Option<&'x str> {
    let mut best: Option<(&str, usize)> = None;
    for c in candidates {
        let d = edit_distance(name, c);
        if d <= 2.max(name.len() / 3) && best.map_or(true, |(_, bd)| d < bd) {
            best = Some((c, d));
        }
    }
    best.filter(|&(_, d)| d > 0).map(|(c, _)| c)
}

fn bound_help(bound: &str) -> &'static str {
    match bound {
        "Num" | "Ord" => "satisfied by: int, float",
        "Hash" => "satisfied by: int, bool, str (for hash())",
        _ => "satisfied by: int, float, bool, str",
    }
}

pub const BUILTINS: &[&str] = &[
    "print", "len", "push", "to_float", "to_int", "char_at", "substr", "chr",
    "len_cp", "char_at_cp", "substr_cp", "chr_cp", "hash",
    "chan_new", "chan_close",
    "chan_send_int", "chan_send_float", "chan_send_bool", "chan_send_str",
    "chan_recv_int", "chan_recv_float", "chan_recv_bool", "chan_recv_str",
    "spawn", "join_int", "join_float", "join_bool", "join_str",
];

struct Scope {
    vars: Vec<HashMap<String, Type>>,
    /// Frame indices where enclosing lambdas begin. Variables declared below
    /// the innermost boundary are captured (read-only inside the lambda).
    boundaries: Vec<usize>,
}

impl Scope {
    fn new() -> Self {
        Scope {
            vars: vec![HashMap::new()],
            boundaries: Vec::new(),
        }
    }
    fn push(&mut self) {
        self.vars.push(HashMap::new());
    }
    fn pop(&mut self) {
        self.vars.pop();
    }
    fn declare(&mut self, name: &str, ty: Type) -> bool {
        self.vars
            .last_mut()
            .unwrap()
            .insert(name.to_string(), ty)
            .is_none()
    }
    fn lookup(&self, name: &str) -> Option<&Type> {
        self.vars.iter().rev().find_map(|m| m.get(name))
    }
    /// All names currently in scope, for did-you-mean suggestions.
    fn all_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .vars
            .iter()
            .flat_map(|m| m.keys().cloned())
            .collect();
        names.sort();
        names.dedup();
        names
    }
    /// True if `name` resolves to a variable declared outside the innermost
    /// enclosing lambda (i.e. it would be captured by value).
    fn is_captured(&self, name: &str) -> bool {
        let Some(&boundary) = self.boundaries.last() else {
            return false;
        };
        for (i, frame) in self.vars.iter().enumerate().rev() {
            if frame.contains_key(name) {
                return i < boundary;
            }
        }
        false
    }
}

impl<'a> Checker<'a> {
    pub fn new(program: &'a Program, source: &'a str) -> Self {
        Checker {
            program,
            source,
            signatures: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            type_params: Vec::new(),
            type_param_bounds: HashMap::new(),
            type_params_stack: Vec::new(),
            type_param_bounds_stack: Vec::new(),
            generic_fns: HashMap::new(),
            instantiations: std::cell::RefCell::new(Vec::new()),
            mangle_map: std::cell::RefCell::new(HashMap::new()),
            diags: Vec::new(),
            loop_depth: 0,
        }
    }

    /// Puts a function/struct/enum's type parameters (and bounds) in scope.
    fn enter_type_params(&mut self, tps: &[TypeParam]) {
        self.type_params_stack.push(self.type_params.clone());
        self.type_param_bounds_stack
            .push(self.type_param_bounds.clone());
        self.type_params = tps.iter().map(|tp| tp.name.clone()).collect();
        self.type_param_bounds = tps
            .iter()
            .map(|tp| (tp.name.clone(), tp.bounds.clone()))
            .collect();
    }

    fn exit_type_params(&mut self) {
        self.type_params = self.type_params_stack.pop().unwrap_or_default();
        self.type_param_bounds = self
            .type_param_bounds_stack
            .pop()
            .unwrap_or_default();
    }

    fn struct_type_params(&self) -> HashMap<String, Vec<String>> {
        self.program
            .structs
            .iter()
            .filter(|s| !s.type_params.is_empty())
            .map(|s| {
                (
                    s.name.clone(),
                    s.type_params.iter().map(|tp| tp.name.clone()).collect(),
                )
            })
            .collect()
    }

    fn enum_type_params(&self) -> HashMap<String, Vec<String>> {
        self.program
            .enums
            .iter()
            .filter(|e| !e.type_params.is_empty())
            .map(|e| {
                (
                    e.name.clone(),
                    e.type_params.iter().map(|tp| tp.name.clone()).collect(),
                )
            })
            .collect()
    }

    fn unify(
        &self,
        param: &Type,
        arg: &Type,
        tps: &[&str],
        subst: &mut HashMap<String, Type>,
    ) -> bool {
        unify_ty(
            param,
            arg,
            tps,
            subst,
            &self.mangle_map.borrow(),
            &self.struct_type_params(),
            &self.enum_type_params(),
        )
    }

    fn expand_generic_nominal(&mut self, ty: &Type) -> Type {
        match ty {
            Type::App(name, args) => {
                let args: Vec<Type> = args
                    .iter()
                    .map(|a| self.expand_generic_nominal(a))
                    .collect();
                if self.program.structs.iter().any(|s| s.name == *name) {
                    self.ensure_struct_instance(name, &args);
                    Type::Struct(mangle(name, &args))
                } else if self.program.enums.iter().any(|e| e.name == *name) {
                    self.ensure_enum_instance(name, &args);
                    Type::Enum(mangle(name, &args))
                } else {
                    Type::App(name.clone(), args)
                }
            }
            Type::Struct(name) => {
                if let Some(sd) = self.program.structs.iter().find(|s| s.name == *name) {
                    if !sd.type_params.is_empty()
                        && sd
                            .type_params
                            .iter()
                            .all(|tp| self.type_params.contains(&tp.name))
                    {
                        let args: Vec<Type> = sd
                            .type_params
                            .iter()
                            .map(|tp| Type::TypeVar(tp.name.clone()))
                            .collect();
                        self.ensure_struct_instance(name, &args);
                        return Type::Struct(mangle(name, &args));
                    }
                }
                ty.clone()
            }
            Type::Enum(name) => {
                if let Some(ed) = self.program.enums.iter().find(|e| e.name == *name) {
                    if !ed.type_params.is_empty()
                        && ed
                            .type_params
                            .iter()
                            .all(|tp| self.type_params.contains(&tp.name))
                    {
                        let args: Vec<Type> = ed
                            .type_params
                            .iter()
                            .map(|tp| Type::TypeVar(tp.name.clone()))
                            .collect();
                        self.ensure_enum_instance(name, &args);
                        return Type::Enum(mangle(name, &args));
                    }
                }
                ty.clone()
            }
            Type::Array(inner) => Type::Array(Box::new(self.expand_generic_nominal(inner))),
            Type::Fn(ps, r) => Type::Fn(
                ps.iter().map(|p| self.expand_generic_nominal(p)).collect(),
                Box::new(self.expand_generic_nominal(r)),
            ),
            _ => ty.clone(),
        }
    }

    fn ensure_struct_instance(&mut self, template: &str, type_args: &[Type]) {
        let mangled = mangle(template, type_args);
        if self.structs.contains_key(&mangled) {
            self.mangle_map
                .borrow_mut()
                .entry(mangled)
                .or_insert((template.to_string(), type_args.to_vec()));
            return;
        }
        let Some(sd) = self.program.structs.iter().find(|s| s.name == template) else {
            return;
        };
        let subst: HashMap<String, Type> = sd
            .type_params
            .iter()
            .map(|tp| tp.name.clone())
            .zip(type_args.iter().cloned())
            .collect();
        let fields: Vec<Param> = sd
            .fields
            .iter()
            .map(|f| Param {
                name: f.name.clone(),
                ty: apply_subst(&f.ty, &subst),
                span: f.span,
            })
            .collect();
        self.structs.insert(mangled.clone(), fields);
        self.mangle_map
            .borrow_mut()
            .insert(mangled, (template.to_string(), type_args.to_vec()));
    }

    fn ensure_enum_instance(&mut self, template: &str, type_args: &[Type]) {
        let mangled = mangle(template, type_args);
        if self.enums.contains_key(&mangled) {
            self.mangle_map
                .borrow_mut()
                .entry(mangled)
                .or_insert((template.to_string(), type_args.to_vec()));
            return;
        }
        let Some(ed) = self.program.enums.iter().find(|e| e.name == template) else {
            return;
        };
        let subst: HashMap<String, Type> = ed
            .type_params
            .iter()
            .map(|tp| tp.name.clone())
            .zip(type_args.iter().cloned())
            .collect();
        let variants: Vec<EnumVariant> = ed
            .variants
            .iter()
            .map(|v| EnumVariant {
                name: v.name.clone(),
                payloads: v
                    .payloads
                    .iter()
                    .map(|p| apply_subst(p, &subst))
                    .collect(),
                span: v.span,
            })
            .collect();
        self.enums.insert(mangled.clone(), variants);
        self.mangle_map
            .borrow_mut()
            .insert(mangled, (template.to_string(), type_args.to_vec()));
    }

    fn subst_type(&mut self, ty: &Type, subst: &HashMap<String, Type>) -> Type {
        match ty {
            Type::TypeVar(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
            Type::Array(inner) => Type::Array(Box::new(self.subst_type(inner, subst))),
            Type::Fn(ps, r) => Type::Fn(
                ps.iter().map(|p| self.subst_type(p, subst)).collect(),
                Box::new(self.subst_type(r, subst)),
            ),
            Type::Struct(name) => {
                if let Some(tp_list) = self.struct_type_params().get(name) {
                    let mut args = Vec::new();
                    for tp in tp_list {
                        match subst.get(tp) {
                            Some(t) => args.push(self.subst_type(t, subst)),
                            None => return Type::Struct(name.clone()),
                        }
                    }
                    let mangled = mangle(name, &args);
                    if args.iter().any(contains_typevar) {
                        return Type::Struct(mangled);
                    }
                    self.ensure_struct_instance(name, &args);
                    Type::Struct(mangled)
                } else {
                    Type::Struct(name.clone())
                }
            }
            Type::Enum(name) => {
                if let Some(tp_list) = self.enum_type_params().get(name) {
                    let mut args = Vec::new();
                    for tp in tp_list {
                        match subst.get(tp) {
                            Some(t) => args.push(self.subst_type(t, subst)),
                            None => return Type::Enum(name.clone()),
                        }
                    }
                    let mangled = mangle(name, &args);
                    if args.iter().any(contains_typevar) {
                        return Type::Enum(mangled);
                    }
                    self.ensure_enum_instance(name, &args);
                    Type::Enum(mangled)
                } else {
                    Type::Enum(name.clone())
                }
            }
            _ => ty.clone(),
        }
    }

    fn type_args_from_expected(&self, enum_name: &str, expected: &Type) -> Option<Vec<Type>> {
        if let Type::Enum(en) = expected {
            if let Some((template, args)) = self.mangle_map.borrow().get(en).cloned() {
                if template == enum_name && !args.iter().any(contains_typevar) {
                    return Some(args);
                }
            }
        }
        None
    }

    fn check_struct_ctor(
        &mut self,
        name: &str,
        sd: &StructDef,
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
    ) -> Option<Type> {
        if args.len() != sd.fields.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "'{}' takes {} field argument(s), found {}",
                    name,
                    sd.fields.len(),
                    args.len()
                ),
                span,
            ));
            return None;
        }
        self.enter_type_params(&sd.type_params);
        let tp_names: Vec<&str> = sd.type_params.iter().map(|tp| tp.name.as_str()).collect();
        let mut subst: HashMap<String, Type> = HashMap::new();
        let mut ok = true;
        for (field, arg) in sd.fields.iter().zip(args) {
            let Some(aty) = self.infer(arg, scope, None) else {
                ok = false;
                continue;
            };
            let aty = self.normalize_type(&aty);
            let fty = self.expand_generic_nominal(&self.normalize_type(&field.ty));
            if !self.unify(&fty, &aty, &tp_names, &mut subst) {
                self.diags.push(Diagnostic::new(
                    "E030",
                    format!(
                        "type mismatch: field '{}' of '{}' expects '{}', found '{}'",
                        field.name,
                        name,
                        apply_subst(&fty, &subst),
                        aty
                    ),
                    arg.span,
                ));
                ok = false;
            }
        }
        self.exit_type_params();
        if !ok {
            return None;
        }
        let mut type_args = Vec::new();
        for tp in &sd.type_params {
            match subst.get(&tp.name) {
                Some(t) => type_args.push(t.clone()),
                None => {
                    self.diags.push(
                        Diagnostic::new(
                            "E064",
                            format!(
                                "cannot infer type parameter '{}' of '{}' from the arguments",
                                tp.name, name
                            ),
                            span,
                        )
                        .with_help(
                            "every type parameter must appear in at least one field type",
                        ),
                    );
                    return None;
                }
            }
        }
        for (tp, ta) in sd.type_params.iter().zip(type_args.iter()) {
            for b in &tp.bounds {
                if !self.satisfies_bound(ta, b) {
                    self.diags.push(
                        Diagnostic::new(
                            "E069",
                            format!(
                                "type '{}' does not satisfy the constraint '{}: {}' of '{}'",
                                ta, tp.name, b, name
                            ),
                            span,
                        )
                        .with_help(bound_help(b)),
                    );
                    return None;
                }
            }
        }
        self.ensure_struct_instance(name, &type_args);
        if !type_args.iter().any(contains_typevar) {
            self.instantiations
                .borrow_mut()
                .push((span, name.to_string(), type_args.clone()));
        }
        Some(Type::Struct(mangle(name, &type_args)))
    }

    fn check_enum_variant_call(
        &mut self,
        enum_name: &str,
        variant_name: &str,
        ed: &EnumDef,
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
        expected: Option<&Type>,
    ) -> Option<Type> {
        let Some(variant) = ed.variants.iter().find(|v| v.name == variant_name) else {
            self.diags.push(Diagnostic::new(
                "E060",
                format!("enum '{}' has no variant '{}'", enum_name, variant_name),
                span,
            ));
            return None;
        };
        if ed.type_params.is_empty() {
            let n = variant.payloads.len();
            if args.len() != n {
                self.diags.push(Diagnostic::new(
                    "E045",
                    format!(
                        "{}::{} takes exactly {} argument(s), found {}",
                        enum_name,
                        variant_name,
                        n,
                        args.len()
                    ),
                    span,
                ));
            } else if n > 0 {
                self.check_args(
                    &format!("{}::{}", enum_name, variant_name),
                    &variant.payloads,
                    args,
                    span,
                    scope,
                );
            }
            return Some(Type::Enum(enum_name.to_string()));
        }

        self.enter_type_params(&ed.type_params);
        let tp_names: Vec<&str> = ed.type_params.iter().map(|tp| tp.name.as_str()).collect();
        let mut subst: HashMap<String, Type> = HashMap::new();
        let mut ok = true;

        let n = variant.payloads.len();
        if args.len() != n {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "{}::{} takes exactly {} argument(s), found {}",
                    enum_name,
                    variant_name,
                    n,
                    args.len()
                ),
                span,
            ));
            self.exit_type_params();
            return None;
        }
        for (payload_ty, arg) in variant.payloads.iter().zip(args) {
            let Some(aty) = self.infer(arg, scope, None) else {
                self.exit_type_params();
                return None;
            };
            let aty = self.normalize_type(&aty);
            let pty = self.expand_generic_nominal(&self.normalize_type(payload_ty));
            if !self.unify(&pty, &aty, &tp_names, &mut subst) {
                self.diags.push(Diagnostic::new(
                    "E030",
                    format!(
                        "type mismatch: {}::{} expects payload '{}', found '{}'",
                        enum_name,
                        variant_name,
                        apply_subst(&pty, &subst),
                        aty
                    ),
                    arg.span,
                ));
                ok = false;
            }
        }
        if n == 0 {
            if let Some(exp) = expected {
                if let Some(args_from_exp) = self.type_args_from_expected(enum_name, exp) {
                    for (tp, ta) in ed.type_params.iter().zip(args_from_exp.iter()) {
                        subst.insert(tp.name.clone(), ta.clone());
                    }
                }
            }
        }

        self.exit_type_params();
        if !ok {
            return None;
        }

        let mut type_args = Vec::new();
        for tp in &ed.type_params {
            match subst.get(&tp.name) {
                Some(t) => type_args.push(t.clone()),
                None => {
                    self.diags.push(
                        Diagnostic::new(
                            "E064",
                            format!(
                                "cannot infer type parameter '{}' of '{}::{}'",
                                tp.name, enum_name, variant_name
                            ),
                            span,
                        )
                        .with_help(
                            "provide a payload, or annotate the expected enum type",
                        ),
                    );
                    return None;
                }
            }
        }
        for (tp, ta) in ed.type_params.iter().zip(type_args.iter()) {
            for b in &tp.bounds {
                if !self.satisfies_bound(ta, b) {
                    self.diags.push(
                        Diagnostic::new(
                            "E069",
                            format!(
                                "type '{}' does not satisfy the constraint '{}: {}' of '{}'",
                                ta, tp.name, b, enum_name
                            ),
                            span,
                        )
                        .with_help(bound_help(b)),
                    );
                    return None;
                }
            }
        }
        self.ensure_enum_instance(enum_name, &type_args);
        if !type_args.iter().any(contains_typevar) {
            self.instantiations
                .borrow_mut()
                .push((span, enum_name.to_string(), type_args.clone()));
        }
        Some(Type::Enum(mangle(enum_name, &type_args)))
    }

    /// Does `ty` satisfy the named constraint bound?
    fn satisfies_bound(&self, ty: &Type, bound: &str) -> bool {
        // a type variable satisfies a bound if it declares an implying bound
        if let Type::TypeVar(n) = ty {
            let bounds = self.type_param_bounds.get(n).cloned().unwrap_or_default();
            return match bound {
                "Eq" => bounds.iter().any(|b| b == "Eq" || b == "Ord"),
                other => bounds.iter().any(|b| b == other),
            };
        }
        match bound {
            "Eq" => matches!(ty, Type::Int | Type::Float | Type::Bool | Type::Str),
            "Ord" => matches!(ty, Type::Int | Type::Float),
            "Num" => matches!(ty, Type::Int | Type::Float),
            "Hash" => matches!(ty, Type::Int | Type::Bool | Type::Str),
            _ => false,
        }
    }

    /// True if the type parameter (by name) declares a bound implying `bound`.
    fn typevar_allows(&self, name: &str, bound: &str) -> bool {
        self.satisfies_bound(&Type::TypeVar(name.to_string()), bound)
    }

    /// Source text under a span (for quoting operands in fixes).
    fn snippet(&self, span: Span) -> &str {
        let start = (span.start as usize).min(self.source.len());
        let end = (span.end as usize).min(self.source.len());
        &self.source[start..end]
    }

    pub fn check(mut self) -> Result<Vec<(Span, String, Vec<Type>)>, Vec<Diagnostic>> {
        // pass 0: collect structs
        for s in &self.program.structs {
            if BUILTINS.contains(&s.name.as_str()) || s.name == "result" || s.name == "memory" {
                self.diags.push(Diagnostic::new(
                    "E020",
                    format!("'{}' is a reserved name and cannot be used for a struct", s.name),
                    s.span,
                ));
                continue;
            }
            if self.structs.contains_key(&s.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("struct '{}' is defined more than once", s.name),
                    s.span,
                ));
                continue;
            }
            let mut seen: HashMap<&str, ()> = HashMap::new();
            for f in &s.fields {
                if seen.insert(&f.name, ()).is_some() {
                    self.diags.push(Diagnostic::new(
                        "E023",
                        format!("duplicate field '{}' in struct '{}'", f.name, s.name),
                        f.span,
                    ));
                }
            }
            self.structs.insert(s.name.clone(), s.fields.clone());
        }

        // pass 0b: collect enums
        for e in &self.program.enums {
            if BUILTINS.contains(&e.name.as_str()) || e.name == "result" || e.name == "memory" {
                self.diags.push(Diagnostic::new(
                    "E020",
                    format!("'{}' is a reserved name and cannot be used for an enum", e.name),
                    e.span,
                ));
                continue;
            }
            if self.structs.contains_key(&e.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("'{}' is already the name of a struct", e.name),
                    e.span,
                ));
                continue;
            }
            if self.enums.contains_key(&e.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("enum '{}' is defined more than once", e.name),
                    e.span,
                ));
                continue;
            }
            if e.variants.len() > 65535 {
                self.diags.push(
                    Diagnostic::new(
                        "E054",
                        format!(
                            "enum '{}' has {} variants; the maximum is 65535",
                            e.name,
                            e.variants.len()
                        ),
                        e.span,
                    )
                    .with_help("split large enums or use a different data structure"),
                );
                continue;
            }
            let mut seen: HashMap<&str, ()> = HashMap::new();
            for v in &e.variants {
                if seen.insert(&v.name, ()).is_some() {
                    self.diags.push(Diagnostic::new(
                        "E055",
                        format!("duplicate variant '{}' in enum '{}'", v.name, e.name),
                        v.span,
                    ));
                }
            }
            self.enums.insert(e.name.clone(), e.variants.clone());
        }
        // validate field and payload types now that ALL type names (structs
        // and enums, in any declaration order) are known
        for s in &self.program.structs {
            self.enter_type_params(&s.type_params);
            for f in &s.fields {
                self.validate_type(&f.ty, f.span);
            }
            self.exit_type_params();
        }
        for e in &self.program.enums {
            self.enter_type_params(&e.type_params);
            for v in &e.variants {
                for ty in &v.payloads {
                    self.validate_type(ty, v.span);
                }
            }
            self.exit_type_params();
        }

        // pass 1: collect function signatures
        let mut first_def: HashMap<String, (Span, bool)> = HashMap::new();
        for f in &self.program.functions {
            if BUILTINS.contains(&f.name.as_str()) {
                self.diags.push(
                    Diagnostic::new(
                        "E020",
                        format!("'{}' is a builtin function and cannot be redefined", f.name),
                        f.span,
                    )
                    .with_help(format!("builtins: {}", BUILTINS.join(", "))),
                );
                continue;
            }
            if f.name == "memory" || f.name == "result" {
                self.diags.push(
                    Diagnostic::new(
                        "E020",
                        format!("'{}' is a reserved name and cannot be used for a function", f.name),
                        f.span,
                    )
                    .with_help("'memory' is the WebAssembly memory export; 'result' is bound in ensures clauses"),
                );
                continue;
            }
            if self.structs.contains_key(&f.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("'{}' is already the name of a struct", f.name),
                    f.span,
                ));
                continue;
            }
            if self.enums.contains_key(&f.name) {
                self.diags.push(Diagnostic::new(
                    "E021",
                    format!("'{}' is already the name of an enum", f.name),
                    f.span,
                ));
                continue;
            }
            if let Some((orig_span, orig_is_std)) = first_def.get(&f.name).cloned() {
                // report std collisions at the user's definition, not inside the prelude
                if f.is_std && !orig_is_std {
                    self.diags.push(
                        Diagnostic::new(
                            "E021",
                            format!(
                                "'{}' is a machino standard library function and cannot be redefined",
                                f.name
                            ),
                            orig_span,
                        )
                        .with_help("pick a different name; std names are listed in docs/agent-guide.md"),
                    );
                } else {
                    self.diags.push(Diagnostic::new(
                        "E021",
                        format!("function '{}' is defined more than once", f.name),
                        f.span,
                    ));
                }
                continue;
            }
            first_def.insert(f.name.clone(), (f.span, f.is_std));
            if !f.type_params.is_empty() {
                if f.is_extern {
                    self.diags.push(Diagnostic::new(
                        "E068",
                        format!("extern function '{}' cannot be generic", f.name),
                        f.span,
                    ));
                    continue;
                }
                let idx = self
                    .program
                    .functions
                    .iter()
                    .position(|g| std::ptr::eq(g, f))
                    .unwrap();
                self.generic_fns.insert(f.name.clone(), idx);
            }
            self.enter_type_params(&f.type_params);
            let params: Vec<Type> = f
                .params
                .iter()
                .map(|p| {
                    self.validate_type(&p.ty, p.span);
                    self.concretize_type(&p.ty, p.span)
                })
                .collect();
            self.validate_type(&f.ret, f.span);
            let ret = self.concretize_type(&f.ret, f.span);
            self.exit_type_params();
            if f.is_extern {
                self.validate_extern_types(f);
            }
            self.signatures.insert(
                f.name.clone(),
                Signature { params, ret },
            );
        }

        // pass 2: check bodies, contracts, tests
        for f in &self.program.functions {
            self.check_function(f);
        }
        let mut test_names: HashMap<&str, ()> = HashMap::new();
        for t in &self.program.tests {
            if test_names.insert(&t.name, ()).is_some() {
                self.diags.push(Diagnostic::new(
                    "E022",
                    format!("duplicate test name \"{}\"", t.name),
                    t.span,
                ));
            }
            let mut scope = Scope::new();
            self.check_stmts(&t.body, &mut scope, &Type::Unit, true);
        }

        if self.diags.is_empty() {
            Ok(self.instantiations.into_inner())
        } else {
            Err(self.diags)
        }
    }

    /// Resolve `Name<T,...>` applications to mangled struct/enum types and
    /// ensure the concrete instantiation exists. Other types are normalized.
    fn concretize_type(&mut self, ty: &Type, span: Span) -> Type {
        match ty {
            Type::App(name, args) => {
                let args: Vec<Type> = args
                    .iter()
                    .map(|a| self.concretize_type(a, span))
                    .collect();
                for a in &args {
                    self.validate_type(a, span);
                }
                if let Some(sd) = self.program.structs.iter().find(|s| s.name == *name) {
                    if sd.type_params.is_empty() {
                        self.diags.push(
                            Diagnostic::new(
                                "E018",
                                format!("type '{}' is not generic", name),
                                span,
                            )
                            .with_help("omit the type arguments, or declare type parameters on the struct"),
                        );
                        return Type::Struct(name.clone());
                    }
                    if sd.type_params.len() != args.len() {
                        self.diags.push(Diagnostic::new(
                            "E018",
                            format!(
                                "type '{}' expects {} type argument(s), found {}",
                                name,
                                sd.type_params.len(),
                                args.len()
                            ),
                            span,
                        ));
                        return Type::App(name.clone(), args);
                    }
                    for (tp, ta) in sd.type_params.iter().zip(args.iter()) {
                        for b in &tp.bounds {
                            if !self.satisfies_bound(ta, b) {
                                self.diags.push(
                                    Diagnostic::new(
                                        "E069",
                                        format!(
                                            "type '{}' does not satisfy bound '{}' required by '{}<...>'",
                                            ta, b, name
                                        ),
                                        span,
                                    )
                                    .with_help(bound_help(b)),
                                );
                            }
                        }
                    }
                    self.ensure_struct_instance(name, &args);
                    return Type::Struct(mangle(name, &args));
                }
                if let Some(ed) = self.program.enums.iter().find(|e| e.name == *name) {
                    if ed.type_params.is_empty() {
                        self.diags.push(
                            Diagnostic::new(
                                "E018",
                                format!("type '{}' is not generic", name),
                                span,
                            )
                            .with_help("omit the type arguments, or declare type parameters on the enum"),
                        );
                        return Type::Enum(name.clone());
                    }
                    if ed.type_params.len() != args.len() {
                        self.diags.push(Diagnostic::new(
                            "E018",
                            format!(
                                "type '{}' expects {} type argument(s), found {}",
                                name,
                                ed.type_params.len(),
                                args.len()
                            ),
                            span,
                        ));
                        return Type::App(name.clone(), args);
                    }
                    for (tp, ta) in ed.type_params.iter().zip(args.iter()) {
                        for b in &tp.bounds {
                            if !self.satisfies_bound(ta, b) {
                                self.diags.push(
                                    Diagnostic::new(
                                        "E069",
                                        format!(
                                            "type '{}' does not satisfy bound '{}' required by '{}<...>'",
                                            ta, b, name
                                        ),
                                        span,
                                    )
                                    .with_help(bound_help(b)),
                                );
                            }
                        }
                    }
                    self.ensure_enum_instance(name, &args);
                    return Type::Enum(mangle(name, &args));
                }
                self.diags.push(
                    Diagnostic::new("E018", format!("unknown type '{}'", name), span).with_help(
                        "valid types: int, float, bool, str, [T], fn(T...) -> R, Name<T,...>, or a declared struct/enum",
                    ),
                );
                Type::App(name.clone(), args)
            }
            Type::Array(inner) => Type::Array(Box::new(self.concretize_type(inner, span))),
            Type::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(|p| self.concretize_type(p, span))
                    .collect(),
                Box::new(self.concretize_type(ret, span)),
            ),
            other => self.normalize_type(other),
        }
    }

    fn validate_type(&mut self, ty: &Type, span: Span) {
        match ty {
            Type::TypeVar(name) => {
                // Type variables must be declared in the current scope
                if !self.type_params.contains(name) {
                    self.diags.push(
                        Diagnostic::new("E018", format!("unknown type parameter '{}'", name), span)
                            .with_help("type parameters must be declared in the function/struct/enum signature"),
                    );
                }
            }
            Type::App(_, _) => {
                let _ = self.concretize_type(ty, span);
            }
            Type::Struct(name) => {
                if !self.structs.contains_key(name)
                    && !self.enums.contains_key(name)
                    && !self.mangle_map.borrow().contains_key(name)
                {
                    // Bare generic template names are allowed in signatures of
                    // generic items; applications must use Name<T,...>.
                    let is_generic_template = self
                        .program
                        .structs
                        .iter()
                        .any(|s| s.name == *name && !s.type_params.is_empty())
                        || self
                            .program
                            .enums
                            .iter()
                            .any(|e| e.name == *name && !e.type_params.is_empty());
                    if !is_generic_template {
                        self.diags.push(
                            Diagnostic::new("E018", format!("unknown type '{}'", name), span).with_help(
                                "valid types: int, float, bool, str, [T], fn(T...) -> R, Name<T,...>, or a declared struct/enum",
                            ),
                        );
                    }
                }
            }
            Type::Enum(name) => {
                if !self.enums.contains_key(name) && !self.mangle_map.borrow().contains_key(name) {
                    self.diags.push(
                        Diagnostic::new("E018", format!("unknown enum '{}'", name), span).with_help(
                            "valid types: int, float, bool, str, [T], fn(T...) -> R, Name<T,...>, or a declared struct/enum",
                        ),
                    );
                }
            }
            Type::Array(inner) => self.validate_type(inner, span),
            Type::Fn(params, ret) => {
                for p in params {
                    self.validate_type(p, span);
                }
                self.validate_type(ret, span);
            }
            _ => {}
        }
    }

    fn validate_extern_types(&mut self, f: &Function) {
        let ok = |t: &Type| {
            matches!(
                t,
                Type::Int | Type::Float | Type::Bool | Type::Str | Type::Unit
            ) || matches!(t, Type::Array(inner) if matches!(**inner, Type::Int | Type::Float | Type::Bool | Type::Str))
        };
        for p in &f.params {
            if !ok(&p.ty) {
                self.diags.push(
                    Diagnostic::new(
                        "E026",
                        format!("extern parameter type '{}' is not host-transferable", p.ty),
                        p.span,
                    )
                    .with_help("extern signatures may use int, float, bool, str, and arrays of those"),
                );
            }
        }
        if !ok(&f.ret) {
            self.diags.push(Diagnostic::new(
                "E026",
                format!("extern return type '{}' is not host-transferable", f.ret),
                f.span,
            ));
        }
    }

    fn check_function(&mut self, f: &Function) {
        // generic bodies are checked polymorphically: T is a rigid type that
        // supports only what its constraint bounds allow
        self.enter_type_params(&f.type_params);
        self.check_function_inner(f);
        self.exit_type_params();
    }

    fn check_function_inner(&mut self, f: &Function) {
        let mut scope = Scope::new();
        for p in &f.params {
            let pty = self.expand_generic_nominal(&p.ty);
            if !scope.declare(&p.name, pty) {
                self.diags.push(Diagnostic::new(
                    "E023",
                    format!("duplicate parameter name '{}'", p.name),
                    p.span,
                ));
            }
        }

        for c in &f.requires {
            let ty = self.infer(&c.expr, &mut scope, None);
            self.expect_bool(ty, c.expr.span, "requires clause");
        }
        {
            // 'result' is in scope for ensures clauses
            let mut ens_scope = Scope::new();
            for p in &f.params {
                ens_scope.declare(&p.name, self.expand_generic_nominal(&p.ty));
            }
            if f.ret != Type::Unit {
                ens_scope.declare("result", self.expand_generic_nominal(&f.ret));
            } else if !f.ensures.is_empty() {
                self.diags.push(
                    Diagnostic::new(
                        "E024",
                        format!(
                            "function '{}' has an ensures clause but returns nothing",
                            f.name
                        ),
                        f.span,
                    )
                    .with_help("ensures constrains 'result'; add a return type or remove it"),
                );
            }
            for c in &f.ensures {
                let ty = self.infer(&c.expr, &mut ens_scope, None);
                self.expect_bool(ty, c.expr.span, "ensures clause");
            }
        }

        if f.is_extern {
            return;
        }

        let body_ret = self.expand_generic_nominal(&f.ret);
        self.check_stmts(&f.body, &mut scope, &body_ret, false);

        if f.ret != Type::Unit && !always_returns(&f.body) {
            self.diags.push(
                Diagnostic::new(
                    "E025",
                    format!(
                        "function '{}' returns '{}' but not all paths return a value",
                        f.name, f.ret
                    ),
                    f.span,
                )
                .with_help("add a 'return <expr>' at the end of the function"),
            );
        }
    }

    fn check_stmts(&mut self, stmts: &[Stmt], scope: &mut Scope, ret: &Type, in_test: bool) {
        scope.push();
        for s in stmts {
            self.check_stmt(s, scope, ret, in_test);
        }
        scope.pop();
    }

    /// Normalize a type: Type::Struct(name) becomes Type::Enum(name) if name is an enum.
    /// This allows the parser to use Type::Struct for all user-defined types,
    /// and we fix them up during type checking.
    fn normalize_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Struct(name) if self.enums.contains_key(name) => Type::Enum(name.clone()),
            Type::Array(inner) => Type::Array(Box::new(self.normalize_type(inner))),
            Type::Fn(params, ret) => Type::Fn(
                params.iter().map(|p| self.normalize_type(p)).collect(),
                Box::new(self.normalize_type(ret)),
            ),
            _ => ty.clone(),
        }
    }

    /// Check if two types are equal, normalizing them first.
    fn types_equal(&self, a: &Type, b: &Type) -> bool {
        self.normalize_type(a) == self.normalize_type(b)
    }

    fn check_stmt(&mut self, stmt: &Stmt, scope: &mut Scope, ret: &Type, in_test: bool) {
        match &stmt.kind {
            StmtKind::Let { name, ty, value } => {
                let annotated = ty.as_ref().map(|t| {
                    self.validate_type(t, stmt.span);
                    self.concretize_type(t, stmt.span)
                });
                let inferred = self.infer(value, scope, annotated.as_ref());
                let final_ty = match (annotated, inferred) {
                    (Some(annotated), Some(actual)) => {
                        if !self.types_equal(&annotated, &actual) {
                            self.diags.push(
                                Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: '{}' is declared as '{}' but the value has type '{}'",
                                        name,
                                        self.normalize_type(&annotated),
                                        self.normalize_type(&actual)
                                    ),
                                    value.span,
                                ),
                            );
                        }
                        self.normalize_type(&annotated)
                    }
                    (Some(annotated), None) => annotated,
                    (None, Some(actual)) => actual,
                    (None, None) => Type::Int, // error already reported
                };
                if final_ty == Type::Unit {
                    self.diags.push(
                        Diagnostic::new(
                            "E031",
                            format!("cannot bind '{}' to a unit (no-value) expression", name),
                            value.span,
                        )
                        .with_help("this call returns nothing; call it as a statement instead"),
                    );
                }
                scope.declare(name, final_ty);
            }
            StmtKind::Assign { name, value } => {
                if scope.is_captured(name) {
                    self.diags.push(
                        Diagnostic::new(
                            "E049",
                            format!("cannot assign to captured variable '{}'", name),
                            stmt.span,
                        )
                        .with_help(
                            "lambdas capture by value; to share mutable state, capture a struct or array and mutate its contents",
                        ),
                    );
                }
                let var_ty = scope.lookup(name).cloned();
                match var_ty {
                    Some(t) => {
                        if let Some(actual) = self.infer(value, scope, Some(&t)) {
                            if !self.types_equal(&actual, &t) {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: '{}' has type '{}' but the value has type '{}'",
                                        name,
                                        self.normalize_type(&t),
                                        self.normalize_type(&actual)
                                    ),
                                    value.span,
                                ));
                            }
                        }
                    }
                    None => {
                        self.diags.push(
                            Diagnostic::new(
                                "E032",
                                format!("assignment to undeclared variable '{}'", name),
                                stmt.span,
                            )
                            .with_help(format!("declare it first: let {} = ...", name)),
                        );
                    }
                }
            }
            StmtKind::IndexAssign { base, index, value } => {
                let base_ty = self.infer(base, scope, None);
                if let Some(ity) = self.infer(index, scope, Some(&Type::Int)) {
                    if ity != Type::Int {
                        self.diags.push(Diagnostic::new(
                            "E033",
                            format!("array index must be 'int', found '{}'", ity),
                            index.span,
                        ));
                    }
                }
                match base_ty {
                    Some(Type::Array(elem)) => {
                        if let Some(vty) = self.infer(value, scope, Some(&elem)) {
                            if !self.types_equal(&vty, &elem) {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: array elements are '{}' but the value has type '{}'",
                                        self.normalize_type(&elem),
                                        self.normalize_type(&vty)
                                    ),
                                    value.span,
                                ));
                            }
                        }
                    }
                    Some(other) => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("cannot index-assign into a value of type '{}'", other),
                            stmt.span,
                        ));
                    }
                    None => {}
                }
            }
            StmtKind::FieldAssign { base, field, value } => {
                let base_ty = self.infer(base, scope, None);
                match base_ty {
                    Some(Type::Struct(sname)) => {
                        match self.field_type(&sname, field) {
                            Some(fty) => {
                                if let Some(vty) = self.infer(value, scope, Some(&fty)) {
                                    if !self.types_equal(&vty, &fty) {
                                        self.diags.push(Diagnostic::new(
                                            "E030",
                                            format!(
                                                "type mismatch: field '{}.{}' is '{}' but the value has type '{}'",
                                                sname,
                                                field,
                                                self.normalize_type(&fty),
                                                self.normalize_type(&vty)
                                            ),
                                            value.span,
                                        ));
                                    }
                                }
                            }
                            None => {
                                self.diags.push(self.no_field(&sname, field, stmt.span));
                            }
                        }
                    }
                    Some(other) => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("type '{}' has no fields to assign", other),
                            stmt.span,
                        ));
                    }
                    None => {}
                }
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                let cty = self.infer(cond, scope, Some(&Type::Bool));
                self.expect_bool(cty, cond.span, "if condition");
                self.check_stmts(then_body, scope, ret, in_test);
                self.check_stmts(else_body, scope, ret, in_test);
            }
            StmtKind::While {
                cond,
                invariant,
                body,
            } => {
                let cty = self.infer(cond, scope, Some(&Type::Bool));
                self.expect_bool(cty, cond.span, "while condition");
                if let Some(inv) = invariant {
                    let ity = self.infer(inv, scope, Some(&Type::Bool));
                    self.expect_bool(ity, inv.span, "while invariant");
                }
                self.loop_depth += 1;
                self.check_stmts(body, scope, ret, in_test);
                self.loop_depth -= 1;
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                for (e, what) in [(start, "range start"), (end, "range end")] {
                    if let Some(t) = self.infer(e, scope, Some(&Type::Int)) {
                        if t != Type::Int {
                            self.diags.push(Diagnostic::new(
                                "E033",
                                format!("{} must be 'int', found '{}'", what, t),
                                e.span,
                            ));
                        }
                    }
                }
                scope.push();
                scope.declare(var, Type::Int);
                self.loop_depth += 1;
                self.check_stmts(body, scope, ret, in_test);
                self.loop_depth -= 1;
                scope.pop();
            }
            StmtKind::Break | StmtKind::Continue => {
                if self.loop_depth == 0 {
                    let word = if matches!(stmt.kind, StmtKind::Break) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.diags.push(Diagnostic::new(
                        "E027",
                        format!("'{}' outside of a loop", word),
                        stmt.span,
                    ));
                }
            }
            StmtKind::Return(value) => {
                if in_test {
                    self.diags.push(Diagnostic::new(
                        "E036",
                        "'return' is not allowed inside a test block",
                        stmt.span,
                    ));
                    return;
                }
                match (value, ret) {
                    (None, Type::Unit) => {}
                    (None, expected) => {
                        self.diags.push(Diagnostic::new(
                            "E037",
                            format!("this function must return a value of type '{}'", expected),
                            stmt.span,
                        ));
                    }
                    (Some(e), expected) => {
                        if *expected == Type::Unit {
                            self.diags.push(
                                Diagnostic::new(
                                    "E038",
                                    "this function has no return type but returns a value",
                                    e.span,
                                )
                                .with_help("add '-> <type>' to the function signature"),
                            );
                        } else if let Some(actual) = self.infer(e, scope, Some(expected)) {
                            if !self.types_equal(&actual, expected) {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: function returns '{}' but this value has type '{}'",
                                        self.normalize_type(expected), self.normalize_type(&actual)
                                    ),
                                    e.span,
                                ));
                            }
                        }
                    }
                }
            }
            StmtKind::Assert(expr) => {
                let ty = self.infer(expr, scope, Some(&Type::Bool));
                self.expect_bool(ty, expr.span, "assert");
            }
            StmtKind::Expr(expr) => {
                self.infer(expr, scope, None);
            }
        }
    }

    fn expect_bool(&mut self, ty: Option<Type>, span: Span, what: &str) {
        if let Some(t) = ty {
            if t != Type::Bool {
                self.diags.push(Diagnostic::new(
                    "E039",
                    format!("{} must be 'bool', found '{}'", what, t),
                    span,
                ));
            }
        }
    }

    fn field_type(&self, struct_name: &str, field: &str) -> Option<Type> {
        self.structs
            .get(struct_name)?
            .iter()
            .find(|f| f.name == field)
            .map(|f| f.ty.clone())
    }

    fn no_field(&self, struct_name: &str, field: &str, span: Span) -> Diagnostic {
        let fields = self
            .structs
            .get(struct_name)
            .map(|fs| {
                fs.iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut d = Diagnostic::new(
            "E048",
            format!("struct '{}' has no field '{}'", struct_name, field),
            span,
        )
        .with_help(format!("fields of {}: {}", struct_name, fields));
        if let Some(fs) = self.structs.get(struct_name) {
            if let Some(suggestion) = closest(field, fs.iter().map(|f| f.name.as_str())) {
                // the field name sits at the end of the base.field span
                let fspan = Span::new(
                    span.end.saturating_sub(field.len() as u32),
                    span.end,
                );
                d = d.with_fix(fspan, suggestion.to_string());
            }
        }
        d
    }

    /// Infers an expression type. Returns None if an error was already
    /// reported for this subtree (to avoid cascading diagnostics).
    fn infer(&mut self, expr: &Expr, scope: &mut Scope, expected: Option<&Type>) -> Option<Type> {
        match &expr.kind {
            ExprKind::Int(_) => Some(Type::Int),
            ExprKind::Float(_) => Some(Type::Float),
            ExprKind::Bool(_) => Some(Type::Bool),
            ExprKind::Str(_) => Some(Type::Str),
            ExprKind::Var(name) => {
                if let Some(t) = scope.lookup(name) {
                    return Some(t.clone());
                }
                // a bare function name is a first-class function value
                if let Some(sig) = self.signatures.get(name) {
                    if self.generic_fns.contains_key(name) {
                        self.diags.push(
                            Diagnostic::new(
                                "E065",
                                format!(
                                    "generic function '{}' cannot be used as a value",
                                    name
                                ),
                                expr.span,
                            )
                            .with_help("call it directly so its type arguments can be inferred"),
                        );
                        return None;
                    }
                    return Some(Type::Fn(sig.params.clone(), Box::new(sig.ret.clone())));
                }
                // check if this is an enum variant without payload (Enum::Variant)
                if let Some(colon_pos) = name.rfind("::") {
                    let enum_name = &name[..colon_pos];
                    let variant_name = &name[colon_pos + 2..];
                    if let Some(ed) = self.program.enums.iter().find(|e| e.name == enum_name) {
                        if let Some(variant) = ed.variants.iter().find(|v| v.name == variant_name) {
                            if !variant.payloads.is_empty() {
                                self.diags.push(Diagnostic::new(
                                    "E063",
                                    format!(
                                        "variant '{}::{}' has payload(s) and must be called as a function",
                                        enum_name, variant_name
                                    ),
                                    expr.span,
                                ).with_help(format!("use {}::{}(...)", enum_name, variant_name)));
                                return None;
                            }
                            if !ed.type_params.is_empty() {
                                return self.check_enum_variant_call(
                                    enum_name,
                                    variant_name,
                                    ed,
                                    &[],
                                    expr.span,
                                    scope,
                                    expected,
                                );
                            }
                            return Some(Type::Enum(enum_name.to_string()));
                        }
                    }
                }
                let mut d =
                    Diagnostic::new("E035", format!("unknown variable '{}'", name), expr.span);
                if self.structs.contains_key(name) {
                    d = d.with_help(format!(
                        "'{}' is a struct; construct it: {}(...)",
                        name, name
                    ));
                } else {
                    let names = scope.all_names();
                    if let Some(suggestion) = closest(name, names.iter().map(|s| s.as_str())) {
                        let suggestion = suggestion.to_string();
                        d = d
                            .with_help(format!("did you mean '{}'?", suggestion))
                            .with_fix(expr.span, suggestion);
                    }
                }
                self.diags.push(d);
                None
            }
            ExprKind::Array(elems) => {
                let expected_elem = match expected {
                    Some(Type::Array(e)) => Some(e.as_ref()),
                    _ => None,
                };
                if elems.is_empty() {
                    return match expected_elem {
                        Some(e) => Some(Type::Array(Box::new(e.clone()))),
                        None => {
                            self.diags.push(
                                Diagnostic::new(
                                    "E040",
                                    "cannot infer the element type of an empty array literal",
                                    expr.span,
                                )
                                .with_help("annotate it: let xs: [int] = []"),
                            );
                            None
                        }
                    };
                }
                let first = self.infer(&elems[0], scope, expected_elem)?;
                for e in &elems[1..] {
                    if let Some(t) = self.infer(e, scope, Some(&first)) {
                        if t != first {
                            self.diags.push(Diagnostic::new(
                                "E041",
                                format!(
                                    "array elements must all have the same type: first is '{}', this is '{}'",
                                    first, t
                                ),
                                e.span,
                            ));
                        }
                    }
                }
                Some(Type::Array(Box::new(first)))
            }
            ExprKind::Index(base, index) => {
                let base_ty = self.infer(base, scope, None)?;
                if let Some(ity) = self.infer(index, scope, Some(&Type::Int)) {
                    if ity != Type::Int {
                        self.diags.push(Diagnostic::new(
                            "E033",
                            format!("array index must be 'int', found '{}'", ity),
                            index.span,
                        ));
                    }
                }
                match base_ty {
                    Type::Array(elem) => Some(*elem),
                    other => {
                        self.diags.push(
                            Diagnostic::new(
                                "E034",
                                format!("cannot index into a value of type '{}'", other),
                                base.span,
                            )
                            .with_help("only arrays [T] can be indexed; use char_at(s, i) for strings"),
                        );
                        None
                    }
                }
            }
            ExprKind::Field(base, field) => {
                let base_ty = self.infer(base, scope, None)?;
                match base_ty {
                    Type::Struct(sname) => match self.field_type(&sname, field) {
                        Some(t) => Some(t),
                        None => {
                            self.diags.push(self.no_field(&sname, field, expr.span));
                            None
                        }
                    },
                    other => {
                        self.diags.push(Diagnostic::new(
                            "E034",
                            format!("type '{}' has no fields", other),
                            expr.span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Un(op, inner) => {
                let ty = self.infer(inner, scope, None)?;
                match op {
                    UnOp::Neg => match ty {
                        Type::Int | Type::Float => Some(ty),
                        other => {
                            self.diags.push(Diagnostic::new(
                                "E042",
                                format!("unary '-' requires 'int' or 'float', found '{}'", other),
                                expr.span,
                            ));
                            None
                        }
                    },
                    UnOp::Not => {
                        if ty != Type::Bool {
                            self.diags.push(Diagnostic::new(
                                "E042",
                                format!("'!' requires 'bool', found '{}'", ty),
                                expr.span,
                            ));
                            return None;
                        }
                        Some(Type::Bool)
                    }
                }
            }
            ExprKind::Bin(op, lhs, rhs) => {
                let lt = self.infer(lhs, scope, None)?;
                let rt = self.infer(rhs, scope, Some(&lt))?;
                use BinOp::*;
                // operations on a constrained type variable (inside a generic
                // function body): what the bounds permit is well-typed
                if let (Type::TypeVar(a), Type::TypeVar(b)) = (&lt, &rt) {
                    if a == b {
                        let (needed, result) = match op {
                            Add | Sub | Mul | Div => ("Num", lt.clone()),
                            Lt | Le | Gt | Ge => ("Ord", Type::Bool),
                            Eq | Ne => ("Eq", Type::Bool),
                            _ => ("", Type::Unit),
                        };
                        if !needed.is_empty() {
                            if self.typevar_allows(a, needed) {
                                return Some(result);
                            }
                            self.diags.push(
                                Diagnostic::new(
                                    "E069",
                                    format!(
                                        "'{}' on values of type parameter '{}' requires the '{}' constraint",
                                        op_name(*op), a, needed
                                    ),
                                    expr.span,
                                )
                                .with_help(format!(
                                    "declare the bound in the signature: fn<{}: {}> ...",
                                    a, needed
                                )),
                            );
                            return None;
                        }
                    }
                }
                match op {
                    Add => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        (Type::Float, Type::Float) => Some(Type::Float),
                        (Type::Str, Type::Str) => Some(Type::Str),
                        _ => {
                            self.diags.push(self.numeric_mismatch(
                                "+", &lt, &rt, lhs.span, rhs.span, expr.span,
                            ));
                            None
                        }
                    },
                    Sub | Mul | Div => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        (Type::Float, Type::Float) => Some(Type::Float),
                        _ => {
                            self.diags.push(self.numeric_mismatch(
                                op_name(*op), &lt, &rt, lhs.span, rhs.span, expr.span,
                            ));
                            None
                        }
                    },
                    Mod => match (&lt, &rt) {
                        (Type::Int, Type::Int) => Some(Type::Int),
                        _ => {
                            self.diags.push(
                                Diagnostic::new(
                                    "E043",
                                    format!("'%' requires int operands, found '{}' and '{}'", lt, rt),
                                    expr.span,
                                ),
                            );
                            None
                        }
                    },
                    Lt | Le | Gt | Ge => match (&lt, &rt) {
                        (Type::Int, Type::Int) | (Type::Float, Type::Float) => Some(Type::Bool),
                        _ => {
                            self.diags.push(self.numeric_mismatch(
                                op_name(*op), &lt, &rt, lhs.span, rhs.span, expr.span,
                            ));
                            None
                        }
                    },
                    Eq | Ne => {
                        if lt != rt {
                            self.diags.push(Diagnostic::new(
                                "E043",
                                format!(
                                    "cannot compare '{}' with '{}': operand types must match",
                                    lt, rt
                                ),
                                expr.span,
                            ));
                            return None;
                        }
                        if matches!(
                            lt,
                            Type::Array(_) | Type::Struct(_) | Type::Fn(_, _) | Type::TypeVar(_)
                        ) {
                            self.diags.push(
                                Diagnostic::new(
                                    "E044",
                                    format!("values of type '{}' cannot be compared with '==' or '!='", lt),
                                    expr.span,
                                )
                                .with_help("compare field-by-field or element-by-element (for type parameters, add an Eq constraint)"),
                            );
                            return None;
                        }
                        Some(Type::Bool)
                    }
                    And | Or => {
                        if lt != Type::Bool || rt != Type::Bool {
                            self.diags.push(Diagnostic::new(
                                "E043",
                                format!(
                                    "'{}' requires bool operands, found '{}' and '{}'",
                                    op_name(*op),
                                    lt,
                                    rt
                                ),
                                expr.span,
                            ));
                            return None;
                        }
                        Some(Type::Bool)
                    }
                }
            }
            ExprKind::Call(name, type_args, args) => {
                self.check_call(name, type_args, args, expr.span, scope, expected)
            }
            ExprKind::Lambda(lambda) => {
                for p in &lambda.params {
                    self.validate_type(&p.ty, p.span);
                }
                self.validate_type(&lambda.ret, expr.span);
                // the body is a new function scope: it can read (capture)
                // enclosing variables but not reassign them, and break /
                // continue / return refer to the lambda itself
                scope.push();
                scope.boundaries.push(scope.vars.len() - 1);
                let mut seen: HashMap<String, ()> = HashMap::new();
                for p in &lambda.params {
                    if seen.insert(p.name.clone(), ()).is_some() {
                        self.diags.push(Diagnostic::new(
                            "E023",
                            format!("duplicate parameter name '{}'", p.name),
                            p.span,
                        ));
                    }
                    scope.declare(&p.name, p.ty.clone());
                }
                let saved_loop = self.loop_depth;
                self.loop_depth = 0;
                self.check_stmts(&lambda.body, scope, &lambda.ret.clone(), false);
                self.loop_depth = saved_loop;
                if lambda.ret != Type::Unit && !always_returns(&lambda.body) {
                    self.diags.push(
                        Diagnostic::new(
                            "E025",
                            format!(
                                "lambda returns '{}' but not all paths return a value",
                                lambda.ret
                            ),
                            expr.span,
                        )
                        .with_help("add a 'return <expr>' at the end of the lambda body"),
                    );
                }
                scope.boundaries.pop();
                scope.pop();
                Some(Type::Fn(
                    lambda.params.iter().map(|p| p.ty.clone()).collect(),
                    Box::new(lambda.ret.clone()),
                ))
            }
            ExprKind::Match(m) => {
                // infer scrutinee type
                let scrutinee_ty = self.infer(&m.scrutinee, scope, None)?;
                
                if m.arms.is_empty() {
                    self.diags.push(Diagnostic::new(
                        "E052",
                        "match expression has no arms".to_string(),
                        expr.span,
                    ));
                    return None;
                }
                
                // check patterns and collect result types
                let mut result_type: Option<Type> = None;
                let mut seen_patterns = Vec::new();
                
                for arm in &m.arms {
                    // check pattern against scrutinee type and get bindings
                    self.check_pattern(&arm.pattern, &scrutinee_ty, arm.span, scope)?;
                    let bindings = self.extract_pattern_bindings(&arm.pattern, &scrutinee_ty, arm.span)?;
                    
                    // collect pattern bindings
                    scope.push();
                    for (name, ty) in bindings {
                        scope.declare(&name, ty);
                    }
                    
                    // check arm body
                    let arm_ty = self.infer(&arm.body, scope, result_type.as_ref())?;
                    scope.pop();
                    
                    // unify result types
                    match result_type {
                        None => result_type = Some(arm_ty),
                        Some(ref rt) if rt != &arm_ty => {
                            self.diags.push(Diagnostic::new(
                                "E056",
                                format!(
                                    "match arms have inconsistent types: first arm returns '{}', this arm returns '{}'",
                                    rt, arm_ty
                                ),
                                arm.body.span,
                            ));
                            return None;
                        }
                        _ => {}
                    }
                    
                    seen_patterns.push(arm.pattern.clone());
                }
                
                // check exhaustiveness
                if !is_exhaustive(&scrutinee_ty, &seen_patterns, &self.enums) {
                    self.diags.push(
                        Diagnostic::new(
                            "E057",
                            format!("match is not exhaustive for type '{}'", scrutinee_ty),
                            expr.span,
                        )
                        .with_help("add arms to cover all cases, or add a wildcard '_' arm"),
                    );
                }
                
                result_type
            }
        }
    }

    fn check_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_ty: &Type,
        span: Span,
        scope: &Scope,
    ) -> Option<Type> {
        match pattern {
            Pattern::Wildcard | Pattern::Var(_) => Some(scrutinee_ty.clone()),
            Pattern::Int(_) => {
                if scrutinee_ty != &Type::Int {
                    self.diags.push(Diagnostic::new(
                        "E058",
                        format!("pattern expects 'int', but scrutinee has type '{}'", scrutinee_ty),
                        span,
                    ));
                    None
                } else {
                    Some(Type::Int)
                }
            }
            Pattern::Bool(_) => {
                if scrutinee_ty != &Type::Bool {
                    self.diags.push(Diagnostic::new(
                        "E058",
                        format!("pattern expects 'bool', but scrutinee has type '{}'", scrutinee_ty),
                        span,
                    ));
                    None
                } else {
                    Some(Type::Bool)
                }
            }
            Pattern::Str(_) => {
                if scrutinee_ty != &Type::Str {
                    self.diags.push(Diagnostic::new(
                        "E058",
                        format!("pattern expects 'str', but scrutinee has type '{}'", scrutinee_ty),
                        span,
                    ));
                    None
                } else {
                    Some(Type::Str)
                }
            }
            Pattern::Variant(enum_name, variant_name) | Pattern::VariantPayload(enum_name, variant_name, _) => {
                // check that scrutinee is the expected enum type
                let expected_ty = Type::Enum(enum_name.clone());
                if scrutinee_ty != &expected_ty && scrutinee_ty != &Type::Struct(enum_name.clone()) {
                    self.diags.push(Diagnostic::new(
                        "E058",
                        format!("pattern expects '{}', but scrutinee has type '{}'", expected_ty, scrutinee_ty),
                        span,
                    ));
                    return None;
                }
                
                // validate variant exists
                let Some(variants) = self.enums.get(enum_name).cloned() else {
                    self.diags.push(Diagnostic::new(
                        "E059",
                        format!("unknown enum '{}'", enum_name),
                        span,
                    ));
                    return None;
                };
                
                let Some(var_def) = variants.iter().find(|v| v.name == *variant_name).cloned() else {
                    self.diags.push(Diagnostic::new(
                        "E060",
                        format!("enum '{}' has no variant '{}'", enum_name, variant_name),
                        span,
                    ));
                    return None;
                };
                
                // check payload
                match pattern {
                    Pattern::Variant(_, _) => {
                        if !var_def.payloads.is_empty() {
                            self.diags.push(Diagnostic::new(
                                "E061",
                                format!("variant '{}::{}' has payload(s) but pattern does not bind them", enum_name, variant_name),
                                span,
                            ).with_help(format!("use {}::{}(...)", enum_name, variant_name)));
                            None
                        } else {
                            Some(expected_ty)
                        }
                    }
                    Pattern::VariantPayload(_, _, inners) => {
                        if var_def.payloads.is_empty() {
                            self.diags.push(Diagnostic::new(
                                "E062",
                                format!("variant '{}::{}' has no payload but pattern expects one", enum_name, variant_name),
                                span,
                            ).with_help(format!("use {}::{} without parentheses", enum_name, variant_name)));
                            None
                        } else if inners.len() != var_def.payloads.len() {
                            self.diags.push(Diagnostic::new(
                                "E061",
                                format!(
                                    "variant '{}::{}' has {} payload(s) but pattern binds {}",
                                    enum_name,
                                    variant_name,
                                    var_def.payloads.len(),
                                    inners.len()
                                ),
                                span,
                            ));
                            None
                        } else {
                            for (inner, payload_ty) in inners.iter().zip(var_def.payloads.iter()) {
                                self.check_pattern(inner, payload_ty, span, scope)?;
                            }
                            Some(expected_ty)
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn extract_pattern_bindings(
        &self,
        pattern: &Pattern,
        ty: &Type,
        span: Span,
    ) -> Option<Vec<(String, Type)>> {
        match pattern {
            Pattern::Wildcard => Some(vec![]),
            Pattern::Var(name) => Some(vec![(name.clone(), ty.clone())]),
            Pattern::Int(_) | Pattern::Bool(_) | Pattern::Str(_) => Some(vec![]),
            Pattern::Variant(_, _) => Some(vec![]),
            Pattern::VariantPayload(enum_name, variant_name, inners) => {
                let variants = self.enums.get(enum_name)?;
                let var_def = variants.iter().find(|v| v.name == *variant_name)?;
                let mut bindings = Vec::new();
                for (inner, payload_ty) in inners.iter().zip(var_def.payloads.iter()) {
                    bindings.extend(self.extract_pattern_bindings(inner, payload_ty, span)?);
                }
                Some(bindings)
            }
        }
    }

    fn check_args(&mut self, name: &str, params: &[Type], args: &[Expr], span: Span, scope: &mut Scope) {
        if args.len() != params.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "'{}' takes {} argument(s), found {}",
                    name,
                    params.len(),
                    args.len()
                ),
                span,
            ));
        }
        for (i, arg) in args.iter().enumerate() {
            if let Some(param_ty) = params.get(i) {
                if let Some(actual) = self.infer(arg, scope, Some(param_ty)) {
                    if !self.types_equal(&actual, param_ty) {
                        self.diags.push(Diagnostic::new(
                            "E030",
                            format!(
                                "type mismatch: argument {} of '{}' expects '{}', found '{}'",
                                i + 1,
                                name,
                                self.normalize_type(param_ty),
                                self.normalize_type(&actual)
                            ),
                            arg.span,
                        ));
                    }
                }
            } else {
                self.infer(arg, scope, None);
            }
        }
    }

    fn check_call(
        &mut self,
        name: &str,
        type_args: &[Type],
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
        expected: Option<&Type>,
    ) -> Option<Type> {
        // 1. a local variable holding a function value shadows everything
        if let Some(Type::Fn(params, ret)) = scope.lookup(name).cloned() {
            self.check_args(name, &params, args, span, scope);
            return Some(*ret);
        }
        match name {
            "print" => {
                if args.len() != 1 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("print takes exactly 1 argument, found {}", args.len()),
                        span,
                    ));
                    return Some(Type::Unit);
                }
                let ty = self.infer(&args[0], scope, None)?;
                if matches!(ty, Type::Array(_) | Type::Struct(_) | Type::Fn(_, _)) {
                    self.diags.push(
                        Diagnostic::new(
                            "E046",
                            format!("print does not accept values of type '{}'", ty),
                            args[0].span,
                        )
                        .with_help("print scalar fields/elements, or build a str with str_of_int and '+'"),
                    );
                }
                Some(Type::Unit)
            }
            "len" => {
                if args.len() != 1 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("len takes exactly 1 argument, found {}", args.len()),
                        span,
                    ));
                    return Some(Type::Int);
                }
                let ty = self.infer(&args[0], scope, None)?;
                match ty {
                    Type::Array(_) | Type::Str => Some(Type::Int),
                    other => {
                        self.diags.push(Diagnostic::new(
                            "E046",
                            format!("len requires an array or str, found '{}'", other),
                            args[0].span,
                        ));
                        Some(Type::Int)
                    }
                }
            }
            "push" => {
                if args.len() != 2 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("push takes exactly 2 arguments, found {}", args.len()),
                        span,
                    ));
                    return None;
                }
                let arr_ty = self.infer(&args[0], scope, expected)?;
                match arr_ty {
                    Type::Array(elem) => {
                        if let Some(vty) = self.infer(&args[1], scope, Some(&elem)) {
                            if !self.types_equal(&vty, &elem) {
                                self.diags.push(Diagnostic::new(
                                    "E030",
                                    format!(
                                        "type mismatch: pushing '{}' into an array of '{}'",
                                        self.normalize_type(&vty),
                                        self.normalize_type(&elem)
                                    ),
                                    args[1].span,
                                ));
                            }
                        }
                        Some(Type::Array(elem))
                    }
                    other => {
                        self.diags.push(
                            Diagnostic::new(
                                "E046",
                                format!("push requires an array as its first argument, found '{}'", other),
                                args[0].span,
                            )
                            .with_help("push returns a new array: let xs2 = push(xs, v)"),
                        );
                        None
                    }
                }
            }
            "to_float" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Float, "to_float")
            }
            "to_int" => {
                self.check_builtin_sig(args, span, scope, &[Type::Float], Type::Int, "to_int")
            }
            "char_at" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int],
                Type::Int,
                "char_at",
            ),
            "substr" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int, Type::Int],
                Type::Str,
                "substr",
            ),
            "chr" => self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Str, "chr"),
            "len_cp" => {
                self.check_builtin_sig(args, span, scope, &[Type::Str], Type::Int, "len_cp")
            }
            "char_at_cp" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int],
                Type::Int,
                "char_at_cp",
            ),
            "substr_cp" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Str, Type::Int, Type::Int],
                Type::Str,
                "substr_cp",
            ),
            "chr_cp" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Str, "chr_cp")
            }
            "hash" => {
                if args.len() != 1 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("hash takes exactly 1 argument, found {}", args.len()),
                        span,
                    ));
                    return Some(Type::Int);
                }
                if let Some(ty) = self.infer(&args[0], scope, None) {
                    if !self.satisfies_bound(&ty, "Hash") {
                        self.diags.push(
                            Diagnostic::new(
                                "E046",
                                format!("hash requires a Hash type, found '{}'", ty),
                                args[0].span,
                            )
                            .with_help("satisfied by: int, bool, str"),
                        );
                    }
                }
                Some(Type::Int)
            }
            "chan_new" => {
                if args.len() != 0 {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!("chan_new takes no arguments, found {}", args.len()),
                        span,
                    ));
                }
                Some(Type::Int)
            }
            "chan_close" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Unit, "chan_close")
            }
            "chan_send_int" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Int, Type::Int],
                Type::Unit,
                "chan_send_int",
            ),
            "chan_send_float" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Int, Type::Float],
                Type::Unit,
                "chan_send_float",
            ),
            "chan_send_bool" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Int, Type::Bool],
                Type::Unit,
                "chan_send_bool",
            ),
            "chan_send_str" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Int, Type::Str],
                Type::Unit,
                "chan_send_str",
            ),
            "chan_recv_int" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Int, "chan_recv_int")
            }
            "chan_recv_float" => self.check_builtin_sig(
                args,
                span,
                scope,
                &[Type::Int],
                Type::Float,
                "chan_recv_float",
            ),
            "chan_recv_bool" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Bool, "chan_recv_bool")
            }
            "chan_recv_str" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Str, "chan_recv_str")
            }
            "spawn" => {
                // spawn(f, args...) -> int (task handle). Runs f on its own
                // OS thread with deep-copied arguments (interpreter only).
                if args.is_empty() {
                    self.diags.push(
                        Diagnostic::new(
                            "E045",
                            "spawn takes a function value plus its arguments",
                            span,
                        )
                        .with_help("spawn(worker, 42) then join_int(handle)"),
                    );
                    return Some(Type::Int);
                }
                let fty = self.infer(&args[0], scope, None)?;
                let Type::Fn(params, ret) = fty else {
                    self.diags.push(Diagnostic::new(
                        "E046",
                        format!(
                            "spawn requires a function value as its first argument, found '{}'",
                            fty
                        ),
                        args[0].span,
                    ));
                    return Some(Type::Int);
                };
                if !matches!(*ret, Type::Int | Type::Float | Type::Bool | Type::Str) {
                    self.diags.push(
                        Diagnostic::new(
                            "E071",
                            format!(
                                "spawned functions must return int, float, bool or str, found '{}'",
                                ret
                            ),
                            args[0].span,
                        )
                        .with_help("join_int/join_float/join_bool/join_str retrieve the result"),
                    );
                }
                if args.len() - 1 != params.len() {
                    self.diags.push(Diagnostic::new(
                        "E045",
                        format!(
                            "spawned function takes {} argument{}, found {}",
                            params.len(),
                            if params.len() == 1 { "" } else { "s" },
                            args.len() - 1
                        ),
                        span,
                    ));
                    return Some(Type::Int);
                }
                for (a, want) in args[1..].iter().zip(&params) {
                    if let Some(ty) = self.infer(a, scope, Some(want)) {
                        if !self.types_equal(&ty, want) {
                            self.diags.push(Diagnostic::new(
                                "E030",
                                format!("spawn argument requires '{}', found '{}'", want, ty),
                                a.span,
                            ));
                        }
                    }
                }
                Some(Type::Int)
            }
            "join_int" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Int, "join_int")
            }
            "join_float" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Float, "join_float")
            }
            "join_bool" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Bool, "join_bool")
            }
            "join_str" => {
                self.check_builtin_sig(args, span, scope, &[Type::Int], Type::Str, "join_str")
            }
            _ => {
                // check if this is an enum variant constructor (Enum::Variant)
                if let Some(colon_pos) = name.rfind("::") {
                    let enum_name = &name[..colon_pos];
                    let variant_name = &name[colon_pos + 2..];
                    if self.enums.contains_key(enum_name) {
                        if let Some(ed) = self.program.enums.iter().find(|e| e.name == enum_name) {
                            return self.check_enum_variant_call(
                                enum_name,
                                variant_name,
                                ed,
                                args,
                                span,
                                scope,
                                expected,
                            );
                        }
                    }
                }
                // 2. struct constructor
                if let Some(sd) = self.program.structs.iter().find(|s| s.name == *name) {
                    if !sd.type_params.is_empty() {
                        return self.check_struct_ctor(name, sd, args, span, scope);
                    }
                    let fields = self.structs.get(name).cloned()?;
                    let params: Vec<Type> = fields.iter().map(|f| f.ty.clone()).collect();
                    self.check_args(name, &params, args, span, scope);
                    return Some(Type::Struct(name.to_string()));
                }
                // 3a. generic function: turbofish or infer type arguments
                if let Some(&idx) = self.generic_fns.get(name) {
                    let f = &self.program.functions[idx];
                    return self.check_generic_call(f, type_args, args, span, scope);
                }
                if !type_args.is_empty() {
                    self.diags.push(Diagnostic::new(
                        "E064",
                        format!(
                            "turbofish type arguments are only valid on generic functions; '{}' is not generic",
                            name
                        ),
                        span,
                    ));
                }
                // 3. named function or extern
                let (params, ret) = match self.signatures.get(name) {
                    Some(sig) => (sig.params.clone(), sig.ret.clone()),
                    None => {
                        let mut d = Diagnostic::new(
                            "E047",
                            format!("unknown function '{}'", name),
                            span,
                        );
                        let candidates: Vec<&str> = self
                            .signatures
                            .keys()
                            .map(|s| s.as_str())
                            .chain(BUILTINS.iter().copied())
                            .collect();
                        if let Some(suggestion) = closest(name, candidates.into_iter()) {
                            let suggestion = suggestion.to_string();
                            // the call expr's span starts at the callee name
                            let name_span = Span::new(
                                span.start,
                                span.start + name.len() as u32,
                            );
                            d = d
                                .with_help(format!("did you mean '{}'?", suggestion))
                                .with_fix(name_span, suggestion);
                        } else {
                            d = d.with_help(format!("builtins: {}", BUILTINS.join(", ")));
                        }
                        self.diags.push(d);
                        return None;
                    }
                };
                self.check_args(name, &params, args, span, scope);
                Some(ret)
            }
        }
    }

    /// Checks a call to a generic function: infers the type arguments from the
    /// argument types by unification, verifies constraint bounds, records the
    /// instantiation for the monomorphizer, and returns the substituted
    /// return type.
    fn check_generic_call(
        &mut self,
        f: &'a Function,
        explicit_type_args: &[Type],
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
    ) -> Option<Type> {
        if args.len() != f.params.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "'{}' takes {} argument(s), found {}",
                    f.name,
                    f.params.len(),
                    args.len()
                ),
                span,
            ));
            return None;
        }
        if !explicit_type_args.is_empty() && explicit_type_args.len() != f.type_params.len() {
            self.diags.push(Diagnostic::new(
                "E064",
                format!(
                    "'{}' expects {} type argument(s), found {}",
                    f.name,
                    f.type_params.len(),
                    explicit_type_args.len()
                ),
                span,
            ));
            return None;
        }
        let tp_names: Vec<&str> = f.type_params.iter().map(|tp| tp.name.as_str()).collect();
        let mut subst: HashMap<String, Type> = HashMap::new();
        let mut ok = true;
        let outer_params = self.type_params.clone();
        self.enter_type_params(&f.type_params);
        // turbofish seeds the substitution before argument unification
        if !explicit_type_args.is_empty() {
            for (tp, ta) in f.type_params.iter().zip(explicit_type_args.iter()) {
                self.validate_type(ta, span);
                let ta = self.normalize_type(ta);
                subst.insert(tp.name.clone(), ta);
            }
        }
        for (i, (p, a)) in f.params.iter().zip(args.iter()).enumerate() {
            let Some(aty) = self.infer(a, scope, None) else {
                ok = false;
                continue;
            };
            let aty = self.normalize_type(&aty);
            let pty = self.expand_generic_nominal(&self.normalize_type(&p.ty));
            if !self.unify(&pty, &aty, &tp_names, &mut subst) {
                self.diags.push(Diagnostic::new(
                    "E030",
                    format!(
                        "type mismatch: argument {} of '{}' expects '{}', found '{}'",
                        i + 1,
                        f.name,
                        apply_subst(&pty, &subst),
                        aty
                    ),
                    a.span,
                ));
                ok = false;
            }
        }
        if !ok {
            self.exit_type_params();
            return None;
        }
        // every type parameter must be fixed by turbofish and/or the arguments
        let mut type_args = Vec::new();
        for tp in &f.type_params {
            match subst.get(&tp.name) {
                Some(t) => type_args.push(t.clone()),
                None if outer_params.contains(&tp.name) => {
                    type_args.push(Type::TypeVar(tp.name.clone()));
                }
                None => {
                    self.diags.push(
                        Diagnostic::new(
                            "E064",
                            format!(
                                "cannot infer type parameter '{}' of '{}' from the arguments",
                                tp.name, f.name
                            ),
                            span,
                        )
                        .with_help(
                            "provide an explicit turbofish (e.g. foo::<int>(...)) or ensure every type parameter appears in a parameter type",
                        ),
                    );
                    self.exit_type_params();
                    return None;
                }
            }
        }
        // inferred types must satisfy the declared constraint bounds
        for (tp, ta) in f.type_params.iter().zip(type_args.iter()) {
            for b in &tp.bounds {
                if !self.satisfies_bound(ta, b) {
                    self.diags.push(
                        Diagnostic::new(
                            "E069",
                            format!(
                                "type '{}' does not satisfy the constraint '{}: {}' of '{}'",
                                ta, tp.name, b, f.name
                            ),
                            span,
                        )
                        .with_help(bound_help(b)),
                    );
                    self.exit_type_params();
                    return None;
                }
            }
        }
        self.instantiations
            .borrow_mut()
            .push((span, f.name.clone(), type_args));
        let expanded_ret = self.expand_generic_nominal(&self.normalize_type(&f.ret));
        let ret = self.subst_type(&expanded_ret, &subst);
        self.exit_type_params();
        Some(ret)
    }

    fn check_builtin_sig(
        &mut self,
        args: &[Expr],
        span: Span,
        scope: &mut Scope,
        params: &[Type],
        ret: Type,
        name: &str,
    ) -> Option<Type> {
        if args.len() != params.len() {
            self.diags.push(Diagnostic::new(
                "E045",
                format!(
                    "{} takes exactly {} argument(s), found {}",
                    name,
                    params.len(),
                    args.len()
                ),
                span,
            ));
            return Some(ret);
        }
        for (arg, want) in args.iter().zip(params) {
            if let Some(ty) = self.infer(arg, scope, Some(want)) {
                if &ty != want {
                    self.diags.push(Diagnostic::new(
                        "E030",
                        format!("{} requires '{}', found '{}'", name, want, ty),
                        arg.span,
                    ));
                }
            }
        }
        Some(ret)
    }

    fn numeric_mismatch(
        &self,
        op: &str,
        lt: &Type,
        rt: &Type,
        lspan: Span,
        rspan: Span,
        span: Span,
    ) -> Diagnostic {
        let mut d = Diagnostic::new(
            "E043",
            format!(
                "'{}' cannot be applied to '{}' and '{}'",
                op, lt, rt
            ),
            span,
        );
        if (lt == &Type::Int && rt == &Type::Float) || (lt == &Type::Float && rt == &Type::Int) {
            d = d.with_help(
                "machino has no implicit numeric conversion; use to_float(x) or to_int(x)",
            );
            // machine-applicable fix: wrap the int-typed operand in to_float(...)
            let (int_span, _) = if lt == &Type::Int { (lspan, rspan) } else { (rspan, lspan) };
            let text = self.snippet(int_span);
            if !text.is_empty() {
                d = d.with_fix(int_span, format!("to_float({})", text));
            }
        }
        d
    }
}

fn op_name(op: BinOp) -> &'static str {
    use BinOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        Eq => "==",
        Ne => "!=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
        And => "&&",
        Or => "||",
    }
}

pub fn always_returns(stmts: &[Stmt]) -> bool {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(_) => return true,
            StmtKind::If {
                then_body,
                else_body,
                ..
            } => {
                if !else_body.is_empty()
                    && always_returns(then_body)
                    && always_returns(else_body)
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Check if a set of patterns exhaustively covers a type.
/// Simplified exhaustiveness checking:
/// - For int/float/str/bool: only accept if there's a wildcard or var
/// - For enums: check that all variants are covered or there's a wildcard
fn is_exhaustive(
    ty: &Type,
    patterns: &[Pattern],
    enums: &HashMap<String, Vec<EnumVariant>>,
) -> bool {
    // wildcard or variable catches everything
    if patterns
        .iter()
        .any(|p| matches!(p, Pattern::Wildcard | Pattern::Var(_)))
    {
        return true;
    }

    match ty {
        Type::Int | Type::Float | Type::Str | Type::Bool => {
            // for primitive types, we require a wildcard since we can't enumerate all values
            false
        }
        Type::Enum(enum_name) | Type::Struct(enum_name) => {
            // check if it's actually an enum
            let Some(variants) = enums.get(enum_name) else {
                return false;
            };
            
            // collect covered variants
            let mut covered = std::collections::HashSet::new();
            for p in patterns {
                match p {
                    Pattern::Variant(e, v) | Pattern::VariantPayload(e, v, _) if e == enum_name => {
                        covered.insert(v.as_str());
                    }
                    _ => {}
                }
            }
            
            // check that all variants are covered
            variants.iter().all(|v| covered.contains(v.name.as_str()))
        }
        _ => {
            // arrays, functions, unit: require wildcard
            false
        }
    }
}
