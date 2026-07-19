//! Canonical source formatter (`machino fmt`). Machino promises "one
//! canonical style"; this enforces it mechanically so agent-generated code
//! is always diff-stable.
//!
//! The formatter works on the token stream (with comments preserved), not
//! the AST, and re-lexes its own output to prove the token stream is
//! unchanged before anything is written. Rules:
//!   - 4-space indentation derived from brace depth
//!   - canonical spacing between tokens (one space around binary operators,
//!     none inside call parens, space after commas/colons, ...)
//!   - at most one consecutive blank line; no trailing whitespace
//!   - comments stay on their own line or at end of line, one space before #

/// A raw token for formatting: machino tokens plus comments and newlines.
#[derive(Debug, Clone, PartialEq)]
enum FTok {
    Word(String),    // identifiers, keywords, numbers
    Str(String),     // raw source slice including quotes
    Punct(String),   // operators and punctuation
    Comment(String), // includes leading '#'
    Newline,
}

/// Lexes source preserving comments and newline structure. Never fails:
/// unknown characters pass through as punctuation (the compiler proper
/// reports errors; fmt only refuses to output token-stream changes).
fn flex(source: &str) -> Vec<FTok> {
    let b = source.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        match c {
            ' ' | '\t' | '\r' => i += 1,
            '\n' => {
                toks.push(FTok::Newline);
                i += 1;
            }
            '#' => {
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                toks.push(FTok::Comment(source[start..i].trim_end().to_string()));
            }
            '"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < b.len() && b[i] == b'"' {
                    i += 1;
                }
                toks.push(FTok::Str(source[start..i].to_string()));
            }
            c if c.is_ascii_alphanumeric() || c == '_' => {
                let start = i;
                while i < b.len()
                    && ((b[i] as char).is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.')
                {
                    // consume '.' only inside a float literal (digit.digit)
                    if b[i] == b'.' {
                        let is_float = (b[start] as char).is_ascii_digit()
                            && i + 1 < b.len()
                            && (b[i + 1] as char).is_ascii_digit();
                        if !is_float {
                            break;
                        }
                    }
                    i += 1;
                }
                toks.push(FTok::Word(source[start..i].to_string()));
            }
            _ => {
                // multi-char operators first
                let two = if i + 1 < b.len() { &source[i..i + 2] } else { "" };
                let op = match two {
                    "->" | "=>" | "==" | "!=" | "<=" | ">=" | "&&" | "||" | ".." | "::" => {
                        i += 2;
                        two.to_string()
                    }
                    _ => {
                        i += 1;
                        c.to_string()
                    }
                };
                toks.push(FTok::Punct(op));
            }
        }
    }
    toks
}

/// True if `t` ends a value (so a following '-'/'(' etc. is binary/indexing).
fn ends_value(t: &FTok) -> bool {
    match t {
        FTok::Word(w) => !is_stmt_keyword(w),
        FTok::Str(_) => true,
        FTok::Punct(p) => matches!(p.as_str(), ")" | "]" | "}"),
        _ => false,
    }
}

fn is_stmt_keyword(w: &str) -> bool {
    matches!(
        w,
        "fn" | "extern"
            | "let"
            | "if"
            | "else"
            | "while"
            | "for"
            | "in"
            | "break"
            | "continue"
            | "return"
            | "requires"
            | "ensures"
            | "test"
            | "assert"
            | "struct"
            | "import"
            | "enum"
            | "invariant"
            | "match"
    )
}

/// Formats machino source into the canonical style.
pub fn format_source(source: &str) -> String {
    let toks = flex(source);
    let mut out = String::new();
    let mut indent: usize = 0;
    let mut line = String::new();
    // a blank line was requested; emitted lazily so blanks collapse and
    // never appear right after '{' or right before '}'
    let mut pending_blank = false;
    let mut last_open = false; // last emitted line ended with '{'
    let mut paren_depth = 0usize;
    // '<' immediately after `fn`/`struct`/`enum` opens generic type params
    let mut generic_depth = 0usize;
    // a '{'/'}' just flushed its own line; the next single source newline
    // is layout we already emitted, not a blank-line request
    let mut swallow_newline = false;

    let flush = |out: &mut String,
                 line: &mut String,
                 indent: usize,
                 pending_blank: &mut bool,
                 last_open: &mut bool| {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if !out.is_empty() {
                *pending_blank = true;
            }
        } else {
            if *pending_blank && !*last_open && !trimmed.starts_with('}') {
                out.push('\n');
            }
            *pending_blank = false;
            for _ in 0..indent {
                out.push_str("    ");
            }
            out.push_str(trimmed);
            out.push('\n');
            *last_open = trimmed.ends_with('{');
        }
        line.clear();
    };

    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let prev_sig = if line.is_empty() {
            None
        } else {
            // last significant token on this line
            toks[..i].iter().rev().find(|t| !matches!(t, FTok::Newline))
        };
        if !matches!(t, FTok::Newline) {
            swallow_newline = false;
        }
        match t {
            FTok::Newline => {
                if paren_depth > 0 {
                    // newlines inside parens/brackets are joins
                    if !line.is_empty() && !line.ends_with(' ') && !line.ends_with('(') && !line.ends_with('[') {
                        line.push(' ');
                    }
                } else if swallow_newline && line.is_empty() {
                    swallow_newline = false;
                } else {
                    flush(&mut out, &mut line, indent, &mut pending_blank, &mut last_open);
                }
            }
            FTok::Comment(c) => {
                if line.trim().is_empty() {
                    line.push_str(c);
                } else {
                    line = line.trim_end().to_string();
                    line.push_str("  ");
                    line.push_str(c);
                }
            }
            FTok::Punct(p) => {
                match p.as_str() {
                    "{" => {
                        if !line.is_empty() && !line.ends_with(' ') {
                            line.push(' ');
                        }
                        line.push('{');
                        flush(&mut out, &mut line, indent, &mut pending_blank, &mut last_open);
                        indent += 1;
                        swallow_newline = true;
                    }
                    "}" => {
                        flush(&mut out, &mut line, indent, &mut pending_blank, &mut last_open);
                        indent = indent.saturating_sub(1);
                        line.push('}');
                        // `} else` stays on one line
                        let next_is_else = matches!(
                            toks[i + 1..].iter().find(|t| !matches!(t, FTok::Newline)),
                            Some(FTok::Word(w)) if w == "else"
                        ) && toks[i + 1..]
                            .iter()
                            .take_while(|t| matches!(t, FTok::Newline))
                            .count()
                            == 0;
                        if !next_is_else {
                            flush(&mut out, &mut line, indent, &mut pending_blank, &mut last_open);
                            swallow_newline = true;
                        } else {
                            line.push(' ');
                        }
                    }
                    "(" => {
                        paren_depth += 1;
                        // call/grouping: no space when following a value or
                        // fn keyword; space after control keywords
                        if let Some(FTok::Word(w)) = prev_sig {
                            if matches!(w.as_str(), "if" | "while" | "return" | "assert") {
                                if !line.ends_with(' ') {
                                    line.push(' ');
                                }
                            }
                        }
                        line.push('(');
                    }
                    ")" => {
                        paren_depth = paren_depth.saturating_sub(1);
                        line = line.trim_end().to_string();
                        line.push(')');
                    }
                    "[" => {
                        paren_depth += 1;
                        if !ends_value(prev_sig.unwrap_or(&FTok::Newline))
                            && !line.is_empty()
                            && !line.ends_with(' ')
                            && !line.ends_with('(')
                            && !line.ends_with('[')
                        {
                            line.push(' ');
                        }
                        line.push('[');
                    }
                    "]" => {
                        paren_depth = paren_depth.saturating_sub(1);
                        line = line.trim_end().to_string();
                        line.push(']');
                    }
                    "," => {
                        line = line.trim_end().to_string();
                        line.push_str(", ");
                    }
                    ":" => {
                        line = line.trim_end().to_string();
                        line.push_str(": ");
                    }
                    "::" | "." | ".." => {
                        line = line.trim_end().to_string();
                        line.push_str(p);
                    }
                    "<"
                        if matches!(
                            prev_sig,
                            Some(FTok::Word(w))
                                if matches!(w.as_str(), "fn" | "struct" | "enum")
                        ) || matches!(prev_sig, Some(FTok::Punct(p)) if p == "::") =>
                    {
                        // generic params / turbofish: no spaces inside `<...>`
                        generic_depth += 1;
                        line.push('<');
                    }
                    ">" if generic_depth > 0 => {
                        generic_depth -= 1;
                        line = line.trim_end().to_string();
                        line.push('>');
                    }
                    "!" => {
                        line.push('!');
                    }
                    "-" => {
                        let binary = prev_sig.map_or(false, ends_value);
                        if binary {
                            if !line.ends_with(' ') {
                                line.push(' ');
                            }
                            line.push_str("- ");
                        } else {
                            if !line.is_empty()
                                && !line.ends_with(' ')
                                && !line.ends_with('(')
                                && !line.ends_with('[')
                            {
                                line.push(' ');
                            }
                            line.push('-');
                        }
                    }
                    op => {
                        // binary operators and everything else: spaces around
                        if !line.is_empty() && !line.ends_with(' ') {
                            line.push(' ');
                        }
                        line.push_str(op);
                        line.push(' ');
                    }
                }
            }
            FTok::Word(w) | FTok::Str(w) => {
                let need_space = match prev_sig {
                    None => false,
                    Some(FTok::Punct(p)) => !matches!(
                        p.as_str(),
                        "(" | "[" | "." | "::" | ".." | "!" | "<"
                    ) || (p == "<" && generic_depth == 0),
                    Some(_) => true,
                };
                if need_space && !line.is_empty() && !line.ends_with(' ') {
                    line.push(' ');
                }
                // suppress space right after unary minus handled above
                line.push_str(w);
            }
        }
        i += 1;
    }
    flush(&mut out, &mut line, indent, &mut pending_blank, &mut last_open);
    // strip leading blank lines and ensure single trailing newline
    let trimmed = out.trim_start_matches('\n').trim_end();
    let mut result = trimmed.to_string();
    result.push('\n');
    result
}

/// The formatter's safety property: the canonical style may move newlines
/// (statements get their own lines; wrapped calls join) but must never
/// change the real lexer's significant tokens, lose a comment, or produce
/// something that no longer parses. Callers refuse to write output if any
/// of that fails.
pub fn tokens_preserved(original: &str, formatted: &str) -> bool {
    let lex_toks = |src: &str| -> Option<Vec<crate::lexer::Tok>> {
        crate::lexer::lex(src).ok().map(|ts| {
            ts.into_iter()
                .map(|t| t.tok)
                .filter(|t| !matches!(t, crate::lexer::Tok::Newline))
                .collect()
        })
    };
    let (Some(a), Some(b)) = (lex_toks(original), lex_toks(formatted)) else {
        return false;
    };
    if a != b {
        return false;
    }
    // the formatted output must still parse (newline moves are only safe
    // where the grammar treats them as layout)
    let Ok(tokens) = crate::lexer::lex(formatted) else {
        return false;
    };
    if crate::parser::Parser::new(&tokens, formatted)
        .parse_program()
        .is_err()
    {
        return false;
    }
    let comments = |src: &str| -> Vec<String> {
        flex(src)
            .into_iter()
            .filter_map(|t| match t {
                FTok::Comment(c) => Some(c),
                _ => None,
            })
            .collect()
    };
    comments(original) == comments(formatted)
}
