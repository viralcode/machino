//! Type inference for generic functions.
//! Implements Hindley-Milner style unification for inferring type arguments.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use std::collections::HashMap;

/// A type inference variable (placeholder for unknown types).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TyVar(usize);

/// Type with inference variables.
#[derive(Debug, Clone, PartialEq)]
pub enum InferType {
    Concrete(Type),
    Var(TyVar),
}

/// Type inference context.
pub struct InferCtx {
    next_var: usize,
    substitution: HashMap<TyVar, InferType>,
}

impl InferCtx {
    pub fn new() -> Self {
        InferCtx {
            next_var: 0,
            substitution: HashMap::new(),
        }
    }
    
    /// Create a fresh type variable.
    pub fn fresh_var(&mut self) -> TyVar {
        let v = TyVar(self.next_var);
        self.next_var += 1;
        v
    }
    
    /// Apply current substitution to a type.
    fn apply(&self, ty: &InferType) -> InferType {
        match ty {
            InferType::Var(v) => {
                if let Some(t) = self.substitution.get(v) {
                    self.apply(t)
                } else {
                    InferType::Var(v.clone())
                }
            }
            InferType::Concrete(t) => match t {
                Type::Array(inner) => {
                    InferType::Concrete(Type::Array(Box::new(
                        self.concrete(&self.apply(&InferType::Concrete(*inner.clone())))
                    )))
                }
                Type::Fn(params, ret) => {
                    let params = params.iter().map(|p| {
                        self.concrete(&self.apply(&InferType::Concrete(p.clone())))
                    }).collect();
                    let ret = Box::new(self.concrete(&self.apply(&InferType::Concrete(*ret.clone()))));
                    InferType::Concrete(Type::Fn(params, ret))
                }
                _ => InferType::Concrete(t.clone()),
            }
        }
    }
    
    /// Convert InferType to concrete Type (panic on unresolved variables).
    fn concrete(&self, ty: &InferType) -> Type {
        match ty {
            InferType::Concrete(t) => t.clone(),
            InferType::Var(_) => panic!("unresolved type variable"),
        }
    }
    
    /// Check if a type variable occurs in a type (for occurs check).
    fn occurs(&self, v: &TyVar, ty: &InferType) -> bool {
        match self.apply(ty) {
            InferType::Var(u) => u == *v,
            InferType::Concrete(Type::Array(inner)) => {
                self.occurs(v, &InferType::Concrete(*inner))
            }
            InferType::Concrete(Type::Fn(params, ret)) => {
                params.iter().any(|p| self.occurs(v, &InferType::Concrete(p.clone())))
                    || self.occurs(v, &InferType::Concrete(*ret))
            }
            _ => false,
        }
    }
    
    /// Unify two types.
    pub fn unify(&mut self, a: &InferType, b: &InferType, span: Span) -> Result<(), Diagnostic> {
        let a = self.apply(a);
        let b = self.apply(b);
        
        match (&a, &b) {
            (InferType::Var(u), InferType::Var(v)) if u == v => Ok(()),
            (InferType::Var(u), t) | (t, InferType::Var(u)) => {
                if self.occurs(u, t) {
                    return Err(Diagnostic::new(
                        "E999",
                        "infinite type detected in unification".to_string(),
                        span,
                    ));
                }
                self.substitution.insert(u.clone(), t.clone());
                Ok(())
            }
            (InferType::Concrete(t1), InferType::Concrete(t2)) => {
                match (t1, t2) {
                    _ if t1 == t2 => Ok(()),
                    (Type::Array(inner1), Type::Array(inner2)) => {
                        self.unify(
                            &InferType::Concrete(*inner1.clone()),
                            &InferType::Concrete(*inner2.clone()),
                            span
                        )
                    }
                    (Type::Fn(params1, ret1), Type::Fn(params2, ret2)) => {
                        if params1.len() != params2.len() {
                            return Err(Diagnostic::new(
                                "E999",
                                format!("function type arity mismatch: {} vs {}", params1.len(), params2.len()),
                                span,
                            ));
                        }
                        for (p1, p2) in params1.iter().zip(params2.iter()) {
                            self.unify(&InferType::Concrete(p1.clone()), &InferType::Concrete(p2.clone()), span)?;
                        }
                        self.unify(&InferType::Concrete(*ret1.clone()), &InferType::Concrete(*ret2.clone()), span)
                    }
                    _ => Err(Diagnostic::new(
                        "E999",
                        format!("type mismatch: cannot unify '{}' with '{}'", t1, t2),
                        span,
                    )),
                }
            }
        }
    }
    
    /// Infer type arguments for a generic function call.
    pub fn infer_call_types(
        &mut self,
        func: &Function,
        args: &[Type],
        span: Span
    ) -> Result<Vec<Type>, Diagnostic> {
        // Create type variables for each type parameter
        let mut type_var_map: HashMap<String, TyVar> = HashMap::new();
        for tparam in &func.type_params {
            type_var_map.insert(tparam.clone(), self.fresh_var());
        }
        
        // Substitute type parameters in function parameters with type variables
        let infer_params: Vec<InferType> = func.params.iter()
            .map(|p| self.type_to_infer(&p.ty, &type_var_map))
            .collect();
        
        // Unify with actual argument types
        if args.len() != infer_params.len() {
            return Err(Diagnostic::new(
                "E999",
                format!("function '{}' expects {} arguments, got {}", func.name, func.params.len(), args.len()),
                span,
            ));
        }
        
        for (param_ty, arg_ty) in infer_params.iter().zip(args.iter()) {
            self.unify(param_ty, &InferType::Concrete(arg_ty.clone()), span)?;
        }
        
        // Extract solved type arguments
        let mut type_args = Vec::new();
        for tparam in &func.type_params {
            let var = type_var_map.get(tparam).unwrap();
            let solved = self.apply(&InferType::Var(var.clone()));
            match solved {
                InferType::Concrete(t) => type_args.push(t),
                InferType::Var(_) => {
                    return Err(Diagnostic::new(
                        "E999",
                        format!("could not infer type parameter '{}' for function '{}'", tparam, func.name),
                        span,
                    ).with_help("try providing explicit type arguments"));
                }
            }
        }
        
        Ok(type_args)
    }
    
    /// Convert a Type to InferType, replacing type parameters with variables.
    fn type_to_infer(&self, ty: &Type, var_map: &HashMap<String, TyVar>) -> InferType {
        match ty {
            Type::TypeVar(name) => {
                if let Some(v) = var_map.get(name) {
                    InferType::Var(v.clone())
                } else {
                    InferType::Concrete(ty.clone())
                }
            }
            Type::Array(inner) => {
                let inner_infer = self.type_to_infer(inner, var_map);
                match inner_infer {
                    InferType::Concrete(t) => InferType::Concrete(Type::Array(Box::new(t))),
                    _ => inner_infer, // Keep as variable if inner is a variable
                }
            }
            Type::Fn(params, ret) => {
                let params_infer: Vec<_> = params.iter()
                    .map(|p| match self.type_to_infer(p, var_map) {
                        InferType::Concrete(t) => t,
                        _ => p.clone(), // Fallback
                    })
                    .collect();
                let ret_infer = match self.type_to_infer(ret, var_map) {
                    InferType::Concrete(t) => t,
                    _ => (**ret).clone(),
                };
                InferType::Concrete(Type::Fn(params_infer, Box::new(ret_infer)))
            }
            _ => InferType::Concrete(ty.clone()),
        }
    }
}
