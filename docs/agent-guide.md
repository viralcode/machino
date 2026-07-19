# machino for AI agents

> Paste this file into your agent's context (system prompt, rules file, or
> retrieval store). It contains everything a model needs to write correct
> machino zero-shot, plus the tool loop to verify it.

## What machino is

machino is a small, statically typed language that compiles to WebAssembly.
Files end in `.mno`. There is exactly one way to write each construct. The
compiler gives structured JSON errors designed for you to repair against.

## The loop you should run

```
1. write program.mno
2. machino check program.mno --json     # fix until "ok": true
3. machino test  program.mno --json     # fix until failed == 0
4. machino run   program.mno [args...]  # execute fn main() (native runtime)
5. machino build program.mno -o out.wasm  # ship: runs in any WASM host
```

Every diagnostic has `code`, `line`, `col`, `message`, and often `help` with
the exact fix. Trust `help`.

## Complete syntax reference

```
# comment (to end of line)
import "lib/util.mno"        # imports at the top; paths relative to this file
import "pkg:mathx/mathx.mno" # package import (machino_modules/, see below)
import "geometry.mno" as geo # namespaced: use geo::fn_name, geo::TypeName

struct Point {               # structs: nominal record types
    x: float
    y: float
}

enum Option {                # enums: sum types (tagged unions)
    None
    Some(int)
}

enum Result {
    Ok(str)
    Err(str)
}

# functions; statements end at NEWLINES, there are no semicolons
fn dist2(a: Point, b: Point) -> float {
    let dx = b.x - a.x
    let dy = b.y - a.y
    return dx * dx + dy * dy
}

# contracts: checked at runtime, failures name the clause
fn safe_div(a: int, b: int) -> int
    requires b != 0
    ensures result * b <= a
{
    return a / b
}

# generics: constraints are Eq (== !=), Ord (< <= > >=), Num (+ - * /);
# type arguments are inferred at each call site
fn<T: Ord> max2(a: T, b: T) -> T {
    if a > b {
        return a
    }
    return b
}

# first-class functions and capturing lambdas
fn map_ints(xs: [int], f: fn(int) -> int) -> [int] {
    let out: [int] = []
    for i in 0..len(xs) {
        out = push(out, f(xs[i]))
    }
    return out
}

fn make_adder(n: int) -> fn(int) -> int {
    return fn(x: int) -> int { return x + n }   # captures n by value
}

# pattern matching on enums (exhaustive check at compile-time)
fn unwrap_or(opt: Option, default: int) -> int {
    return match opt {
        Option::Some(v) => v
        Option::None => default
    }
}

# host imports (FFI/capabilities); the host must provide them
extern fn clock_ms() -> int

# entry point: exactly this signature
fn main() {
    let p = Point(1.0, 2.0)      # positional constructor
    p.x = 3.0                    # field assignment (structs are references)
    let xs = [1, 2, 3]
    let ys = push(xs, 4)         # push returns a NEW array
    xs[0] = 10                   # element assignment, bounds-checked
    for i in 0..len(ys) {        # for over int ranges (end exclusive)
        if i == 2 { continue }
        if i == 3 { break }
        print(ys[i])
    }
    let msg = ("long strings wrap"
        + " only inside parentheses")   # newlines end statements otherwise
    print(msg)
    let doubled = map_ints(xs, fn(x: int) -> int { return x * 2 })
    print(doubled[0])
}

test "math" {
    assert safe_div(10, 2) == 5
}

# snapshot test: passes only if print output equals the string exactly
test "output" expects "hello\n5" {
    print("hello")
    print(5)
}
```

## Types and rules that trip models up

- Types: `int` (i64, **checked arithmetic** — overflow traps), `float` (f64),
  `bool`, `str` (byte string), `[T]`, struct names, enum names, `fn(T...) -> R`.
- **No implicit conversions.** `1 + 2.0` is an error. Use `to_float(i)` /
  `to_int(f)`. Float literals need digits on both sides: `2.0`, not `2.`.
- `+` on strings concatenates. `%` is int-only. Comparisons don't chain.
- `push(xs, v)` does NOT mutate; always rebind: `xs = push(xs, v)`.
- **Enums:** construct with `Enum::Variant` (unit) or `Enum::Variant(payload)`.
  Deconstruct with `match`.
- **Match expressions must be exhaustive.** The type checker ensures all cases
  are covered. Patterns: `_` (wildcard), `var` (binding), literals, 
  `Enum::Variant`, `Enum::Variant(pattern)`.
- Structs/arrays are references; `==` works only on scalars and `str`.
- No methods: `p.dist()` is an error; write `dist(p)`.
- Lambdas: `fn(x: int) -> int { return x + n }` is an expression; it
  captures enclosing variables **by value** and captured variables are
  **read-only** (E049). To share mutable state, capture a struct or array
  and mutate its *contents*. Lambdas can't recurse (no name) and have no
  contracts. Named functions also work as `fn`-typed values.
- Calls apply to *names* only — `f(1)(2)` doesn't parse. Bind the
  intermediate: `let g = f(1)` then `g(2)`.
- Each statement on its own line; wrap long expressions in `(...)`.
- Every path in a value-returning function must `return`.
- In `ensures`, `result` is the return value; parameters are the *original*
  arguments.
- Empty array literals need an annotation: `let xs: [int] = []`.

## Builtins (the only ones)

`print(x)`, `len(x)`, `push(xs, v)`, `to_float(i)`, `to_int(f)`,
`char_at(s, i)` (byte as int), `substr(s, a, b)`, `chr(byte)`,
`spawn(f, args...)` -> task handle, `join_int(h)` / `join_float(h)` /
`join_bool(h)` / `join_str(h)` (concurrency; interpreter only — `machino
build` rejects them)

## Standard prelude (always available, names reserved)

- numbers: `str_of_int`, `str_of_bool`, `str_of_float(f, decimals)`,
  `parse_int(s)` (requires `is_int_str(s)`), `is_int_str`, `is_digit`,
  `abs_int`, `min_int`, `max_int`, `sum_ints`, `index_of_int`, `sort_ints`
- float math: `sqrt(x)` (requires `x >= 0.0`), `pow_int(base, exp)`
  (requires `exp >= 0`), `pow_float(base, exp)` (int exp, may be negative),
  `floor(f)`/`ceil(f)`/`round(f)` (-> int), `abs_float`, `min_float`,
  `max_float`
- strings: `find`, `find_from`, `contains`, `starts_with`, `ends_with`,
  `split(s, sep)`, `join(parts, sep)`, `trim`, `is_space`, `to_upper`,
  `to_lower`, `repeat`
- maps: `StrMap` (str → str): `strmap_new()`, `strmap_set(m, k, v)`,
  `strmap_get(m, k)` (requires key present), `strmap_get_or(m, k, default)`,
  `strmap_has`, `strmap_len`; `IntMap` (int → int) with the same shape:
  `intmap_new`, `intmap_set`, `intmap_get`, `intmap_get_or`, `intmap_has`,
  `intmap_len`
- JSON: enum `Json` (`JNull | JBool(bool) | JNum(float) | JStr(str) |
  JArr([Json]) | JObj(JsonObj)`); `json_parse(s) -> JsonParsed`
  (`JVal(Json) | JError(str)`), `json_serialize(v) -> str`,
  `json_obj_new()`, `json_obj_set(o, k, v)`, `json_obj_get(o, k)`
  (returns `Json::JNull` when absent)
- time (UTC, unix epoch ms): `time_iso(ms) -> str` ("1970-01-01T00:00:00Z"),
  `time_from_ms(ms) -> Time` (fields `year month day hour minute second
  millis`), `time_format(t)`, `pad2`

## Host externs (declare what you need with `extern fn`)

Provided by `machino run` (the native runtime):

```
extern fn clock_ms() -> int
extern fn sleep_ms(ms: int)
extern fn read_file(path: str) -> str
extern fn write_file(path: str, data: str) -> bool
extern fn file_exists(path: str) -> bool
extern fn read_line() -> str
extern fn getenv(name: str) -> str
extern fn http_get(url: str) -> str
extern fn args() -> [str]
extern fn exit(code: int)
extern fn tcp_listen(port: int) -> int
extern fn tcp_accept(listener: int) -> int
extern fn tcp_read(conn: int) -> str
extern fn tcp_write(conn: int, data: str) -> int
extern fn tcp_close(handle: int)
```

Servers: `tcp_listen` → loop `tcp_accept` → `tcp_read` → `tcp_write` →
`tcp_close`. See `examples/http_server.mno` for a complete HTTP server.
The Node WASM host provides everything except `tcp_*`.

## Packages

```
machino pkg init myapp                 # creates machino.pkg in this directory
machino pkg add mathx ../mathx         # local path dependency (copied)
machino pkg add strkit https://github.com/user/strkit 0.2.0   # git, at a tag
machino pkg sync                       # (re)install everything + machino.lock
```

Then import with the `pkg:` prefix: `import "pkg:mathx/mathx.mno"`.
Dependencies land in `machino_modules/` (commit machino.pkg and
machino.lock; ignore machino_modules). Transitive deps are flattened —
one source per package name.

Any import can be namespaced: `import "lib.mno" as lib` renames every
top-level item lib.mno defines to `lib::name` — call `lib::helper(...)`,
annotate `let p: lib::Point`, match `lib::Verdict::Yes`. Without `as`,
imported names stay global (collisions are errors).

## Other tools

- `machino fmt file.mno [--check|--stdout]` — canonical formatter (safe:
  refuses any edit that changes the token stream)
- `machino query file.mno [name]` — JSON signatures/contracts/generics of
  every top-level item; use it to look up an unfamiliar file's API
- `machino run file.mno --trace` — one JSON object per user-function
  call/return on stderr: `{"event":"call","fn":"fib","depth":1,"args":["5"]}`
- `machino fuzz file.mno [--runs N]` — random-input testing driven by
  contracts; reports the failing input when a contract or trap fires
- `machino check file.mno --verify` — static contract proofs with Z3 for
  loop-free, call-free int/bool functions (build with `--features smt`)
- `machino build file.mno --gc` — experimental WASM-GC backend (scalars,
  strings, arrays; no structs/enums/closures yet, E070)

## Error codes you will see most

| Code | Meaning | Usual fix |
|------|---------|-----------|
| E030 | type mismatch | insert `to_float`/`to_int`, or fix the declared type |
| E035 | unknown variable | declare with `let`, or check spelling |
| E043 | operator/operand mismatch | no mixed int/float math |
| E025 | not all paths return | add a final `return` |
| E040 | empty array needs annotation | `let xs: [int] = []` |
| E011 | expected end of statement | split lines, or wrap in `(...)` |
| E021 | name already defined | prelude names are reserved; rename |
| E048 | no such struct field | check the field list in the error's help |
| E049 | assigning to a captured variable | capture a struct/array and mutate its contents |

## Deployment

`machino build` emits a self-contained standard `.wasm` module (exports
`main`, `memory`, `alloc`; imports only `env.print_*`, `env.fail`, and your
`extern fn`s). Run it with `node runners/run.mjs out.wasm [args...]`, drop it
into `runners/run.html` in a browser, or embed it in any WebAssembly runtime.
Contracts, asserts, bounds checks, and overflow checks stay enforced in the
compiled binary, with the same messages as the interpreter.

The compiled module includes a mark-sweep garbage collector: loops that
allocate freely (string building, `push` in a loop, closures) run in bounded
memory. Memory is capped at 1 GiB; a program whose *live* data exceeds that
traps with `out of memory`.
