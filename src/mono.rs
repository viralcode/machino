//! Monomorphization pass: converts generic functions into concrete instances.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::infer::InferCtx;
use std::collections::HashMap;

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
        struct_instantiations: HashMap::new(),
        enum_instantiations: HashMap::new(),
        to_generate: Vec::new(),
    };
    
    // Find all generic function calls and type usages
    mono.collect_instantiations()?;
    
    // Generate concrete versions of functions
    let mut new_functions = Vec::new();
    for (call, _) in &mono.instantiations {
        let concrete_fn = mono.instantiate_function(call)?;
        new_functions.push(concrete_fn);
    }
    
    // Generate concrete versions of structs
    let mut new_structs = Vec::new();
    for (call, _) in &mono.struct_instantiations {
        let concrete_struct = mono.instantiate_struct(call)?;
        new_structs.push(concrete_struct);
    }
    
    // Generate concrete versions of enums
    let mut new_enums = Vec::new();
    for (call, _) in &mono.enum_instantiations {
        let concrete_enum = mono.instantiate_enum(call)?;
        new_enums.push(concrete_enum);
    }
    
    // Build new program with monomorphized definitions
    let mut functions = program.functions.clone();
    let mut structs = program.structs.clone();
    let mut enums = program.enums.clone();
    
    // Remove generic definitions that have been instantiated
    functions.retain(|f| f.type_params.is_empty());
    structs.retain(|s| s.type_params.is_empty());
    enums.retain(|e| e.type_params.is_empty());
    
    // Add concrete instances
    functions.extend(new_functions);
    structs.extend(new_structs);
    enums.extend(new_enums);
    
    Ok(Program {
        imports: program.imports.clone(),
        functions,
        structs,
        enums,
        tests: program.tests.clone(),
    })
}

struct Monomorphizer<'a> {
    program: &'a Program,
    instantiations: HashMap<GenericCall, usize>,
    struct_instantiations: HashMap<GenericCall, usize>,
    enum_instantiations: HashMap<GenericCall, usize>,
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
                        // Infer type arguments from call site
                        let type_args = self.infer_type_args(func, args, expr.span)?;
                        
                        let call = GenericCall {
                            function_name: name.clone(),
                            type_args,
                        };
                        
                        if !self.instantiations.contains_key(&call) {
                            let idx = self.instantiations.len();
                            self.instantiations.insert(call.clone(), idx);
                            self.to_generate.push(call);
                        }
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
        
        // Substitute in contracts
        let requires = template.requires.iter()
            .map(|c| self.substitute_contract(c, &subst))
            .collect();
        let ensures = template.ensures.iter()
            .map(|c| self.substitute_contract(c, &subst))
            .collect();
        
        // Substitute in body
        let body = template.body.iter()
            .map(|s| self.substitute_stmt(s, &subst))
            .collect();
        
        Ok(Function {
            name: new_name,
            type_params: vec![], // concrete version has no type params
            params,
            ret,
            requires,
            ensures,
            body,
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
    
    fn infer_type_args(&self, func: &Function, args: &[Expr], span: Span) -> Result<Vec<Type>, Diagnostic> {
        // Simple type inference: we need type information from the checker
        // For now, return an error saying we can't infer types yet
        // In a full implementation, we would get type info from the checker
        Err(Diagnostic::new(
            "E999",
            format!("type inference for generic function '{}' not yet implemented; explicit type arguments required", func.name),
            span,
        ).with_help("generics support is in development; use concrete types for now"))
    }
    
    fn substitute_expr(&self, expr: &Expr, subst: &HashMap<String, Type>) -> Expr {
        Expr {
            kind: match &expr.kind {
                ExprKind::Call(name, args) => {
                    ExprKind::Call(name.clone(), args.iter().map(|a| self.substitute_expr(a, subst)).collect())
                }
                ExprKind::Bin(op, l, r) => {
                    ExprKind::Bin(*op, Box::new(self.substitute_expr(l, subst)), Box::new(self.substitute_expr(r, subst)))
                }
                ExprKind::Un(op, e) => {
                    ExprKind::Un(*op, Box::new(self.substitute_expr(e, subst)))
                }
                ExprKind::Index(base, idx) => {
                    ExprKind::Index(Box::new(self.substitute_expr(base, subst)), Box::new(self.substitute_expr(idx, subst)))
                }
                ExprKind::Field(base, field) => {
                    ExprKind::Field(Box::new(self.substitute_expr(base, subst)), field.clone())
                }
                ExprKind::Array(elems) => {
                    ExprKind::Array(elems.iter().map(|e| self.substitute_expr(e, subst)).collect())
                }
                ExprKind::Match(m) => {
                    ExprKind::Match(Box::new(Match {
                        scrutinee: self.substitute_expr(&m.scrutinee, subst),
                        arms: m.arms.iter().map(|arm| MatchArm {
                            pattern: arm.pattern.clone(), // Patterns don't need substitution
                            body: self.substitute_expr(&arm.body, subst),
                            span: arm.span,
                        }).collect(),
                    }))
                }
                ExprKind::Lambda(l) => {
                    ExprKind::Lambda(Box::new(Lambda {
                        id: l.id,
                        params: l.params.iter().map(|p| Param {
                            name: p.name.clone(),
                            ty: self.substitute_type(&p.ty, subst),
                            span: p.span,
                        }).collect(),
                        ret: self.substitute_type(&l.ret, subst),
                        body: l.body.iter().map(|s| self.substitute_stmt(s, subst)).collect(),
                    }))
                }
                _ => expr.kind.clone(),
            },
            span: expr.span,
        }
    }
    
    fn substitute_stmt(&self, stmt: &Stmt, subst: &HashMap<String, Type>) -> Stmt {
        Stmt {
            kind: match &stmt.kind {
                StmtKind::Let { name, ty, value } => {
                    StmtKind::Let {
                        name: name.clone(),
                        ty: ty.as_ref().map(|t| self.substitute_type(t, subst)),
                        value: self.substitute_expr(value, subst),
                    }
                }
                StmtKind::Assign { name, value } => {
                    StmtKind::Assign {
                        name: name.clone(),
                        value: self.substitute_expr(value, subst),
                    }
                }
                StmtKind::IndexAssign { base, index, value } => {
                    StmtKind::IndexAssign {
                        base: self.substitute_expr(base, subst),
                        index: self.substitute_expr(index, subst),
                        value: self.substitute_expr(value, subst),
                    }
                }
                StmtKind::FieldAssign { base, field, value } => {
                    StmtKind::FieldAssign {
                        base: self.substitute_expr(base, subst),
                        field: field.clone(),
                        value: self.substitute_expr(value, subst),
                    }
                }
                StmtKind::If { cond, then_body, else_body } => {
                    StmtKind::If {
                        cond: self.substitute_expr(cond, subst),
                        then_body: then_body.iter().map(|s| self.substitute_stmt(s, subst)).collect(),
                        else_body: else_body.iter().map(|s| self.substitute_stmt(s, subst)).collect(),
                    }
                }
                StmtKind::While { cond, body } => {
                    StmtKind::While {
                        cond: self.substitute_expr(cond, subst),
                        body: body.iter().map(|s| self.substitute_stmt(s, subst)).collect(),
                    }
                }
                StmtKind::For { var, start, end, body } => {
                    StmtKind::For {
                        var: var.clone(),
                        start: self.substitute_expr(start, subst),
                        end: self.substitute_expr(end, subst),
                        body: body.iter().map(|s| self.substitute_stmt(s, subst)).collect(),
                    }
                }
                StmtKind::Return(Some(e)) => StmtKind::Return(Some(self.substitute_expr(e, subst))),
                StmtKind::Assert(e) => StmtKind::Assert(self.substitute_expr(e, subst)),
                StmtKind::Expr(e) => StmtKind::Expr(self.substitute_expr(e, subst)),
                _ => stmt.kind.clone(),
            },
            span: stmt.span,
        }
    }
    
    fn substitute_contract(&self, contract: &Contract, subst: &HashMap<String, Type>) -> Contract {
        Contract {
            expr: self.substitute_expr(&contract.expr, subst),
            text: contract.text.clone(),
        }
    }

fn instantiate_struct(&self, call: &GenericCall) -> Result<StructDef, Diagnostic> {
    let template = self.program.structs
        .iter()
        .find(|s| s.name == call.function_name)
        .ok_or_else(|| Diagnostic::new(
            "E999",
            format!("generic struct '{}' not found", call.function_name),
            Span::new(0, 0),
        ))?;
    
    // Create substitution map
    let mut subst = HashMap::new();
    for (param, arg) in template.type_params.iter().zip(&call.type_args) {
        subst.insert(param.clone(), arg.clone());
    }
    
    // Generate new struct name
    let new_name = self.mangle_name(&template.name, &call.type_args);
    
    // Substitute types in fields
    let fields = template.fields.iter().map(|f| Param {
        name: f.name.clone(),
        ty: self.substitute_type(&f.ty, &subst),
        span: f.span,
    }).collect();
    
    Ok(StructDef {
        name: new_name,
        type_params: vec![],
        fields,
        is_std: template.is_std,
        span: template.span,
    })
}

fn instantiate_enum(&self, call: &GenericCall) -> Result<EnumDef, Diagnostic> {
    let template = self.program.enums
        .iter()
        .find(|e| e.name == call.function_name)
        .ok_or_else(|| Diagnostic::new(
            "E999",
            format!("generic enum '{}' not found", call.function_name),
            Span::new(0, 0),
        ))?;
    
    // Create substitution map
    let mut subst = HashMap::new();
    for (param, arg) in template.type_params.iter().zip(&call.type_args) {
        subst.insert(param.clone(), arg.clone());
    }
    
    // Generate new enum name
    let new_name = self.mangle_name(&template.name, &call.type_args);
    
    // Substitute types in variants
    let variants = template.variants.iter().map(|v| EnumVariant {
        name: v.name.clone(),
        payload: v.payload.as_ref().map(|ty| self.substitute_type(ty, &subst)),
        span: v.span,
    }).collect();
    
    Ok(EnumDef {
        name: new_name,
        type_params: vec![],
        variants,
        is_std: template.is_std,
        span: template.span,
    })
}

fn mangle_name(&self, base: &str, type_args: &[Type]) -> String {
    let mut name = base.to_string();
    for ty in type_args {
        name.push('_');
        name.push_str(&self.type_to_string(ty));
    }
    name
}

fn type_to_string(&self, ty: &Type) -> String {
    match ty {
        Type::Int => "int".to_string(),
        Type::Float => "float".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Str => "str".to_string(),
        Type::Unit => "unit".to_string(),
        Type::Array(inner) => format!("arr_{}", self.type_to_string(inner)),
        Type::Struct(name) => name.replace("::", "_"),
        Type::Enum(name) => name.replace("::", "_"),
        Type::Fn(params, ret) => {
            let ps: Vec<_> = params.iter().map(|p| self.type_to_string(p)).collect();
            format!("fn_{}_{}", ps.join("_"), self.type_to_string(ret))
        }
        Type::TypeVar(name) => name.clone(),
    }
}
}
