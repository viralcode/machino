#!/usr/bin/env node
// Runs a machino .wasm module built with the WASM-GC backend
// (machino build --gc). Requires Node 22+ (WebAssembly GC proposal).
//
//   node runners/run-gc.mjs program.wasm [program args...]
//
// Host capabilities match runners/run.mjs (minus TCP). Strings use the
// exported str_len / str_at / make_str / str_set accessors. Spawn args are
// int/bool/float/str: scalars in an i64 array (floats bitcast; strings via
// spawn_pack_str handles resolved in the host before the worker starts).

import { readFileSync, writeFileSync, existsSync, readSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import {
  Worker,
  isMainThread,
  workerData,
} from "node:worker_threads";

const require = createRequire(import.meta.url);

let exports;
let wasmModule;
let nextTask = 1n;
const tasks = new Map();
let nextChan = 1n;
const channels = new Map();
/** Host-side string pool for GC spawn_pack_str → task_spawn. */
let spawnStrPool = isMainThread ? [] : (workerData?.spawnStrPool ?? []);
const programArgs = isMainThread ? process.argv.slice(3) : (workerData?.programArgs ?? []);

function readStr(ref) {
  if (ref == null) return "";
  const len = exports.str_len(ref);
  const bytes = new Uint8Array(len);
  for (let i = 0; i < len; i++) bytes[i] = exports.str_at(ref, i);
  return new TextDecoder().decode(bytes);
}

function makeStr(text) {
  const bytes = new TextEncoder().encode(text ?? "");
  const ref = exports.make_str(BigInt(bytes.length));
  for (let i = 0; i < bytes.length; i++) exports.str_set(ref, i, bytes[i]);
  return ref;
}

function readLineSync() {
  const buf = Buffer.alloc(1);
  let line = "";
  for (;;) {
    let n = 0;
    try {
      n = readSync(0, buf, 0, 1, null);
    } catch (e) {
      if (e.code === "EAGAIN") continue;
      throw e;
    }
    if (n === 0) break;
    const ch = buf.toString("utf8");
    if (ch === "\n") break;
    if (ch !== "\r") line += ch;
  }
  return line;
}

function runtimeError(msg) {
  console.error(msg);
  process.exit(1);
}

function getChan(id) {
  const ch = channels.get(Number(id));
  if (!ch) runtimeError(`runtime error: unknown channel ${id}`);
  return ch;
}

function chanSendEnqueue(ch, val) {
  if (ch.closed) runtimeError("runtime error: send on closed channel");
  ch.q.push(val);
  if (ch.waiters.length) ch.waiters.shift()();
}

function chanRecvWait(ch) {
  for (;;) {
    if (ch.q.length) return ch.q.shift();
    if (ch.closed) runtimeError("runtime error: recv on empty closed channel");
    const sab = new SharedArrayBuffer(4);
    const ia = new Int32Array(sab);
    ch.waiters.push(() => Atomics.notify(ia, 0));
    Atomics.wait(ia, 0, 0, 50);
  }
}

function takeTask(h) {
  const key = Number(h);
  const sab = tasks.get(key);
  if (!sab) runtimeError(`runtime error: join on unknown or already-joined task ${key}`);
  tasks.delete(key);
  const status = new Int32Array(sab, 0, 1);
  Atomics.wait(status, 0, 0);
  if (Atomics.load(status, 0) === 2) {
    const len = new DataView(sab).getInt32(4, true);
    const msg = new TextDecoder().decode(new Uint8Array(sab.slice(8, 8 + len)));
    runtimeError(msg || "runtime error: task failed");
  }
  return sab;
}

function makeImports() {
  return {
    env: {
      fail(ref) {
        const msg = readStr(ref);
        console.error(msg);
        throw new Error(msg);
      },
      print_i64(v) {
        console.log(v.toString());
      },
      print_f64(v) {
        console.log(Number.isInteger(v) && Number.isFinite(v) ? v.toFixed(1) : String(v));
      },
      print_bool(v) {
        console.log(v === 0 ? "false" : "true");
      },
      print_str(ref) {
        console.log(readStr(ref));
      },
      chan_new() {
        const id = nextChan++;
        channels.set(Number(id), { q: [], closed: false, waiters: [] });
        return id;
      },
      chan_close(id) {
        const ch = getChan(id);
        ch.closed = true;
        while (ch.waiters.length) ch.waiters.shift()();
      },
      chan_send_i64(id, val) {
        chanSendEnqueue(getChan(id), { kind: "i64", v: val });
      },
      chan_send_f64(id, val) {
        chanSendEnqueue(getChan(id), { kind: "f64", v: val });
      },
      chan_send_str(id, ref) {
        chanSendEnqueue(getChan(id), { kind: "str", text: readStr(ref) });
      },
      chan_recv_i64(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "i64") {
          runtimeError("runtime error: chan_recv_i64: channel delivered a value of a different type");
        }
        return v.v;
      },
      chan_recv_f64(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "f64") {
          runtimeError("runtime error: chan_recv_f64: channel delivered a value of a different type");
        }
        return v.v;
      },
      chan_recv_str(id) {
        const v = chanRecvWait(getChan(id));
        if (v.kind !== "str") {
          runtimeError("runtime error: chan_recv_str: channel delivered a value of a different type");
        }
        return makeStr(v.text);
      },
      spawn_pack_str(strRef) {
        const id = spawnStrPool.length;
        spawnStrPool.push(readStr(strRef));
        return BigInt(id);
      },
      task_spawn(nameRef, sigRef, argvRef) {
        const fnName = readStr(nameRef);
        const sig = readStr(sigRef);
        const paramSig = (sig.split(":")[0] || "");
        const n = exports.i64arr_len(argvRef);
        const args = [];
        const poolSnapshot = spawnStrPool.slice();
        spawnStrPool.length = 0;
        for (let i = 0; i < n; i++) {
          const raw = exports.i64arr_get(argvRef, i);
          const c = paramSig[i] || "i";
          if (c === "f") {
            const buf = new ArrayBuffer(8);
            new DataView(buf).setBigInt64(0, BigInt(raw), true);
            args.push({ kind: "f", v: new DataView(buf).getFloat64(0, true) });
          } else if (c === "s") {
            args.push({ kind: "s", text: poolSnapshot[Number(raw)] ?? "" });
          } else {
            args.push({ kind: "i", v: BigInt(raw) });
          }
        }
        const h = nextTask++;
        const sab = new SharedArrayBuffer(8 + 65536);
        tasks.set(Number(h), sab);
        const worker = new Worker(new URL(import.meta.url), {
          workerData: {
            module: wasmModule,
            fnName,
            sig,
            args,
            sab,
            programArgs,
            spawnStrPool: [],
          },
        });
        worker.on("error", (e) => {
          console.error(String(e));
          process.exit(1);
        });
        return h;
      },
      task_join_i64(h) {
        return new DataView(takeTask(h)).getBigInt64(8, true);
      },
      task_join_f64(h) {
        return new DataView(takeTask(h)).getFloat64(8, true);
      },
      task_join_str(h) {
        const sab = takeTask(h);
        const len = new DataView(sab).getInt32(8, true);
        const bytes = new Uint8Array(sab, 12, len);
        return makeStr(new TextDecoder().decode(bytes));
      },
      clock_ms() {
        return BigInt(Date.now());
      },
      sleep_ms(ms) {
        const shared = new Int32Array(new SharedArrayBuffer(4));
        Atomics.wait(shared, 0, 0, Number(ms));
      },
      read_file(pathRef) {
        return makeStr(readFileSync(readStr(pathRef), "utf8"));
      },
      write_file(pathRef, dataRef) {
        try {
          writeFileSync(readStr(pathRef), readStr(dataRef));
          return 1n;
        } catch {
          return 0n;
        }
      },
      file_exists(pathRef) {
        return existsSync(readStr(pathRef)) ? 1n : 0n;
      },
      read_line() {
        return makeStr(readLineSync());
      },
      getenv(nameRef) {
        return makeStr(process.env[readStr(nameRef)] ?? "");
      },
      args() {
        runtimeError("runtime error: args() is not supported by the GC host; use machino run");
      },
      exit(code) {
        process.exit(Number(code));
      },
      http_get(urlRef) {
        const url = readStr(urlRef);
        try {
          const { execFileSync } = require("node:child_process");
          return makeStr(execFileSync("curl", ["-fsSL", url], { encoding: "utf8" }));
        } catch (e) {
          return makeStr(String(e));
        }
      },
    },
  };
}

async function workerMain() {
  const { module, fnName, sig, args, sab } = workerData;
  const status = new Int32Array(sab, 0, 1);
  const post = (code, len) => {
    new DataView(sab).setInt32(4, len ?? 0, true);
    Atomics.store(status, 0, code);
    Atomics.notify(status, 0);
  };
  try {
    const instance = new WebAssembly.Instance(module, makeImports());
    exports = instance.exports;
    const fn = instance.exports[fnName];
    if (typeof fn !== "function") {
      throw new Error(`runtime error: spawn target '${fnName}' is not exported`);
    }
    const conv = args.map((a) => {
      if (a && typeof a === "object") {
        if (a.kind === "f") return a.v;
        if (a.kind === "s") return makeStr(a.text);
        return a.v;
      }
      return a;
    });
    const result = fn(...conv);
    const ret = sig.split(":")[1] || "i";
    const dv = new DataView(sab);
    if (ret === "f") {
      dv.setFloat64(8, Number(result), true);
    } else if (ret === "s") {
      const text = readStr(result);
      const bytes = new TextEncoder().encode(text);
      dv.setInt32(8, bytes.length, true);
      new Uint8Array(sab, 12, bytes.length).set(bytes);
    } else {
      dv.setBigInt64(8, BigInt(result ?? 0), true);
    }
    post(1, 0);
  } catch (e) {
    const msg = String(e?.message || e);
    const bytes = new TextEncoder().encode(msg);
    new Uint8Array(sab, 8, Math.min(bytes.length, 65520)).set(bytes.subarray(0, 65520));
    post(2, bytes.length);
  }
}

if (!isMainThread) {
  await workerMain();
} else {
  const file = process.argv[2];
  if (!file) {
    console.error("usage: node runners/run-gc.mjs <program.wasm> [args...]");
    process.exit(2);
  }
  const bytes = await readFile(file);
  wasmModule = await WebAssembly.compile(bytes);
  const instance = new WebAssembly.Instance(wasmModule, makeImports());
  exports = instance.exports;
  try {
    instance.exports.main();
  } catch (e) {
    if (e instanceof WebAssembly.RuntimeError) {
      console.error(
        "runtime error: trap (contract violation, assert failure, integer overflow, or out-of-bounds)"
      );
      process.exit(1);
    }
    if (e instanceof Error && String(e.message).startsWith("runtime error:")) {
      process.exit(1);
    }
    throw e;
  }
}
