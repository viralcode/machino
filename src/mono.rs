//! Monomorphization pass: converts generic functions into concrete instances.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::{HashMap, HashSet};

/// Information about a call to a generic function.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct GenericCall {
    function_name: String,
    type_args: Vec<Type>,
}

/// Monomorphize a program: replace generic functions with concrete instances.
pub fn monomorphize(program: &Program) -> Result<Program, Diagnostic> {
    let mut mono = Monomorphizer {
        program,
        instantiations: HashMap::new(),
        to_generate: Vec::new(),
    };
    
    // Find all generic function calls
    mono.collect_instantiations()?;
    
    // Generate concrete versions
    let mut new_functions = Vec::new();
    for (call, _) in &mono.instantiations {
        let concrete_fn = mono.instantiate_function(call)?;
        new_functions.push(concrete_fn);
    }
    
    // Build new program with monomorphized functions
    let mut functions = program.functions.clone();
    
    // Remove generic functions that have been instantiated
    functions.retain(|f| f.type_params.is_empty());
    
    // Add concrete instances
    functions.extend(new_functions);
    
    Ok(Program {
        imports: program.imports.clone(),
        functions,
        structs: program.structs.clone(),
        enums: program.enums.clone(),
        tests: program.tests.clone(),
    })
}

struct Monomorphizer<'a> {
    program: &'a Program,
    instantiations: HashMap<GenericCall, usize>,
    to_generate: Vec<GenericCall>,
}

impl<'a> Monomorphizer<'a> {
    fn collect_instantiations(&mut self) -> Result<(), Diagnostic> {
        // Scan all function bodies for generic calls
        for func in &self.program.functions {
            for stmt in &func.body {
                self.collect_from_stmt(stmt)?;
            }
        }
        Ok(())
    }
    
    fn collect_from_stmt(&mut self, stmt: &Stmt) -> Result<(), Diagnostic> {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.collect_from_expr(value),
            StmtKind::Assign { value, .. } => self.collect_from_expr(value),
            StmtKind::IndexAssign { base, index, value } => {
                self.collect_from_expr(base)?;
                self.collect_from_expr(index)?;
                self.collect_from_expr(value)
            }
            StmtKind::FieldAssign { base, value, .. } => {
                self.collect_from_expr(base)?;
                self.collect_from_expr(value)
            }
            StmtKind::If { cond, then_body, else_body } => {
                self.collect_from_expr(cond)?;
                for s in then_body {
                    self.collect_from_stmt(s)?;
                }
                for s in else_body {
                    self.collect_from_stmt(s)?;
                }
                Ok(())
            }
            StmtKind::While { cond, body } => {
                self.collect_from_expr(cond)?;
                for s in body {
                    self.collect_from_stmt(s)?;
                }
                Ok(())
            }
            StmtKind::For { start, end, body, .. } => {
                self.collect_from_expr(start)?;
                self.collect_from_expr(end)?;
                for s in body {
                    self.collect_from_stmt(s)?;
                }
                Ok(())
            }
            StmtKind::Return(Some(e)) => self.collect_from_expr(e),
            StmtKind::Assert(e) => self.collect_from_expr(e),
            StmtKind::Expr(e) => self.collect_from_expr(e),
            _ => Ok(()),
        }
    }
    
    fn collect_from_expr(&mut self, expr: &Expr) -> Result<(), Diagnostic> {
        match &expr.kind {
            ExprKind::Call(name, args) => {
                // Check if this is a generic function call
                if let Some(func) = self.program.functions.iter().find(|f| f.name == *name) {
                    if !func.type_params.is_empty() {
                        // TODO: infer type arguments from call site
                        // For now, we'll handle this in a later pass
                    }
                }
                for arg in args {
                    self.collect_from_expr(arg)?;
                }
                Ok(())
            }
            ExprKind::Bin(_, l, r) => {
                self.collect_from_expr(l)?;
                self.collect_from_expr(r)
            }
            ExprKind::Un(_, e) => self.collect_from_expr(e),
            ExprKind::Index(base, idx) => {
                self.collect_from_expr(base)?;
                self.collect_from_expr(idx)
            }
            ExprKind::Field(base, _) => self.collect_from_expr(base),
            ExprKind::Array(elems) => {
                for e in elems {
                    self.collect_from_expr(e)?;
                }
                Ok(())
            }
            ExprKind::Match(m) => {
                self.collect_from_expr(&m.scrutinee)?;
                for arm in &m.arms {
                    self.collect_from_expr(&arm.body)?;
                }
                Ok(())
            }
            ExprKind::Lambda(l) => {
                for s in &l.body {
                    self.collect_from_stmt(s)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    
    fn instantiate_function(&self, call: &GenericCall) -> Result<Function, Diagnostic> {
        let template = self.program.functions
            .iter()
            .find(|f| f.name == call.function_name)
            .ok_or_else(|| Diagnostic::new(
                "E999",
                format!("generic function '{}' not found", call.function_name),
                Span::new(0, 0),
            ))?;
        
        // Create substitution map
        let mut subst = HashMap::new();
        for (param, arg) in template.type_params.iter().zip(&call.type_args) {
            subst.insert(param.clone(), arg.clone());
        }
        
        // Generate new function name
        let mut type_suffix = String::new();
        for ty in &call.type_args {
            type_suffix.push('_');
            type_suffix.push_str(&format!("{}", ty).replace(|c: char| !c.is_alphanumeric(), "_"));
        }
        let new_name = format!("{}{}", template.name, type_suffix);
        
        // Substitute types in params and return type
        let params = template.params.iter().map(|p| Param {
            name: p.name.clone(),
            ty: self.substitute_type(&p.ty, &subst),
            span: p.span,
        }).collect();
        
        let ret = self.substitute_type(&template.ret, &subst);
        
        Ok(Function {
            name: new_name,
            type_params: vec![], // concrete version has no type params
            params,
            ret,
            requires: template.requires.clone(), // TODO: substitute in contracts
            ensures: template.ensures.clone(),
            body: template.body.clone(), // TODO: substitute in body
            is_extern: false,
            is_std: template.is_std,
            span: template.span,
        })
    }
    
    fn substitute_type(&self, ty: &Type, subst: &HashMap<String, Type>) -> Type {
        match ty {
            Type::TypeVar(name) => subst.get(name).cloned().unwrap_or(ty.clone()),
            Type::Array(inner) => Type::Array(Box::new(self.substitute_type(inner, subst))),
            Type::Fn(params, ret) => Type::Fn(
                params.iter().map(|p| self.substitute_type(p, subst)).collect(),
                Box::new(self.substitute_type(ret, subst)),
            ),
            _ => ty.clone(),
        }
    }
}
