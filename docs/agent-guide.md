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

struct Point {               # structs: nominal record types
    x: float
    y: float
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

# first-class functions (no closures; pass state as arguments)
fn map_ints(xs: [int], f: fn(int) -> int) -> [int] {
    let out: [int] = []
    for i in 0..len(xs) {
        out = push(out, f(xs[i]))
    }
    return out
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
}

test "math" {
    assert safe_div(10, 2) == 5
}
```

## Types and rules that trip models up

- Types: `int` (i64, **checked arithmetic** — overflow traps), `float` (f64),
  `bool`, `str` (byte string), `[T]`, struct names, `fn(T...) -> R`.
- **No implicit conversions.** `1 + 2.0` is an error. Use `to_float(i)` /
  `to_int(f)`. Float literals need digits on both sides: `2.0`, not `2.`.
- `+` on strings concatenates. `%` is int-only. Comparisons don't chain.
- `push(xs, v)` does NOT mutate; always rebind: `xs = push(xs, v)`.
- Structs/arrays are references; `==` works only on scalars and `str`.
- No methods: `p.dist()` is an error; write `dist(p)`.
- No closures: a `fn`-typed value must be a named top-level function.
- Each statement on its own line; wrap long expressions in `(...)`.
- Every path in a value-returning function must `return`.
- In `ensures`, `result` is the return value; parameters are the *original*
  arguments.
- Empty array literals need an annotation: `let xs: [int] = []`.

## Builtins (the only ones)

`print(x)`, `len(x)`, `push(xs, v)`, `to_float(i)`, `to_int(f)`,
`char_at(s, i)` (byte as int), `substr(s, a, b)`, `chr(byte)`

## Standard prelude (always available, names reserved)

- numbers: `str_of_int`, `str_of_bool`, `str_of_float(f, decimals)`,
  `parse_int(s)` (requires `is_int_str(s)`), `is_int_str`, `is_digit`,
  `abs_int`, `min_int`, `max_int`, `sum_ints`, `index_of_int`, `sort_ints`
- strings: `find`, `find_from`, `contains`, `starts_with`, `ends_with`,
  `split(s, sep)`, `join(parts, sep)`, `trim`, `is_space`, `to_upper`,
  `to_lower`, `repeat`
- map: `StrMap` (str → str): `strmap_new()`, `strmap_set(m, k, v)`,
  `strmap_get(m, k)` (requires key present), `strmap_get_or(m, k, default)`,
  `strmap_has`, `strmap_len`

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

## Deployment

`machino build` emits a self-contained standard `.wasm` module (exports
`main`, `memory`, `alloc`; imports only `env.print_*`, `env.fail`, and your
`extern fn`s). Run it with `node runners/run.mjs out.wasm [args...]`, drop it
into `runners/run.html` in a browser, or embed it in any WebAssembly runtime.
Contracts, asserts, bounds checks, and overflow checks stay enforced in the
compiled binary, with the same messages as the interpreter.
