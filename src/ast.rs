//! Abstract syntax tree for machino.

use crate::diag::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Float,
    Bool,
    Str,
    Array(Box<Type>),
    /// A named struct type.
    Struct(String),
    /// A named enum type.
    Enum(String),
    /// A function value type: fn(params) -> ret.
    Fn(Vec<Type>, Box<Type>),
    /// Only valid as a function return type.
    Unit,
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::Float => write!(f, "float"),
            Type::Bool => write!(f, "bool"),
            Type::Str => write!(f, "str"),
            Type::Array(inner) => write!(f, "[{}]", inner),
            Type::Struct(name) => write!(f, "{}", name),
            Type::Enum(name) => write!(f, "{}", name),
            Type::Fn(params, ret) => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", p)?;
                }
                write!(f, ")")?;
                if **ret != Type::Unit {
                    write!(f, " -> {}", ret)?;
                }
                Ok(())
            }
            Type::Unit => write!(f, "unit"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    Var(String),
    Array(Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// Struct field access: base.field
    Field(Box<Expr>, String),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Un(UnOp, Box<Expr>),
    Call(String, Vec<Expr>),
    /// A lambda expression: fn(x: int) -> int { ... }
    /// Captures enclosing variables by value at creation.
    Lambda(Box<Lambda>),
    /// A match expression: match expr { Pattern => expr, ... }
    Match(Box<Match>),
}

#[derive(Debug, Clone)]
pub struct Match {
    pub scrutinee: Expr,
    pub arms: Vec<MatchArm>,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    /// Wildcard pattern: _
    Wildcard,
    /// Variable binding: x (binds the value to x)
    Var(String),
    /// Integer literal: 42
    Int(i64),
    /// Boolean literal: true or false
    Bool(bool),
    /// String literal: "hello"
    Str(String),
    /// Enum variant without payload: Option::None
    Variant(String, String),
    /// Enum variant with payload: Option::Some(x)
    VariantPayload(String, String, Box<Pattern>),
}

#[derive(Debug, Clone)]
pub struct Lambda {
    /// Unique per program, assigned by the parser.
    pub id: usize,
    pub params: Vec<Param>,
    pub ret: Type,
    pub body: Vec<Stmt>,
}

impl Lambda {
    /// Syntactic free variables of the lambda body: names referenced but not
    /// bound by the lambda's params or local declarations. Sorted and
    /// deduplicated so every backend sees the same capture order. Names that
    /// resolve to global functions/structs are filtered out by the caller
    /// (they are not variables).
    pub fn free_names(&self) -> Vec<String> {
        let mut bound: Vec<Vec<String>> =
            vec![self.params.iter().map(|p| p.name.clone()).collect()];
        let mut free: Vec<String> = Vec::new();
        free_in_stmts(&self.body, &mut bound, &mut free);
        free.sort();
        free.dedup();
        free
    }
}

fn is_bound(bound: &[Vec<String>], name: &str) -> bool {
    bound.iter().any(|frame| frame.iter().any(|n| n == name))
}

fn free_in_stmts(stmts: &[Stmt], bound: &mut Vec<Vec<String>>, free: &mut Vec<String>) {
    bound.push(Vec::new());
    for s in stmts {
        match &s.kind {
            StmtKind::Let { name, value, .. } => {
                free_in_expr(value, bound, free);
                bound.last_mut().unwrap().push(name.clone());
            }
            StmtKind::Assign { name, value } => {
                if !is_bound(bound, name) {
                    free.push(name.clone());
                }
                free_in_expr(value, bound, free);
            }
            StmtKind::IndexAssign { base, index, value } => {
                free_in_expr(base, bound, free);
                free_in_expr(index, bound, free);
                free_in_expr(value, bound, free);
            }
            StmtKind::FieldAssign { base, value, .. } => {
                free_in_expr(base, bound, free);
                free_in_expr(value, bound, free);
            }
            StmtKind::If {
                cond,
                then_body,
                else_body,
            } => {
                free_in_expr(cond, bound, free);
                free_in_stmts(then_body, bound, free);
                free_in_stmts(else_body, bound, free);
            }
            StmtKind::While { cond, body } => {
                free_in_expr(cond, bound, free);
                free_in_stmts(body, bound, free);
            }
            StmtKind::For {
                var,
                start,
                end,
                body,
            } => {
                free_in_expr(start, bound, free);
                free_in_expr(end, bound, free);
                bound.push(vec![var.clone()]);
                free_in_stmts(body, bound, free);
                bound.pop();
            }
            StmtKind::Return(Some(e)) | StmtKind::Assert(e) | StmtKind::Expr(e) => {
                free_in_expr(e, bound, free)
            }
            _ => {}
        }
    }
    bound.pop();
}

fn free_in_expr(expr: &Expr, bound: &mut Vec<Vec<String>>, free: &mut Vec<String>) {
    match &expr.kind {
        ExprKind::Var(name) => {
            if !is_bound(bound, name) {
                free.push(name.clone());
            }
        }
        ExprKind::Array(elems) => {
            for e in elems {
                free_in_expr(e, bound, free);
            }
        }
        ExprKind::Index(a, b) => {
            free_in_expr(a, bound, free);
            free_in_expr(b, bound, free);
        }
        ExprKind::Field(a, _) => free_in_expr(a, bound, free),
        ExprKind::Bin(_, a, b) => {
            free_in_expr(a, bound, free);
            free_in_expr(b, bound, free);
        }
        ExprKind::Un(_, a) => free_in_expr(a, bound, free),
        ExprKind::Call(name, args) => {
            if !is_bound(bound, name) {
                free.push(name.clone());
            }
            for a in args {
                free_in_expr(a, bound, free);
            }
        }
        ExprKind::Lambda(inner) => {
            // the inner lambda's free names that aren't bound here bubble up
            for n in inner.free_names() {
                if !is_bound(bound, &n) {
                    free.push(n);
                }
            }
        }
        ExprKind::Match(m) => {
            free_in_expr(&m.scrutinee, bound, free);
            for arm in &m.arms {
                // pattern bindings are scoped to the arm body
                bound.push(pattern_bindings(&arm.pattern));
                free_in_expr(&arm.body, bound, free);
                bound.pop();
            }
        }
        _ => {}
    }
}

fn pattern_bindings(pat: &Pattern) -> Vec<String> {
    match pat {
        Pattern::Var(name) => vec![name.clone()],
        Pattern::VariantPayload(_, _, inner) => pattern_bindings(inner),
        _ => vec![],
    }
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Let {
        name: String,
        ty: Option<Type>,
        value: Expr,
    },
    Assign {
        name: String,
        value: Expr,
    },
    IndexAssign {
        base: Expr,
        index: Expr,
        value: Expr,
    },
    FieldAssign {
        base: Expr,
        field: String,
        value: Expr,
    },
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    For {
        var: String,
        start: Expr,
        end: Expr,
        body: Vec<Stmt>,
    },
    Break,
    Continue,
    Return(Option<Expr>),
    Assert(Expr),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Contract {
    pub expr: Expr,
    /// Source text of the contract expression, used in failure messages.
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub requires: Vec<Contract>,
    pub ensures: Vec<Contract>,
    pub body: Vec<Stmt>,
    pub is_extern: bool,
    /// True for functions that come from the standard prelude.
    pub is_std: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<Param>,
    pub is_std: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<EnumVariant>,
    pub is_std: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TestBlock {
    pub name: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub functions: Vec<Function>,
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
    pub tests: Vec<TestBlock>,
    /// Import paths declared with `import "..."`; resolved by the loader,
    /// ignored when the bundled source is re-parsed.
    pub imports: Vec<(String, Span)>,
}
