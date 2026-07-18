# machino

**A programming language designed for AI agents, not humans — compiled to WebAssembly so it runs on any machine.**

[![CI](https://github.com/viralcode/machino/actions/workflows/ci.yml/badge.svg)](https://github.com/viralcode/machino/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Made with Rust](https://img.shields.io/badge/rust-zero%20dependencies-orange.svg)](Cargo.toml)

Today, AI agents write software in languages built for human ergonomics — Python, JavaScript, C++ — and inherit decades of ambiguity, undefined behavior, and style debates along the way. machino flips the design goal. It is a language whose primary *user is a machine*: one canonical syntax, no undefined behavior, contracts and tests baked into the grammar, and compiler errors emitted as structured JSON that an agent can repair against in a tight loop.

It compiles to **WebAssembly** — the closest thing that exists to a portable, sandboxed, formally specified machine language — so one compiled binary runs identically in browsers, servers, phones, and edge workers.

```
fn fib(n: int) -> int
    requires n >= 0
    ensures result >= 0
{
    if n < 2 {
        return n
    }
    return fib(n - 1) + fib(n - 2)
}

fn main() {
    print(fib(15))
}

test "fib values" {
    assert fib(10) == 55
}
```

The `requires`/`ensures` contracts and the `test` block aren't a framework — they're language syntax, enforced by the toolchain in both the interpreter *and* the compiled WebAssembly binary. A violated contract in production traps with the exact failing clause:

```
contract violation: requires 'b != 0' failed when calling 'safe_div'
```

## Why an AI-first language is different

| Human-first languages | machino |
|---|---|
| Many ways to write the same thing | One canonical syntax — generated code is diffable and predictable |
| Undefined behavior, silent overflow | Every fault is a defined, messaged trap |
| Tests live in external frameworks | `test` / `assert` / `requires` / `ensures` are grammar |
| Error messages are prose for humans | Stable-coded JSON diagnostics with fix hints (`--json`) |
| Ambient authority — any code can hit the network | Capability-based: only declared `extern fn`s reach the host |
| Needs a runtime or per-platform build | One `.wasm` binary, every platform |

The core bet: **agents get good at languages with fast, precise, machine-readable feedback.** machino's entire toolchain is that feedback loop.

## Quick start

```sh
git clone https://github.com/viralcode/machino
cd machino
cargo build --release              # zero dependencies, builds in seconds

./target/release/machino run examples/fib.mno
./target/release/machino test examples/sort.mno
```

Compile to a portable binary and run it anywhere:

```sh
./target/release/machino build examples/fib.mno -o fib.wasm
node runners/run.mjs fib.wasm      # Node / Deno / Bun
open runners/run.html              # or drag the .wasm into a browser
```

The same `.wasm` runs in wasmtime, wasmer, Cloudflare Workers — any WebAssembly host that provides the five-function interface in [SPEC.md](SPEC.md).

## The agent loop

This is the workflow machino is built around. Point your agent at [`docs/agent-guide.md`](docs/agent-guide.md) (a paste-into-context doc that teaches any LLM the language zero-shot) and have it run:

```sh
machino check program.mno --json   # 1. fix until "ok": true
machino test  program.mno --json   # 2. fix until failed == 0
machino run   program.mno          # 3. execute
machino build program.mno          # 4. ship a .wasm
```

Every diagnostic has a stable error code, a location, and usually an actionable fix:

```json
{"severity":"error","code":"E043",
 "message":"'+' cannot be applied to 'int' and 'float'",
 "file":"program.mno","line":2,"col":12,"endLine":2,"endCol":17,
 "help":"machino has no implicit numeric conversion; use to_float(x) or to_int(x)"}
```

## The training-data problem

A new language starts with zero corpus, which is the real moat and the real risk for LLM adoption. machino attacks it two ways:

- **`machino synth`** generates random programs and only emits ones that pass the real type checker — every sample in the corpus is verified, compilable machino. Pair each with its interpreter output for (program, behavior) training pairs.

  ```sh
  machino synth --count 1000 --seed 42 --out corpus/
  ```

- **`docs/agent-guide.md`** is compact enough to sit in a system prompt, so frontier models can write machino zero-shot today, without fine-tuning.

## How it works

The whole pipeline is ~4,500 lines of dependency-free Rust — small enough to read in an afternoon:

| Path | What it is |
|---|---|
| `src/lexer.rs` → `src/parser.rs` → `src/checker.rs` | frontend: newline-terminated grammar, full static typing, stable `E0xx` diagnostics |
| `src/interp.rs` | reference interpreter — the language's ground-truth semantics (`run`, `test`) |
| `src/wasm.rs` | WebAssembly **binary emitter**: hand-rolled encoding, no LLVM, no external toolchain |
| `src/synth.rs` | verified-corpus generator |
| `runners/` | reference hosts: Node CLI and a drag-and-drop browser page |
| `tests/cli.rs` | end-to-end suite, including a wasm-vs-interpreter equivalence check on every example |

Compiled modules use a simple, documented ABI: every value is one wasm value (`i64`/`f64`), strings and arrays live in linear memory as `[len][payload]`, and the module imports exactly five host functions (`print_*`, `fail`) plus whatever `extern fn`s the program declares. That `extern fn` seam is the FFI *and* the capability system — a machino program cannot name any host power the host didn't explicitly provide.

## Language at a glance

- **Types:** `int` (i64), `float` (f64), `bool`, `str`, `[T]` — no implicit conversions, ever
- **Control flow:** `if` / `else`, `while`, `return`
- **Contracts:** `requires` (checked at entry), `ensures` (checks `result` at exit)
- **Builtins:** `print`, `len`, `push`, `to_float`, `to_int`
- **Safety:** bounds-checked indexing, trapping division by zero, block scoping, no nulls, no globals
- **Interop:** `extern fn` → WASM imports, provided by the host

Full details in [SPEC.md](SPEC.md).

## Status and roadmap

This is **v0.1** — a real, working end-to-end system, intentionally small enough to hold in a model's context window. Known limits, documented in the spec: no structs or `for` loops yet, no GC (bump allocator; fine for short-lived programs), and int overflow traps in the interpreter but wraps in WASM.

Planned for v0.2+: structs, pattern matching, unified overflow semantics, WASM-GC, a module system, and static contract verification (SMT) for a decidable subset.

## License

[MIT](LICENSE)
