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
    for ex in ["hello", "fib", "sort", "contracts"] {
        let out = machino(&["check", &format!("examples/{}.mno", ex)]);
        assert!(out.status.success(), "check failed for {}: {:?}", ex, out);
    }
}

#[test]
fn tests_pass_on_examples() {
    for ex in ["fib", "sort", "contracts"] {
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
    for ex in ["fib", "sort", "contracts"] {
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
