//! Recursive-descent parser. The grammar is deliberately small and regular:
//! there is exactly one way to write each construct.

use crate::ast::*;
use crate::diag::{Diagnostic, Span};
use crate::lexer::{Tok, Token};

pub struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    source: &'a str,
}

type PResult<T> = Result<T, Diagnostic>;

impl<'a> Parser<'a> {
    pub fn new(tokens: &'a [Token], source: &'a str) -> Self {
        Parser {
            tokens,
            pos: 0,
            source,
        }
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
        let mut tests = Vec::new();
        loop {
            self.skip_newlines();
            match self.peek() {
                Tok::Eof => break,
                Tok::Fn => functions.push(self.parse_function(false)?),
                Tok::Extern => {
                    self.advance();
                    functions.push(self.parse_function(true)?);
                }
                Tok::Test => tests.push(self.parse_test()?),
                other => {
                    return Err(Diagnostic::new(
                        "E012",
                        format!(
                            "expected 'fn', 'extern fn' or 'test' at top level, found {}",
                            describe(other)
                        ),
                        self.peek_span(),
                    )
                    .with_help("all code lives inside functions; the entry point is 'fn main()'"));
                }
            }
        }
        Ok(Program { functions, tests })
    }

    fn parse_function(&mut self, is_extern: bool) -> PResult<Function> {
        let fn_tok = self.expect(Tok::Fn, "'fn'")?;
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
                params,
                ret,
                requires,
                ensures,
                body: Vec::new(),
                is_extern: true,
                span: fn_tok.span.merge(end),
            });
        }

        let body = self.parse_block()?;
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(Function {
            name,
            params,
            ret,
            requires,
            ensures,
            body,
            is_extern: false,
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
        let body = self.parse_block()?;
        let end = self.tokens[self.pos.saturating_sub(1)].span;
        Ok(TestBlock {
            name,
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
                let body = self.parse_block()?;
                let end = self.tokens[self.pos.saturating_sub(1)].span;
                Ok(Stmt {
                    kind: StmtKind::While { cond, body },
                    span: start.merge(end),
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
            Tok::Ident(name) => {
                // lookahead: assignment, index assignment, or expression
                let next = &self.tokens[self.pos + 1].tok;
                match next {
                    Tok::Assign => {
                        self.advance();
                        self.advance();
                        let value = self.parse_expr()?;
                        let span = start.merge(value.span);
                        self.expect_stmt_end()?;
                        Ok(Stmt {
                            kind: StmtKind::Assign { name, value },
                            span,
                        })
                    }
                    Tok::LBracket => {
                        // could be a[i] = v  (index assign) or a[i] used in expression
                        let save = self.pos;
                        self.advance(); // ident
                        self.advance(); // [
                        let index = self.parse_expr()?;
                        if self.eat(&Tok::RBracket) && self.eat(&Tok::Assign) {
                            let value = self.parse_expr()?;
                            let span = start.merge(value.span);
                            self.expect_stmt_end()?;
                            return Ok(Stmt {
                                kind: StmtKind::IndexAssign { name, index, value },
                                span,
                            });
                        }
                        self.pos = save;
                        let expr = self.parse_expr()?;
                        let span = expr.span;
                        self.expect_stmt_end()?;
                        Ok(Stmt {
                            kind: StmtKind::Expr(expr),
                            span,
                        })
                    }
                    _ => {
                        let expr = self.parse_expr()?;
                        let span = expr.span;
                        self.expect_stmt_end()?;
                        Ok(Stmt {
                            kind: StmtKind::Expr(expr),
                            span,
                        })
                    }
                }
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

    fn parse_type(&mut self) -> PResult<Type> {
        match self.advance() {
            Token {
                tok: Tok::Ident(s),
                span,
            } => match s.as_str() {
                "int" => Ok(Type::Int),
                "float" => Ok(Type::Float),
                "bool" => Ok(Type::Bool),
                "str" => Ok(Type::Str),
                other => Err(Diagnostic::new(
                    "E018",
                    format!("unknown type '{}'", other),
                    span,
                )
                .with_help("valid types: int, float, bool, str, [T]")),
            },
            Token {
                tok: Tok::LBracket, ..
            } => {
                let inner = self.parse_type()?;
                self.expect(Tok::RBracket, "']' to close array type")?;
                Ok(Type::Array(Box::new(inner)))
            }
            t => Err(Diagnostic::new(
                "E018",
                format!("expected a type, found {}", describe(&t.tok)),
                t.span,
            )
            .with_help("valid types: int, float, bool, str, [T]")),
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
            if matches!(self.peek(), Tok::LBracket) {
                self.advance();
                let index = self.parse_expr()?;
                let rb = self.expect(Tok::RBracket, "']' to close index")?;
                let span = expr.span.merge(rb.span);
                expr = Expr {
                    kind: ExprKind::Index(Box::new(expr), Box::new(index)),
                    span,
                };
            } else {
                break;
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
            Tok::Ident(name) => {
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
                        kind: ExprKind::Call(name, args),
                        span: span.merge(rp.span),
                    })
                } else {
                    Ok(Expr {
                        kind: ExprKind::Var(name),
                        span,
                    })
                }
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
        Tok::Return => "'return'".to_string(),
        Tok::True => "'true'".to_string(),
        Tok::False => "'false'".to_string(),
        Tok::Requires => "'requires'".to_string(),
        Tok::Ensures => "'ensures'".to_string(),
        Tok::Test => "'test'".to_string(),
        Tok::Assert => "'assert'".to_string(),
        Tok::LParen => "'('".to_string(),
        Tok::RParen => "')'".to_string(),
        Tok::LBrace => "'{'".to_string(),
        Tok::RBrace => "'}'".to_string(),
        Tok::LBracket => "'['".to_string(),
        Tok::RBracket => "']'".to_string(),
        Tok::Comma => "','".to_string(),
        Tok::Colon => "':'".to_string(),
        Tok::Arrow => "'->'".to_string(),
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
