# machino native C runtime

This directory holds the **C runtime** linked with code emitted by machino's
LLVM/Clang native backend. The compiler generates C (or LLVM IR) that calls
`mno_*` helpers; you compile and link that output with `machino_rt.c` using
Clang to produce a native executable.

## Build

```sh
clang -pthread -O3 -flto -march=native -o prog prog.c runtime/native/machino_rt.c
```

Link with `-pthread` (on macOS, `clang` also accepts `-pthread` and links
libpthread). Or compile the runtime object once and link it:

```sh
clang -c -std=c11 -pthread -O3 runtime/native/machino_rt.c -o machino_rt.o
clang -pthread -O3 -flto -o prog prog.c machino_rt.o
```

Include the header from generated code:

```c
#include "machino_rt.h"
```

Call `mno_init(argc, argv)` from `main` before running generated machino code.

## Value model

| machino type | C representation |
|--------------|------------------|
| `int`, `bool` | `mno_i64` (`int64_t`) |
| `float` | `mno_f64` (`double`) |
| `str`, arrays, structs, enums | heap pointer stored in `mno_i64` |

Float values stored in generic `mno_i64` slots are bitcast with
`mno_f64_to_bits` / `mno_bits_to_f64`.

## Memory / GC

v1 uses `malloc` for heap objects. Objects are never moved. `mno_arr_push`
returns a **new** array (copy-grow), matching machino's interpreter semantics.

`mno_gc_collect()` is currently a no-op. Reference cycles are not reclaimed;
avoid them or accept leaks until a mark-sweep collector is added.

## Errors

Runtime faults call `mno_fail("runtime error: …")`, print the message to
stderr, and exit with status 1. The last message is also available as
`mno_fail_msg`.

## Host externs

The following mirror `machino run` / WASM host imports (declare with
`extern fn` in machino source):

- `mno_args`, `mno_getenv`, `mno_clock_ms`, `mno_sleep_ms`
- `mno_read_file`, `mno_write_file`, `mno_file_exists`, `mno_read_line`, `mno_exit`
- `mno_http_get` (uses `curl` via `popen`; returns empty string on failure)
- `mno_tcp_listen`, `mno_tcp_accept`, `mno_tcp_read`, `mno_tcp_write`, `mno_tcp_close`
- `mno_closure_*`, `mno_value_clone`, `mno_task_*`, `mno_chan_*` (closures, deep copy,
  pthread tasks, and channels — require `-pthread`)

Requires POSIX (macOS, Linux). No dependencies beyond libc and libpthread.

## Smoke test

```sh
clang -c -std=c11 -pthread runtime/native/machino_rt.c -o /tmp/mno_rt.o
```
