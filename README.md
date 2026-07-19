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

**New to machino?** Work through the progressive tutorial — hello → contracts →
generics → concurrency → native I/O → SMT → deploy:

→ **[tutorial/](tutorial/)** ([start here](tutorial/README.md))

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

The whole pipeline is ~15,000 lines of dependency-free Rust — small enough to read in a weekend:

| Path | What it is |
|---|---|
| `src/lexer.rs` → `src/parser.rs` → `src/checker.rs` | frontend: newline-terminated grammar, full static typing, stable `E0xx` diagnostics |
| `src/interp.rs` | reference interpreter + native runtime: files, stdin, env, TCP sockets (`run`, `test`) |
| `src/wasm.rs` | WebAssembly **binary emitter**: hand-rolled encoding, closures, threads, and a mark-sweep GC — no LLVM, no external toolchain |
| `src/wasmgc.rs` | second backend targeting the **WASM-GC proposal** (`build --gc`): the host's collector manages memory |
| `src/mono.rs` | monomorphization: generic templates become concrete functions before codegen |
| `src/smt.rs` | static contract verification with Z3 (`check --verify`) |
| `src/pkg.rs` | the package system: manifest, installer, lockfile |
| `src/std.mno` | the standard prelude, written in machino itself |
| `src/synth.rs` | verified-corpus generator |
| `runners/` | reference hosts: Node CLI and a drag-and-drop browser page |
| `tests/cli.rs` | end-to-end suite: wasm-vs-interpreter equivalence on every feature, plus a live HTTP-server test |

Compiled modules use a simple, documented ABI: every value is one wasm value (`i64`/`f64`), heap objects live in linear memory behind a 16-byte GC header, and the module imports exactly five host functions (`print_*`, `fail`) plus whatever `extern fn`s the program declares. That `extern fn` seam is the FFI *and* the capability system — a machino program cannot name any host power the host didn't explicitly provide.

Every compiled binary also carries its own **precise mark-sweep garbage collector** — about 300 instructions of hand-emitted WASM. Pointer-typed locals are mirrored into shadow-stack frames that the collector scans as roots; collection triggers at safepoints under an adaptive threshold; freed blocks coalesce into a free list. No engine support, no external runtime: allocation-heavy loops run in bounded memory in any stock WebAssembly host.

## You can write real software in it — including servers

`examples/http_server.mno` is a complete HTTP server written in machino: request parsing with the std string library, path routing, 404s, and graceful shutdown. Sockets are capabilities — the program declares the `extern fn`s it needs and the runtime provides them:

```sh
machino run examples/http_server.mno 8080
curl http://localhost:8080/hello/world   # -> hello, world!
```

The `machino run` interpreter doubles as a **native runtime** with files, stdin, environment, CLI args, TCP sockets, and clock — so agents can ship CLI tools and network services directly, the way they'd use `node script.js` or `python app.py`.

## Language at a glance

- **Types:** `int` (i64, checked arithmetic), `float` (f64), `bool`, `str` (UTF-8 bytes), `[T]`, structs, enums, `fn(T...) -> R` function values — no implicit conversions, ever
- **Generics:** `fn<T: Ord> max2(a: T, b: T) -> T` with call-site type inference, constraint bounds (`Eq`, `Ord`, `Num`, `Hash`), `+` for multiple bounds, and `where` clauses — monomorphized at compile time so both backends stay simple. Generic `struct`/`enum` declarations can be constructed directly (`Box(42)`, `HashMap(...)`)
- **Data modeling:** nominal `struct`s with positional constructors and field assignment, sum types (`enum`) with pattern matching — including recursive enums (`JArr([Json])`); generic `HashMap` (open-addressing, `K: Eq + Hash`) alongside concrete `StrMap`/`IntMap`
- **Strings:** byte APIs (`len`/`char_at`/`substr`/`chr`) plus Unicode codepoint APIs (`len_cp`/`char_at_cp`/`substr_cp`/`chr_cp`)
- **Pattern matching:** exhaustive `match` expressions over enums and literals with compile-time coverage checks
- **Closures:** lambdas capture enclosing variables by value — `fn(x: int) -> int { return x + n }` — compiled to lifted functions + closure objects, GC-managed
- **Memory:** precise mark-sweep garbage collector compiled *into* every binary; allocation-heavy programs run in bounded memory on any WASM host
- **Control flow:** `if` / `else`, `while`, `for i in a..b`, `break`, `continue`, `return`
- **Contracts:** `requires` (checked at entry), `ensures` (checks `result` at exit) — enforced in the interpreter *and* in compiled WASM, with identical messages; `machino check --verify` proves a decidable subset statically with Z3
- **Concurrency:** `spawn`/`join_*` for shared-nothing tasks; `chan_new`/`chan_send_*`/`chan_recv_*`/`chan_close` for typed message passing between tasks (interpreter + linear WASM)
- **Std prelude, written in machino itself:** strings (`split`/`join`/`trim`/`find`/…), numbers (`sqrt`, `pow_int`, `pow_float`, `floor`/`ceil`/`round`, `parse_int`, `str_of_float`), **JSON parse + serialize** (`json_parse`, `json_serialize`), ISO-8601 time (`time_iso`, `time_from_ms`), `StrMap` and `IntMap`, sorting — dead-code-eliminated from compiled binaries
- **Modules and packages:** `import "lib/util.mno"`, namespaced imports `import "geometry.mno" as geo` (then `geo::dist(...)`, `geo::Point`, `geo::Quadrant::First`), plus a package system — `machino pkg add mathx ../mathx` (or a git URL), then `import "pkg:mathx/mathx.mno"`, with transitive flattening, a lockfile, and `pkg publish`
- **Testing:** `test` blocks with `assert`, snapshot tests (`test "x" expects "1\n2" { ... }` compares print output), plus `machino fuzz` for contract-driven property testing
- **Safety:** bounds-checked indexing, trapping overflow and division by zero, block scoping, no nulls, no globals, no undefined behavior
- **Interop:** `extern fn` → WASM imports, provided by the host (files, sockets, env, clock in the native runtime)

Full details in [SPEC.md](SPEC.md), the formal grammar in [docs/grammar.ebnf](docs/grammar.ebnf), and the diagnostic JSON schema in [docs/diagnostics.schema.json](docs/diagnostics.schema.json).

## Toolchain

| Command | What it does |
|---|---|
| `machino check [--json] [--verify]` | typecheck; `--verify` statically proves contracts with Z3 (build with `--features smt`) |
| `machino test [--json]` | run `test` blocks, including `expects` snapshots |
| `machino run [--trace]` | interpret; `--trace` streams `{"event":"call",...}` JSON per user function call to stderr |
| `machino build [-o out.wasm] [--gc] [--native] [--stack-mib N] [--no-cache]` | compile to WebAssembly (content-hash cached); `--gc` → WASM-GC; `--native` → `wasmtime compile` AOT of the linear module |
| `machino fmt [--check\|--stdout]` | canonical formatter; refuses any edit that would change the token stream |
| `machino query file.mno [name]` | machine-readable signatures, generics, contracts of every top-level item |
| `machino fuzz [--runs N]` | random-input property testing driven by `requires`/`ensures` |
| `machino synth` | generate a verified training corpus |
| `machino pkg init/add/sync/publish` | package manager + registry client |

## Tutorial

Step-by-step from basics to the full language (runnable lessons under
[`tutorial/code/`](tutorial/code/)):

**[tutorial/README.md](tutorial/README.md)** — hello, types, control flow,
functions, arrays/strings, structs, enums, contracts, generics, closures,
HashMap, JSON/stdlib, concurrency, native externs, SMT verify, WASM/GC/AOT
deploy, packages, and the agent toolchain loop.

## Packages

Ready-to-import libraries live in **[`packages/`](packages/)** (~42 packages):
collections, HTTP/CLI, encoding/csv, **regex**, **DOM**, plus an advanced math
tier (`mathadv`, `linalg`, `complex`, `numeric`, `signal`, `bigint`, …).
Install with `machino pkg add` and `import "pkg:regex/regex.mno"`.
See [`packages/README.md`](packages/README.md), try [`packages/demo`](packages/demo),
and the package demos `examples/pkg_*.mno` (math, text/regex, HTTP, DOM, science).

DOM: declare via `packages/dom` → real browser bindings in
[`runners/run.html`](runners/run.html) / [`runners/dom_host.mjs`](runners/dom_host.mjs);
`machino run` uses a virtual DOM so the same programs test natively.

## Examples

Curated demos live in [`examples/`](examples/). A 100-file language corpus (each with `main` + `test` blocks) lives in [`test/ex_*.mno`](test/). Larger HTTP services also sit under [`test/`](test/).

```sh
./target/release/machino run examples/neural_net.mno
./target/release/machino test test/ex_073_bfs_graph.mno
./target/release/machino run test/weather_api.mno 8092
```

### `examples/` — curated demos

| File | What it shows |
|---|---|
| [`examples/hello.mno`](examples/hello.mno) | Smallest program |
| [`examples/fib.mno`](examples/fib.mno) | Recursive Fibonacci + contracts + tests |
| [`examples/sort.mno`](examples/sort.mno) | Arrays, index assignment, sortedness helper |
| [`examples/structs.mno`](examples/structs.mno) | Nominal structs with reference semantics |
| [`examples/enums.mno`](examples/enums.mno) | Enums and pattern matching |
| [`examples/contracts.mno`](examples/contracts.mno) | Strings, floats, conversions, contract-guarded math |
| [`examples/generics.mno`](examples/generics.mno) | Generic functions, inferred args, constraint bounds |
| [`examples/generic_sort.mno`](examples/generic_sort.mno) | Generic sort |
| [`examples/complete_generics.mno`](examples/complete_generics.mno) | Broader generics coverage |
| [`examples/higher_order.mno`](examples/higher_order.mno) | First-class `fn(...)` values |
| [`examples/closures.mno`](examples/closures.mno) | Lambdas capturing by value |
| [`examples/concurrency.mno`](examples/concurrency.mno) | `spawn` / `join_*` |
| [`examples/json.mno`](examples/json.mno) | `json_parse` / `json_serialize` |
| [`examples/stdlib_tour.mno`](examples/stdlib_tour.mno) | Prelude tour: math, IntMap, time |
| [`examples/wordcount.mno`](examples/wordcount.mno) | Text processing with StrMap |
| [`examples/maze_solver.mno`](examples/maze_solver.mno) | BFS maze solver |
| [`examples/neural_net.mno`](examples/neural_net.mno) | 2-2-1 MLP that learns XOR |
| [`examples/http_server.mno`](examples/http_server.mno) | Real HTTP server via TCP capabilities |
| [`examples/wasmgc.mno`](examples/wasmgc.mno) / [`complete_wasmgc.mno`](examples/complete_wasmgc.mno) | WASM-GC backend |
| [`examples/smt_contracts.mno`](examples/smt_contracts.mno) / [`complete_smt.mno`](examples/complete_smt.mno) | SMT-verifiable contracts |
| [`examples/registry.mno`](examples/registry.mno) | Package registry client sketch |
| [`examples/namespaces/`](examples/namespaces/) | Namespaced imports (`geo::Point`, …) |
| [`examples/pkg_math_lab.mno`](examples/pkg_math_lab.mno) | Packages: mathadv / complex / linalg / numeric |
| [`examples/pkg_text_pipeline.mno`](examples/pkg_text_pipeline.mno) | Packages: regex / encoding / template / csv |
| [`examples/pkg_http_router.mno`](examples/pkg_http_router.mno) | Packages: httpkit / urlparse / cli |
| [`examples/pkg_dom_ui.mno`](examples/pkg_dom_ui.mno) | Packages: DOM (virtual + browser WASM) |
| [`examples/pkg_science.mno`](examples/pkg_science.mno) | Packages: statsadv / signal / bigint / geom2d |

### `test/` — 100 language examples + services

Every `test/ex_NNN_*.mno` typechecks and passes `machino test`. Run them all with:

```sh
for f in test/ex_*.mno; do ./target/release/machino test "$f" || exit 1; done
```

| Range | Topic |
|---|---|
| `ex_001`–`ex_010` | Basics: print, int/float/bool/str, arrays, `if`, `while`, `for`, `break`/`continue` |
| `ex_011`–`ex_020` | Functions, recursion, `requires`/`ensures`, early return |
| `ex_021`–`ex_030` | Structs, enums, `match` |
| `ex_031`–`ex_040` | Generics, higher-order functions, closures, `Box` |
| `ex_041`–`ex_050` | Strings, Unicode codepoints, StrMap/IntMap/HashMap |
| `ex_051`–`ex_060` | JSON, time, float math, `hash` |
| `ex_061`–`ex_075` | Algorithms: search, GCD, primes, DP, BFS, knapsack, Levenshtein |
| `ex_076`–`ex_085` | Snapshot tests, nested arrays, spawn/join, channels, generic bounds |
| `ex_086`–`ex_100` | Mini apps: Caesar, RPN, perceptron, todos, ledger, tic-tac-toe, Roman, neural forward |

| File | What it is |
|---|---|
| [`test/ex_001_hello.mno`](test/ex_001_hello.mno) … [`test/ex_100_neural_step.mno`](test/ex_100_neural_step.mno) | 100 self-contained examples (see ranges above) |
| [`test/weather_api.mno`](test/weather_api.mno) | Live weather HTTP API (Open-Meteo via `http_get`) |
| [`test/task_backend_service.mno`](test/task_backend_service.mno) | Stateful task backend with persistence and auth |

## Status and roadmap

This is **v1.2**. Deploy paths are aligned: `machino run` is the official native OS runtime ([docs/native-runtime.md](docs/native-runtime.md)); linear and `--gc` WASM both support externs, channels, and spawn/join; annotations accept `HashMap<str, int>`; SMT covers floats and bounded `while`.

What 1.2 closed out (on top of 1.1):

- **Generic type annotations** — `let m: HashMap<str, int> = …`
- **WASM-GC parity** — extern imports, channels, spawn/join (int/bool spawn args); `runners/run-gc.mjs` host
- **Cross-worker channels** — SharedArrayBuffer queues in `runners/run.mjs`
- **Native story** — documented `machino run` capabilities; `machino build --native` AOT via `wasmtime compile`
- **SMT expansion** — float (reals), string-lite (`len`/`==`), bounded `while i < N` unrolling

**Remaining bounds:** GC spawn of float/str/struct args; call-site turbofish; unbounded `while` proofs (no invariant syntax yet); hosted package registry stays out of the toolchain.

## License

[MIT](LICENSE)
