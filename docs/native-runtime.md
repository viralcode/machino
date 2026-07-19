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

Contracts, asserts, bounds checks, and overflow traps are enforced the same way
as in compiled WASM.

## Relation to WASM hosts

| Path | Use when |
|------|----------|
| `machino run` | Native OS access (TCP, files, env) — **this runtime** |
| `machino build` + `runners/run.mjs` | Portable linear WASM (Node); no TCP |
| `machino build --gc` + `runners/run-gc.mjs` | WASM-GC host (Node 22+); spawn args int/bool |
| `machino build --native` | AOT the linear `.wasm` with `wasmtime compile` (requires wasmtime on PATH) |

The WASM import surface mirrors the table above except TCP (async in Node). A
custom host can implement any subset of those import names.
