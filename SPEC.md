# The machino Language Specification (v0.1)

machino is an AI-first programming language. It is designed for code that is
*written and verified by machines*: the syntax is small and canonical, the
semantics are fully defined (no undefined behavior), contracts are part of the
language, and the compiler targets WebAssembly — a portable, sandboxed,
formally specified machine language that runs everywhere.

## Design principles

1. **One way to write everything.** No optional syntax, no style choices.
   Generated code is diffable and predictable.
2. **No undefined behavior.** Every runtime fault (division by zero, index out
   of bounds, integer overflow, contract violation) is a defined trap with a
   message, never silent corruption.
3. **Contracts are code.** `requires`/`ensures` clauses and `test` blocks are
   first-class syntax, checked by the toolchain — the agent writes the spec
   and the implementation together.
4. **Diagnostics are an API.** Every error has a stable code (`E0xx`) and is
   available as JSON (`--json`) for generate-check-repair loops.
5. **Capability-based host access.** A machino program can only touch the
   outside world through `extern fn` imports the host explicitly provides.

## Lexical structure

- Source files use UTF-8 and the `.mno` extension.
- Comments start with `#` and run to end of line.
- Statements are terminated by **newlines** — there are no semicolons.
  Newlines inside `(...)` or `[...]` are ignored, so long calls can wrap.
- Identifiers match `[A-Za-z_][A-Za-z0-9_]*`.
- Keywords: `fn extern let if else while return true false requires ensures
  test assert`.
- Reserved names: `result` (bound in `ensures`), `memory`, and the builtins
  `print len push to_float to_int`.

## Types

| Type    | Description                          | WASM representation |
|---------|--------------------------------------|---------------------|
| `int`   | 64-bit signed integer                | `i64`               |
| `float` | 64-bit IEEE-754                      | `f64`               |
| `bool`  | `true` / `false`                     | `i64` (0 or 1)      |
| `str`   | immutable UTF-8 string               | `i64` pointer       |
| `[T]`   | array of `T` (fixed length; `push` returns a new array) | `i64` pointer |

There are **no implicit conversions**. `int` + `float` is a type error; use
`to_float(i)` or `to_int(f)` (truncating). Array elements must all have one
type. Arrays are reference values: index assignment through one binding is
visible through aliases. Array equality (`==`) is not defined; compare
element-wise.

## Programs

A program is a sequence of function definitions, `extern` declarations, and
`test` blocks. The entry point is `fn main()` (no parameters, no return
value).

### Functions

```
fn name(param: type, ...) -> ret_type
    requires <bool expr>     # zero or more
    ensures  <bool expr>     # zero or more; 'result' is the return value
{
    <statements>
}
```

- The `-> ret_type` clause is omitted for functions that return nothing.
- If a function has a return type, **every control path must return** (checked
  statically, error `E025`).
- `requires` clauses are evaluated on entry against the arguments; `ensures`
  clauses on exit against the *original* arguments and `result`. A false
  contract traps with a message naming the clause. Contracts are enforced in
  both the interpreter and compiled WASM.

### Extern functions

```
extern fn clock_ms() -> int
```

Declares a host-provided import (WASM import module `env`). This is machino's
FFI and capability system: the program cannot name any host power that the
host does not supply. Extern parameter/return types must be scalars or `str`.
The reference host (`runners/run.mjs`) provides `clock_ms`.

### Tests

```
test "descriptive name" {
    assert <bool expr>
}
```

Run with `machino test`. Test names must be unique. `return` is not allowed
inside tests.

## Statements

```
let x = expr             # declare + initialize (type inferred)
let x: [int] = []        # annotation required only when inference can't decide
x = expr                 # reassignment (same type)
xs[i] = expr             # array element assignment (bounds-checked)
if cond { ... }          # cond must be bool
if cond { ... } else { ... }
if cond { ... } else if ... { ... }
while cond { ... }
return expr              # or bare 'return' in a unit function
assert expr              # trap with location if false
expr                     # expression statement (e.g. print(x))
```

Variables are block-scoped. Inner blocks may shadow outer names.

## Expressions

Precedence, low to high: `||`, `&&`, comparisons (`== != < <= > >=`,
non-associative), `+ -`, `* / %`, unary `- !`, postfix indexing `xs[i]`,
atoms. `&&`/`||` short-circuit.

- `+` is defined for `int/int`, `float/float`, and `str/str` (concatenation).
- `%` is `int` only. Integer division/modulo by zero traps.
- `int` arithmetic overflow traps in the interpreter (and wraps in WASM v0.1 —
  a known divergence to be unified in v0.2).

## Builtins

| Builtin       | Signature                | Notes                          |
|---------------|--------------------------|--------------------------------|
| `print(x)`    | any non-array type       | writes a line to host output   |
| `len(x)`      | `[T]` or `str` → `int`   | array length / byte length     |
| `push(xs, v)` | `([T], T)` → `[T]`       | returns a **new** array        |
| `to_float(i)` | `int` → `float`          |                                |
| `to_int(f)`   | `float` → `int`          | truncates toward zero          |

## Memory model (compiled WASM)

Values are 8 bytes each. `str` and `[T]` point into linear memory with layout
`[len: i64][payload]`. Allocation is a bump allocator that grows memory on
demand; v0.1 has no garbage collector (allocations live until the program
exits). The heap pointer is mutable global 0; the module exports `memory` and
every user function by name.

## Host interface

A machino `.wasm` module imports from module `env`:

| Import        | Signature       | Purpose                                    |
|---------------|-----------------|--------------------------------------------|
| `fail(msg)`   | `(i64)`         | called with a `str` pointer before a trap  |
| `print_i64`   | `(i64)`         | print an int                               |
| `print_f64`   | `(f64)`         | print a float                              |
| `print_bool`  | `(i64)`         | print a bool                               |
| `print_str`   | `(i64)`         | print a str (pointer)                      |
| *user externs*| as declared     | one import per `extern fn`                 |

## Diagnostics

`machino check --json` prints one JSON object:
`{"ok":bool,"errors":n,"diagnostics":[{severity,code,message,file,line,col,endLine,endCol,help?}]}`.
Error codes are stable across releases: `E001–E005` lexical, `E010–E019`
syntax, `E020–E047` types and semantics.

## Roadmap (v0.2+)

- structs / records, `for` loops, pattern matching
- unify int-overflow semantics (trapping arithmetic in WASM)
- garbage collection (or WASM-GC types)
- module system and a package registry
- static contract verification (SMT) for a decidable subset
