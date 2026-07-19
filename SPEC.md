# The machino Language Specification (v0.7)

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
  false requires ensures test assert struct import enum match`.
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
| `EnumName`  | a declared enum (sum type)                    | `i64` pointer       |
| `fn(T...) -> R` | a function value (named fn or closure)    | `i64` closure ptr   |

There are **no implicit conversions**. `int` + `float` is a type error; use
`to_float(i)` or `to_int(f)` (truncating; traps outside int range). Array
elements must all have one type. Arrays and structs are reference values:
mutation through one binding is visible through aliases. `==`/`!=` are defined
for scalars and `str` only.

## Programs, modules, and packages

A program is a sequence of `import` declarations, function definitions,
`struct` definitions, `enum` definitions, `extern` declarations, and `test` 
blocks. The entry point is `fn main()` (no parameters, no return value).

```
import "lib/util.mno"
import "pkg:mathx/mathx.mno"
```

Plain imports are resolved relative to the importing file, transitively, and
deduplicated. All definitions share one flat namespace (a name collision is
error `E021`). Tests in imported files run as part of `machino test`.

An import may carry a **namespace alias**:

```
import "geometry.mno" as geo
```

Every top-level function, struct, and enum the aliased file defines is then
reachable only as `geo::name`: calls (`geo::dist(a, b)`), constructors
(`geo::Point(1.0, 2.0)`), type annotations (`let p: geo::Point`), enum
variants (`geo::Quadrant::First`), and match patterns all take the prefix.
Inside the imported file itself nothing changes — its own unqualified
references keep working. A file must be imported under one consistent alias
(or consistently without one) across the whole program.

`pkg:` imports resolve against `machino_modules/` in the **project root** —
the nearest ancestor directory of the entry file containing a `machino.pkg`
manifest. The manifest is a line-based format:

```
name myapp
version 0.1.0
dep mathx ../mathx                                # local path
dep strkit https://github.com/user/strkit 0.2.0   # git URL, optional tag
```

- `machino pkg init <name>` creates the manifest.
- `machino pkg add <name> <source> [ref]` adds a dependency and installs it.
- `machino pkg sync` installs every dependency into `machino_modules/`
  (path deps are copied, git deps are shallow-cloned at the given tag),
  resolves transitive `machino.pkg` manifests, flattens them (one version
  per name; conflicting sources are an error), and records what was
  installed in `machino.lock`.

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
  Function types are written `fn(int, str) -> bool`.

### Lambdas (capturing closures)

```
let add5 = fn(x: int) -> int { return x + 5 }

fn make_adder(n: int) -> fn(int) -> int {
    return fn(x: int) -> int { return x + n }
}
```

A lambda is an expression. Parameter and return types are always written out
(omit `-> R` for a unit lambda). Lambdas **capture enclosing variables by
value at creation**:

- Captured variables are **read-only** inside the lambda — assigning to one
  is error `E049`. Because arrays and structs are reference values, capturing
  one lets the lambda mutate its *contents*, which is the idiomatic way to
  share mutable state:

```
let counter = Counter(0)
let tick = fn() -> int {
    counter.count = counter.count + 1
    return counter.count
}
```

- Lambdas have no contracts and no name (they cannot recurse into
  themselves; name a function for that).
- `break`/`continue`/`return` inside a lambda refer to the lambda itself.

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

### Enums

```
enum Option {
    None
    Some(int)
}

enum Result {
    Ok(str)
    Err(str)
}
```

Enums are sum types (tagged unions). Each variant may carry:
- **nothing** (unit variant): `Option::None`
- **a single payload** of any type: `Option::Some(42)`

Variants are constructed by calling them like functions:
- `Option::None` for unit variants
- `Option::Some(42)` for variants with payloads

Enums are deconstructed using `match` expressions.

### Pattern matching

```
let opt = Option::Some(42)
let value = match opt {
    Option::Some(v) => v
    Option::None => 0
}
```

Match expressions take a scrutinee and a list of arms. Each arm has a pattern
and a body. The first matching arm's body is evaluated, and its value is the
match result.

Patterns:
- `_` — wildcard, matches anything
- `var` — binds to a variable
- `42`, `true`, `"hello"` — literal matches
- `Enum::Variant` — matches a unit variant
- `Enum::Variant(pattern)` — matches a variant with payload, binds nested pattern

Match expressions must be **exhaustive** — the type checker verifies that all
possible values are covered. Match can be used as an expression (in `let` or
`return`) or as a statement (for side effects).

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

test "snapshot" expects "line1\nline2" {
    print("line1")
    print("line2")
}
```

Run with `machino test`. Test names must be unique. `return` is not allowed
inside tests. A test with an `expects` string is a **snapshot test**: its
`print` output (lines joined with `\n`, no trailing newline) must equal the
string exactly, otherwise the test fails with a `snapshot mismatch` message
showing both strings.

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
match expr { arms }      # pattern match (can be statement or expression)
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
| `spawn(f, ...)` | `(fn(...) -> S, args...)` → `int` | task handle; interpreter only |
| `join_int(h)` | `int` → `int`                    | joins a task returning `int` |
| `join_float(h)` | `int` → `float`                | likewise for `float`         |
| `join_bool(h)` | `int` → `bool`                  | likewise for `bool`          |
| `join_str(h)` | `int` → `str`                    | likewise for `str`           |

### Concurrency (interpreter only)

`spawn(f, args...)` starts `f` on a fresh OS thread and returns a task
handle. The arguments (and, for closures, the captured environment) are
**deep-copied** across the thread boundary — tasks share no mutable state
with their parent or each other, so a program's results are deterministic
regardless of scheduling. `f` must return `int`, `float`, `bool`, or `str`
(`E071`); retrieve the result with the matching `join_*`, which blocks until
the task finishes. Joining a handle twice, or a handle that never existed,
is a runtime error. A contract violation or trap inside a task surfaces as
a runtime error at the `join_*` call site. The WASM backends have no thread
support, so `machino build` rejects programs that use spawn/join (`E072`).

## Standard prelude

Every program is compiled together with a prelude written in machino itself
(`src/std.mno`). Its names are reserved. Unused prelude functions are removed
from compiled WASM.

- **Formatting/parsing:** `str_of_int`, `str_of_bool`, `str_of_float(f, decimals)`,
  `parse_int`, `is_int_str`, `is_digit`
- **Strings:** `find`, `find_from`, `contains`, `starts_with`, `ends_with`,
  `split`, `join`, `trim`, `is_space`, `to_upper`, `to_lower`, `repeat`
- **Maps:** `struct StrMap` (str → str) with `strmap_new`, `strmap_set`,
  `strmap_get`, `strmap_get_or`, `strmap_has`, `strmap_index`, `strmap_len`;
  `struct IntMap` (int → int) with the same operations under `intmap_*`
- **Ints:** `abs_int`, `min_int`, `max_int`, `sum_ints`, `index_of_int`,
  `sort_ints`
- **Float math** (pure machino, identical in both backends): `sqrt`
  (Newton's method; requires `x >= 0.0`), `pow_int` (requires `exp >= 0`),
  `pow_float` (int exponent, may be negative), `floor`, `ceil`, `round`
  (float → int), `abs_float`, `min_float`, `max_float`
- **JSON:** `enum Json { JNull, JBool(bool), JNum(float), JStr(str),
  JArr([Json]), JObj(JsonObj) }` with `json_parse(s) -> JsonParsed`
  (`JVal(Json) | JError(str)` — errors carry a byte offset),
  `json_serialize(v) -> str` (compact, keys in insertion order),
  `json_obj_new`, `json_obj_set`, `json_obj_get`. Numbers are `float`;
  integral values serialize without a fraction. `\uXXXX` escapes decode to
  a byte below U+0100, `?` otherwise (strings are byte strings).
- **Time** (UTC, unix epoch milliseconds): `struct Time { year month day
  hour minute second millis }`, `time_from_ms(ms)` (civil-from-days
  algorithm, requires `ms >= 0`), `time_format(t)`, `time_iso(ms)`
  (ISO 8601, e.g. `2001-09-09T01:46:40Z`), `pad2`

## Memory model (compiled WASM)

Values are 8 bytes each. Every heap object carries a 16-byte header:
`[meta: i64][bitmap: i64][payload]`, where `meta` packs a type tag (bits
0–2: bytes / scalar array / pointer array / struct / free block), a GC mark
bit (bit 3), and a count (bits 4+). For structs and closures the bitmap
flags which payload words are pointers. Enums are represented as struct-like
objects with a tag field (variant index) and a payload field (variant data
if present). Function values are closure objects
`[header][table_slot][captures...]`; calls through them use a `funcref`
table and `call_indirect` with a hidden environment parameter (named
functions used as values get a static singleton closure and a wrapper).

**Garbage collection.** Compiled modules include a precise mark-sweep
collector. Pointer-typed variables are mirrored into shadow-stack frames in
linear memory, which the collector scans as roots; collection runs only at
safepoints (function entry and loop back-edges) when allocation since the
last cycle exceeds an adaptive threshold (min 1 MiB). Objects never move;
freed blocks are coalesced into a free list that the allocator searches
before bumping. Memory is capped at 1 GiB — allocating past it (with
nothing to collect) traps with `out of memory`. The interpreter uses
reference counting; the only divergence is pathological reference cycles
(e.g. an array pushed into a struct it contains), which the interpreter
leaks and the compiled GC reclaims.

The module exports `memory`, `alloc`, and every non-prelude function. Hosts
that create objects (strings, arrays) must write the 16-byte header — see
`runners/run.mjs` for the reference implementation.

## Diagnostics

`machino check --json` prints one JSON object:
`{"ok":bool,"errors":n,"diagnostics":[{severity,code,message,file,line,col,endLine,endCol,help?,fix?}]}`.
Positions map to the correct file across imports. Error codes are stable:
`E001–E005` lexical, `E010–E019` syntax, `E020–E072` types and semantics.
Some diagnostics carry a machine-applicable `fix`
(`{line,col,endLine,endCol,replace}`): apply it by replacing that range with
`replace`. The full schema is `docs/diagnostics.schema.json`; the formal
grammar is `docs/grammar.ebnf`.

## Generics

Functions may declare type parameters with optional constraint bounds:

```
fn<T> identity(x: T) -> T {
    return x
}

fn<T: Ord> max2(a: T, b: T) -> T {
    if a > b {
        return a
    }
    return b
}
```

- Bounds: `Eq` enables `==`/`!=`, `Ord` enables comparisons (and implies
  `Eq`), `Num` enables `+ - * /`. Using an operator without the matching
  bound is a compile error (`E065`).
- Type arguments are inferred at every call site by unification; there is
  no explicit turbofish syntax. Ambiguous or conflicting inference is an
  error with the conflicting types named.
- The compiler **monomorphizes**: each distinct instantiation becomes a
  concrete function before codegen, so both backends see only concrete
  types. `machino query` still reports the generic template as written.
- `struct<T>`/`enum<T>` declarations parse, but constructing a generic
  struct/enum directly is rejected (`E066`); pass values through generic
  functions instead.

## Static verification (`machino check --verify`)

Built with `--features smt` (Z3), the verifier symbolically executes a
decidable subset — loop-free, call-free functions over `int`/`bool`
(array `len`/indexing and struct fields become uninterpreted symbols) —
and reports, per function: contracts **proved**, a **counterexample**
(concrete argument values violating an `ensures`), or **vacuous requires**
(no input can satisfy them). Functions outside the subset are skipped and
still enforced at runtime, as always.

## Tooling

- `machino fmt [--check|--stdout]` — the canonical formatter. It rewrites
  token spacing, indentation (4 spaces), and blank-line placement, then
  re-lexes its output and refuses to write anything that would change the
  parser-visible token stream or lose a comment.
- `machino query file.mno [name]` — JSON description (name, type params
  with bounds, param/return types, contracts) of every top-level item.
- `machino run file.mno --trace` — emits one JSON object per user-function
  call and return on stderr while the program runs normally on stdout:
  `{"event":"call","fn":"fib","depth":1,"args":["5"]}` /
  `{"event":"return","fn":"fib","depth":1,"value":"5"}`. Std-prelude calls
  are not traced.
- `machino fuzz file.mno [--runs N] [--seed S]` — generates random
  arguments for every non-generic function whose parameters are scalars or
  arrays of scalars, skips inputs that fail `requires`, and reports any
  input that then violates an `ensures` or traps.
- `machino synth` — emits random, type-checker-verified programs for
  training corpora.
- `machino pkg publish [--registry url] [--token t]` — packs the current
  package and uploads it to a registry over HTTP (client side; running a
  registry server is out of scope).

## Known limits (v0.7)

- Generic structs/enums cannot be instantiated directly (`E066`); generic
  functions cover the common cases.
- `spawn`/`join_*` are interpreter-only; `machino build` rejects them
  (`E072`). Spawned functions must return a scalar (`E071`).
- The WASM-GC backend (`build --gc`) covers scalars, strings, arrays of
  scalars, and all control flow, but not structs, enums, closures, or
  match (`E070`); the default linear-memory backend covers everything.
- SMT verification covers loop-free, call-free int/bool contracts;
  everything else stays runtime-checked.
- Enum variants carry at most one payload value; 255 variants max.
- Structs are limited to 60 fields (`E050`); GC bitmaps are one word.
- Contracts on `extern fn`s are enforced by the interpreter but not in
  compiled WASM.
- The interpreter's call depth defaults to 4096 (`MACHINO_MAX_DEPTH` env
  var overrides); compiled WASM has a 4 MiB shadow stack for pointer
  frames. Compiled-module memory is capped at 1 GiB.
- Reference cycles are reclaimed by the compiled GC but leak in the
  interpreter (they require deliberately circular data).

## Roadmap

- Hosted public registry service
- Threads in compiled WASM (shared-nothing workers)
- Generic structs/enums end-to-end; `where` clauses
- Incremental compilation


