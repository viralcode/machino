mod ast;
mod checker;
mod diag;
mod fmt;
mod interp;
mod lexer;
mod mono;
mod ns;
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
  machino check <file.mno> [--json] [--verify]   type-check; --verify proves contracts with Z3
  machino test  <file.mno> [--json]              run test blocks (contracts enforced)
  machino run   <file.mno> [args...] [--trace]   run fn main(); --trace emits JSON call events
  machino build <file.mno> [-o out.wasm]         compile to a portable .wasm module
                [--gc] [--stack-mib N] [--no-cache]
  machino query <file.mno>                       JSON signatures of every top-level item
  machino fmt   <file.mno> [--check]             canonical formatter
  machino fuzz  <file.mno> [--runs N] [--seed S] contract-driven property testing
  machino synth [--count N] [--seed S] [--out DIR]  generate a verified corpus

  machino pkg init <name>                create a machino.pkg manifest here
  machino pkg add <name> <source> [ref]  add a dependency (path or git URL)
  machino pkg sync                       install deps into machino_modules/
  machino pkg publish [--registry URL]   upload this package to a registry

Import from packages with:  import \"pkg:<name>/<file>.mno\"

Run .wasm output anywhere: browsers, Node (see runners/run.mjs), wasmtime, etc.
Built with --gc, output targets the WASM-GC proposal (runners/run-gc.mjs, Node 22+).";

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
        "query" => cmd_query(rest),
        "fmt" => cmd_fmt(rest),
        "fuzz" => cmd_fuzz(rest),
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
    /// Namespace alias if this file was imported with `import ... as alias`.
    alias: Option<String>,
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

    /// Remaps a bundle-level diagnostic (including any fix span) into
    /// file-local coordinates.
    fn map_diag(&self, d: &Diagnostic) -> (Diagnostic, &str, &str) {
        let (path, src, local) = self.locate(d.span);
        let mut mapped = Diagnostic::new(d.code, d.message.clone(), local);
        mapped.help = d.help.clone();
        if let Some(fix) = &d.fix {
            let (_, _, fix_local) = self.locate(fix.span);
            mapped.fix = Some(diag::Fix {
                span: fix_local,
                replacement: fix.replacement.clone(),
            });
        }
        (mapped, path, src)
    }

    fn render_error(&self, d: &Diagnostic) -> String {
        let (mapped, path, src) = self.map_diag(d);
        mapped.render_human(src, path)
    }

    fn error_json(&self, d: &Diagnostic) -> String {
        let (mapped, path, src) = self.map_diag(d);
        mapped.to_json(src, path)
    }

    fn runtime_location(&self, span: Span) -> String {
        let (path, src, local) = self.locate(span);
        let (line, col) = line_col(src, local.start);
        format!("{}:{}:{}", path, line, col)
    }
}

/// Parses one file just enough to discover its imports (path, alias).
fn discover_imports(source: &str, path: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let tokens = lexer::lex(source)
        .map_err(|d| d.render_human(source, path))?;
    let program = parser::Parser::new(&tokens, source)
        .parse_program()
        .map_err(|d| d.render_human(source, path))?;
    Ok(program
        .imports
        .into_iter()
        .map(|(p, alias, _)| (p, alias))
        .collect())
}

/// Reads the entry file plus its transitive imports and appends the std
/// prelude, producing a single bundle with a segment map for diagnostics.
fn bundle_sources(entry: &str) -> Result<(String, Vec<Segment>), String> {
    // (display path, source, alias)
    let mut ordered: Vec<(String, String, Option<String>)> = Vec::new();
    // canonical path -> alias it was first imported with
    let mut visited: std::collections::HashMap<PathBuf, Option<String>> =
        std::collections::HashMap::new();
    let mut queue: Vec<(PathBuf, String, Option<String>)> = Vec::new();

    let entry_path = PathBuf::from(entry);
    queue.push((entry_path.clone(), entry.to_string(), None));

    while let Some((path, display, alias)) = queue.pop() {
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        if let Some(prev) = visited.get(&canon) {
            if *prev != alias {
                return Err(format!(
                    "error: '{}' is imported under two different namespaces ({} vs {}); \
                     use one alias consistently",
                    display,
                    prev.as_deref().unwrap_or("<none>"),
                    alias.as_deref().unwrap_or("<none>")
                ));
            }
            continue;
        }
        visited.insert(canon, alias.clone());
        let source = std::fs::read_to_string(&path)
            .map_err(|e| format!("error: cannot read '{}': {}", display, e))?;
        let imports = discover_imports(&source, &display)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        for (imp, imp_alias) in imports {
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
            queue.push((ipath.clone(), ipath.display().to_string(), imp_alias));
        }
        ordered.push((display, source, alias));
    }

    let mut bundle = String::new();
    let mut segments = Vec::new();
    for (path, source, alias) in &ordered {
        segments.push(Segment {
            path: path.clone(),
            start: bundle.len() as u32,
            len: source.len() as u32 + 1,
            alias: alias.clone(),
        });
        bundle.push_str(source);
        bundle.push('\n');
    }
    segments.push(Segment {
        path: "<machino std>".to_string(),
        start: bundle.len() as u32,
        len: STD_PRELUDE.len() as u32,
        alias: None,
    });
    bundle.push_str(STD_PRELUDE);
    Ok((bundle, segments))
}

/// Lex + parse + type-check a bundle. The std segment starts at `std_start`.
/// When `mono` is false the checked program keeps its generic templates
/// (used by `query`, which reports signatures as written).
fn compile_bundle_opts(
    bundle: &str,
    std_start: u32,
    mono: bool,
) -> Result<ast::Program, Vec<Diagnostic>> {
    compile_bundle_full(bundle, std_start, mono, &[])
}

fn compile_bundle_full(
    bundle: &str,
    std_start: u32,
    mono: bool,
    aliases: &[ns::AliasedSegment],
) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(bundle).map_err(|d| vec![d])?;
    let mut program = parser::Parser::new(&tokens, bundle)
        .parse_program()
        .map_err(|d| vec![d])?;
    ns::apply(&mut program, aliases);
    for f in &mut program.functions {
        f.is_std = f.span.start >= std_start;
    }
    for s in &mut program.structs {
        s.is_std = s.span.start >= std_start;
    }
    let instantiations = checker::Checker::new(&program, bundle).check()?;
    // Generic functions were checked polymorphically; instantiate the concrete
    // versions every call site needs and rewrite calls to use them.
    if mono
        && (!instantiations.is_empty()
            || program.functions.iter().any(|f| !f.type_params.is_empty()))
    {
        program = mono::monomorphize_with(&program, &instantiations)
            .map_err(|d| vec![d])?;
        // re-check the fully concrete program (cheap; catches substitution bugs)
        checker::Checker::new(&program, bundle).check()?;
    }
    Ok(program)
}

fn compile_bundle(bundle: &str, std_start: u32) -> Result<ast::Program, Vec<Diagnostic>> {
    compile_bundle_opts(bundle, std_start, true)
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
    load_opts(path, json, true)
}

fn load_opts(path: &str, json: bool, mono: bool) -> Option<Loaded> {
    let (bundle, segments) = match bundle_sources(path) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{}", msg);
            return None;
        }
    };
    let std_start = segments.last().map(|s| s.start).unwrap_or(0);
    let aliases: Vec<ns::AliasedSegment> = segments
        .iter()
        .filter_map(|s| {
            s.alias.as_ref().map(|a| ns::AliasedSegment {
                start: s.start,
                end: s.start + s.len,
                alias: a.clone(),
            })
        })
        .collect();
    match compile_bundle_full(&bundle, std_start, mono, &aliases) {
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
    let verify = rest.iter().any(|a| a == "--verify");
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
            let mut verify_json = String::new();
            let mut verify_failed = false;
            if verify {
                if !smt::smt_available() {
                    eprintln!(
                        "error: this machino binary was built without the SMT verifier; rebuild with: cargo build --features smt"
                    );
                    return ExitCode::from(2);
                }
                let reports = smt::verify_program(&loaded.program);
                let mut items: Vec<String> = Vec::new();
                for r in &reports {
                    if r.vacuous_requires {
                        verify_failed = true;
                        if json {
                            items.push(format!(
                                "{{\"function\":\"{}\",\"clause\":\"requires\",\"result\":\"vacuous\",\"detail\":\"requires clauses are contradictory: no input can satisfy them\"}}",
                                diag::json_escape(&r.function)
                            ));
                        } else {
                            println!(
                                "VERIFY {}: requires clauses are contradictory (no input can call this function)",
                                r.function
                            );
                        }
                    }
                    for (clause, result) in &r.clauses {
                        let (status, detail) = match result {
                            smt::VerifyResult::Verified => ("proved", String::new()),
                            smt::VerifyResult::Counterexample(m) => {
                                verify_failed = true;
                                ("counterexample", m.clone())
                            }
                            smt::VerifyResult::Unknown(m) => ("unknown", m.clone()),
                        };
                        if json {
                            items.push(format!(
                                "{{\"function\":\"{}\",\"clause\":\"{}\",\"result\":\"{}\",\"detail\":\"{}\"}}",
                                diag::json_escape(&r.function),
                                diag::json_escape(clause),
                                status,
                                diag::json_escape(&detail)
                            ));
                        } else {
                            match result {
                                smt::VerifyResult::Verified => {
                                    println!("VERIFY {}: ensures {}  [proved]", r.function, clause)
                                }
                                smt::VerifyResult::Counterexample(m) => println!(
                                    "VERIFY {}: ensures {}  [COUNTEREXAMPLE {}]",
                                    r.function, clause, m
                                ),
                                smt::VerifyResult::Unknown(m) => println!(
                                    "VERIFY {}: ensures {}  [unknown: {}] (runtime enforcement still applies)",
                                    r.function, clause, m
                                ),
                            }
                        }
                    }
                }
                verify_json = format!(",\"verify\":[{}]", items.join(","));
            }
            if json {
                println!(
                    "{{\"ok\":{},\"errors\":0,\"functions\":{},\"structs\":{},\"tests\":{}{}}}",
                    !verify_failed, n_fns, n_structs, n_tests, verify_json
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
            if verify_failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
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

    interp::set_spawn_program(loaded.program.clone());
    let mut passed = 0usize;
    let mut results: Vec<String> = Vec::new();
    for t in &loaded.program.tests {
        let captured = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let mut interp = interp::Interp::new(&loaded.program);
        if t.expects.is_some() {
            let sink = captured.clone();
            interp.output = Box::new(move |line| sink.borrow_mut().push(line.to_string()));
        }
        let run = interp.run_stmts_as_test(&t.body);
        drop(interp);
        // snapshot comparison happens only when the body itself succeeded
        let outcome = run.and_then(|()| {
            if let Some(want) = &t.expects {
                let got = captured.borrow().join("\n");
                if got != want.as_str() {
                    return Err(interp::RuntimeError::new(
                        format!(
                            "snapshot mismatch: expected {:?}, got {:?}",
                            want, got
                        ),
                        t.span,
                    ));
                }
            }
            Ok(())
        });
        match outcome {
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
    let trace = rest.iter().any(|a| a == "--trace");
    let Some(path) = parse_file_arg(rest, "run") else {
        return ExitCode::from(2);
    };
    let file_pos = rest.iter().position(|a| a == path).unwrap_or(0);
    let program_args: Vec<String> = rest[file_pos + 1..]
        .iter()
        .filter(|a| *a != "--trace")
        .cloned()
        .collect();
    let Some(loaded) = load(path, false) else {
        return ExitCode::FAILURE;
    };
    // spawned threads execute against their own copy of the program
    interp::set_spawn_program(loaded.program.clone());
    let mut interp = interp::Interp::new(&loaded.program);
    interp.args = program_args;
    if trace {
        // one JSON object per line on stderr, so stdout stays clean
        interp.trace = Some(Box::new(|line| eprintln!("{}", line)));
    }
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

/// FNV-1a 64-bit content hash, used as the incremental build cache key.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn cmd_build(rest: &[String]) -> ExitCode {
    let use_gc = rest.iter().any(|a| a == "--gc");
    let no_cache = rest.iter().any(|a| a == "--no-cache");
    let stack_mib: u32 = rest
        .iter()
        .position(|a| a == "--stack-mib")
        .and_then(|i| rest.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(16);
    // drop flag-value pairs so the positional file argument is found correctly
    let positional: Vec<String> = {
        let mut out = Vec::new();
        let mut skip = false;
        for a in rest {
            if skip {
                skip = false;
                continue;
            }
            if a == "-o" || a == "--stack-mib" {
                skip = true;
                continue;
            }
            out.push(a.clone());
        }
        out
    };
    let Some(path) = parse_file_arg(&positional, "build") else {
        return ExitCode::from(2);
    };
    let path = path.to_string();
    let path = path.as_str();
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
    if use_gc {
        if let Some(span) = find_concurrency_use(&loaded.program) {
            let d = Diagnostic::new(
                "E072",
                "the WASM-GC backend has no thread or channel support; use the default backend for spawn/join and channels",
                span,
            )
            .with_help("compile without --gc, or run with 'machino run'");
            eprintln!("{}", loaded.render_error(&d));
            return ExitCode::FAILURE;
        }
    } else if let Some(d) = validate_spawn_targets(&loaded.program) {
        eprintln!("{}", loaded.render_error(&d));
        return ExitCode::FAILURE;
    }
    // incremental compilation: the cache key covers the whole source bundle
    // (entry file + imports + std), compiler version, backend, and flags
    let cache_key = {
        let mut input = loaded.bundle.clone().into_bytes();
        input.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
        input.extend_from_slice(if use_gc { b"gc" } else { b"linear" });
        input.extend_from_slice(&stack_mib.to_le_bytes());
        fnv1a64(&input)
    };
    let cache_dir = std::env::temp_dir().join("machino-build-cache");
    let cache_file = cache_dir.join(format!("{:016x}.wasm", cache_key));
    if !no_cache {
        if let Ok(bytes) = std::fs::read(&cache_file) {
            return match std::fs::write(&out_path, &bytes) {
                Ok(()) => {
                    println!("wrote {} ({} bytes, cached)", out_path, bytes.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("error: cannot write '{}': {}", out_path, e);
                    ExitCode::FAILURE
                }
            };
        }
    }
    let bytes = if use_gc {
        match wasmgc::compile(&loaded.program) {
            Ok(b) => b,
            Err(d) => {
                eprintln!("{}", loaded.render_error(&d));
                return ExitCode::FAILURE;
            }
        }
    } else {
        wasm::compile_with_stack(&loaded.program, &loaded.bundle, stack_mib * 1024 * 1024)
    };
    if !no_cache {
        // best-effort: a failed cache write must not fail the build
        let _ = std::fs::create_dir_all(&cache_dir)
            .and_then(|()| std::fs::write(&cache_file, &bytes));
    }
    match std::fs::write(&out_path, &bytes) {
        Ok(()) => {
            println!("wrote {} ({} bytes)", out_path, bytes.len());
            if use_gc {
                println!("run it on a WASM-GC host:  node runners/run-gc.mjs {}", out_path);
            } else {
                println!("run it anywhere:  node runners/run.mjs {}", out_path);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: cannot write '{}': {}", out_path, e);
            ExitCode::FAILURE
        }
    }
}

/// Compiled spawn runs the target in a fresh module instance on another
/// thread, located by its export name — so the first argument must be a
/// named top-level function, not a lambda or a variable holding one.
fn validate_spawn_targets(program: &ast::Program) -> Option<Diagnostic> {
    fn in_expr(e: &ast::Expr, fns: &std::collections::HashSet<&str>) -> Option<Diagnostic> {
        use ast::ExprKind::*;
        match &e.kind {
            Call(name, args) => {
                if name == "spawn" {
                    let ok = matches!(&args[0].kind, Var(n) if fns.contains(n.as_str()));
                    if !ok {
                        return Some(
                            Diagnostic::new(
                                "E074",
                                "compiled spawn requires a named top-level function as its first argument",
                                args[0].span,
                            )
                            .with_help(
                                "lambdas and function-typed variables can be spawned by the interpreter only; name the function at top level",
                            ),
                        );
                    }
                }
                args.iter().find_map(|a| in_expr(a, fns))
            }
            Array(elems) => elems.iter().find_map(|a| in_expr(a, fns)),
            Index(a, b) | Bin(_, a, b) => in_expr(a, fns).or_else(|| in_expr(b, fns)),
            Field(a, _) | Un(_, a) => in_expr(a, fns),
            Lambda(l) => in_stmts(&l.body, fns),
            Match(m) => in_expr(&m.scrutinee, fns)
                .or_else(|| m.arms.iter().find_map(|arm| in_expr(&arm.body, fns))),
            _ => None,
        }
    }
    fn in_stmts(
        stmts: &[ast::Stmt],
        fns: &std::collections::HashSet<&str>,
    ) -> Option<Diagnostic> {
        use ast::StmtKind::*;
        stmts.iter().find_map(|s| match &s.kind {
            Let { value, .. } | Assign { value, .. } => in_expr(value, fns),
            IndexAssign { base, index, value } => in_expr(base, fns)
                .or_else(|| in_expr(index, fns))
                .or_else(|| in_expr(value, fns)),
            FieldAssign { base, value, .. } => {
                in_expr(base, fns).or_else(|| in_expr(value, fns))
            }
            If {
                cond,
                then_body,
                else_body,
            } => in_expr(cond, fns)
                .or_else(|| in_stmts(then_body, fns))
                .or_else(|| in_stmts(else_body, fns)),
            While { cond, body } => in_expr(cond, fns).or_else(|| in_stmts(body, fns)),
            For {
                start, end, body, ..
            } => in_expr(start, fns)
                .or_else(|| in_expr(end, fns))
                .or_else(|| in_stmts(body, fns)),
            Return(Some(e)) | Assert(e) | Expr(e) => in_expr(e, fns),
            _ => None,
        })
    }
    let fns: std::collections::HashSet<&str> = program
        .functions
        .iter()
        .filter(|f| !f.is_extern)
        .map(|f| f.name.as_str())
        .collect();
    program
        .functions
        .iter()
        .find_map(|f| in_stmts(&f.body, &fns))
}

/// Returns the span of the first spawn/join or chan_* call, if any. The
/// WASM-GC backend has no thread or channel support, so `machino build --gc`
/// rejects these programs.
fn find_concurrency_use(program: &ast::Program) -> Option<Span> {
    const UNSUPPORTED: &[&str] = &[
        "spawn",
        "join_int",
        "join_float",
        "join_bool",
        "join_str",
        "chan_new",
        "chan_close",
        "chan_send_int",
        "chan_send_float",
        "chan_send_bool",
        "chan_send_str",
        "chan_recv_int",
        "chan_recv_float",
        "chan_recv_bool",
        "chan_recv_str",
    ];
    fn in_expr(e: &ast::Expr) -> Option<Span> {
        use ast::ExprKind::*;
        match &e.kind {
            Call(name, args) => {
                if UNSUPPORTED.contains(&name.as_str()) {
                    return Some(e.span);
                }
                args.iter().find_map(in_expr)
            }
            Array(elems) => elems.iter().find_map(in_expr),
            Index(a, b) | Bin(_, a, b) => in_expr(a).or_else(|| in_expr(b)),
            Field(a, _) | Un(_, a) => in_expr(a),
            Lambda(l) => in_stmts(&l.body),
            Match(m) => in_expr(&m.scrutinee)
                .or_else(|| m.arms.iter().find_map(|arm| in_expr(&arm.body))),
            _ => None,
        }
    }
    fn in_stmts(stmts: &[ast::Stmt]) -> Option<Span> {
        use ast::StmtKind::*;
        stmts.iter().find_map(|s| match &s.kind {
            Let { value, .. } | Assign { value, .. } => in_expr(value),
            IndexAssign { base, index, value } => in_expr(base)
                .or_else(|| in_expr(index))
                .or_else(|| in_expr(value)),
            FieldAssign { base, value, .. } => in_expr(base).or_else(|| in_expr(value)),
            If {
                cond,
                then_body,
                else_body,
            } => in_expr(cond)
                .or_else(|| in_stmts(then_body))
                .or_else(|| in_stmts(else_body)),
            While { cond, body } => in_expr(cond).or_else(|| in_stmts(body)),
            For {
                start, end, body, ..
            } => in_expr(start)
                .or_else(|| in_expr(end))
                .or_else(|| in_stmts(body)),
            Return(Some(e)) | Assert(e) | Expr(e) => in_expr(e),
            _ => None,
        })
    }
    program.functions.iter().find_map(|f| in_stmts(&f.body))
}

const PKG_USAGE: &str = "usage:\n  machino pkg init <name>\n  machino pkg add <name> <source> [ref]\n  machino pkg sync\n  machino pkg publish [--registry <url>] [--token <token>]";

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
            ("publish", publish_args) => {
                let get_flag = |name: &str| -> Option<String> {
                    publish_args
                        .iter()
                        .position(|a| a == name)
                        .and_then(|i| publish_args.get(i + 1))
                        .cloned()
                };
                let registry_url = get_flag("--registry")
                    .or_else(|| std::env::var("MACHINO_REGISTRY").ok())
                    .ok_or_else(|| {
                        "error: no registry configured; pass --registry <url> or set MACHINO_REGISTRY"
                            .to_string()
                    });
                let token = get_flag("--token")
                    .or_else(|| std::env::var("MACHINO_TOKEN").ok())
                    .unwrap_or_default();
                registry_url.and_then(|url| {
                    if !cwd.join("machino.pkg").exists() {
                        return Err(
                            "error: no machino.pkg here; run 'machino pkg init <name>' first"
                                .to_string(),
                        );
                    }
                    registry::upload_package(&url, &cwd, &token).map(|msg| println!("{}", msg))
                })
            }
            _ => Err(PKG_USAGE.to_string()),
        },
        None => Err(PKG_USAGE.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{}", msg);
            ExitCode::from(2)
        }
    }
}

/// `machino query <file> [name] [--std]` — JSON introspection over the
/// program's signatures, for agents that need type information without
/// parsing machino source themselves.
fn cmd_query(rest: &[String]) -> ExitCode {
    let include_std = rest.iter().any(|a| a == "--std");
    let Some(path) = parse_file_arg(rest, "query") else {
        return ExitCode::from(2);
    };
    let filter: Option<&str> = rest
        .iter()
        .filter(|a| !a.starts_with("--"))
        .map(|s| s.as_str())
        .find(|s| *s != path);
    // query reports signatures as written: keep generic templates, don't
    // replace them with monomorphized copies
    let Some(loaded) = load_opts(path, true, false) else {
        return ExitCode::FAILURE;
    };
    let p = &loaded.program;

    let type_json = |t: &ast::Type| format!("\"{}\"", diag::json_escape(&t.to_string()));
    let mut fns: Vec<String> = Vec::new();
    for f in &p.functions {
        if (f.is_std && !include_std) || filter.map_or(false, |n| n != f.name) {
            continue;
        }
        let params: Vec<String> = f
            .params
            .iter()
            .map(|pr| {
                format!(
                    "{{\"name\":\"{}\",\"type\":{}}}",
                    diag::json_escape(&pr.name),
                    type_json(&pr.ty)
                )
            })
            .collect();
        let tparams: Vec<String> = f
            .type_params
            .iter()
            .map(|tp| {
                format!(
                    "{{\"name\":\"{}\",\"bounds\":[{}]}}",
                    diag::json_escape(&tp.name),
                    tp.bounds
                        .iter()
                        .map(|b| format!("\"{}\"", b))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect();
        let requires: Vec<String> = f
            .requires
            .iter()
            .map(|c| format!("\"{}\"", diag::json_escape(&c.text)))
            .collect();
        let ensures: Vec<String> = f
            .ensures
            .iter()
            .map(|c| format!("\"{}\"", diag::json_escape(&c.text)))
            .collect();
        fns.push(format!(
            "{{\"name\":\"{}\",\"typeParams\":[{}],\"params\":[{}],\"returns\":{},\"requires\":[{}],\"ensures\":[{}],\"extern\":{},\"std\":{}}}",
            diag::json_escape(&f.name),
            tparams.join(","),
            params.join(","),
            type_json(&f.ret),
            requires.join(","),
            ensures.join(","),
            f.is_extern,
            f.is_std
        ));
    }
    let mut structs: Vec<String> = Vec::new();
    for s in &p.structs {
        if (s.is_std && !include_std) || filter.map_or(false, |n| n != s.name) {
            continue;
        }
        let fields: Vec<String> = s
            .fields
            .iter()
            .map(|fd| {
                format!(
                    "{{\"name\":\"{}\",\"type\":{}}}",
                    diag::json_escape(&fd.name),
                    type_json(&fd.ty)
                )
            })
            .collect();
        structs.push(format!(
            "{{\"name\":\"{}\",\"fields\":[{}]}}",
            diag::json_escape(&s.name),
            fields.join(",")
        ));
    }
    let mut enums: Vec<String> = Vec::new();
    for e in &p.enums {
        if filter.map_or(false, |n| n != e.name) {
            continue;
        }
        let variants: Vec<String> = e
            .variants
            .iter()
            .map(|v| match &v.payload {
                Some(t) => format!(
                    "{{\"name\":\"{}\",\"payload\":{}}}",
                    diag::json_escape(&v.name),
                    type_json(t)
                ),
                None => format!("{{\"name\":\"{}\"}}", diag::json_escape(&v.name)),
            })
            .collect();
        enums.push(format!(
            "{{\"name\":\"{}\",\"variants\":[{}]}}",
            diag::json_escape(&e.name),
            variants.join(",")
        ));
    }
    let tests: Vec<String> = p
        .tests
        .iter()
        .filter(|t| filter.map_or(true, |n| n == t.name))
        .map(|t| format!("\"{}\"", diag::json_escape(&t.name)))
        .collect();
    println!(
        "{{\"functions\":[{}],\"structs\":[{}],\"enums\":[{}],\"tests\":[{}]}}",
        fns.join(","),
        structs.join(","),
        enums.join(","),
        tests.join(",")
    );
    ExitCode::SUCCESS
}

/// `machino fmt <files...> [--check|--stdout]` — canonical formatter.
fn cmd_fmt(rest: &[String]) -> ExitCode {
    let check_only = rest.iter().any(|a| a == "--check");
    let to_stdout = rest.iter().any(|a| a == "--stdout");
    let files: Vec<&String> = rest.iter().filter(|a| !a.starts_with("--")).collect();
    if files.is_empty() {
        eprintln!("usage: machino fmt <file.mno>... [--check|--stdout]");
        return ExitCode::from(2);
    }
    let mut changed = 0usize;
    for f in &files {
        let source = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: cannot read '{}': {}", f, e);
                return ExitCode::FAILURE;
            }
        };
        // formatting requires valid syntax: report real parse errors instead
        // of a confusing safety-check failure
        if let Err(d) = lexer::lex(&source)
            .and_then(|toks| parser::Parser::new(&toks, &source).parse_program())
        {
            eprint!("{}", d.render_human(&source, f));
            return ExitCode::FAILURE;
        }
        let formatted = fmt::format_source(&source);
        if !fmt::tokens_preserved(&source, &formatted) {
            eprintln!(
                "error: refusing to format '{}': formatting would change the token stream (please report this)",
                f
            );
            return ExitCode::FAILURE;
        }
        if to_stdout {
            print!("{}", formatted);
            continue;
        }
        if formatted != source {
            changed += 1;
            if check_only {
                println!("would reformat: {}", f);
            } else {
                if let Err(e) = std::fs::write(f, &formatted) {
                    eprintln!("error: cannot write '{}': {}", f, e);
                    return ExitCode::FAILURE;
                }
                println!("formatted: {}", f);
            }
        }
    }
    if check_only && changed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Generates a random value of the given type for fuzzing. Returns None for
/// types fuzzing can't construct (functions, enums).
fn fuzz_value(
    ty: &ast::Type,
    structs: &std::collections::HashMap<String, Vec<ast::Param>>,
    rng: &mut synth::Rng,
) -> Option<interp::Value> {
    use interp::Value;
    Some(match ty {
        ast::Type::Int => {
            // biased toward boundary values, where contracts break
            let picks: [i64; 10] = [0, 1, -1, 2, -2, 7, 100, -100, i64::MAX, i64::MIN];
            match rng.range(3) {
                0 => Value::Int(picks[rng.range(picks.len() as u64) as usize]),
                1 => Value::Int(rng.range(2001) as i64 - 1000),
                _ => Value::Int(rng.next() as i64),
            }
        }
        ast::Type::Float => {
            let picks: [f64; 6] = [0.0, 1.0, -1.0, 0.5, 1e9, -1e9];
            match rng.range(2) {
                0 => Value::Float(picks[rng.range(picks.len() as u64) as usize]),
                _ => Value::Float((rng.range(2_000_001) as f64 - 1_000_000.0) / 1000.0),
            }
        }
        ast::Type::Bool => Value::Bool(rng.range(2) == 1),
        ast::Type::Str => {
            let n = rng.range(9);
            let mut s = String::new();
            for _ in 0..n {
                s.push((b'a' + rng.range(26) as u8) as char);
            }
            Value::Str(std::rc::Rc::new(s.into_bytes()))
        }
        ast::Type::Array(elem) => {
            let n = rng.range(6);
            let mut items = Vec::new();
            for _ in 0..n {
                items.push(fuzz_value(elem, structs, rng)?);
            }
            Value::Array(std::rc::Rc::new(std::cell::RefCell::new(items)))
        }
        ast::Type::Struct(name) => {
            let fields = structs.get(name)?;
            let mut map = std::collections::HashMap::new();
            for f in fields {
                map.insert(f.name.clone(), fuzz_value(&f.ty, structs, rng)?);
            }
            Value::Struct(std::rc::Rc::new(std::cell::RefCell::new(map)))
        }
        _ => return None,
    })
}

fn fuzz_display(v: &interp::Value) -> String {
    use interp::Value;
    match v {
        Value::Str(s) => format!("{:?}", String::from_utf8_lossy(s)),
        Value::Array(items) => {
            let inner: Vec<String> = items.borrow().iter().map(fuzz_display).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Struct(fields) => {
            let mut inner: Vec<String> = fields
                .borrow()
                .iter()
                .map(|(k, v)| format!("{}: {}", k, fuzz_display(v)))
                .collect();
            inner.sort();
            format!("{{{}}}", inner.join(", "))
        }
        other => other.display(),
    }
}

/// `machino fuzz <file.mno> [fn] [--runs N] [--seed S]` — contract-driven
/// property testing: random inputs are sampled (rejecting ones that violate
/// `requires`), then the function runs with full contract enforcement. Any
/// ensures violation, assert failure, or trap is a bug, reported with the
/// concrete counterexample arguments.
fn cmd_fuzz(rest: &[String]) -> ExitCode {
    let json = rest.iter().any(|a| a == "--json");
    let get_flag = |name: &str| -> Option<String> {
        rest.iter()
            .position(|a| a == name)
            .and_then(|i| rest.get(i + 1))
            .cloned()
    };
    let runs: usize = get_flag("--runs").and_then(|v| v.parse().ok()).unwrap_or(200);
    let seed: u64 = get_flag("--seed").and_then(|v| v.parse().ok()).unwrap_or(42);
    let mut positional = rest
        .iter()
        .filter(|a| !a.starts_with("--"))
        .filter(|a| Some(a.as_str()) != get_flag("--runs").as_deref())
        .filter(|a| Some(a.as_str()) != get_flag("--seed").as_deref());
    let Some(path) = positional.next() else {
        eprintln!("usage: machino fuzz <file.mno> [fn] [--runs N] [--seed S]");
        return ExitCode::from(2);
    };
    let target_fn = positional.next().cloned();
    let Some(loaded) = load(path, json) else {
        return ExitCode::FAILURE;
    };
    let structs: std::collections::HashMap<String, Vec<ast::Param>> = loaded
        .program
        .structs
        .iter()
        .map(|s| (s.name.clone(), s.fields.clone()))
        .collect();

    let targets: Vec<&ast::Function> = loaded
        .program
        .functions
        .iter()
        .filter(|f| !f.is_std && !f.is_extern && f.name != "main")
        .filter(|f| match &target_fn {
            Some(n) => &f.name == n,
            // by default fuzz every function that declares a contract
            None => !f.requires.is_empty() || !f.ensures.is_empty(),
        })
        .collect();
    if targets.is_empty() {
        eprintln!(
            "nothing to fuzz: {} (no {} found)",
            path,
            target_fn
                .as_deref()
                .map(|n| format!("function named '{}'", n))
                .unwrap_or_else(|| "functions with contracts".to_string())
        );
        return ExitCode::from(2);
    }

    let mut rng = synth::Rng::new(seed);
    let mut failed = 0usize;
    let mut results: Vec<String> = Vec::new();
    for f in targets {
        // functions with un-fuzzable parameter types are skipped explicitly
        if f.params
            .iter()
            .any(|p| fuzz_value(&p.ty, &structs, &mut synth::Rng::new(1)).is_none())
        {
            if json {
                results.push(format!(
                    "{{\"function\":\"{}\",\"status\":\"skipped\",\"reason\":\"parameter types cannot be fuzzed\"}}",
                    diag::json_escape(&f.name)
                ));
            } else {
                println!("SKIP  {} (parameter types cannot be fuzzed)", f.name);
            }
            continue;
        }
        let mut tested = 0usize;
        let mut rejected = 0usize;
        let mut failure: Option<(String, String)> = None; // (args, error)
        let mut attempts = 0usize;
        while tested < runs && attempts < runs * 50 {
            attempts += 1;
            let args: Vec<interp::Value> = f
                .params
                .iter()
                .map(|p| fuzz_value(&p.ty, &structs, &mut rng).unwrap())
                .collect();
            let shown: Vec<String> = args.iter().map(fuzz_display).collect();
            let mut interp = interp::Interp::new(&loaded.program);
            // suppress program output during fuzzing
            interp.output = Box::new(|_| {});
            match interp.call_by_name(&f.name, args) {
                Ok(_) => tested += 1,
                Err(e) => {
                    let is_precondition_reject = e.message.contains("requires")
                        && e.message.contains(&format!("calling '{}'", f.name));
                    if is_precondition_reject {
                        rejected += 1;
                    } else {
                        failure = Some((shown.join(", "), e.message));
                        break;
                    }
                }
            }
        }
        match failure {
            Some((args, error)) => {
                failed += 1;
                if json {
                    results.push(format!(
                        "{{\"function\":\"{}\",\"status\":\"failed\",\"args\":\"{}\",\"error\":\"{}\",\"passed\":{}}}",
                        diag::json_escape(&f.name),
                        diag::json_escape(&args),
                        diag::json_escape(&error),
                        tested
                    ));
                } else {
                    println!("FAIL  {}({})", f.name, args);
                    println!("      {}", error);
                }
            }
            None => {
                if json {
                    results.push(format!(
                        "{{\"function\":\"{}\",\"status\":\"ok\",\"passed\":{},\"rejected\":{}}}",
                        diag::json_escape(&f.name),
                        tested,
                        rejected
                    ));
                } else {
                    println!(
                        "PASS  {} ({} inputs, {} rejected by requires)",
                        f.name, tested, rejected
                    );
                }
            }
        }
    }
    if json {
        println!(
            "{{\"ok\":{},\"failed\":{},\"results\":[{}]}}",
            failed == 0,
            failed,
            results.join(",")
        );
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
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
