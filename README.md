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
| `src/interp.rs` | reference interpreter + native runtime: files, stdin, env, TCP sockets (`run`, `test`) |
| `src/wasm.rs` | WebAssembly **binary emitter**: hand-rolled encoding, no LLVM, no external toolchain |
| `src/std.mno` | the standard prelude, written in machino itself |
| `src/synth.rs` | verified-corpus generator |
| `runners/` | reference hosts: Node CLI and a drag-and-drop browser page |
| `tests/cli.rs` | end-to-end suite: wasm-vs-interpreter equivalence on every feature, plus a live HTTP-server test |

Compiled modules use a simple, documented ABI: every value is one wasm value (`i64`/`f64`), strings and arrays live in linear memory as `[len][payload]`, and the module imports exactly five host functions (`print_*`, `fail`) plus whatever `extern fn`s the program declares. That `extern fn` seam is the FFI *and* the capability system — a machino program cannot name any host power the host didn't explicitly provide.

## You can write real software in it — including servers

`examples/http_server.mno` is a complete HTTP server written in machino: request parsing with the std string library, path routing, 404s, and graceful shutdown. Sockets are capabilities — the program declares the `extern fn`s it needs and the runtime provides them:

```sh
machino run examples/http_server.mno 8080
curl http://localhost:8080/hello/world   # -> hello, world!
```

The `machino run` interpreter doubles as a **native runtime** with files, stdin, environment, CLI args, TCP sockets, and clock — so agents can ship CLI tools and network services directly, the way they'd use `node script.js` or `python app.py`.

## Language at a glance

- **Types:** `int` (i64, checked arithmetic), `float` (f64), `bool`, `str`, `[T]`, structs, `fn(T...) -> R` function values — no implicit conversions, ever
- **Data modeling:** nominal `struct`s with positional constructors and field assignment, string-keyed maps (`StrMap`) in the prelude
- **Control flow:** `if` / `else`, `while`, `for i in a..b`, `break`, `continue`, `return`
- **Contracts:** `requires` (checked at entry), `ensures` (checks `result` at exit) — enforced in the interpreter *and* in compiled WASM, with identical messages
- **Std prelude, written in machino itself:** `split`/`join`/`trim`/`find`/`substr`, `parse_int`/`str_of_int`/`str_of_float`, `StrMap`, sorting — dead-code-eliminated from compiled binaries
- **Modules:** `import "lib/util.mno"` with transitive resolution and per-file diagnostics
- **Safety:** bounds-checked indexing, trapping overflow and division by zero, block scoping, no nulls, no globals, no undefined behavior
- **Interop:** `extern fn` → WASM imports, provided by the host (files, sockets, env, clock in the native runtime)

Full details in [SPEC.md](SPEC.md).

## Status and roadmap

This is **v0.2** — a real, working end-to-end system, intentionally small enough to hold in a model's context window. Honest limits, documented in the spec: named functions are first-class but there are no capturing closures, no garbage collector (bump allocator — fine for scripts and request handlers), and no generics yet.

Planned for v0.3+: enums and pattern matching, generics, a WASM-GC backend, static contract verification (SMT) for a decidable subset, and a package registry.

## License

[MIT](LICENSE)
