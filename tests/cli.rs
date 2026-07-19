//! End-to-end tests driving the machino CLI binary.

use std::path::PathBuf;
use std::process::{Command, Output};

fn machino(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_machino"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to run machino binary")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write_temp(name: &str, source: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, source).unwrap();
    path
}

#[test]
fn check_passes_on_examples() {
    for ex in [
        "hello",
        "fib",
        "sort",
        "contracts",
        "structs",
        "wordcount",
        "higher_order",
        "closures",
        "http_server",
        "enums",
        "generics",
        "json",
        "stdlib_tour",
        "concurrency",
        "maze_solver",
        "namespaces/main",
    ] {
        let out = machino(&["check", &format!("examples/{}.mno", ex)]);
        assert!(out.status.success(), "check failed for {}: {:?}", ex, out);
    }
}

#[test]
fn tests_pass_on_examples() {
    for ex in [
        "fib",
        "sort",
        "contracts",
        "structs",
        "wordcount",
        "higher_order",
        "closures",
        "http_server",
        "enums",
        "json",
        "stdlib_tour",
        "concurrency",
        "maze_solver",
        "namespaces/main",
    ] {
        let out = machino(&["test", &format!("examples/{}.mno", ex)]);
        assert!(out.status.success(), "tests failed for {}: {:?}", ex, out);
        assert!(stdout(&out).contains(", 0 failed"));
    }
}

#[test]
fn run_produces_expected_output() {
    let out = machino(&["run", "examples/hello.mno"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out).trim(), "hello from machino");
}

#[test]
fn fib_output_is_correct() {
    let out = machino(&["run", "examples/fib.mno"]);
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "0");
    assert_eq!(lines[10], "55");
    assert_eq!(lines[15], "610");
}

#[test]
fn maze_solver_output_is_correct() {
    let out = machino(&["run", "examples/maze_solver.mno"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        "steps=22 visited=23 moves=DDDDRRRRRRUULLLLUURRRR\n#########\n#S#****G#\n#*#*#####\n#*#*****#\n#*#####*#\n#*******#\n#########"
    );
}

#[test]
fn type_errors_are_structured_json() {
    let path = write_temp(
        "type_error.mno",
        "fn main() {\n    let x: int = 1.5\n}\n",
    );
    let out = machino(&["check", path.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("\"ok\":false"));
    assert!(text.contains("\"code\":\"E030\""));
    assert!(text.contains("\"line\":2"));
}

#[test]
fn requires_contract_is_enforced() {
    let path = write_temp(
        "contract.mno",
        "fn f(n: int) -> int\n    requires n > 0\n{\n    return n\n}\n\nfn main() {\n    print(f(-1))\n}\n",
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("requires 'n > 0' failed"), "got: {}", err);
}

#[test]
fn ensures_contract_is_enforced() {
    let path = write_temp(
        "ensures.mno",
        "fn f(n: int) -> int\n    ensures result > 100\n{\n    return n\n}\n\nfn main() {\n    print(f(1))\n}\n",
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ensures 'result > 100' failed"));
}

#[test]
fn failing_test_is_reported() {
    let path = write_temp(
        "failing.mno",
        "fn main() {\n}\n\ntest \"nope\" {\n    assert 1 == 2\n}\n",
    );
    let out = machino(&["test", path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("FAIL  nope"));
}

#[test]
fn runtime_safety_division_by_zero() {
    let path = write_temp(
        "divzero.mno",
        "fn main() {\n    let z = 0\n    print(10 / z)\n}\n",
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("division by zero"));
}

#[test]
fn runtime_safety_bounds_check() {
    let path = write_temp(
        "oob.mno",
        "fn main() {\n    let xs = [1, 2]\n    print(xs[5])\n}\n",
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("out of bounds"));
}

#[test]
fn build_emits_valid_wasm_header() {
    let src = write_temp("build_me.mno", "fn main() {\n    print(1 + 1)\n}\n");
    let out_wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("build_me.wasm");
    let out = machino(&[
        "build",
        src.to_str().unwrap(),
        "-o",
        out_wasm.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{:?}", out);
    let bytes = std::fs::read(&out_wasm).unwrap();
    assert_eq!(&bytes[0..8], b"\0asm\x01\0\0\0");
}

#[test]
fn wasm_output_matches_interpreter_in_node() {
    // skipped silently if node is not installed
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    for ex in [
        "fib",
        "sort",
        "contracts",
        "structs",
        "wordcount",
        "higher_order",
        "closures",
        "maze_solver",
    ] {
        let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{}.wasm", ex));
        let out = machino(&[
            "build",
            &format!("examples/{}.mno", ex),
            "-o",
            wasm.to_str().unwrap(),
        ]);
        assert!(out.status.success());

        let interp_out = stdout(&machino(&["run", &format!("examples/{}.mno", ex)]));
        let node_out = Command::new("node")
            .arg("runners/run.mjs")
            .arg(wasm.to_str().unwrap())
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert_eq!(
            interp_out,
            String::from_utf8_lossy(&node_out.stdout),
            "wasm/interpreter divergence for {}",
            ex
        );
    }
}

#[test]
fn wasm_gc_backend_matches_interpreter_in_node() {
    // WASM-GC needs Node 22+; skipped silently if node is not installed
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    for ex in ["fib", "sort", "structs", "higher_order", "closures"] {
        let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("gc_{}.wasm", ex));
        let out = machino(&[
            "build",
            "--gc",
            &format!("examples/{}.mno", ex),
            "-o",
            wasm.to_str().unwrap(),
        ]);
        assert!(
            out.status.success(),
            "gc build failed for {}: {}",
            ex,
            String::from_utf8_lossy(&out.stderr)
        );

        let interp_out = stdout(&machino(&["run", &format!("examples/{}.mno", ex)]));
        let node_out = Command::new("node")
            .arg("runners/run-gc.mjs")
            .arg(wasm.to_str().unwrap())
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert_eq!(
            interp_out,
            String::from_utf8_lossy(&node_out.stdout),
            "wasm-gc/interpreter divergence for {}: {}",
            ex,
            String::from_utf8_lossy(&node_out.stderr)
        );
    }
}

// ---- v0.2 features ----

/// Runs a program in the interpreter AND as compiled wasm under Node,
/// asserting both succeed with identical output. Returns the output.
fn assert_parity(name: &str, source: &str) -> String {
    let src = write_temp(&format!("{}.mno", name), source);
    let interp = machino(&["run", src.to_str().unwrap()]);
    assert!(
        interp.status.success(),
        "interp failed for {}: {}",
        name,
        String::from_utf8_lossy(&interp.stderr)
    );
    if Command::new("node").arg("--version").output().is_err() {
        return stdout(&interp);
    }
    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{}.wasm", name));
    let build = machino(&["build", src.to_str().unwrap(), "-o", wasm.to_str().unwrap()]);
    assert!(build.status.success(), "build failed for {}", name);
    let node = Command::new("node")
        .arg("runners/run.mjs")
        .arg(wasm.to_str().unwrap())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert_eq!(
        stdout(&interp),
        String::from_utf8_lossy(&node.stdout),
        "wasm/interpreter divergence for {}",
        name
    );
    stdout(&interp)
}

#[test]
fn structs_work_in_both_backends() {
    let out = assert_parity(
        "structs_parity",
        "struct P {\n    x: int\n    y: int\n}\n\nfn main() {\n    let p = P(3, 4)\n    p.x = p.x + 10\n    let q = p\n    q.y = 40\n    print(p.x)\n    print(p.y)\n}\n",
    );
    assert_eq!(out.trim(), "13\n40");
}

#[test]
fn for_break_continue_parity() {
    let out = assert_parity(
        "loops_parity",
        "fn main() {\n    let total = 0\n    for i in 0..10 {\n        if i == 3 { continue }\n        if i == 7 { break }\n        total = total + i\n    }\n    print(total)\n}\n",
    );
    assert_eq!(out.trim(), "18"); // 0+1+2+4+5+6
}

#[test]
fn string_stdlib_parity() {
    let out = assert_parity(
        "strings_parity",
        "fn main() {\n    print(str_of_int(-42))\n    print(parse_int(\"123\") + 1)\n    print(join(split(\"a,b,c\", \",\"), \"-\"))\n    print(to_upper(trim(\"  ok  \")))\n    print(str_of_bool(contains(\"hello world\", \"lo wo\")))\n    print(substr(\"machino\", 2, 5))\n    print(str_of_float(2.5, 2))\n}\n",
    );
    assert_eq!(
        out.trim(),
        "-42\n124\na-b-c\nOK\ntrue\nchi\n2.50"
    );
}

#[test]
fn first_class_functions_parity() {
    let out = assert_parity(
        "fnvals_parity",
        "fn twice(f: fn(int) -> int, v: int) -> int {\n    return f(f(v))\n}\n\nfn inc(n: int) -> int {\n    return n + 1\n}\n\nfn main() {\n    print(twice(inc, 40))\n    let g = inc\n    print(g(1))\n}\n",
    );
    assert_eq!(out.trim(), "42\n2");
}

#[test]
fn strmap_parity() {
    let out = assert_parity(
        "strmap_parity",
        "fn main() {\n    let m = strmap_new()\n    strmap_set(m, \"a\", \"1\")\n    strmap_set(m, \"b\", \"2\")\n    strmap_set(m, \"a\", \"3\")\n    print(strmap_get(m, \"a\"))\n    print(strmap_get_or(m, \"zz\", \"none\"))\n    print(strmap_len(m))\n}\n",
    );
    assert_eq!(out.trim(), "3\nnone\n2");
}

#[test]
fn int_overflow_traps_in_both_backends() {
    let src = write_temp(
        "overflow.mno",
        "fn main() {\n    let big = 9223372036854775807\n    print(big + 1)\n}\n",
    );
    let interp = machino(&["run", src.to_str().unwrap()]);
    assert!(!interp.status.success());
    assert!(String::from_utf8_lossy(&interp.stderr).contains("integer overflow in '+'"));

    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("overflow.wasm");
    machino(&["build", src.to_str().unwrap(), "-o", wasm.to_str().unwrap()]);
    let node = Command::new("node")
        .arg("runners/run.mjs")
        .arg(wasm.to_str().unwrap())
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(!node.status.success());
    assert!(String::from_utf8_lossy(&node.stderr).contains("integer overflow in '+'"));
}

#[test]
fn imports_resolve_and_map_diagnostics() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("modproj");
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    std::fs::write(
        dir.join("lib/util.mno"),
        "fn triple(n: int) -> int {\n    return n * 3\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("app.mno"),
        "import \"lib/util.mno\"\n\nfn main() {\n    print(triple(14))\n}\n",
    )
    .unwrap();
    let out = machino(&["run", dir.join("app.mno").to_str().unwrap()]);
    assert!(out.status.success(), "{:?}", out);
    assert_eq!(stdout(&out).trim(), "42");

    // an error inside the imported file is reported against that file
    std::fs::write(
        dir.join("lib/util.mno"),
        "fn triple(n: int) -> int {\n    return \"x\"\n}\n",
    )
    .unwrap();
    let bad = machino(&["check", dir.join("app.mno").to_str().unwrap(), "--json"]);
    assert!(!bad.status.success());
    assert!(stdout(&bad).contains("util.mno"));
    assert!(stdout(&bad).contains("\"line\":2"));
}

#[test]
fn http_server_serves_requests() {
    use std::io::{Read, Write};
    let port = 18777u16;
    let mut child = Command::new(env!("CARGO_BIN_EXE_machino"))
        .args(["run", "examples/http_server.mno", &port.to_string()])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start server");

    // wait for the listener to come up
    let mut resp = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(150));
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream
                    .write_all(b"GET /hello/tester HTTP/1.1\r\nHost: x\r\n\r\n")
                    .unwrap();
                stream.read_to_string(&mut resp).unwrap();
                break;
            }
            Err(_) if std::time::Instant::now() < deadline => continue,
            Err(e) => {
                let _ = child.kill();
                panic!("could not connect to machino http server: {}", e);
            }
        }
    }
    assert!(resp.contains("200 OK"), "response was: {}", resp);
    assert!(resp.contains("hello, tester!"), "response was: {}", resp);

    // /quit shuts the server down cleanly
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(b"GET /quit HTTP/1.1\r\n\r\n").unwrap();
    let mut bye = String::new();
    stream.read_to_string(&mut bye).unwrap();
    assert!(bye.contains("bye"));
    let status = child.wait().unwrap();
    assert!(status.success());
}

// ---- v0.3 features ----

#[test]
fn closures_capture_by_value_parity() {
    let out = assert_parity(
        "closures_parity",
        r#"fn make_adder(n: int) -> fn(int) -> int {
    return fn(x: int) -> int { return x + n }
}

fn apply_twice(f: fn(int) -> int, x: int) -> int {
    return f(f(x))
}

fn main() {
    let add5 = make_adder(5)
    let add9 = make_adder(9)
    print(add5(10))
    print(add9(10))
    print(apply_twice(add5, 1))

    let base = "pre-"
    let tag = fn(s: str) -> str { return base + s + "!" }
    print(tag("go"))

    # shared mutable state through a captured array
    let counter = [0]
    let bump = fn() { counter[0] = counter[0] + 1 }
    bump()
    bump()
    bump()
    print(counter[0])

    # a lambda capturing another closure, passed inline
    let mul = fn(a: int, b: int) -> int { return a * b }
    print(apply_twice(fn(x: int) -> int { return mul(x, 3) }, 2))

    # named functions still work as values next to closures
    print(apply_twice(make_adder(1), 0))
}
"#,
    );
    assert_eq!(out.trim(), "15\n19\n11\npre-go!\n3\n18\n2");
}

#[test]
fn captured_variables_are_read_only() {
    let path = write_temp(
        "capture_assign.mno",
        "fn main() {\n    let n = 1\n    let f = fn() { n = 2 }\n    f()\n}\n",
    );
    let out = machino(&["check", path.to_str().unwrap(), "--json"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains("\"code\":\"E049\""));
}

#[test]
fn gc_collects_garbage_under_memory_cap() {
    // Several GB of total string allocation. The wasm module caps memory at
    // 1 GiB, so this only finishes if the collector reclaims garbage.
    let source = r#"fn repeat_str(s: str, n: int) -> str {
    let out = ""
    for i in 0..n {
        out = out + s
    }
    return out
}

fn main() {
    let keep = ""
    let total = 0
    for i in 0..600 {
        let chunk = repeat_str("abcdefgh", 4096)
        total = total + len(chunk)
        if i % 200 == 0 {
            keep = substr(chunk, 0, 8)
        }
    }
    print(total)
    print(keep)
}
"#;
    let out = assert_parity("gc_stress", source);
    assert_eq!(out.trim(), "19660800\nabcdefgh");
}

#[test]
fn gc_preserves_object_graphs() {
    // Long-lived structs with pointer fields must survive collections that
    // reclaim the interleaved garbage.
    let source = r#"struct Node {
    name: str
    vals: [int]
}

fn churn(n: int) -> str {
    let s = ""
    for i in 0..n {
        s = s + "x"
    }
    return s
}

fn main() {
    let keep: [str] = []
    for i in 0..200 {
        let node = Node("node-" + str_of_int(i), [i, i * 2])
        let junk = churn(2000)
        if i % 50 == 0 {
            keep = push(keep, node.name + ":" + str_of_int(node.vals[1]))
        }
    }
    for i in 0..len(keep) {
        print(keep[i])
    }
}
"#;
    let out = assert_parity("gc_graphs", source);
    assert_eq!(
        out.trim(),
        "node-0:0\nnode-50:100\nnode-100:200\nnode-150:300"
    );
}

#[test]
fn pkg_workflow_installs_and_imports() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pkgproj");
    let _ = std::fs::remove_dir_all(&base);
    let lib = base.join("mathx");
    let app = base.join("app");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(lib.join("machino.pkg"), "name mathx\nversion 1.0.0\n").unwrap();
    std::fs::write(
        lib.join("mathx.mno"),
        "fn gcd(a: int, b: int) -> int {\n    if b == 0 { return a }\n    return gcd(b, a % b)\n}\n",
    )
    .unwrap();

    let run_in = |dir: &PathBuf, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_machino"))
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to run machino binary")
    };

    let init = run_in(&app, &["pkg", "init", "app"]);
    assert!(init.status.success(), "{:?}", init);
    let add = run_in(&app, &["pkg", "add", "mathx", "../mathx"]);
    assert!(add.status.success(), "{:?}", add);
    assert!(app.join("machino_modules/mathx/mathx.mno").exists());
    assert!(app.join("machino.lock").exists());
    let lock = std::fs::read_to_string(app.join("machino.lock")).unwrap();
    assert!(lock.contains("mathx ../mathx path"), "lock: {}", lock);

    std::fs::write(
        app.join("main.mno"),
        "import \"pkg:mathx/mathx.mno\"\n\nfn main() {\n    print(gcd(48, 36))\n}\n",
    )
    .unwrap();
    let run = run_in(&app, &["run", "main.mno"]);
    assert!(run.status.success(), "{:?}", run);
    assert_eq!(stdout(&run).trim(), "12");

    // the same program compiles to wasm
    let build = run_in(&app, &["build", "main.mno", "-o", "app.wasm"]);
    assert!(build.status.success(), "{:?}", build);

    // a second sync is idempotent
    let sync = run_in(&app, &["pkg", "sync"]);
    assert!(sync.status.success(), "{:?}", sync);
}

#[test]
fn pkg_transitive_deps_are_flattened() {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("pkgtrans");
    let _ = std::fs::remove_dir_all(&base);
    let leaf = base.join("leaf");
    let mid = base.join("mid");
    let app = base.join("app");
    for d in [&leaf, &mid, &app] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(leaf.join("machino.pkg"), "name leaf\nversion 1.0.0\n").unwrap();
    std::fs::write(
        leaf.join("leaf.mno"),
        "fn double(n: int) -> int {\n    return n * 2\n}\n",
    )
    .unwrap();
    std::fs::write(
        mid.join("machino.pkg"),
        "name mid\nversion 1.0.0\ndep leaf ../leaf\n",
    )
    .unwrap();
    std::fs::write(
        mid.join("mid.mno"),
        "import \"pkg:leaf/leaf.mno\"\n\nfn quad(n: int) -> int {\n    return double(double(n))\n}\n",
    )
    .unwrap();
    std::fs::write(
        app.join("machino.pkg"),
        "name app\nversion 0.1.0\ndep mid ../mid\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.mno"),
        "import \"pkg:mid/mid.mno\"\n\nfn main() {\n    print(quad(5))\n}\n",
    )
    .unwrap();

    let run_in = |dir: &PathBuf, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_machino"))
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to run machino binary")
    };
    let sync = run_in(&app, &["pkg", "sync"]);
    assert!(sync.status.success(), "{:?}", sync);
    // both mid and its dependency leaf land flat in machino_modules
    assert!(app.join("machino_modules/mid/mid.mno").exists());
    assert!(app.join("machino_modules/leaf/leaf.mno").exists());
    let run = run_in(&app, &["run", "main.mno"]);
    assert!(run.status.success(), "{:?}", run);
    assert_eq!(stdout(&run).trim(), "20");
}

// ---- v0.7 features ----

#[test]
fn stdlib_math_parity() {
    let out = assert_parity(
        "stdmath",
        r#"fn main() {
    print(str_of_float(sqrt(2.0), 6))
    print(pow_int(3, 7))
    print(str_of_float(pow_float(2.0, -3), 5))
    print(floor(-1.5))
    print(ceil(-1.5))
    print(round(0.5))
    print(str_of_float(abs_float(-2.25), 2))
    print(min_float(0.5, -0.5))
    print(max_float(0.5, -0.5))
}
"#,
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "1.414214");
    assert_eq!(lines[1], "2187");
    assert_eq!(lines[2], "0.12500");
    assert_eq!(lines[3], "-2");
    assert_eq!(lines[4], "-1");
    assert_eq!(lines[5], "1");
}

#[test]
fn stdlib_json_round_trip_parity() {
    let out = assert_parity(
        "stdjson",
        r#"fn main() {
    let src = "{\"a\": [1, 2.5, \"x\\ny\"], \"b\": {\"c\": null}, \"d\": -1e2, \"e\": true}"
    let result = match json_parse(src) {
        JsonParsed::JVal(v) => json_serialize(v)
        JsonParsed::JError(e) => "error: " + e
    }
    print(result)
    let bad = match json_parse("[1,") {
        JsonParsed::JVal(v) => "parsed"
        JsonParsed::JError(e) => "rejected"
    }
    print(bad)
}
"#,
    );
    assert_eq!(
        out.lines().next().unwrap(),
        r#"{"a":[1,2.5,"x\ny"],"b":{"c":null},"d":-100,"e":true}"#
    );
    assert_eq!(out.lines().nth(1).unwrap(), "rejected");
}

#[test]
fn stdlib_time_and_intmap_parity() {
    let out = assert_parity(
        "stdtime",
        r#"fn main() {
    print(time_iso(0))
    print(time_iso(1000000000000))
    let m = intmap_new()
    intmap_set(m, 7, 1)
    intmap_set(m, 7, 2)
    print(intmap_get(m, 7))
    print(intmap_get_or(m, 8, -1))
    print(intmap_len(m))
}
"#,
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "1970-01-01T00:00:00Z");
    assert_eq!(lines[1], "2001-09-09T01:46:40Z");
    assert_eq!(lines[2], "2");
    assert_eq!(lines[3], "-1");
    assert_eq!(lines[4], "1");
}

#[test]
fn match_payload_bindings_work_in_wasm() {
    // regression: the wasm backend used to panic computing arm body types
    // that referenced a pattern binding
    let out = assert_parity(
        "matchbind",
        r#"enum E {
    A(str)
    B(int)
    C
}

fn show(e: E) -> str {
    return match e {
        E::A(s) => s
        E::B(n) => str_of_int(n)
        E::C => "c"
    }
}

fn main() {
    print(show(E::A("hi")))
    print(show(E::B(42)))
    print(show(E::C))
}
"#,
    );
    assert_eq!(out, "hi\n42\nc\n");
}

#[test]
fn namespaced_imports_work() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("nstest");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.mno"),
        r#"struct Pair {
    a: int
    b: int
}

enum Verdict {
    Yes
    No
}

fn sum(p: Pair) -> int {
    return p.a + p.b
}

fn judge(n: int) -> Verdict {
    if n > 0 {
        return Verdict::Yes
    }
    return Verdict::No
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.mno"),
        r#"import "lib.mno" as lib

fn sum(a: int, b: int) -> int {
    return a + b
}

fn main() {
    let p: lib::Pair = lib::Pair(2, 3)
    print(lib::sum(p))
    print(sum(10, 20))
    let v = lib::judge(5)
    let s = match v {
        lib::Verdict::Yes => "yes"
        lib::Verdict::No => "no"
    }
    print(s)
}
"#,
    )
    .unwrap();
    let out = machino(&["run", dir.join("main.mno").to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "5\n30\nyes\n");
}

#[test]
fn snapshot_tests_compare_output() {
    let path = write_temp(
        "snapshot.mno",
        "fn main() {\n}\n\ntest \"good\" expects \"1\\n2\" {\n    print(1)\n    print(2)\n}\n\ntest \"bad\" expects \"3\" {\n    print(4)\n}\n",
    );
    let out = machino(&["test", path.to_str().unwrap()]);
    assert!(!out.status.success());
    let text = stdout(&out);
    assert!(text.contains("PASS  good"), "{}", text);
    assert!(text.contains("FAIL  bad"), "{}", text);
    assert!(text.contains("snapshot mismatch"), "{}", text);
}

#[test]
fn spawn_join_runs_in_parallel_threads() {
    let path = write_temp(
        "spawn.mno",
        r#"fn work(n: int) -> int {
    let acc = 0
    for i in 0..n {
        acc = acc + i
    }
    return acc
}

fn shout(s: str) -> str {
    return to_upper(s)
}

fn main() {
    let h1 = spawn(work, 1000)
    let h2 = spawn(work, 2000)
    let h3 = spawn(shout, "done")
    print(join_int(h1) + join_int(h2))
    print(join_str(h3))
}
"#,
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "2498500\nDONE\n");
}

#[test]
fn build_compiles_concurrency_and_runs_it() {
    let path = write_temp(
        "spawnbuild.mno",
        "fn f(n: int) -> int {\n    return n * 2\n}\n\nfn main() {\n    let h = spawn(f, 21)\n    print(join_int(h))\n}\n",
    );
    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spawnbuild.wasm");
    let out = machino(&["build", path.to_str().unwrap(), "-o", wasm.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // skipped silently if node is not installed
    if Command::new("node").arg("--version").output().is_ok() {
        let run = Command::new("node")
            .args(["runners/run.mjs", wasm.to_str().unwrap()])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .unwrap();
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(stdout(&run), "42\n");
    }
}

#[test]
fn gc_extern_channel_spawn() {
    let path = write_temp(
        "gc_parity.mno",
        r#"extern fn clock_ms() -> int

fn square(n: int) -> int {
    return n * n
}

fn main() {
    print(clock_ms() > 0)
    let ch = chan_new()
    chan_send_int(ch, 3)
    print(chan_recv_int(ch))
    chan_close(ch)
    let h = spawn(square, 11)
    print(join_int(h))
}
"#,
    );
    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gc_parity.wasm");
    let built = machino(&[
        "build",
        "--gc",
        "--no-cache",
        path.to_str().unwrap(),
        "-o",
        wasm.to_str().unwrap(),
    ]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let runner = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runners/run-gc.mjs");
    let out = std::process::Command::new("node")
        .args([runner.to_str().unwrap(), wasm.to_str().unwrap()])
        .output()
        .expect("node");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("true\n"), "{}", s);
    assert!(s.contains("3\n"), "{}", s);
    assert!(s.contains("121\n"), "{}", s);
}

#[test]
fn hashmap_type_annotation() {
    let path = write_temp(
        "hm_ann.mno",
        r#"fn main() {
    let ks: [str] = []
    let vs: [int] = []
    let m: HashMap<str, int> = HashMap(ks, vs, empty_buckets(8), 8)
    hashmap_set(m, "a", 1)
    print(hashmap_get(m, "a"))
}

test "ann" {
    let ks: [str] = []
    let vs: [int] = []
    let m: HashMap<str, int> = HashMap(ks, vs, empty_buckets(8), 8)
    hashmap_set(m, "a", 1)
    assert hashmap_get(m, "a") == 1
}
"#,
    );
    let out = machino(&["test", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn generic_hashmap_and_unicode_and_channels() {
    let path = write_temp(
        "v11.mno",
        r#"fn worker(ch: int) -> int {
    chan_send_int(ch, 21)
    return 0
}

fn main() {
    let ks: [str] = []
    let vs: [int] = []
    let m = HashMap(ks, vs, empty_buckets(8), 8)
    hashmap_set(m, "a", 1)
    hashmap_set(m, "b", 2)
    print(hashmap_get(m, "a"))
    print(hashmap_len(m))
    print(len_cp("A🎉"))
    print(substr_cp("A🎉B", 1, 2))
    let ch = chan_new()
    let h = spawn(worker, ch)
    print(chan_recv_int(ch) * 2)
    print(join_int(h))
    chan_close(ch)
}
"#,
    );
    let out = machino(&["run", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(stdout(&out), "1\n2\n2\n🎉\n42\n0\n");

    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("v11_hm.wasm");
    let built = machino(&[
        "build",
        "--no-cache",
        path.to_str().unwrap(),
        "-o",
        wasm.to_str().unwrap(),
    ]);
    // channels+spawn compile; hashmap+unicode subset also builds — full file uses both
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

#[test]
#[cfg(feature = "smt")]
fn verify_float_and_bounded_while() {
    let path = write_temp(
        "smt_v12.mno",
        r#"fn add1(x: float) -> float
ensures result > x
{
    return x + 1.0
}

fn sum_while() -> int
ensures result == 45
{
    let i = 0
    let s = 0
    while i < 10 {
        s = s + i
        i = i + 1
    }
    return s
}

fn main() {}
"#,
    );
    let out = machino(&["check", path.to_str().unwrap(), "--verify"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("add1") && text.contains("proved"), "{}", text);
    assert!(
        text.contains("sum_while") && text.contains("proved"),
        "{}",
        text
    );
}

#[test]
fn build_native_llvm_smoke() {
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let path = write_temp(
        "native_llvm.mno",
        "fn main() {\n    print(1 + 1)\n}\n",
    );
    let exe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("native_llvm_bin");
    let _ = std::fs::remove_file(&exe);
    let out = machino(&[
        "build",
        "--native",
        "--no-cache",
        path.to_str().unwrap(),
        "-o",
        exe.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(exe.is_file(), "expected native executable {}", exe.display());
    let run = Command::new(&exe).output().expect("run native exe");
    assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "2");
}

fn native_build_run(src: &str, bin: &str) -> (bool, String, String) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = dir.join(src);
    let exe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(bin);
    let _ = std::fs::remove_file(&exe);
    let out = machino(&[
        "build",
        "--native",
        "--no-cache",
        path.to_str().unwrap(),
        "-o",
        exe.to_str().unwrap(),
    ]);
    if !out.status.success() {
        return (
            false,
            String::new(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        );
    }
    let run = Command::new(&exe).output().expect("run");
    (
        run.status.success(),
        String::from_utf8_lossy(&run.stdout).to_string(),
        String::from_utf8_lossy(&run.stderr).to_string(),
    )
}

#[test]
fn build_native_closures_spawn_channels() {
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let (ok, stdout, err) = native_build_run("test/ex_036_closure_capture.mno", "nat_clos");
    assert!(ok, "closure: {err}");
    assert_eq!(stdout.trim(), "15");

    let (ok, stdout, err) = native_build_run("test/ex_081_spawn_join.mno", "nat_spawn");
    assert!(ok, "spawn: {err}");
    assert_eq!(stdout.trim(), "144");

    let (ok, stdout, err) = native_build_run("test/ex_082_channels.mno", "nat_chan");
    assert!(ok, "chan: {err}");
    assert_eq!(stdout.replace('\r', "").trim(), "11\n22");

    let (ok, stdout, err) = native_build_run("test/ex_034_map_hof.mno", "nat_map");
    assert!(ok, "hof map: {err}");
    assert_eq!(stdout.trim(), "6");

    // Float/str spawn must not treat IEEE bits as heap pointers.
    let (ok, stdout, err) = native_build_run("test/ex_104_gc_spawn_types.mno", "nat_gsp");
    assert!(ok, "float spawn: {err}");
    assert_eq!(stdout.trim(), "3.5");
}

#[test]
fn build_native_universal_macos() {
    if !cfg!(target_os = "macos") {
        return;
    }
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    if Command::new("lipo").arg("-info").arg("/usr/bin/true").output().is_err() {
        return;
    }
    let path = write_temp("native_univ.mno", "fn main() {\n    print(42)\n}\n");
    let exe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("native_univ_bin");
    let _ = std::fs::remove_file(&exe);
    let out = machino(&[
        "build",
        "--native",
        "--universal",
        "--no-cache",
        path.to_str().unwrap(),
        "-o",
        exe.to_str().unwrap(),
    ]);
    if !out.status.success() {
        // Cross slice may be unavailable without full Xcode SDK support.
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("E080") || err.contains("x86_64") || err.contains("lipo"),
            "unexpected universal failure: {err}"
        );
        return;
    }
    let lipo = Command::new("lipo")
        .args(["-archs", exe.to_str().unwrap()])
        .output()
        .expect("lipo -archs");
    let archs = String::from_utf8_lossy(&lipo.stdout);
    assert!(
        archs.contains("arm64") && archs.contains("x86_64"),
        "expected universal arches, got {archs}"
    );
    let run = Command::new(&exe).output().expect("run universal");
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "42");
}

#[test]
fn build_native_target_host_triple() {
    if Command::new("clang").arg("--version").output().is_err() {
        return;
    }
    let path = write_temp("native_tgt.mno", "fn main() {\n    print(9)\n}\n");
    let exe = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("native_tgt_bin");
    let _ = std::fs::remove_file(&exe);
    // Use the compiler's default target via an explicit empty skip — pass
    // a known-good apple/linux host triple from `clang -dumpmachine`.
    let dump = Command::new("clang")
        .arg("-dumpmachine")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    let Some(triple) = dump else { return };
    let out = machino(&[
        "build",
        "--native",
        "--no-cache",
        "--target",
        &triple,
        path.to_str().unwrap(),
        "-o",
        exe.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let run = Command::new(&exe).output().expect("run");
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "9");
}

#[test]
fn build_rejects_lambda_spawn_target() {
    let path = write_temp(
        "spawnlambda.mno",
        "fn main() {\n    let f = fn(n: int) -> int {\n        return n\n    }\n    let h = spawn(f, 1)\n    print(join_int(h))\n}\n",
    );
    let wasm = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("spawnlambda.wasm");
    let out = machino(&["build", path.to_str().unwrap(), "-o", wasm.to_str().unwrap()]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("E074"), "{}", err);
}

#[test]
fn run_trace_emits_json_events() {
    let path = write_temp(
        "trace.mno",
        "fn inc(n: int) -> int {\n    return n + 1\n}\n\nfn main() {\n    print(inc(41))\n}\n",
    );
    let out = machino(&["run", path.to_str().unwrap(), "--trace"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), "42\n");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains(r#"{"event":"call","fn":"inc","depth":1,"args":["41"]}"#),
        "{}",
        err
    );
    assert!(
        err.contains(r#"{"event":"return","fn":"inc","depth":1,"value":"42"}"#),
        "{}",
        err
    );
}

#[test]
fn fmt_is_canonical_and_idempotent() {
    let path = write_temp(
        "fmt.mno",
        "fn main(){let x=1+2\nif x>2{print(x)}else{print(0)}}\n",
    );
    let out = machino(&["fmt", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let formatted = std::fs::read_to_string(&path).unwrap();
    assert!(formatted.contains("    let x = 1 + 2\n"), "{}", formatted);
    // formatting a second time changes nothing
    let check = machino(&["fmt", path.to_str().unwrap(), "--check"]);
    assert!(check.status.success());
    // and the formatted program still runs
    let run = machino(&["run", path.to_str().unwrap()]);
    assert!(run.status.success());
    assert_eq!(stdout(&run), "3\n");
}

#[test]
fn query_reports_generic_templates() {
    let path = write_temp(
        "query.mno",
        "fn<T: Ord> max2(a: T, b: T) -> T {\n    if a > b {\n        return a\n    }\n    return b\n}\n\nfn main() {\n    print(max2(1, 2))\n}\n",
    );
    let out = machino(&["query", path.to_str().unwrap()]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(text.contains("\"name\":\"max2\""), "{}", text);
    assert!(text.contains("\"bounds\":[\"Ord\"]"), "{}", text);
    // monomorphized copies must not leak into query output
    assert!(!text.contains("max2$"), "{}", text);
}

#[test]
fn fuzz_finds_contract_violations() {
    let path = write_temp(
        "fuzzable.mno",
        "fn bad(n: int) -> int\n    ensures result > n\n{\n    return n\n}\n\nfn main() {\n}\n",
    );
    let out = machino(&["fuzz", path.to_str().unwrap(), "--runs", "50"]);
    let text = stdout(&out);
    assert!(
        text.contains("bad") && (text.contains("FAIL") || text.contains("violation")),
        "fuzz did not surface the ensures violation: {}",
        text
    );
}

#[test]
fn synth_generates_verified_programs() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("corpus");
    let out = machino(&[
        "synth",
        "--count",
        "5",
        "--seed",
        "123",
        "--out",
        dir.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    for i in 0..5 {
        let sample = dir.join(format!("sample_{:05}.mno", i));
        let check = machino(&["check", sample.to_str().unwrap()]);
        assert!(check.status.success(), "generated sample {} failed check", i);
        let run = machino(&["run", sample.to_str().unwrap()]);
        assert!(run.status.success(), "generated sample {} failed to run", i);
    }
}
