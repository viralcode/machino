//! Structured diagnostics. Machino's primary "user" is an AI agent in a
//! generate-check-repair loop, so every diagnostic is available both as
//! human-readable text and as machine-readable JSON with a stable error code.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// A machine-applicable fix: replace the bytes in `span` with `replacement`.
/// Agents can apply this mechanically without re-deriving the edit.
#[derive(Debug, Clone)]
pub struct Fix {
    pub span: Span,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// Stable error code, e.g. "E003". Agents can key repair strategies on it.
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    /// Actionable fix suggestion.
    pub help: Option<String>,
    /// Optional machine-applicable edit that resolves the diagnostic.
    pub fix: Option<Fix>,
}

impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            code,
            message: message.into(),
            span,
            help: None,
            fix: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Attach a machine-applicable fix that replaces `span` with `replacement`.
    pub fn with_fix(mut self, span: Span, replacement: impl Into<String>) -> Self {
        self.fix = Some(Fix {
            span,
            replacement: replacement.into(),
        });
        self
    }
}

/// Computes 1-based (line, column) for a byte offset.
pub fn line_col(source: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let mut line = 1u32;
    let mut col = 1u32;
    for (i, ch) in source.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

impl Diagnostic {
    pub fn to_json(&self, source: &str, path: &str) -> String {
        let (line, col) = line_col(source, self.span.start);
        let (end_line, end_col) = line_col(source, self.span.end);
        let help = match &self.help {
            Some(h) => format!(",\"help\":\"{}\"", json_escape(h)),
            None => String::new(),
        };
        let fix = match &self.fix {
            Some(f) => {
                let (fl, fc) = line_col(source, f.span.start);
                let (fel, fec) = line_col(source, f.span.end);
                format!(
                    ",\"fix\":{{\"line\":{},\"col\":{},\"endLine\":{},\"endCol\":{},\"replace\":\"{}\"}}",
                    fl,
                    fc,
                    fel,
                    fec,
                    json_escape(&f.replacement)
                )
            }
            None => String::new(),
        };
        format!(
            "{{\"severity\":\"error\",\"code\":\"{}\",\"message\":\"{}\",\"file\":\"{}\",\"line\":{},\"col\":{},\"endLine\":{},\"endCol\":{}{}{}}}",
            self.code,
            json_escape(&self.message),
            json_escape(path),
            line,
            col,
            end_line,
            end_col,
            help,
            fix
        )
    }

    pub fn render_human(&self, source: &str, path: &str) -> String {
        let (line, col) = line_col(source, self.span.start);
        let mut out = format!(
            "error[{}]: {}\n  --> {}:{}:{}\n",
            self.code, self.message, path, line, col
        );
        if let Some(src_line) = source.lines().nth(line as usize - 1) {
            let line_no = line.to_string();
            out.push_str(&format!("{} | {}\n", line_no, src_line));
            let mut underline = String::new();
            for _ in 0..(line_no.len() + 3 + col as usize - 1) {
                underline.push(' ');
            }
            let span_len = ((self.span.end - self.span.start) as usize).max(1);
            let visible = span_len.min(src_line.len().saturating_sub(col as usize - 1)).max(1);
            for _ in 0..visible {
                underline.push('^');
            }
            out.push_str(&underline);
            out.push('\n');
        }
        if let Some(help) = &self.help {
            out.push_str(&format!("help: {}\n", help));
        }
        out
    }
}
