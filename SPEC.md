# The machino Language Specification (v0.2)

machino is an AI-first programming language. It is designed for code that is
*written and verified by machines*: the syntax is small and canonical, the
semantics are fully defined (no undefined behavior), contracts are part of the
language, and the compiler targets WebAssembly — a portable, sandboxed,
formally specified machine language that runs everywhere.

## Design principles

1. **One way to write everything.** No optional syntax, no style choices.
   Generated code is diffable and predictable.
2. **No undefined behavior.** Every runtime fault (division by zero, integer
   overflow, index out of bounds, contract violation) is a defined trap with a
   message, never silent corruption. The interpreter and the compiled
   WebAssembly agree on all of them.
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
  Newlines inside `(...)` or `[...]` are ignored, so long expressions can wrap
  by adding parentheses.
- Identifiers match `[A-Za-z_][A-Za-z0-9_]*`.
- Keywords: `fn extern let if else while for in break continue return true
  false requires ensures test assert struct import`.
- Reserved names: `result` (bound in `ensures`), `memory`, `alloc`, the
  builtins, and every std-prelude function and struct (see below).

## Types

| Type        | Description                                   | WASM representation |
|-------------|-----------------------------------------------|---------------------|
| `int`       | 64-bit signed integer (checked arithmetic)    | `i64`               |
| `float`     | 64-bit IEEE-754                               | `f64`               |
| `bool`      | `true` / `false`                              | `i64` (0 or 1)      |
| `str`       | immutable byte string (usually UTF-8)         | `i64` pointer       |
| `[T]`       | array of `T` (fixed length; `push` copies)    | `i64` pointer       |
| `StructName`| a declared struct (nominal, reference type)   | `i64` pointer       |
| `fn(T...) -> R` | a first-class named function             | `i64` table index   |

There are **no implicit conversions**. `int` + `float` is a type error; use
`to_float(i)` or `to_int(f)` (truncating; traps outside int range). Array
elements must all have one type. Arrays and structs are reference values:
mutation through one binding is visible through aliases. `==`/`!=` are defined
for scalars and `str` only.

## Programs and modules

A program is a sequence of `import` declarations, function definitions,
`struct` definitions, `extern` declarations, and `test` blocks. The entry
point is `fn main()` (no parameters, no return value).

```
import "lib/util.mno"
```

Imports are resolved relative to the importing file, transitively, and
deduplicated. All definitions share one flat namespace (a name collision is
error `E021`). Tests in imported files run as part of `machino test`.

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
- If a function has a return type, **every control path must return** (`E025`).
- `requires` clauses are evaluated on entry against the arguments; `ensures`
  clauses on exit against the *original* arguments and `result`. A false
  contract traps with a message naming the clause — in both backends.
- Named functions are first-class values: `let f = double`, then `f(21)`.
  Function types are written `fn(int, str) -> bool`. There are **no capturing
  closures**; pass state as arguments.

### Structs

```
struct Point {
    x: float
    y: float
}
```

Constructed positionally — `Point(1.0, 2.0)` — with fields read as `p.x` and
assigned as `p.x = 3.0` (including nested paths like `rect.a.x = 0.0`).
Structs are reference types with nominal typing. Machino has no methods: write
plain functions that take the struct as an argument.

### Extern functions

```
extern fn tcp_listen(port: int) -> int
```

Declares a host-provided import (WASM import module `env`). This is machino's
FFI and capability system. Extern signatures may use scalars, `str`, and
arrays of scalars/`str` (`E026` otherwise).

**The native runtime** (`machino run`) provides these externs:

| Extern | Signature |
|---|---|
| `clock_ms` | `() -> int` — Unix time in ms |
| `sleep_ms` | `(ms: int)` |
| `read_file` | `(path: str) -> str` (traps if unreadable) |
| `write_file` | `(path: str, data: str) -> bool` |
| `file_exists` | `(path: str) -> bool` |
| `read_line` | `() -> str` — one stdin line, `""` at EOF |
| `getenv` | `(name: str) -> str` — `""` if unset |
| `args` | `() -> [str]` — CLI args after the file name |
| `exit` | `(code: int)` |
| `tcp_listen` | `(port: int) -> int` — returns a listener handle |
| `tcp_accept` | `(listener: int) -> int` — blocks; returns a connection |
| `tcp_read` | `(conn: int) -> str` — one read, up to 64 KiB |
| `tcp_write` | `(conn: int, data: str) -> int` |
| `tcp_close` | `(handle: int)` |

The Node host (`runners/run.mjs`) provides all of these except the `tcp_*`
family (Node sockets are asynchronous; bring your own host or WASI sockets
for compiled servers). The browser host provides the print family, `clock_ms`,
`getenv`, and `read_line` only. Declaring an extern the host lacks fails at
instantiation — capability denial, working as intended.

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
p.field = expr           # struct field assignment
if cond { ... } else if ... { ... } else { ... }
while cond { ... }
for i in a..b { ... }    # i: int, from a inclusive to b exclusive
break                    # exit the innermost loop
continue                 # next iteration (for-loops still increment)
return expr              # or bare 'return' in a unit function
assert expr              # trap with location if false
expr                     # expression statement (e.g. print(x))
```

Variables are block-scoped. Inner blocks may shadow outer names. The for-loop
bound `b` is evaluated once, before the loop.

## Expressions

Precedence, low to high: `||`, `&&`, comparisons (`== != < <= > >=`,
non-associative), `+ -`, `* / %`, unary `- !`, postfix (`xs[i]`, `p.f`),
atoms. `&&`/`||` short-circuit.

- `+` is defined for `int/int`, `float/float`, and `str/str` (concatenation).
- `%` is `int` only.
- `int` arithmetic **traps on overflow** (and division/modulo by zero) in both
  backends, with identical messages.

## Builtins

| Builtin       | Signature                        | Notes                       |
|---------------|----------------------------------|------------------------------|
| `print(x)`    | any scalar or `str`              | writes a line to host output |
| `len(x)`      | `[T]` or `str` → `int`           | array length / byte length   |
| `push(xs, v)` | `([T], T)` → `[T]`               | returns a **new** array      |
| `to_float(i)` | `int` → `float`                  |                              |
| `to_int(f)`   | `float` → `int`                  | truncates; traps out of range|
| `char_at(s,i)`| `(str, int)` → `int`             | byte value, bounds-checked   |
| `substr(s,a,b)`| `(str, int, int)` → `str`       | bytes `[a, b)`, checked      |
| `chr(c)`      | `int` → `str`                    | one byte, `0..=255`          |

## Standard prelude

Every program is compiled together with a prelude written in machino itself
(`src/std.mno`). Its names are reserved. Unused prelude functions are removed
from compiled WASM.

- **Formatting/parsing:** `str_of_int`, `str_of_bool`, `str_of_float(f, decimals)`,
  `parse_int`, `is_int_str`, `is_digit`
- **Strings:** `find`, `find_from`, `contains`, `starts_with`, `ends_with`,
  `split`, `join`, `trim`, `is_space`, `to_upper`, `to_lower`, `repeat`
- **Map:** `struct StrMap` with `strmap_new`, `strmap_set`, `strmap_get`,
  `strmap_get_or`, `strmap_has`, `strmap_index`, `strmap_len`
- **Ints:** `abs_int`, `min_int`, `max_int`, `sum_ints`, `index_of_int`,
  `sort_ints`

## Memory model (compiled WASM)

Values are 8 bytes each. `str`/`[T]` point into linear memory with layout
`[len: i64][payload]`; structs are `[field0][field1]...`. Allocation is a bump
allocator that grows memory on demand; there is no garbage collector
(allocations live until the program exits — fine for scripts and request
handlers, not for unbounded-allocation loops). The module exports `memory`,
`alloc`, and every non-prelude function; function values use a `funcref`
table and `call_indirect`.

## Diagnostics

`machino check --json` prints one JSON object:
`{"ok":bool,"errors":n,"diagnostics":[{severity,code,message,file,line,col,endLine,endCol,help?}]}`.
Positions map to the correct file across imports. Error codes are stable:
`E001–E005` lexical, `E010–E019` syntax, `E020–E048` types and semantics.

## Known limits (v0.2)

- No capturing closures (named functions as values only).
- No garbage collector (bump allocator; `push`/`+` allocate fresh copies).
- No generics; `StrMap` is `str → str` (encode other value types).
- Contracts on `extern fn`s are enforced by the interpreter but not in
  compiled WASM.
- The interpreter's call depth defaults to 4096 (`MACHINO_MAX_DEPTH` env var
  overrides).

## Roadmap (v0.3+)

- enums and pattern matching, generics
- WASM-GC backend, tree-shaken prelude in the interpreter
- static contract verification (SMT) for a decidable subset
- package registry
