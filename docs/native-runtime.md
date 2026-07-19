# Native OS runtime (`machino run`)

`machino run` is the **official native OS runtime** for machino — not a
dev-only interpreter. It executes type-checked programs directly on the host
with full capability access. Compiled WASM is for portable deployment; native
run is for servers, CLIs, and anything that needs sockets or the filesystem
without a separate host glue layer.

```sh
machino run examples/http_server.mno 8080
machino run test/weather_api.mno 8092
```

## Capabilities

Declare what you need with `extern fn`; undeclared host powers are unreachable.

| Extern | Role |
|--------|------|
| `args() -> [str]` | CLI arguments after the script path |
| `getenv(name: str) -> str` | Environment variable (empty if unset) |
| `clock_ms() -> int` | Unix epoch milliseconds |
| `sleep_ms(ms: int)` | Block the current task |
| `read_file` / `write_file` / `file_exists` | Filesystem |
| `read_line() -> str` | One line from stdin |
| `exit(code: int)` | Terminate the process |
| `http_get(url: str) -> str` | Blocking HTTP GET body |
| `tcp_listen` / `tcp_accept` / `tcp_read` / `tcp_write` / `tcp_close` | TCP sockets |
| `dom_*` | Virtual DOM + events (`dom_add_listener` / `dom_dispatch` / `dom_dispatch_event`, `dom_last_event_*`, layout/dataset helpers). Browser: `runners/run.html` + `dom_host.mjs` |
| `db_open` / `db_close` / `db_exec` / `db_query` | DB drivers: `memory`, `sqlite`, `mysql`, `postgres`, `mongo` (CLI-backed). See `packages/db` |

Contracts, asserts, bounds checks, and overflow traps are enforced the same way
as in compiled WASM.

## Relation to WASM hosts

| Path | Use when |
|------|----------|
| `machino run` | Native OS access (TCP, files, env) — **this runtime** |
| `machino build` + `runners/run.mjs` | Portable linear WASM (Node); TCP via `tcp_host.mjs` |
| `machino build --gc` + `runners/run-gc.mjs` | WASM-GC host (Node 22+); spawn args int/bool/float/str |
| `machino build --native` | **Clang/LLVM host executable** — emits C, links `runtime/native/machino_rt.c`, produces a real native binary (also writes `.ll` LLVM IR beside the build) |

`machino build --native` requires `clang` on `PATH` (or `MACHINO_CC`). The
binary includes the host externs (files, TCP, env, …) from the C runtime — no
WebAssembly host needed. Current limits: no lambdas / first-class function
values, and no `spawn`/channels yet (use `machino run` or WASM for those).

The WASM import surface mirrors the table above (Node TCP via `tcp_host.mjs`).
Browsers have no raw TCP — use HTTP or `packages/ws`.
