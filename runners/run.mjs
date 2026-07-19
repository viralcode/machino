#!/usr/bin/env node
// Runs a machino-compiled .wasm module in Node.
//
//   node runners/run.mjs program.wasm [program args...]
//
// This file is the reference implementation of the machino host interface.
// Any WebAssembly host can run machino programs by providing these imports.
// Every heap object has a 16-byte header: [meta: i64][word1: i64][payload].
// meta = tag (bits 0-2) | mark (bit 3) | count << 4. Strings are tag 0
// (count = byte length), arrays of scalars tag 1, arrays of pointers tag 2,
// structs/closures tag 3 (word1 = inline pointer bitmap), big structs tag 4
// (word1 = address of a static multi-word bitmap in the data segment).
// The module exports `alloc` and `heap_base`; hosts must write the header on
// objects they create.
//
// Concurrency: `spawn`/`join_*` compile to the task_spawn/task_join_* imports.
// This host deep-copies the argument object graph out of the spawning
// instance, runs the named exported function in a fresh instance of the same
// module on a worker thread, and ships the scalar/str result back through a
// SharedArrayBuffer.

import { readFileSync, writeFileSync, existsSync, readSync } from "node:fs";
import { readFile } from "node:fs/promises";
import {
  Worker,
  isMainThread,
  workerData,
} from "node:worker_threads";

let memory; // set after instantiation
let alloc; // exported allocator
let heapBase = Infinity; // exported heap base (static data lives below it)

function readStr(addr) {
  const a = Number(addr);
  const view = new DataView(memory.buffer);
  const len = Number(view.getBigInt64(a, true) >> 4n);
  return new TextDecoder().decode(new Uint8Array(memory.buffer, a + 16, len));
}

function makeStr(text) {
  const bytes = new TextEncoder().encode(text);
  return makeBytes(bytes);
}

function makeBytes(bytes) {
  const addr = Number(alloc(BigInt(16 + bytes.length)));
  const view = new DataView(memory.buffer);
  view.setBigInt64(addr, BigInt(bytes.length) << 4n, true); // tag 0 = bytes
  view.setBigInt64(addr + 8, 0n, true);
  new Uint8Array(memory.buffer, addr + 16, bytes.length).set(bytes);
  return BigInt(addr);
}

function makeStrArray(items) {
  const ptrs = items.map((s) => makeStr(s));
  const addr = Number(alloc(BigInt(16 + 8 * ptrs.length)));
  const view = new DataView(memory.buffer);
  view.setBigInt64(addr, (BigInt(ptrs.length) << 4n) | 2n, true); // tag 2 = ptr array
  view.setBigInt64(addr + 8, 0n, true);
  for (let i = 0; i < ptrs.length; i++) {
    view.setBigInt64(addr + 16 + 8 * i, ptrs[i], true);
  }
  return BigInt(addr);
}

function readLineSync() {
  const buf = Buffer.alloc(1);
  let line = "";
  for (;;) {
    let n = 0;
    try {
      n = readSync(0, buf, 0, 1, null);
    } catch (e) {
      if (e.code === "EAGAIN") continue; // non-blocking stdin; retry
      throw e;
    }
    if (n === 0) break; // EOF
    const ch = buf.toString("utf8");
    if (ch === "\n") break;
    if (ch !== "\r") line += ch;
  }
  return line;
}

// ---- task argument marshalling (shared-nothing deep copy) ----

// Serializes the object graph rooted at addr into plain structured-clonable
// data. Static objects (interned strings, singletons, below heap_base) keep
// their address: the worker runs the same module, so its data segment is
// identical.
function serializeGraph(addr, seen) {
  if (addr === 0n) return { static: 0n };
  const a = Number(addr);
  if (a < heapBase) return { static: addr };
  if (seen.has(a)) return { ref: seen.get(a) };
  const id = seen.size;
  seen.set(a, id);
  const view = new DataView(memory.buffer);
  const meta = view.getBigInt64(a, true);
  const w1 = view.getBigInt64(a + 8, true);
  const tag = Number(meta & 7n);
  const count = Number(meta >> 4n);
  const node = { id, meta, w1, kids: [] };
  if (tag === 0) {
    node.bytes = new Uint8Array(memory.buffer.slice(a + 16, a + 16 + count));
    return node;
  }
  const words = new BigInt64Array(count);
  for (let i = 0; i < count; i++) {
    words[i] = view.getBigInt64(a + 16 + 8 * i, true);
  }
  const isPtr = (i) => {
    if (tag === 2) return true; // array of pointers
    if (tag === 3) return ((w1 >> BigInt(i)) & 1n) === 1n; // inline bitmap
    if (tag === 4) {
      // static multi-word bitmap at address w1
      const word = view.getBigInt64(Number(w1) + 8 * Math.floor(i / 64), true);
      return ((word >> BigInt(i % 64)) & 1n) === 1n;
    }
    return false;
  };
  for (let i = 0; i < count; i++) {
    if (isPtr(i) && words[i] !== 0n) {
      node.kids.push([i, serializeGraph(words[i], seen)]);
      words[i] = 0n; // patched during reconstruction
    }
  }
  node.words = words;
  return node;
}

// Rebuilds a serialized graph in *this* instance's memory via alloc.
// Host-driven allocations never collect, so partially built objects are safe.
function reconstructGraph(node, byId) {
  if ("static" in node) return node.static;
  if ("ref" in node) return byId.get(node.ref);
  const tag = Number(node.meta & 7n);
  const count = Number(node.meta >> 4n);
  const size = tag === 0 ? 16 + Math.ceil(count / 8) * 8 : 16 + 8 * count;
  const addr = Number(alloc(BigInt(size)));
  byId.set(node.id, BigInt(addr));
  let view = new DataView(memory.buffer);
  view.setBigInt64(addr, node.meta, true);
  view.setBigInt64(addr + 8, node.w1, true);
  if (tag === 0) {
    new Uint8Array(memory.buffer, addr + 16, node.bytes.length).set(node.bytes);
    return BigInt(addr);
  }
  for (let i = 0; i < count; i++) {
    view.setBigInt64(addr + 16 + 8 * i, node.words[i], true);
  }
  for (const [i, kid] of node.kids) {
    const kaddr = reconstructGraph(kid, byId);
    // alloc may have grown memory; take a fresh view
    new DataView(memory.buffer).setBigInt64(addr + 16 + 8 * i, kaddr, true);
  }
  return BigInt(addr);
}

// ---- host imports ----

const tasks = new Map();
let nextTask = 1;
let wasmModule; // compiled module, shared with workers

// ---- channels (main-thread host queues) ----

const channels = new Map();
let nextChan = 1;

function runtimeError(msg) {
  console.error(msg);
  process.exit(1);
}

function getChan(id) {
  const ch = channels.get(Number(id));
  if (!ch) {
    runtimeError(`runtime error: no channel with handle ${id}`);
  }
  return ch;
}

function makeChannel() {
  return {
    queue: [],
    closed: false,
    signal: new Int32Array(new SharedArrayBuffer(4)),
  };
}

function chanSendEnqueue(ch, val) {
  if (ch.closed) {
    runtimeError("runtime error: send on closed channel");
  }
  ch.queue.push(val);
  Atomics.add(ch.signal, 0, 1);
  Atomics.notify(ch.signal, 0);
}

function chanRecvWait(ch) {
  for (;;) {
    if (ch.queue.length > 0) {
      return ch.queue.shift();
    }
    if (ch.closed) {
      runtimeError("runtime error: receive on closed empty channel");
    }
    const epoch = Atomics.load(ch.signal, 0);
    Atomics.wait(ch.signal, 0, epoch);
  }
}

function copyStrBytes(addr) {
  const a = Number(addr);
  const view = new DataView(memory.buffer);
  const len = Number(view.getBigInt64(a, true) >> 4n);
  return new Uint8Array(memory.buffer.slice(a + 16, a + 16 + len));
}

function makeImports(programArgs) {
  return {
    env: {
      // called just before the module traps on a contract/assert/bounds failure
      fail(msgAddr) {
        console.error(readStr(msgAddr));
      },
      print_i64(v) {
        console.log(v.toString());
      },
      print_f64(v) {
        console.log(Number.isInteger(v) && Number.isFinite(v) ? v.toFixed(1) : String(v));
      },
      print_bool(v) {
        console.log(v === 0n ? "false" : "true");
      },
      print_str(addr) {
        console.log(readStr(addr));
      },
      // ---- tasks (spawn/join) ----
      task_spawn(nameAddr, sigAddr, argvAddr) {
        const fnName = readStr(nameAddr);
        const sig = readStr(sigAddr);
        const paramSig = sig.split(":")[0];
        const view = new DataView(memory.buffer);
        const base = Number(argvAddr) + 16;
        const args = [];
        for (let i = 0; i < paramSig.length; i++) {
          const c = paramSig[i];
          if (c === "f") {
            args.push({ kind: "f", v: view.getFloat64(base + 8 * i, true) });
          } else if (c === "i" || c === "b") {
            args.push({ kind: "i", v: view.getBigInt64(base + 8 * i, true) });
          } else {
            const ptr = view.getBigInt64(base + 8 * i, true);
            args.push({ kind: "p", node: serializeGraph(ptr, new Map()) });
          }
        }
        const sab = new SharedArrayBuffer(4096, { maxByteLength: 1 << 28 });
        const worker = new Worker(new URL(import.meta.url), {
          workerData: { module: wasmModule, fnName, sig, args, sab },
        });
        worker.unref();
        const h = nextTask++;
        tasks.set(h, sab);
        return BigInt(h);
      },
      task_join_i64(h) {
        const sab = takeTask(h);
        return new DataView(sab).getBigInt64(8, true);
      },
      task_join_f64(h) {
        const sab = takeTask(h);
        return new DataView(sab).getFloat64(8, true);
      },
      task_join_str(h) {
        const sab = takeTask(h);
        const len = new DataView(sab).getInt32(4, true);
        const bytes = new Uint8Array(sab.slice(8, 8 + len));
        return makeBytes(bytes);
      },
      // ---- channels (blocking queues on the main host thread) ----
      chan_new() {
        const id = nextChan++;
        channels.set(id, makeChannel());
        return BigInt(id);
      },
      chan_close(id) {
        const ch = getChan(id);
        if (ch.closed) return;
        ch.closed = true;
        Atomics.add(ch.signal, 0, 1);
        Atomics.notify(ch.signal, 0);
      },
      chan_send_i64(id, val) {
        chanSendEnqueue(getChan(id), { kind: "i64", v: val });
      },
      chan_send_f64(id, val) {
        chanSendEnqueue(getChan(id), { kind: "f64", v: val });
      },
      chan_send_str(id, ptr) {
        chanSendEnqueue(getChan(id), { kind: "str", bytes: copyStrBytes(ptr) });
      },
      chan_recv_i64(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "i64") {
          runtimeError(
            "runtime error: chan_recv_i64: channel delivered a value of a different type"
          );
        }
        return v.v;
      },
      chan_recv_f64(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "f64") {
          runtimeError(
            "runtime error: chan_recv_f64: channel delivered a value of a different type"
          );
        }
        return v.v;
      },
      chan_recv_str(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "str") {
          runtimeError(
            "runtime error: chan_recv_str: channel delivered a value of a different type"
          );
        }
        return makeBytes(v.bytes);
      },
      // ---- native externs (declare the ones you need with `extern fn`) ----
      clock_ms() {
        return BigInt(Date.now());
      },
      sleep_ms(ms) {
        const shared = new Int32Array(new SharedArrayBuffer(4));
        Atomics.wait(shared, 0, 0, Number(ms));
      },
      read_file(pathAddr) {
        return makeBytes(readFileSync(readStr(pathAddr)));
      },
      write_file(pathAddr, dataAddr) {
        try {
          const a = Number(dataAddr);
          const view = new DataView(memory.buffer);
          const len = Number(view.getBigInt64(a, true) >> 4n);
          writeFileSync(readStr(pathAddr), new Uint8Array(memory.buffer, a + 16, len));
          return 1n;
        } catch {
          return 0n;
        }
      },
      file_exists(pathAddr) {
        return existsSync(readStr(pathAddr)) ? 1n : 0n;
      },
      read_line() {
        return makeStr(readLineSync());
      },
      getenv(nameAddr) {
        return makeStr(process.env[readStr(nameAddr)] ?? "");
      },
      args() {
        return makeStrArray(programArgs);
      },
      exit(code) {
        process.exit(Number(code));
      },
      // TCP sockets are provided by machino's native runtime (`machino run`).
      // Node's socket API is asynchronous, so this synchronous WASM host does
      // not implement them; provide your own host (or WASI sockets) if needed.
    },
  };
}

function takeTask(h) {
  const key = Number(h);
  const sab = tasks.get(key);
  if (!sab) {
    console.error(`runtime error: join on unknown or already-joined task ${key}`);
    process.exit(1);
  }
  tasks.delete(key);
  const status = new Int32Array(sab, 0, 1);
  Atomics.wait(status, 0, 0); // block until the worker posts a result
  if (Atomics.load(status, 0) === 2) {
    const len = new DataView(sab).getInt32(4, true);
    const msg = new TextDecoder().decode(new Uint8Array(sab.slice(8, 8 + len)));
    console.error(msg || "runtime error: task failed");
    process.exit(1);
  }
  return sab;
}

function setInstance(instance) {
  memory = instance.exports.memory;
  alloc = instance.exports.alloc;
  if (instance.exports.heap_base !== undefined) {
    heapBase = Number(instance.exports.heap_base.value);
  }
}

// ---- worker entry: run one task in a fresh instance ----

async function workerMain() {
  const { module, fnName, sig, args, sab } = workerData;
  const status = new Int32Array(sab, 0, 1);
  const post = (code, len) => {
    new DataView(sab).setInt32(4, len ?? 0, true);
    Atomics.store(status, 0, code);
    Atomics.notify(status, 0);
  };
  try {
    wasmModule = module;
    const instance = await WebAssembly.instantiate(module, makeImports([]));
    setInstance(instance);
    const conv = args.map((a) =>
      a.kind === "p" ? reconstructGraph(a.node, new Map()) : a.v
    );
    const result = instance.exports[fnName](...conv);
    const retc = sig.split(":")[1];
    const view = new DataView(sab);
    if (retc === "f") {
      view.setFloat64(8, result, true);
      post(1, 8);
    } else if (retc === "s") {
      const a = Number(result);
      const mv = new DataView(memory.buffer);
      const len = Number(mv.getBigInt64(a, true) >> 4n);
      if (8 + len > sab.byteLength) sab.grow(8 + len);
      new Uint8Array(sab, 8, len).set(new Uint8Array(memory.buffer, a + 16, len));
      post(1, len);
    } else {
      view.setBigInt64(8, BigInt(result), true);
      post(1, 8);
    }
  } catch (e) {
    const msg = new TextEncoder().encode(
      e instanceof WebAssembly.RuntimeError
        ? "runtime error: task trapped (contract or assertion failed)"
        : String(e?.message ?? e)
    );
    if (8 + msg.length > sab.byteLength) sab.grow(8 + msg.length);
    new Uint8Array(sab, 8, msg.length).set(msg);
    post(2, msg.length);
  }
}

// ---- main entry ----

async function main() {
  const file = process.argv[2];
  if (!file) {
    console.error("usage: node runners/run.mjs <program.wasm> [args...]");
    process.exit(2);
  }
  const programArgs = process.argv.slice(3);
  const bytes = await readFile(file);
  wasmModule = await WebAssembly.compile(bytes);
  const instance = await WebAssembly.instantiate(wasmModule, makeImports(programArgs));
  setInstance(instance);

  try {
    instance.exports.main();
  } catch (e) {
    if (e instanceof WebAssembly.RuntimeError) {
      // the fail() import already printed the machino-level message
      process.exit(1);
    }
    throw e;
  }
}

if (isMainThread) {
  await main();
} else {
  await workerMain();
}
