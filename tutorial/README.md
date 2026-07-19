# machino tutorial — from hello to a full program

A progressive path through the language. Every lesson has a runnable file under
[`code/`](code/). Build the compiler once, then work top to bottom.

```sh
cargo build --release
alias m=./target/release/machino

m test tutorial/code/01_hello.mno
m run  tutorial/code/01_hello.mno
```

Run every lesson’s tests:

```sh
for f in tutorial/code/*.mno; do
  echo "== $f =="
  ./target/release/machino test "$f" || exit 1
done
```

| # | Topic | File |
|---|---|---|
| 01 | Hello + entry point | [`code/01_hello.mno`](code/01_hello.mno) |
| 02 | Values, types, operators | [`code/02_values.mno`](code/02_values.mno) |
| 03 | Control flow | [`code/03_control.mno`](code/03_control.mno) |
| 04 | Functions & recursion | [`code/04_functions.mno`](code/04_functions.mno) |
| 05 | Arrays & strings | [`code/05_arrays_strings.mno`](code/05_arrays_strings.mno) |
| 06 | Structs | [`code/06_structs.mno`](code/06_structs.mno) |
| 07 | Enums & `match` | [`code/07_enums.mno`](code/07_enums.mno) |
| 08 | Contracts & tests | [`code/08_contracts_tests.mno`](code/08_contracts_tests.mno) |
| 09 | Generics | [`code/09_generics.mno`](code/09_generics.mno) |
| 10 | Closures | [`code/10_closures.mno`](code/10_closures.mno) |
| 11 | `HashMap<str, int>` | [`code/11_hashmaps.mno`](code/11_hashmaps.mno) |
| 12 | JSON & stdlib | [`code/12_json_stdlib.mno`](code/12_json_stdlib.mno) |
| 13 | Concurrency | [`code/13_concurrency.mno`](code/13_concurrency.mno) |
| 14 | Native runtime / externs | [`code/14_native_externs.mno`](code/14_native_externs.mno) |
| 15 | SMT verification | [`code/15_verify_smt.mno`](code/15_verify_smt.mno) |
| — | Deploy (WASM / GC / AOT) | [below](#16-deploy--wasm-gc-and-native-aot) |
| — | Packages & modules | [below](#17-modules-and-packages) |
| — | Agent workflow | [below](#18-the-agent-loop-full-toolchain) |

Companion docs: [SPEC.md](../SPEC.md) · [agent-guide](../docs/agent-guide.md) ·
[native runtime](../docs/native-runtime.md)

---

## 01 — Hello

Every program needs `fn main()`. Statements end at newlines (no semicolons).
`print` accepts `int`, `float`, `bool`, or `str`.

```sh
machino run tutorial/code/01_hello.mno
machino test tutorial/code/01_hello.mno
```

`test "name" expects "…"` is a **snapshot test**: printed output must match
exactly (including newlines).

---

## 02 — Values and types

Four scalars: `int` (i64, checked arithmetic), `float` (f64), `bool`, `str`
(byte string). **No implicit conversions** — use `to_float` / `to_int`.

Float literals need digits on both sides: `2.0`, not `2.`.
`+` on strings concatenates. `%` is int-only.

---

## 03 — Control flow

- `if` / `else` (braces required)
- `while cond { … }`
- `for i in a..b { … }` — end exclusive
- `break` / `continue`

Every path in a value-returning function must `return`.

---

## 04 — Functions and recursion

```
fn name(args) -> RetType
    requires …
    ensures …
{ … }
```

`requires` runs at call entry; `ensures` at exit (`result` is the return value).
Violations trap with the failing clause in both the interpreter and compiled WASM.

---

## 05 — Arrays and strings

```
let xs: [int] = []          # empty arrays need a type
xs = push(xs, 1)            # push returns a NEW array — rebind
xs[0] = 10                  # bounds-checked
```

Byte APIs: `len`, `char_at`, `substr`, `chr`.
Unicode codepoints: `len_cp`, `char_at_cp`, `substr_cp`, `chr_cp`.

---

## 06 — Structs

Nominal records with positional constructors. Fields are mutable; values are
references (assigning a struct copies the reference).

```
struct Point { x: float  y: float }
let p = Point(1.0, 2.0)
p.x = 3.0
```

There are no methods — write free functions: `dist(p)`, not `p.dist()`.

---

## 07 — Enums and match

```
enum Option { None  Some(int) }
match opt {
    Option::Some(v) => v
    Option::None => 0
}
```

`match` must be **exhaustive**. Construct with `Enum::Variant` or
`Enum::Variant(payload)`.

---

## 08 — Contracts and tests

```
test "name" {
    assert expr
}

test "snap" expects "1\n2\n" {
    print(1)
    print(2)
}
```

Contracts are language syntax, not a library. Same checks in `machino run`,
`machino test`, and compiled `.wasm`.

---

## 09 — Generics

```
fn<T: Ord> max2(a: T, b: T) -> T { … }
struct<T> Box { val: T }
```

Bounds: `Eq`, `Ord`, `Num`, `Hash` (combine with `+`). Call-site types are
inferred; monomorphization happens before codegen. Construct generics directly:
`Box(42)`.

---

## 10 — Closures

```
fn(x: int) -> int { return x + n }   # captures n by value
```

Captured variables are **read-only**. To share mutable state, capture a struct
or array and mutate its contents. Named functions also work as `fn`-typed values.
Calls apply to names only — bind intermediates: `let g = f(1)` then `g(2)`.

---

## 11 — HashMap

```
let ks: [str] = []
let vs: [int] = []
let m: HashMap<str, int> = HashMap(ks, vs, empty_buckets(8), 8)
hashmap_set(m, "a", 1)
hashmap_get(m, "a")
```

Type annotations may use generic apps: `HashMap<str, int>` (nested apps work
too). Also available: concrete `StrMap` / `IntMap`.

---

## 12 — JSON and the stdlib

Always in scope (names reserved): string helpers (`split`/`join`/`trim`/…),
math (`sqrt`, `pow_*`, `floor`/`ceil`/`round`), time (`time_iso`, …), and JSON:

```
json_parse(s) -> JsonParsed   # JVal(Json) | JError(str)
json_serialize(v) -> str
```

`Json` is a recursive enum: `JNull | JBool | JNum | JStr | JArr | JObj`.

---

## 13 — Concurrency

```
let h = spawn(square, 12)     # named top-level fn only when compiled
print(join_int(h))

let ch = chan_new()
chan_send_int(ch, 7)
print(chan_recv_int(ch))
chan_close(ch)
```

- Interpreter + linear WASM + `--gc` all support channels and spawn
- Under `--gc`, spawn args are currently `int`/`bool` only
- Lambdas as spawn targets work in `machino run`; compiled builds need a
  named function (`E074`)

---

## 14 — Native runtime (`machino run`)

`machino run` is the **official native OS runtime** — files, TCP, env, args,
clock, `http_get`. Declare capabilities with `extern fn`; undeclared host
powers are unreachable.

```sh
machino run tutorial/code/14_native_externs.mno
machino run examples/http_server.mno 8080
```

Full capability table: [docs/native-runtime.md](../docs/native-runtime.md).

---

## 15 — SMT verification

Build with Z3, then prove contracts without running them:

```sh
cargo build --release --features smt
./target/release/machino check tutorial/code/15_verify_smt.mno --verify
```

Decidable subset (v1.2): `int`/`bool`/`float` (floats as mathematical reals),
string-lite (`len` / `==`), constant-bound `for`, and `while i < N` /
`while i <= N` when the bound is a constant. Everything else stays
runtime-checked (`unknown` in the verifier).

---

## 16 — Deploy: WASM, GC, and native AOT

```sh
# Linear memory + embedded mark-sweep GC (default)
machino build tutorial/code/04_functions.mno -o out.wasm
node runners/run.mjs out.wasm

# Host-managed WASM-GC (Node 22+)
machino build tutorial/code/13_concurrency.mno --gc -o out-gc.wasm
node runners/run-gc.mjs out-gc.wasm

# AOT with wasmtime (must be on PATH)
machino build tutorial/code/04_functions.mno --native -o out.wasm
# writes out.cwasm — host imports still needed for print/externs
```

Prefer `machino run` when you need TCP/files. Prefer `.wasm` when you need a
portable sandbox.

---

## 17 — Modules and packages

```
import "lib/util.mno"
import "geometry.mno" as geo     # then geo::Point, geo::dist(...)
import "pkg:mathx/mathx.mno"     # after machino pkg add / sync
```

```sh
machino pkg init myapp
machino pkg add mathx /path/to/machino/packages/mathx
machino pkg sync
```

Official libraries (vec, mathx, httpkit, encoding, …) live in
[`packages/`](../packages/) — see [`packages/README.md`](../packages/README.md)
and try `packages/demo`. Also see `examples/namespaces/` for `as` imports.

---

## 18 — The agent loop (full toolchain)

```sh
machino check program.mno --json     # fix until "ok": true
machino test  program.mno --json     # fix until failed == 0
machino run   program.mno            # native execute
machino build program.mno -o out.wasm
machino fmt   program.mno            # canonical form
machino query program.mno            # JSON signatures
machino fuzz  program.mno --runs 100 # contract-driven random tests
machino synth --count 100 --out corpus/
```

Paste [`docs/agent-guide.md`](../docs/agent-guide.md) into an agent’s context
for zero-shot writing. Stable error codes (`E0xx`) and `--json` diagnostics
are designed for automated repair.

---

## Rules that trip people (and models) up

| Rule | Fix |
|---|---|
| No `int` + `float` | `to_float` / `to_int` |
| Empty `[]` | `let xs: [int] = []` |
| `push` doesn’t mutate | `xs = push(xs, v)` |
| Float literal | `2.0`, not `2.` |
| No methods | free functions |
| Match must be exhaustive | cover every variant |
| Captured vars read-only | mutate through struct/array |
| Every path returns | final `return` |
| Compiled spawn | named top-level function |

---

## Where to go next

| Goal | Start here |
|---|---|
| Bigger demos | [`examples/`](../examples/) — HTTP server, maze, neural net, wordcount |
| 100 small drills | [`test/ex_*.mno`](../test/) |
| Formal language | [`SPEC.md`](../SPEC.md) |
| Agent system prompt | [`docs/agent-guide.md`](../docs/agent-guide.md) |
| Grammar | [`docs/grammar.ebnf`](../docs/grammar.ebnf) |

You’ve now covered the full surface people use day to day: types, data,
contracts, generics, concurrency, native I/O, verification, and deploy.
Write something real — `examples/http_server.mno` is a good template — and
keep the check → test → run → build loop tight.
