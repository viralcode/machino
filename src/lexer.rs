//! Lexer. Machino is newline-terminated (no semicolons) with one canonical
//! syntax for everything. Newlines inside parentheses/brackets are ignored so
//! long call expressions can wrap.

use crate::diag::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals & identifiers
    Int(i64),
    Float(f64),
    Str(String),
    Ident(String),
    // keywords
    Fn,
    Extern,
    Let,
    If,
    Else,
    While,
    For,
    In,
    Break,
    Continue,
    Return,
    True,
    False,
    Requires,
    Ensures,
    Test,
    Assert,
    Struct,
    Import,
    Enum,
    Match,
    Where,
    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,     // .
    DotDot,  // ..
    Arrow,   // ->
    FatArrow, // =>
    Assign,  // =
    // operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    AndAnd,
    OrOr,
    Bang,
    // structure
    Newline,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

pub fn lex(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let bytes = source.as_bytes();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0usize;
    let mut depth = 0i32; // paren/bracket depth; newlines inside are ignored

    macro_rules! push {
        ($tok:expr, $start:expr, $end:expr) => {
            tokens.push(Token {
                tok: $tok,
                span: Span::new($start as u32, $end as u32),
            })
        };
    }

    while i < bytes.len() {
        let c = bytes[i] as char;
        let start = i;
        match c {
            ' ' | '\t' | '\r' => {
                i += 1;
            }
            '\n' => {
                i += 1;
                if depth == 0 {
                    // collapse consecutive newlines
                    if !matches!(tokens.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
                        push!(Tok::Newline, start, i);
                    }
                }
            }
            '#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            '(' => {
                depth += 1;
                i += 1;
                push!(Tok::LParen, start, i);
            }
            ')' => {
                depth -= 1;
                i += 1;
                push!(Tok::RParen, start, i);
            }
            '[' => {
                depth += 1;
                i += 1;
                push!(Tok::LBracket, start, i);
            }
            ']' => {
                depth -= 1;
                i += 1;
                push!(Tok::RBracket, start, i);
            }
            '{' => {
                i += 1;
                push!(Tok::LBrace, start, i);
            }
            '}' => {
                i += 1;
                push!(Tok::RBrace, start, i);
            }
            ',' => {
                i += 1;
                push!(Tok::Comma, start, i);
            }
            ':' => {
                i += 1;
                push!(Tok::Colon, start, i);
            }
            '.' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                    i += 2;
                    push!(Tok::DotDot, start, i);
                } else {
                    i += 1;
                    push!(Tok::Dot, start, i);
                }
            }
            '+' => {
                i += 1;
                push!(Tok::Plus, start, i);
            }
            '*' => {
                i += 1;
                push!(Tok::Star, start, i);
            }
            '/' => {
                i += 1;
                push!(Tok::Slash, start, i);
            }
            '%' => {
                i += 1;
                push!(Tok::Percent, start, i);
            }
            '-' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    i += 2;
                    push!(Tok::Arrow, start, i);
                } else {
                    i += 1;
                    push!(Tok::Minus, start, i);
                }
            }
            '=' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    i += 2;
                    push!(Tok::EqEq, start, i);
                } else if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                    i += 2;
                    push!(Tok::FatArrow, start, i);
                } else {
                    i += 1;
                    push!(Tok::Assign, start, i);
                }
            }
            '!' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    i += 2;
                    push!(Tok::NotEq, start, i);
                } else {
                    i += 1;
                    push!(Tok::Bang, start, i);
                }
            }
            '<' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    i += 2;
                    push!(Tok::LtEq, start, i);
                } else {
                    i += 1;
                    push!(Tok::Lt, start, i);
                }
            }
            '>' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                    i += 2;
                    push!(Tok::GtEq, start, i);
                } else {
                    i += 1;
                    push!(Tok::Gt, start, i);
                }
            }
            '&' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'&' {
                    i += 2;
                    push!(Tok::AndAnd, start, i);
                } else {
                    return Err(Diagnostic::new(
                        "E001",
                        "unexpected character '&'",
                        Span::new(start as u32, (start + 1) as u32),
                    )
                    .with_help("logical AND is written '&&'"));
                }
            }
            '|' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'|' {
                    i += 2;
                    push!(Tok::OrOr, start, i);
                } else {
                    return Err(Diagnostic::new(
                        "E001",
                        "unexpected character '|'",
                        Span::new(start as u32, (start + 1) as u32),
                    )
                    .with_help("logical OR is written '||'"));
                }
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    if i >= bytes.len() || bytes[i] == b'\n' {
                        return Err(Diagnostic::new(
                            "E002",
                            "unterminated string literal",
                            Span::new(start as u32, i as u32),
                        )
                        .with_help("close the string with '\"' before the end of the line"));
                    }
                    match bytes[i] {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            if i + 1 >= bytes.len() {
                                return Err(Diagnostic::new(
                                    "E002",
                                    "unterminated string literal",
                                    Span::new(start as u32, i as u32),
                                ));
                            }
                            let esc = bytes[i + 1] as char;
                            let ch = match esc {
                                'n' => '\n',
                                't' => '\t',
                                'r' => '\r',
                                '\\' => '\\',
                                '"' => '"',
                                '0' => '\0',
                                other => {
                                    return Err(Diagnostic::new(
                                        "E003",
                                        format!("unknown escape sequence '\\{}'", other),
                                        Span::new(i as u32, (i + 2) as u32),
                                    )
                                    .with_help("valid escapes: \\n \\t \\r \\\\ \\\" \\0"));
                                }
                            };
                            s.push(ch);
                            i += 2;
                        }
                        _ => {
                            // consume a full UTF-8 codepoint
                            let ch_start = i;
                            let mut ch_end = i + 1;
                            while ch_end < bytes.len() && (bytes[ch_end] & 0xC0) == 0x80 {
                                ch_end += 1;
                            }
                            s.push_str(std::str::from_utf8(&bytes[ch_start..ch_end]).map_err(
                                |_| {
                                    Diagnostic::new(
                                        "E004",
                                        "invalid UTF-8 in string literal",
                                        Span::new(ch_start as u32, ch_end as u32),
                                    )
                                },
                            )?);
                            i = ch_end;
                        }
                    }
                }
                push!(Tok::Str(s), start, i);
            }
            c if c.is_ascii_digit() => {
                let mut end = i;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                let mut is_float = false;
                if end < bytes.len()
                    && bytes[end] == b'.'
                    && end + 1 < bytes.len()
                    && bytes[end + 1].is_ascii_digit()
                {
                    is_float = true;
                    end += 1;
                    while end < bytes.len() && bytes[end].is_ascii_digit() {
                        end += 1;
                    }
                }
                let text = &source[i..end];
                if is_float {
                    let v: f64 = text.parse().map_err(|_| {
                        Diagnostic::new(
                            "E005",
                            format!("invalid float literal '{}'", text),
                            Span::new(i as u32, end as u32),
                        )
                    })?;
                    push!(Tok::Float(v), i, end);
                } else {
                    let v: i64 = text.parse().map_err(|_| {
                        Diagnostic::new(
                            "E005",
                            format!("integer literal '{}' out of range for int (64-bit)", text),
                            Span::new(i as u32, end as u32),
                        )
                    })?;
                    push!(Tok::Int(v), i, end);
                }
                i = end;
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut end = i;
                while end < bytes.len()
                    && ((bytes[end] as char).is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                let word = &source[i..end];
                let tok = match word {
                    "fn" => Tok::Fn,
                    "extern" => Tok::Extern,
                    "let" => Tok::Let,
                    "if" => Tok::If,
                    "else" => Tok::Else,
                    "while" => Tok::While,
                    "for" => Tok::For,
                    "in" => Tok::In,
                    "break" => Tok::Break,
                    "continue" => Tok::Continue,
                    "return" => Tok::Return,
                    "true" => Tok::True,
                    "false" => Tok::False,
                    "requires" => Tok::Requires,
                    "ensures" => Tok::Ensures,
                    "test" => Tok::Test,
                    "assert" => Tok::Assert,
                    "struct" => Tok::Struct,
                    "import" => Tok::Import,
                    "enum" => Tok::Enum,
                    "match" => Tok::Match,
                    "where" => Tok::Where,
                    _ => Tok::Ident(word.to_string()),
                };
                push!(tok, i, end);
                i = end;
            }
            other => {
                return Err(Diagnostic::new(
                    "E001",
                    format!("unexpected character '{}'", other),
                    Span::new(start as u32, (start + other.len_utf8()) as u32),
                ));
            }
        }
    }
    let end = bytes.len();
    if !matches!(tokens.last().map(|t| &t.tok), Some(Tok::Newline) | None) {
        tokens.push(Token {
            tok: Tok::Newline,
            span: Span::new(end as u32, end as u32),
        });
    }
    tokens.push(Token {
        tok: Tok::Eof,
        span: Span::new(end as u32, end as u32),
    });
    Ok(tokens)
}
