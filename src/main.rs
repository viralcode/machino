mod ast;
mod checker;
mod diag;
mod infer;
mod interp;
mod lexer;
mod mono;
mod parser;
mod pkg;
mod registry;
mod smt;
mod synth;
mod wasm;
mod wasmgc;

use diag::{line_col, Diagnostic, Span};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The standard prelude, compiled into every program.
const STD_PRELUDE: &str = include_str!("std.mno");

const USAGE: &str = "machino — an AI-first language that compiles to WebAssembly

USAGE:
  machino check <file.mno> [--json]      type-check; emit structured diagnostics
  machino test  <file.mno> [--json]      run test blocks (contracts enforced)
  machino run   <file.mno> [args...]     run fn main() in the native runtime
  machino build <file.mno> [-o out.wasm] compile to a portable .wasm module
  machino synth [--count N] [--seed S] [--out DIR]  generate a verified corpus

  machino pkg init <name>                create a machino.pkg manifest here
  machino pkg add <name> <source> [ref]  add a dependency (path or git URL)
  machino pkg sync                       install deps into machino_modules/

Import from packages with:  import \"pkg:<name>/<file>.mno\"

Run .wasm output anywhere: browsers, Node (see runners/run.mjs), wasmtime, etc.";

fn main() -> ExitCode {
    // the tree-walking interpreter recurses on the Rust stack; give it room
    // for MACHINO_MAX_DEPTH machino frames
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to spawn main thread")
        .join()
        .expect("main thread panicked")
}

fn real_main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            eprintln!("{}", USAGE);
            return ExitCode::from(2);
        }
    };
    match cmd {
        "check" => cmd_check(rest),
        "test" => cmd_test(rest),
        "run" => cmd_run(rest),
        "build" => cmd_build(rest),
        "synth" => cmd_synth(rest),
        "pkg" => cmd_pkg(rest),
        "--help" | "-h" | "help" => {
            println!("{}", USAGE);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command '{}'\n\n{}", other, USAGE);
            ExitCode::from(2)
        }
    }
}

// ---- source bundling (imports + prelude) ----

struct Segment {
    path: String,
    start: u32,
    len: u32,
}

struct Loaded {
    bundle: String,
    segments: Vec<Segment>,
    program: ast::Program,
}

impl Loaded {
    /// Maps a bundle-relative span to (file path, file source, local span).
    fn locate(&self, span: Span) -> (&str, &str, Span) {
        for seg in &self.segments {
            if span.start >= seg.start && span.start < seg.start + seg.len.max(1) {
                let src = &self.bundle[seg.start as usize..(seg.start + seg.len) as usize];
                let local = Span::new(
                    span.start - seg.start,
                    span.end.saturating_sub(seg.start).min(seg.len),
                );
                return (&seg.path, src, local);
            }
        }
        ("<unknown>", &self.bundle, span)
    }

    fn render_error(&self, d: &Diagnostic) -> String {
        let (path, src, local) = self.locate(d.span);
        let mut mapped = Diagnostic::new(d.code, d.message.clone(), local);
        mapped.help = d.help.clone();
        mapped.render_human(src, path)
    }

    fn error_json(&self, d: &Diagnostic) -> String {
        let (path, src, local) = self.locate(d.span);
        let mut mapped = Diagnostic::new(d.code, d.message.clone(), local);
        mapped.help = d.help.clone();
        mapped.to_json(src, path)
    }

    fn runtime_location(&self, span: Span) -> String {
        let (path, src, local) = self.locate(span);
        let (line, col) = line_col(src, local.start);
        format!("{}:{}:{}", path, line, col)
    }
}

/// Parses one file just enough to discover its imports.
fn discover_imports(source: &str, path: &str) -> Result<Vec<String>, String> {
    let tokens = lexer::lex(source)
        .map_err(|d| d.render_human(source, path))?;
    let program = parser::Parser::new(&tokens, source)
        .parse_program()
        .map_err(|d| d.render_human(source, path))?;
    Ok(program.imports.into_iter().map(|(p, _)| p).collect())
}

/// Reads the entry file plus its transitive imports and appends the std
/// prelude, producing a single bundle with a segment map for diagnostics.
fn bundle_sources(entry: &str) -> Result<(String, Vec<Segment>), String> {
    let mut ordered: Vec<(String, String)> = Vec::new(); // (display path, source)
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut queue: Vec<(PathBuf, String)> = Vec::new();

    let entry_path = PathBuf::from(entry);
    queue.push((entry_path.clone(), entry.to_string()));

    while let Some((path, display)) = queue.pop() {
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !visited.insert(canon) {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("error: cannot read '{}': {}", display, e))?;
        let imports = discover_imports(&source, &display)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        for imp in imports {
            let ipath = if imp.starts_with("pkg:") {
                pkg::resolve_pkg_import(&imp, &entry_path)?
            } else {
                let p = dir.join(&imp);
                if !p.exists() {
                    return Err(format!(
                        "error: cannot resolve import \"{}\" (from {}): file not found at {}",
                        imp,
                        display,
                        p.display()
                    ));
                }
                p
            };
            queue.push((ipath.clone(), ipath.display().to_string()));
        }
        ordered.push((display, source));
    }

    let mut bundle = String::new();
    let mut segments = Vec::new();
    for (path, source) in &ordered {
        segments.push(Segment {
            path: path.clone(),
            start: bundle.len() as u32,
            len: source.len() as u32 + 1,
        });
        bundle.push_str(source);
        bundle.push('\n');
    }
    segments.push(Segment {
        path: "<machino std>".to_string(),
        start: bundle.len() as u32,
        len: STD_PRELUDE.len() as u32,
    });
    bundle.push_str(STD_PRELUDE);
    Ok((bundle, segments))
}

/// Lex + parse + type-check a bundle. The std segment starts at `std_start`.
fn compile_bundle(bundle: &str, std_start: u32) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(bundle).map_err(|d| vec![d])?;
    let mut program = parser::Parser::new(&tokens, bundle)
        .parse_program()
        .map_err(|d| vec![d])?;
    for f in &mut program.functions {
        f.is_std = f.span.start >= std_start;
    }
    for s in &mut program.structs {
        s.is_std = s.span.start >= std_start;
    }
    checker::Checker::new(&program).check()?;
    Ok(program)
}

/// Used by synth, where there are no imports: source + prelude.
fn compile_front(source: &str) -> Result<ast::Program, Vec<Diagnostic>> {
    let mut bundle = String::with_capacity(source.len() + STD_PRELUDE.len() + 1);
    bundle.push_str(source);
    bundle.push('\n');
    let std_start = bundle.len() as u32;
    bundle.push_str(STD_PRELUDE);
    compile_bundle(&bundle, std_start)
}

fn load(path: &str, json: bool) -> Option<Loaded> {
    let (bundle, segments) = match bundle_sources(path) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{}", msg);
            return None;
        }
    };
    let std_start = segments.last().map(|s| s.start).unwrap_or(0);
    match compile_bundle(&bundle, std_start) {
        Ok(program) => Some(Loaded {
            bundle,
            segments,
            program,
        }),
        Err(diags) => {
            let loaded = Loaded {
                bundle,
                segments,
                program: ast::Program {
                    functions: vec![],
                    structs: vec![],
                    enums: vec![],
                    tests: vec![],
                    imports: vec![],
                },
            };
            report(&diags, &loaded, json);
            None
        }
    }
}

fn report(diags: &[Diagnostic], loaded: &Loaded, json: bool) {
    if json {
        let items: Vec<String> = diags.iter().map(|d| loaded.error_json(d)).collect();
        println!(
            "{{\"ok\":false,\"errors\":{},\"diagnostics\":[{}]}}",
            diags.len(),
            items.join(",")
        );
    } else {
        for d in diags {
            eprintln!("{}", loaded.render_error(d));
        }
        eprintln!(
            "check failed: {} error{}",
            diags.len(),
            if diags.len() == 1 { "" } else { "s" }
        );
    }
}

fn parse_file_arg<'a>(rest: &'a [String], cmd: &str) -> Option<&'a str> {
    match rest.iter().find(|a| !a.starts_with("--")) {
        Some(f) => Some(f.as_str()),
        None => {
            eprintln!("usage: machino {} <file.mno>", cmd);
            None
        }
    }
}

fn cmd_check(rest: &[String]) -> ExitCode {
    let json = rest.iter().any(|a| a == "--json");
    let Some(path) = parse_file_arg(rest, "check") else {
        return ExitCode::from(2);
    };
    match load(path, json) {
        Some(loaded) => {
            let n_fns = loaded
                .program
                .functions
                .iter()
                .filter(|f| !f.is_std)
                .count();
            let n_structs = loaded.program.structs.iter().filter(|s| !s.is_std).count();
            let n_tests = loaded.program.tests.len();
            if json {
                println!(
                    "{{\"ok\":true,\"errors\":0,\"functions\":{},\"structs\":{},\"tests\":{}}}",
                    n_fns, n_structs, n_tests
                );
            } else {
                println!(
                    "ok: {} ({} function{}, {} struct{}, {} test{})",
                    path,
                    n_fns,
                    if n_fns == 1 { "" } else { "s" },
                    n_structs,
                    if n_structs == 1 { "" } else { "s" },
                    n_tests,
                    if n_tests == 1 { "" } else { "s" }
                );
            }
            ExitCode::SUCCESS
        }
        None => ExitCode::FAILURE,
    }
}

fn cmd_test(rest: &[String]) -> ExitCode {
    let json = rest.iter().any(|a| a == "--json");
    let Some(path) = parse_file_arg(rest, "test") else {
        return ExitCode::from(2);
    };
    let Some(loaded) = load(path, json) else {
        return ExitCode::FAILURE;
    };
    if loaded.program.tests.is_empty() {
        if json {
            println!("{{\"ok\":true,\"passed\":0,\"failed\":0,\"tests\":[]}}");
        } else {
            println!("no tests found in {}", path);
        }
        return ExitCode::SUCCESS;
    }

    let mut passed = 0usize;
    let mut results: Vec<String> = Vec::new();
    for t in &loaded.program.tests {
        let mut interp = interp::Interp::new(&loaded.program);
        match interp.run_stmts_as_test(&t.body) {
            Ok(()) => {
                passed += 1;
                if json {
                    results.push(format!(
                        "{{\"name\":\"{}\",\"ok\":true}}",
                        diag::json_escape(&t.name)
                    ));
                } else {
                    println!("PASS  {}", t.name);
                }
            }
            Err(e) => {
                let loc = loaded.runtime_location(e.span);
                if json {
                    results.push(format!(
                        "{{\"name\":\"{}\",\"ok\":false,\"error\":\"{}\",\"location\":\"{}\"}}",
                        diag::json_escape(&t.name),
                        diag::json_escape(&e.message),
                        diag::json_escape(&loc)
                    ));
                } else {
                    println!("FAIL  {}", t.name);
                    println!("      {} ({})", e.message, loc);
                }
            }
        }
    }
    let failed = loaded.program.tests.len() - passed;
    if json {
        println!(
            "{{\"ok\":{},\"passed\":{},\"failed\":{},\"tests\":[{}]}}",
            failed == 0,
            passed,
            failed,
            results.join(",")
        );
    } else {
        println!("\n{} passed, {} failed", passed, failed);
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_run(rest: &[String]) -> ExitCode {
    let Some(path) = parse_file_arg(rest, "run") else {
        return ExitCode::from(2);
    };
    let file_pos = rest.iter().position(|a| a == path).unwrap_or(0);
    let program_args: Vec<String> = rest[file_pos + 1..].to_vec();
    let Some(loaded) = load(path, false) else {
        return ExitCode::FAILURE;
    };
    let mut interp = interp::Interp::new(&loaded.program);
    interp.args = program_args;
    match interp.run_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "runtime error: {} ({})",
                e.message,
                loaded.runtime_location(e.span)
            );
            ExitCode::FAILURE
        }
    }
}

fn cmd_build(rest: &[String]) -> ExitCode {
    let Some(path) = parse_file_arg(rest, "build") else {
        return ExitCode::from(2);
    };
    let out_path = rest
        .iter()
        .position(|a| a == "-o")
        .and_then(|i| rest.get(i + 1))
        .cloned()
        .unwrap_or_else(|| {
            let stem = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("out");
            format!("{}.wasm", stem)
        });
    let Some(loaded) = load(path, false) else {
        return ExitCode::FAILURE;
    };
    if !loaded
        .program
        .functions
        .iter()
        .any(|f| f.name == "main" && !f.is_extern)
    {
        eprintln!("error: 'machino build' requires a 'fn main()' entry point");
        return ExitCode::FAILURE;
    }
    let bytes = wasm::compile(&loaded.program, &loaded.bundle);
    match std::fs::write(&out_path, &bytes) {
        Ok(()) => {
            println!("wrote {} ({} bytes)", out_path, bytes.len());
            println!("run it anywhere:  node runners/run.mjs {}", out_path);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write '{}': {}", out_path, e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_pkg(rest: &[String]) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let result = match rest.split_first() {
        Some((sub, args)) => match (sub.as_str(), args) {
            ("init", [name]) => pkg::init(&cwd, name),
            ("add", [name, source]) => pkg::add(&cwd, name, source, None).and_then(|_| {
                pkg::sync(&cwd).map(|_| ())
            }),
            ("add", [name, source, reference]) => {
                pkg::add(&cwd, name, source, Some(reference)).and_then(|_| {
                    pkg::sync(&cwd).map(|_| ())
                })
            }
            ("sync", []) => pkg::sync(&cwd).map(|installed| {
                if installed.is_empty() {
                    println!("no dependencies to install");
                }
            }),
            _ => Err(
                "usage:\n  machino pkg init <name>\n  machino pkg add <name> <source> [ref]\n  machino pkg sync"
                    .to_string(),
            ),
        },
        None => Err(
            "usage:\n  machino pkg init <name>\n  machino pkg add <name> <source> [ref]\n  machino pkg sync"
                .to_string(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{}", msg);
            ExitCode::from(2)
        }
    }
}

fn cmd_synth(rest: &[String]) -> ExitCode {
    let get_flag = |name: &str| -> Option<String> {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .cloned()
    };
    let count: usize = get_flag("--count")
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let seed: u64 = get_flag("--seed")
        .and_then(|v| v.parse().ok())
        .unwrap_or(42);
    let out_dir = get_flag("--out");

    if let Some(dir) = &out_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("error: cannot create '{}': {}", dir, e);
            return ExitCode::FAILURE;
        }
    }

    let mut rng = synth::Rng::new(seed);
    let mut emitted = 0usize;
    let mut attempts = 0usize;
    while emitted < count && attempts < count * 20 {
        attempts += 1;
        let source = synth::generate(&mut rng);
        // only verified programs make it into the corpus
        if compile_front(&source).is_err() {
            continue;
        }
        match &out_dir {
            Some(dir) => {
                let file = format!("{}/sample_{:05}.mno", dir, emitted);
                if let Err(e) = std::fs::write(&file, &source) {
                    eprintln!("error: cannot write '{}': {}", file, e);
                    return ExitCode::FAILURE;
                }
            }
            None => {
                println!("# ---- sample {} ----", emitted);
                println!("{}", source);
            }
        }
        emitted += 1;
    }
    if let Some(dir) = &out_dir {
        println!(
            "generated {} verified programs in {}/ (seed {})",
            emitted, dir, seed
        );
    }
    ExitCode::SUCCESS
}
