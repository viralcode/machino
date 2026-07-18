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
        "http_server",
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
        "http_server",
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
    for ex in ["fib", "sort", "contracts", "structs", "wordcount", "higher_order"] {
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
