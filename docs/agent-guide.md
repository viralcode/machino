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
4. machino run   program.mno            # execute fn main()
5. machino build program.mno -o out.wasm  # ship: runs in any WASM host
```

Every diagnostic has `code`, `line`, `col`, `message`, and often `help` with
the exact fix. Trust `help`.

## Complete syntax reference

```
# comment (to end of line)

# functions; statements end at NEWLINES, there are no semicolons
fn add(a: int, b: int) -> int {
    return a + b
}

# contracts: checked at runtime, failures name the clause
fn safe_div(a: int, b: int) -> int
    requires b != 0
    ensures result * b <= a
{
    return a / b
}

# host imports (FFI/capabilities); the host must provide them
extern fn clock_ms() -> int

# entry point: exactly this signature
fn main() {
    let x = 41                # types are inferred
    x = x + 1                 # reassignment allowed, same type only
    let e: [int] = []         # empty array literal needs an annotation
    let xs = [1, 2, 3]        # array literal
    let ys = push(xs, 4)      # push returns a NEW array
    xs[0] = 10                # element assignment, bounds-checked
    if x > 0 && len(ys) == 4 {
        print("ok")
    } else if x == 0 {
        print("zero")
    } else {
        print("negative")
    }
    let i = 0
    while i < len(ys) {
        print(ys[i])
        i = i + 1
    }
}

# inline tests, run by 'machino test'
test "add works" {
    assert add(2, 2) == 4
}
```

## Types and rules that trip models up

- Types: `int` (i64), `float` (f64), `bool`, `str`, `[T]`. Nothing else.
- **No implicit conversions.** `1 + 2.0` is an error. Use `to_float(i)` /
  `to_int(f)`.
- Float literals need a digit on both sides of the dot: `2.0`, not `2.` or `.5`.
- `+` on strings concatenates. `%` is int-only.
- No `for` loops, no structs, no closures, no recursion limit above 4096.
- `print` takes one argument, any non-array type. Build strings with `+`.
- `push(xs, v)` does NOT mutate; always rebind: `xs = push(xs, v)`.
- Each statement on its own line. A long call may wrap only inside `(...)`.
- Booleans: `&&`, `||`, `!`. Comparisons don't chain (`a < b < c` is an error).
- Functions can only be declared at top level; every path in a value-returning
  function must `return`.
- In `ensures`, `result` is the return value; parameters refer to the
  *original* arguments.

## Builtins (the only ones)

`print(x)`, `len(x)`, `push(xs, v)`, `to_float(i)`, `to_int(f)`

## Error codes you will see most

| Code | Meaning | Usual fix |
|------|---------|-----------|
| E030 | type mismatch | insert `to_float`/`to_int`, or fix the declared type |
| E035 | unknown variable | declare with `let`, or check spelling |
| E043 | operator/operand mismatch | no mixed int/float math |
| E025 | not all paths return | add a final `return` |
| E040 | empty array needs annotation | `let xs: [int] = []` |
| E011 | expected end of statement | split into separate lines |

## Deployment

`machino build` emits a self-contained standard `.wasm` module (exports
`main` and `memory`; imports only `env.print_*`, `env.fail`, and your
`extern fn`s). Run it with `node runners/run.mjs out.wasm`, drop it into
`runners/run.html` in a browser, or embed it in any WebAssembly runtime
(wasmtime, wasmer, cloud edge workers). Contracts, asserts, and bounds checks
stay enforced in the compiled binary.
