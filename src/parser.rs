//! Recursive-descent parser. The grammar is deliberately small and regular:
//! there is exactly one way to write each construct.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{Tok, Token};

pub struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    source: &'a str,
    lambda_counter: usize,
    /// Type parameters of the item currently being parsed. Identifiers in
    /// type position that match one of these become Type::TypeVar.
    current_type_params: Vec<String>,
}

type PResult<T> = Result<T, Diagnostic>;

/// Constraint bounds a type parameter may declare.
const VALID_BOUNDS: &[&str] = &["Eq", "Ord", "Num", "Hash"];

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token], source: &'a str) -> Self {
        Parser {
            tokens,
            pos: 0,
            source,
            lambda_counter: 0,
            current_type_params: Vec::new(),
        }
    }

    /// Parses `<T, U: Ord, ...>` after a fn/struct/enum name, registering the
    /// names so parse_type resolves them as type variables.
    fn parse_type_params(&mut self) -> PResult<Vec<TypeParam>> {
        self.current_type_params.clear();
        let mut type_params = Vec::new();
        if self.eat(&Tok::Lt) {
            loop {
                let tspan = self.peek_span();
                let tname = self.parse_ident("type parameter")?;
                let mut bounds = Vec::new();
                if self.eat(&Tok::Colon) {
                    bounds = self.parse_bounds()?;
                }
                self.current_type_params.push(tname.clone());
                type_params.push(TypeParam {
                    name: tname,
                    bounds,
                    span: tspan,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Gt, "'>' after type parameters")?;
        }
        Ok(type_params)
    }

    /// Parses `Bound` or `Bound + Bound + ...` after a ':' in type-parameter
    /// or where-clause position.
    fn parse_bounds(&mut self) -> PResult<Vec<String>> {
        let mut bounds = Vec::new();
        loop {
            let bspan = self.peek_span();
            let bound = self.parse_ident("constraint name")?;
            if !VALID_BOUNDS.contains(&bound.as_str()) {
                return Err(Diagnostic::new(
                    "E067",
                    format!("unknown constraint '{}'", bound),
                    bspan,
                )
                .with_help("valid constraints: Eq (== !=), Ord (< <= > >=), Num (+ - * /), Hash (hash())"));
            }
            if !bounds.contains(&bound) {
                bounds.push(bound);
            }
            if !self.eat(&Tok::Plus) {
                break;
            }
        }
        Ok(bounds)
    }

    /// Parses an optional `where T: Ord + Num, U: Eq` clause, merging the
    /// bounds into the already-declared type parameters.
    fn parse_where_clause(&mut self, type_params: &mut [TypeParam]) -> PResult<()> {
        if !self.eat(&Tok::Where) {
            return Ok(());
        }
        loop {
            let tspan = self.peek_span();
            let tname = self.parse_ident("type parameter in where clause")?;
            self.expect(Tok::Colon, "':' after type parameter in where clause")?;
            let bounds = self.parse_bounds()?;
            let Some(tp) = type_params.iter_mut().find(|tp| tp.name == tname) else {
                return Err(Diagnostic::new(
                    "E073",
                    format!("where clause names unknown type parameter '{}'", tname),
                    tspan,
                )
                .with_help("declare the parameter in angle brackets first: fn<T> ... where T: Ord"));
            };
            for b in bounds {
                if !tp.bounds.contains(&b) {
                    tp.bounds.push(b);
                }
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        Ok(())
    }

    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn peek_span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: Tok, what: &str) -> PResult<Token> {
        if self.peek() == &tok {
            Ok(self.advance())
        } else {
            Err(Diagnostic::new(
                "E010",
                format!("expected {}, found {}", what, describe(self.peek())),
                self.peek_span(),
            ))
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Tok::Newline) {
            self.advance();
        }
    }

    /// A statement ends at a newline or just before '}' / EOF.
    fn expect_stmt_end(&mut self) -> PResult<()> {
        match self.peek() {
            Tok::Newline => {
                self.advance();
                Ok(())
            }
            Tok::RBrace | Tok::Eof => Ok(()),
            other => Err(Diagnostic::new(
                "E011",
                format!("expected end of statement, found {}", describe(other)),
                self.peek_span(),
            )
            .with_help("each statement goes on its own line")),
        }
    }

    pub fn parse_program(&mut self) -> PResult<Program> {
        let mut functions = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        let mut tests = Vec::new();
        let mut imports = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Tok::Eof => break,
                Tok::Fn => functions.push(self.parse_function(false)?),
                Tok::Extern => {
                    self.advance();
                    functions.push(self.parse_function(true)?);
                }
                Tok::Struct => structs.push(self.parse_struct()?),
                Tok::Enum => enums.push(self.parse_enum()?),
                Tok::Test => tests.push(self.parse_test()?),
                Tok::Import => {
                    let tok = self.advance();
                    match self.advance() {
                        Token {
                            tok: Tok::Str(path),
                            span,
                        } => {
                            // optional namespace: import "path" as alias
                            let mut alias = None;
                            if matches!(self.peek(), Tok::Ident(w) if w == "as") {
                                self.advance();
                                alias = Some(self.parse_ident("namespace alias after 'as'")?);
                            }
                            imports.push((path, alias, tok.span.merge(span)));
                            self.expect_stmt_end()?;
                        }
                        t => {
                            return Err(Diagnostic::new(
                                "E014",
                                format!("expected import path string, found {}", describe(&t.tok)),
                                t.span,
                            )
                            .with_help("imports are written: import \"lib/util.mno\""));
                        }
                    }
                }
                other => {
                    return Err(Diagnostic::new(
                        "E012",
                        format!(
                            "expected 'fn', 'extern fn', 'struct', 'test' or 'import' at top level, found {}",
                            describe(other)
                        ),
                        self.peek_span(),
                    )
                    .with_help("all code lives inside functions; the entry point is 'fn main()'"));
                }
            }
        }
        Ok(Program {
            functions,
            structs,
            enums,
            tests,
            imports,
        })
    }

    fn parse_struct(&mut self) -> PResult<StructDef> {
        let struct_tok = self.expect(Tok::Struct, "'struct'")?;
        let mut type_params = self.parse_type_params()?;
        let name = self.parse_ident("struct name")?;
        self.parse_where_clause(&mut type_params)?;
        self.skip_newlines();
        self.expect(Tok::LBrace, "'{' to open struct body")?;
        let mut fields = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace) {
                self.advance();
                break;
            }
            let fspan = self.peek_span();
            let fname = self.parse_ident("field name")?;
            self.expect(Tok::Colon, "':' after field name")?;
            let ty = self.parse_type()?;
            fields.push(Param {
                name: fname,
                ty,
                span: fspan,
            });
            self.expect_stmt_end()?;
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        if fields.is_empty() {
            return Err(Diagnostic::new(
                "E013",
                format!("struct '{}' has no fields", name),
                struct_tok.span.merge(end),
            )
            .with_help("declare at least one field: struct Point { x: int }"));
        }
        Ok(StructDef {
            name,
            type_params,
            fields,
            is_std: false,
            span: struct_tok.span.merge(end),
        })
    }

    fn parse_enum(&mut self) -> PResult<EnumDef> {
        let enum_tok = self.expect(Tok::Enum, "'enum'")?;
        let mut type_params = self.parse_type_params()?;
        let name = self.parse_ident("enum name")?;
        self.parse_where_clause(&mut type_params)?;
        self.skip_newlines();
        self.expect(Tok::LBrace, "'{' to open enum body")?;
        let mut variants = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace) {
                self.advance();
                break;
            }
            let vspan = self.peek_span();
            let vname = self.parse_ident("variant name")?;
            let payloads = if self.eat(&Tok::LParen) {
                let mut types = Vec::new();
                if !matches!(self.peek(), Tok::RParen) {
                    loop {
                        types.push(self.parse_type()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen, "')' after variant payload type(s)")?;
                types
            } else {
                Vec::new()
            };
            variants.push(EnumVariant {
                name: vname,
                payloads,
                span: vspan,
            });
            self.expect_stmt_end()?;
        }
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        if variants.is_empty() {
            return Err(Diagnostic::new(
                "E051",
                format!("enum '{}' has no variants", name),
                enum_tok.span.merge(end),
            )
            .with_help("declare at least one variant: enum Option { None }")); 
        }
        Ok(EnumDef {
            name,
            type_params,
            variants,
            is_std: false,
            span: enum_tok.span.merge(end),
        })
    }

    fn parse_function(&mut self, is_extern: bool) -> PResult<Function> {
        let fn_tok = self.expect(Tok::Fn, "'fn'")?;
        let type_params = self.parse_type_params()?;
        let name = self.parse_ident("function name")?;
        self.expect(Tok::LParen, "'(' after function name")?;
        let mut params = Vec::new();
        if !matches!(self.peek(), Tok::RParen) {
            loop {
                let pstart = self.peek_span();
                let pname = self.parse_ident("parameter name")?;
                self.expect(Tok::Colon, "':' after parameter name")?;
                let ty = self.parse_type()?;
                params.push(Param {
                    name: pname,
                    ty,
                    span: pstart,
                });
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RParen, "')' after parameters")?;
        let ret = if self.eat(&Tok::Arrow) {
            self.parse_type()?
        } else {
            Type::Unit
        };
        let mut type_params = type_params;
        self.parse_where_clause(&mut type_params)?;

        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Tok::Requires => {
                    self.advance();
                    requires.push(self.parse_contract()?);
                }
                Tok::Ensures => {
                    self.advance();
                    ensures.push(self.parse_contract()?);
                }
                _ => break,
            }
        }

        if is_extern {
            if matches!(self.peek(), Tok::LBrace) {
                return Err(Diagnostic::new(
                    "E013",
                    "extern functions cannot have a body",
                    self.peek_span(),
                )
                .with_help("extern declares a host-provided import; remove the '{ ... }' body"));
            }
            let end = self.peek_span();
            return Ok(Function {
                name,
                type_params,
                params,
                ret,
                requires,
                ensures,
                body: Vec::new(),
                is_extern: true,
                is_std: false,
                span: fn_tok.span.merge(end),
            });
        }

        let body = self.parse_block()?;
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Function {
            name,
            type_params,
            params,
            ret,
            requires,
            ensures,
            body,
            is_extern: false,
            is_std: false,
            span: fn_tok.span.merge(end),
        })
    }

    fn parse_contract(&mut self) -> PResult<Contract> {
        let start = self.peek_span();
        let expr = self.parse_expr()?;
        let end = expr.span;
        let text = self.source[start.start as usize..end.end as usize]
            .trim()
            .to_string();
        // contract clause ends at newline
        if matches!(self.peek(), Tok::Newline) {
            self.advance();
        }
        Ok(Contract { expr, text })
    }

    fn parse_test(&mut self) -> PResult<TestBlock> {
        self.current_type_params.clear();
        let test_tok = self.expect(Tok::Test, "'test'")?;
        let name = match self.advance() {
            Token {
                tok: Tok::Str(s), ..
            } => s,
            t => {
                return Err(Diagnostic::new(
                    "E014",
                    format!("expected test name string, found {}", describe(&t.tok)),
                    t.span,
                )
                .with_help("tests are written: test \"name\" { ... }"));
            }
        };
        // snapshot form: test "name" expects "output" { ... }
        let mut expects = None;
        if matches!(self.peek(), Tok::Ident(w) if w == "expects") {
            self.advance();
            match self.advance() {
                Token {
                    tok: Tok::Str(s), ..
                } => expects = Some(s),
                t => {
                    return Err(Diagnostic::new(
                        "E014",
                        format!("expected expected-output string, found {}", describe(&t.tok)),
                        t.span,
                    )
                    .with_help("snapshot tests are written: test \"name\" expects \"output\" { ... }"));
                }
            }
        }
        let body = self.parse_block()?;
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(TestBlock {
            name,
            expects,
            body,
            span: test_tok.span.merge(end),
        })
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.skip_newlines();
        self.expect(Tok::LBrace, "'{'")?;
        let mut stmts = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Tok::RBrace) {
                self.advance();
                break;
            }
            if matches!(self.peek(), Tok::Eof) {
                return Err(Diagnostic::new(
                    "E015",
                    "unexpected end of file inside block",
                    self.peek_span(),
                )
                .with_help("add the missing '}'"));
            }
            stmts.push(self.parse_stmt()?);
        }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let start = self.peek_span();
        match self.peek().clone() {
            Tok::Let => {
                self.advance();
                let name = self.parse_ident("variable name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(Tok::Assign, "'=' in let binding")?;
                let value = self.parse_expr()?;
                let span = start.merge(value.span);
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Let { name, ty, value },
                    span,
                })
            }
            Tok::If => {
                self.advance();
                let cond = self.parse_expr()?;
                let then_body = self.parse_block()?;
                let mut else_body = Vec::new();
                // allow 'else' on the same line as '}' or the next line
                let save = self.pos;
                self.skip_newlines();
                if self.eat(&Tok::Else) {
                    if matches!(self.peek(), Tok::If) {
                        else_body.push(self.parse_stmt()?);
                    } else {
                        else_body = self.parse_block()?;
                    }
                } else {
                    self.pos = save;
                }
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                Ok(Stmt {
                    kind: StmtKind::If {
                        cond,
                        then_body,
                        else_body,
                    },
                    span: start.merge(end),
                })
            }
            Tok::While => {
                self.advance();
                let cond = self.parse_expr()?;
                let invariant = if self.eat(&Tok::Invariant) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let body = self.parse_block()?;
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                Ok(Stmt {
                    kind: StmtKind::While {
                        cond,
                        invariant,
                        body,
                    },
                    span: start.merge(end),
                })
            }
            Tok::For => {
                self.advance();
                let var = self.parse_ident("loop variable name")?;
                self.expect(Tok::In, "'in' after loop variable")?;
                let range_start = self.parse_expr()?;
                self.expect(Tok::DotDot, "'..' in range")
                    .map_err(|d| d.with_help("for loops iterate over a range: for i in 0..n { ... }"))?;
                let range_end = self.parse_expr()?;
                let body = self.parse_block()?;
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                Ok(Stmt {
                    kind: StmtKind::For {
                        var,
                        start: range_start,
                        end: range_end,
                        body,
                    },
                    span: start.merge(end),
                })
            }
            Tok::Break => {
                self.advance();
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Break,
                    span: start,
                })
            }
            Tok::Continue => {
                self.advance();
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Continue,
                    span: start,
                })
            }
            Tok::Return => {
                self.advance();
                let value = if matches!(self.peek(), Tok::Newline | Tok::RBrace | Tok::Eof) {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                let span = match &value {
                    Some(e) => start.merge(e.span),
                    None => start,
                };
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Return(value),
                    span,
                })
            }
            Tok::Assert => {
                self.advance();
                let expr = self.parse_expr()?;
                let span = start.merge(expr.span);
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Assert(expr),
                    span,
                })
            }
            Tok::Ident(_) => {
                // parse an expression, then decide: assignment or expression stmt
                let expr = self.parse_expr()?;
                if matches!(self.peek(), Tok::Assign) {
                    self.advance();
                    let value = self.parse_expr()?;
                    let span = start.merge(value.span);
                    self.expect_stmt_end()?;
                    let kind = match expr.kind {
                        ExprKind::Var(name) => StmtKind::Assign { name, value },
                        ExprKind::Index(base, index) => StmtKind::IndexAssign {
                            base: *base,
                            index: *index,
                            value,
                        },
                        ExprKind::Field(base, field) => StmtKind::FieldAssign {
                            base: *base,
                            field,
                            value,
                        },
                        _ => {
                            return Err(Diagnostic::new(
                                "E016",
                                "invalid assignment target",
                                expr.span,
                            )
                            .with_help(
                                "you can assign to a variable, an array element xs[i], or a struct field p.x",
                            ));
                        }
                    };
                    return Ok(Stmt { kind, span });
                }
                let span = expr.span;
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Expr(expr),
                    span,
                })
            }
            Tok::Match => {
                // Parse match as an expression statement
                let expr = self.parse_expr()?;
                let span = start.merge(expr.span);
                self.expect_stmt_end()?;
                Ok(Stmt {
                    kind: StmtKind::Expr(expr),
                    span,
                })
            }
            other => Err(Diagnostic::new(
                "E016",
                format!("expected a statement, found {}", describe(&other)),
                start,
            )),
        }
    }

    fn parse_ident(&mut self, what: &str) -> PResult<String> {
        match self.advance() {
            Token {
                tok: Tok::Ident(s), ..
            } => Ok(s),
            t => Err(Diagnostic::new(
                "E017",
                format!("expected {}, found {}", what, describe(&t.tok)),
                t.span,
            )),
        }
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        let t = self.advance();
        match t.tok {
            Tok::Ident(ref name) if name == "_" => Ok(Pattern::Wildcard),
            Tok::Ident(name) => {
                // could be: variable binding, Enum::Variant, or (with a
                // namespaced import) alias::Enum::Variant
                let mut segments = vec![name];
                while self.eat(&Tok::Colon) && self.eat(&Tok::Colon) {
                    segments.push(self.parse_ident("variant name")?);
                }
                if segments.len() == 1 {
                    // variable binding
                    return Ok(Pattern::Var(segments.pop().unwrap()));
                }
                let variant = segments.pop().unwrap();
                let enum_name = segments.join("::");
                if self.eat(&Tok::LParen) {
                    let mut inners = Vec::new();
                    if !matches!(self.peek(), Tok::RParen) {
                        loop {
                            inners.push(self.parse_pattern()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen, "')' after variant payload pattern(s)")?;
                    Ok(Pattern::VariantPayload(enum_name, variant, inners))
                } else {
                    Ok(Pattern::Variant(enum_name, variant))
                }
            }
            Tok::Int(v) => Ok(Pattern::Int(v)),
            Tok::True => Ok(Pattern::Bool(true)),
            Tok::False => Ok(Pattern::Bool(false)),
            Tok::Str(s) => Ok(Pattern::Str(s)),
            other => Err(Diagnostic::new(
                "E053",
                format!("expected a pattern, found {}", describe(&other)),
                t.span,
            )),
        }
    }

    fn parse_type(&mut self) -> PResult<Type> {
        match self.advance() {
            Token {
                tok: Tok::Ident(s), ..
            } => match s.as_str() {
                "int" => Ok(Type::Int),
                "float" => Ok(Type::Float),
                "bool" => Ok(Type::Bool),
                "str" => Ok(Type::Str),
                // a declared type parameter is a type variable; any other
                // identifier is a struct name validated by the type checker
                _ if self.current_type_params.contains(&s) => Ok(Type::TypeVar(s)),
                _ => {
                    // qualified name from a namespaced import: alias::Type
                    let mut name = s;
                    while self.eat(&Tok::Colon) && self.eat(&Tok::Colon) {
                        let seg = self.parse_ident("type name after '::'")?;
                        name = format!("{}::{}", name, seg);
                    }
                    // Generic application: Name<T, U> (nested >> is two Gt tokens)
                    if self.eat(&Tok::Lt) {
                        let open_span = self.peek_span();
                        let mut args = Vec::new();
                        if !matches!(self.peek(), Tok::Gt) {
                            loop {
                                args.push(self.parse_type()?);
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                        }
                        self.expect(Tok::Gt, "'>' to close type arguments")?;
                        if args.is_empty() {
                            return Err(Diagnostic::new(
                                "E018",
                                format!("expected at least one type argument for '{}'", name),
                                open_span,
                            ));
                        }
                        Ok(Type::App(name, args))
                    } else {
                        Ok(Type::Struct(name))
                    }
                }
            },
            Token {
                tok: Tok::LBracket, ..
            } => {
                let inner = self.parse_type()?;
                self.expect(Tok::RBracket, "']' to close array type")?;
                Ok(Type::Array(Box::new(inner)))
            }
            Token { tok: Tok::Fn, .. } => {
                self.expect(Tok::LParen, "'(' in function type")?;
                let mut params = Vec::new();
                if !matches!(self.peek(), Tok::RParen) {
                    loop {
                        params.push(self.parse_type()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen, "')' in function type")?;
                let ret = if self.eat(&Tok::Arrow) {
                    self.parse_type()?
                } else {
                    Type::Unit
                };
                Ok(Type::Fn(params, Box::new(ret)))
            }
            t => Err(Diagnostic::new(
                "E018",
                format!("expected a type, found {}", describe(&t.tok)),
                t.span,
            )
            .with_help(
                "valid types: int, float, bool, str, [T], fn(T...) -> R, Name<T,...>, or a struct name",
            )),
        }
    }

    // ---- expressions (precedence climbing) ----

    pub fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Tok::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Bin(BinOp::Or, Box::new(lhs), Box::new(rhs)),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_cmp()?;
        while matches!(self.peek(), Tok::AndAnd) {
            self.advance();
            let rhs = self.parse_cmp()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Bin(BinOp::And, Box::new(lhs), Box::new(rhs)),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> PResult<Expr> {
        let lhs = self.parse_add()?;
        let op = match self.peek() {
            Tok::EqEq => BinOp::Eq,
            Tok::NotEq => BinOp::Ne,
            Tok::Lt => BinOp::Lt,
            Tok::LtEq => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::GtEq => BinOp::Ge,
            _ => return Ok(lhs),
        };
        self.advance();
        let rhs = self.parse_add()?;
        let span = lhs.span.merge(rhs.span);
        Ok(Expr {
            kind: ExprKind::Bin(op, Box::new(lhs), Box::new(rhs)),
            span,
        })
    }

    fn parse_add(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Bin(op, Box::new(lhs), Box::new(rhs)),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            let span = lhs.span.merge(rhs.span);
            lhs = Expr {
                kind: ExprKind::Bin(op, Box::new(lhs), Box::new(rhs)),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.peek_span();
        match self.peek() {
            Tok::Minus => {
                self.advance();
                let inner = self.parse_unary()?;
                let span = start.merge(inner.span);
                Ok(Expr {
                    kind: ExprKind::Un(UnOp::Neg, Box::new(inner)),
                    span,
                })
            }
            Tok::Bang => {
                self.advance();
                let inner = self.parse_unary()?;
                let span = start.merge(inner.span);
                Ok(Expr {
                    kind: ExprKind::Un(UnOp::Not, Box::new(inner)),
                    span,
                })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_atom()?;
        loop {
            match self.peek() {
                Tok::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    let rb = self.expect(Tok::RBracket, "']' to close index")?;
                    let span = expr.span.merge(rb.span);
                    expr = Expr {
                        kind: ExprKind::Index(Box::new(expr), Box::new(index)),
                        span,
                    };
                }
                Tok::Dot => {
                    self.advance();
                    let fspan = self.peek_span();
                    let field = self.parse_ident("field name after '.'")?;
                    if matches!(self.peek(), Tok::LParen) {
                        return Err(Diagnostic::new(
                            "E019",
                            "machino has no methods",
                            fspan,
                        )
                        .with_help(format!(
                            "call a plain function with the value as an argument: {}(...)",
                            field
                        )));
                    }
                    let span = expr.span.merge(fspan);
                    expr = Expr {
                        kind: ExprKind::Field(Box::new(expr), field),
                        span,
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_atom(&mut self) -> PResult<Expr> {
        let t = self.advance();
        let span = t.span;
        match t.tok {
            Tok::Int(v) => Ok(Expr {
                kind: ExprKind::Int(v),
                span,
            }),
            Tok::Float(v) => Ok(Expr {
                kind: ExprKind::Float(v),
                span,
            }),
            Tok::Str(s) => Ok(Expr {
                kind: ExprKind::Str(s),
                span,
            }),
            Tok::True => Ok(Expr {
                kind: ExprKind::Bool(true),
                span,
            }),
            Tok::False => Ok(Expr {
                kind: ExprKind::Bool(false),
                span,
            }),
            Tok::Ident(mut name) => {
                let mut type_args: Vec<Type> = Vec::new();
                // `::` starts either Enum::Variant or turbofish ::<T>
                while matches!(self.peek(), Tok::Colon) {
                    let save = self.pos;
                    if !(self.eat(&Tok::Colon) && self.eat(&Tok::Colon)) {
                        self.pos = save;
                        break;
                    }
                    if self.eat(&Tok::Lt) {
                        // turbofish: name::<T, U>
                        if !matches!(self.peek(), Tok::Gt) {
                            loop {
                                type_args.push(self.parse_type()?);
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                        }
                        if !self.eat(&Tok::Gt) {
                            return Err(Diagnostic::new(
                                "E011",
                                "expected '>' to close turbofish type arguments",
                                self.peek_span(),
                            ));
                        }
                        break;
                    }
                    let variant = self.parse_ident("variant name after '::'")?;
                    name = format!("{}::{}", name, variant);
                }
                if matches!(self.peek(), Tok::LParen) {
                    self.advance();
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Tok::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                    }
                    let rp = self.expect(Tok::RParen, "')' to close call")?;
                    Ok(Expr {
                        kind: ExprKind::Call(name, type_args, args),
                        span: span.merge(rp.span),
                    })
                } else if !type_args.is_empty() {
                    Err(Diagnostic::new(
                        "E011",
                        "turbofish '::<>' must be followed by a call '(...)'",
                        self.peek_span(),
                    ))
                } else {
                    Ok(Expr {
                        kind: ExprKind::Var(name),
                        span,
                    })
                }
            }
            Tok::Fn => {
                // lambda expression: fn(x: int) -> int { ... }
                self.expect(Tok::LParen, "'(' after 'fn' in a lambda")?;
                let mut params = Vec::new();
                if !matches!(self.peek(), Tok::RParen) {
                    loop {
                        let pstart = self.peek_span();
                        let pname = self.parse_ident("parameter name")?;
                        self.expect(Tok::Colon, "':' after parameter name")?;
                        let ty = self.parse_type()?;
                        params.push(Param {
                            name: pname,
                            ty,
                            span: pstart,
                        });
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen, "')' after lambda parameters")?;
                let ret = if self.eat(&Tok::Arrow) {
                    self.parse_type()?
                } else {
                    Type::Unit
                };
                // id assigned before the body parses: outer lambdas get
                // smaller ids than nested ones (compilation order relies on it)
                let id = self.lambda_counter;
                self.lambda_counter += 1;
                let body = self.parse_block()?;
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                Ok(Expr {
                    kind: ExprKind::Lambda(Box::new(Lambda {
                        id,
                        params,
                        ret,
                        body,
                    })),
                    span: span.merge(end),
                })
            }
            Tok::Match => {
                // match expression: match expr { Pattern => expr, ... }
                let scrutinee = Box::new(self.parse_expr()?);
                self.skip_newlines();
                self.expect(Tok::LBrace, "'{' to open match arms")?;
                let mut arms = Vec::new();
                loop {
                    self.skip_newlines();
                    if matches!(self.peek(), Tok::RBrace) {
                        self.advance();
                        break;
                    }
                    let arm_span = self.peek_span();
                    let pattern = self.parse_pattern()?;
                    self.expect(Tok::FatArrow, "'=>' after match pattern")?;
                    let body = self.parse_expr()?;
                    arms.push(MatchArm {
                        pattern,
                        body,
                        span: arm_span,
                    });
                    self.expect_stmt_end()?;
                }
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                if arms.is_empty() {
                    return Err(Diagnostic::new(
                        "E052",
                        "match expression has no arms".to_string(),
                        span.merge(end),
                    )
                    .with_help("add at least one pattern: match x { _ => 0 }"));
                }
                Ok(Expr {
                    kind: ExprKind::Match(Box::new(Match {
                        scrutinee: *scrutinee,
                        arms,
                    })),
                    span: span.merge(end),
                })
            }
            Tok::LParen => {
                let inner = self.parse_expr()?;
                let rp = self.expect(Tok::RParen, "')'")?;
                Ok(Expr {
                    kind: inner.kind,
                    span: span.merge(rp.span),
                })
            }
            Tok::LBracket => {
                let mut elems = Vec::new();
                if !matches!(self.peek(), Tok::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                let rb = self.expect(Tok::RBracket, "']' to close array literal")?;
                Ok(Expr {
                    kind: ExprKind::Array(elems),
                    span: span.merge(rb.span),
                })
            }
            other => Err(Diagnostic::new(
                "E019",
                format!("expected an expression, found {}", describe(&other)),
                span,
            )),
        }
    }
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::Int(v) => format!("integer '{}'", v),
        Tok::Float(v) => format!("float '{}'", v),
        Tok::Str(_) => "string literal".to_string(),
        Tok::Ident(s) => format!("identifier '{}'", s),
        Tok::Fn => "'fn'".to_string(),
        Tok::Extern => "'extern'".to_string(),
        Tok::Let => "'let'".to_string(),
        Tok::If => "'if'".to_string(),
        Tok::Else => "'else'".to_string(),
        Tok::While => "'while'".to_string(),
        Tok::For => "'for'".to_string(),
        Tok::In => "'in'".to_string(),
        Tok::Break => "'break'".to_string(),
        Tok::Continue => "'continue'".to_string(),
        Tok::Return => "'return'".to_string(),
        Tok::True => "'true'".to_string(),
        Tok::False => "'false'".to_string(),
        Tok::Requires => "'requires'".to_string(),
        Tok::Ensures => "'ensures'".to_string(),
        Tok::Test => "'test'".to_string(),
        Tok::Assert => "'assert'".to_string(),
        Tok::Struct => "'struct'".to_string(),
        Tok::Import => "'import'".to_string(),
        Tok::Enum => "'enum'".to_string(),
        Tok::Match => "'match'".to_string(),
        Tok::Where => "'where'".to_string(),
        Tok::Invariant => "'invariant'".to_string(),
        Tok::LParen => "'('".to_string(),
        Tok::RParen => "')'".to_string(),
        Tok::LBrace => "'{'".to_string(),
        Tok::RBrace => "'}'".to_string(),
        Tok::LBracket => "'['".to_string(),
        Tok::RBracket => "']'".to_string(),
        Tok::Comma => "','".to_string(),
        Tok::Colon => "':'".to_string(),
        Tok::Dot => "'.'".to_string(),
        Tok::DotDot => "'..'".to_string(),
        Tok::Arrow => "'->'".to_string(),
        Tok::FatArrow => "'=>'".to_string(),
        Tok::Assign => "'='".to_string(),
        Tok::Plus => "'+'".to_string(),
        Tok::Minus => "'-'".to_string(),
        Tok::Star => "'*'".to_string(),
        Tok::Slash => "'/'".to_string(),
        Tok::Percent => "'%'".to_string(),
        Tok::EqEq => "'=='".to_string(),
        Tok::NotEq => "'!='".to_string(),
        Tok::Lt => "'<'".to_string(),
        Tok::LtEq => "'<='".to_string(),
        Tok::Gt => "'>'".to_string(),
        Tok::GtEq => "'>='".to_string(),
        Tok::AndAnd => "'&&'".to_string(),
        Tok::OrOr => "'||'".to_string(),
        Tok::Bang => "'!'".to_string(),
        Tok::Newline => "end of line".to_string(),
        Tok::Eof => "end of file".to_string(),
    }
}
