mod ast;
mod checker;
mod diag;
mod interp;
mod lexer;
mod parser;
mod synth;
mod wasm;

use diag::{line_col, Diagnostic};
use std::process::ExitCode;

const USAGE: &str = "machino — an AI-first language that compiles to WebAssembly

USAGE:
  machino check <file.mno> [--json]     type-check; emit structured diagnostics
  machino test  <file.mno> [--json]     run test blocks (contracts enforced)
  machino run   <file.mno>              run fn main() in the sandboxed interpreter
  machino build <file.mno> [-o out.wasm] compile to a portable .wasm module
  machino synth [--count N] [--seed S] [--out DIR]  generate a verified corpus

Run .wasm output anywhere: browsers, Node (see runners/run.mjs), wasmtime, etc.";

fn main() -> ExitCode {
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

struct Loaded {
    source: String,
    program: ast::Program,
}

/// Lex + parse + type-check. Prints diagnostics and returns None on failure.
fn load(path: &str, json: bool) -> Option<Loaded> {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read '{}': {}", path, e);
            return None;
        }
    };
    let diags: Vec<Diagnostic> = match compile_front(&source) {
        Ok(program) => return Some(Loaded { source, program }),
        Err(d) => d,
    };
    report(&diags, &source, path, json);
    None
}

fn compile_front(source: &str) -> Result<ast::Program, Vec<Diagnostic>> {
    let tokens = lexer::lex(source).map_err(|d| vec![d])?;
    let program = parser::Parser::new(&tokens, source)
        .parse_program()
        .map_err(|d| vec![d])?;
    checker::Checker::new(&program).check()?;
    Ok(program)
}

fn report(diags: &[Diagnostic], source: &str, path: &str, json: bool) {
    if json {
        let items: Vec<String> = diags.iter().map(|d| d.to_json(source, path)).collect();
        println!(
            "{{\"ok\":false,\"errors\":{},\"diagnostics\":[{}]}}",
            diags.len(),
            items.join(",")
        );
    } else {
        for d in diags {
            eprintln!("{}", d.render_human(source, path));
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
            let n_fns = loaded.program.functions.len();
            let n_tests = loaded.program.tests.len();
            if json {
                println!(
                    "{{\"ok\":true,\"errors\":0,\"functions\":{},\"tests\":{}}}",
                    n_fns, n_tests
                );
            } else {
                println!(
                    "ok: {} ({} function{}, {} test{})",
                    path,
                    n_fns,
                    if n_fns == 1 { "" } else { "s" },
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
                let (line, col) = line_col(&loaded.source, e.span.start);
                if json {
                    results.push(format!(
                        "{{\"name\":\"{}\",\"ok\":false,\"error\":\"{}\",\"line\":{},\"col\":{}}}",
                        diag::json_escape(&t.name),
                        diag::json_escape(&e.message),
                        line,
                        col
                    ));
                } else {
                    println!("FAIL  {}", t.name);
                    println!("      {} ({}:{}:{})", e.message, path, line, col);
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
    let Some(loaded) = load(path, false) else {
        return ExitCode::FAILURE;
    };
    let mut interp = interp::Interp::new(&loaded.program);
    match interp.run_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let (line, col) = line_col(&loaded.source, e.span.start);
            eprintln!("runtime error: {} ({}:{}:{})", e.message, path, line, col);
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
    if !loaded.program.functions.iter().any(|f| f.name == "main" && !f.is_extern) {
        eprintln!("error: 'machino build' requires a 'fn main()' entry point");
        return ExitCode::FAILURE;
    }
    let bytes = wasm::compile(&loaded.program, &loaded.source);
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
