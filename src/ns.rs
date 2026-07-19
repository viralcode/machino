//! Namespaced imports: `import "lib/vec.mno" as vec`.
//!
//! The loader concatenates all files into one bundle, so without namespaces
//! every top-level name is global. When an import carries an alias, every
//! function/struct/enum *defined* in that file is renamed to `alias::name`,
//! and unqualified references *inside* that file are rewritten to match.
//! Code in other files refers to the items as `alias::name`, which after
//! renaming resolves naturally. Spans decide which file a node belongs to.

use crate::ast::*;
use crate::diag::Span;
use std::collections::HashMap;

/// One aliased file: the byte range of its segment in the bundle plus the
/// namespace alias its importer chose.
pub struct AliasedSegment {
    pub start: u32,
    pub end: u32,
    pub alias: String,
}

struct Ns<'a> {
    segments: &'a [AliasedSegment],
    /// Per segment: unqualified name -> qualified name, for items defined
    /// in that segment.
    renames: Vec<HashMap<String, String>>,
    /// Locals currently in scope while walking (shadowing beats renaming).
    locals: Vec<Vec<String>>,
}

pub fn apply(program: &mut Program, segments: &[AliasedSegment]) {
    if segments.is_empty() {
        return;
    }
    let mut ns = Ns {
        segments,
        renames: segments.iter().map(|_| HashMap::new()).collect(),
        locals: Vec::new(),
    };
    // pass 1: rename definitions
    for f in &mut program.functions {
        if let Some(i) = ns.segment_of(f.span) {
            let new = format!("{}::{}", segments[i].alias, f.name);
            ns.renames[i].insert(f.name.clone(), new.clone());
            f.name = new;
        }
    }
    for s in &mut program.structs {
        if let Some(i) = ns.segment_of(s.span) {
            let new = format!("{}::{}", segments[i].alias, s.name);
            ns.renames[i].insert(s.name.clone(), new.clone());
            s.name = new;
        }
    }
    for e in &mut program.enums {
        if let Some(i) = ns.segment_of(e.span) {
            let new = format!("{}::{}", segments[i].alias, e.name);
            ns.renames[i].insert(e.name.clone(), new.clone());
            e.name = new;
        }
    }
    // pass 2: rewrite references inside aliased segments
    for f in &mut program.functions {
        let Some(i) = ns.segment_of(f.span) else { continue };
        ns.locals.clear();
        ns.locals
            .push(f.params.iter().map(|p| p.name.clone()).collect());
        for p in &mut f.params {
            ns.rewrite_type(&mut p.ty, i);
        }
        ns.rewrite_type(&mut f.ret, i);
        for c in f.requires.iter_mut().chain(f.ensures.iter_mut()) {
            ns.rewrite_expr(&mut c.expr, i);
        }
        let mut body = std::mem::take(&mut f.body);
        ns.rewrite_stmts(&mut body, i);
        f.body = body;
    }
    for s in &mut program.structs {
        let Some(i) = ns.segment_of(s.span) else { continue };
        for fld in &mut s.fields {
            ns.rewrite_type(&mut fld.ty, i);
        }
    }
    for e in &mut program.enums {
        let Some(i) = ns.segment_of(e.span) else { continue };
        for v in &mut e.variants {
            for ty in &mut v.payloads {
                ns.rewrite_type(ty, i);
            }
        }
    }
    for t in &mut program.tests {
        let Some(i) = ns.segment_of(t.span) else { continue };
        ns.locals.clear();
        ns.locals.push(Vec::new());
        let mut body = std::mem::take(&mut t.body);
        ns.rewrite_stmts(&mut body, i);
        t.body = body;
    }
}

impl<'a> Ns<'a> {
    fn segment_of(&self, span: Span) -> Option<usize> {
        self.segments
            .iter()
            .position(|s| span.start >= s.start && span.start < s.end)
    }

    fn is_local(&self, name: &str) -> bool {
        self.locals
            .iter()
            .any(|frame| frame.iter().any(|n| n == name))
    }

    fn bind(&mut self, name: &str) {
        if let Some(frame) = self.locals.last_mut() {
            frame.push(name.to_string());
        }
    }

    /// Renames a possibly-qualified reference. For `Enum::Variant` the map
    /// key is the first path segment (the enum's unqualified name).
    fn rename_ref(&self, name: &str, seg: usize) -> Option<String> {
        let head = match name.find("::") {
            Some(pos) => &name[..pos],
            None => name,
        };
        if head.len() == name.len() && self.is_local(head) {
            return None;
        }
        let new_head = self.renames[seg].get(head)?;
        Some(format!("{}{}", new_head, &name[head.len()..]))
    }

    fn rewrite_type(&self, ty: &mut Type, seg: usize) {
        match ty {
            Type::Struct(name) | Type::Enum(name) => {
                if let Some(new) = self.renames[seg].get(name.as_str()) {
                    *name = new.clone();
                }
            }
            Type::App(name, args) => {
                if let Some(new) = self.renames[seg].get(name.as_str()) {
                    *name = new.clone();
                }
                for a in args {
                    self.rewrite_type(a, seg);
                }
            }
            Type::Array(inner) => self.rewrite_type(inner, seg),
            Type::Fn(params, ret) => {
                for p in params {
                    self.rewrite_type(p, seg);
                }
                self.rewrite_type(ret, seg);
            }
            _ => {}
        }
    }

    fn rewrite_pattern(&mut self, pat: &mut Pattern, seg: usize) {
        match pat {
            Pattern::Variant(enum_name, _) | Pattern::VariantPayload(enum_name, _, _) => {
                if let Some(new) = self.renames[seg].get(enum_name.as_str()) {
                    *enum_name = new.clone();
                }
                if let Pattern::VariantPayload(_, _, inners) = pat {
                    for inner in inners {
                        self.rewrite_pattern(inner, seg);
                    }
                }
            }
            Pattern::Var(name) => self.bind(&name.clone()),
            _ => {}
        }
    }

    fn rewrite_stmts(&mut self, stmts: &mut [Stmt], seg: usize) {
        for stmt in stmts {
            match &mut stmt.kind {
                StmtKind::Let { name, ty, value } => {
                    self.rewrite_expr(value, seg);
                    if let Some(t) = ty {
                        self.rewrite_type(t, seg);
                    }
                    let n = name.clone();
                    self.bind(&n);
                }
                StmtKind::Assign { value, .. } => self.rewrite_expr(value, seg),
                StmtKind::IndexAssign { base, index, value } => {
                    self.rewrite_expr(base, seg);
                    self.rewrite_expr(index, seg);
                    self.rewrite_expr(value, seg);
                }
                StmtKind::FieldAssign { base, value, .. } => {
                    self.rewrite_expr(base, seg);
                    self.rewrite_expr(value, seg);
                }
                StmtKind::If {
                    cond,
                    then_body,
                    else_body,
                } => {
                    self.rewrite_expr(cond, seg);
                    self.locals.push(Vec::new());
                    self.rewrite_stmts(then_body, seg);
                    self.locals.pop();
                    self.locals.push(Vec::new());
                    self.rewrite_stmts(else_body, seg);
                    self.locals.pop();
                }
                StmtKind::While {
                    cond,
                    invariant,
                    body,
                } => {
                    self.rewrite_expr(cond, seg);
                    if let Some(inv) = invariant {
                        self.rewrite_expr(inv, seg);
                    }
                    self.locals.push(Vec::new());
                    self.rewrite_stmts(body, seg);
                    self.locals.pop();
                }
                StmtKind::For {
                    var,
                    start,
                    end,
                    body,
                } => {
                    self.rewrite_expr(start, seg);
                    self.rewrite_expr(end, seg);
                    self.locals.push(vec![var.clone()]);
                    self.rewrite_stmts(body, seg);
                    self.locals.pop();
                }
                StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                    self.rewrite_expr(e, seg)
                }
                _ => {}
            }
        }
    }

    fn rewrite_expr(&mut self, expr: &mut Expr, seg: usize) {
        match &mut expr.kind {
            ExprKind::Var(name) => {
                if let Some(new) = self.rename_ref(name, seg) {
                    *name = new;
                }
            }
            ExprKind::Call(name, _type_args, args) => {
                if let Some(new) = self.rename_ref(name, seg) {
                    *name = new;
                }
                for a in args {
                    self.rewrite_expr(a, seg);
                }
            }
            ExprKind::Array(elems) => {
                for e in elems {
                    self.rewrite_expr(e, seg);
                }
            }
            ExprKind::Index(a, b) => {
                self.rewrite_expr(a, seg);
                self.rewrite_expr(b, seg);
            }
            ExprKind::Field(a, _) => self.rewrite_expr(a, seg),
            ExprKind::Bin(_, a, b) => {
                self.rewrite_expr(a, seg);
                self.rewrite_expr(b, seg);
            }
            ExprKind::Un(_, a) => self.rewrite_expr(a, seg),
            ExprKind::Lambda(l) => {
                for p in &mut l.params {
                    self.rewrite_type(&mut p.ty, seg);
                }
                self.rewrite_type(&mut l.ret, seg);
                self.locals
                    .push(l.params.iter().map(|p| p.name.clone()).collect());
                let mut body = std::mem::take(&mut l.body);
                self.rewrite_stmts(&mut body, seg);
                l.body = body;
                self.locals.pop();
            }
            ExprKind::Match(m) => {
                self.rewrite_expr(&mut m.scrutinee, seg);
                for arm in &mut m.arms {
                    self.locals.push(Vec::new());
                    let mut pat = std::mem::replace(&mut arm.pattern, Pattern::Wildcard);
                    self.rewrite_pattern(&mut pat, seg);
                    arm.pattern = pat;
                    self.rewrite_expr(&mut arm.body, seg);
                    self.locals.pop();
                }
            }
            _ => {}
        }
    }
}
