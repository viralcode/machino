#!/usr/bin/env node
// Runs a machino-compiled .wasm module in Node.
//
//   node runners/run.mjs program.wasm [program args...]
//
// This file is the reference implementation of the machino host interface.
// Any WebAssembly host can run machino programs by providing these imports.
// Strings cross the boundary through linear memory ([len: i64][bytes]); the
// module exports `alloc` so the host can allocate blocks to return.

import { readFileSync, writeFileSync, existsSync, readSync } from "node:fs";
import { readFile } from "node:fs/promises";

const file = process.argv[2];
if (!file) {
  console.error("usage: node runners/run.mjs <program.wasm> [args...]");
  process.exit(2);
}
const programArgs = process.argv.slice(3);

let memory; // set after instantiation
let alloc; // exported allocator

function readStr(addr) {
  const a = Number(addr);
  const view = new DataView(memory.buffer);
  const len = Number(view.getBigInt64(a, true));
  return new TextDecoder().decode(new Uint8Array(memory.buffer, a + 8, len));
}

function makeStr(text) {
  const bytes = new TextEncoder().encode(text);
  return makeBytes(bytes);
}

function makeBytes(bytes) {
  const addr = Number(alloc(BigInt(8 + bytes.length)));
  const view = new DataView(memory.buffer);
  view.setBigInt64(addr, BigInt(bytes.length), true);
  new Uint8Array(memory.buffer, addr + 8, bytes.length).set(bytes);
  return BigInt(addr);
}

function makeStrArray(items) {
  const ptrs = items.map((s) => makeStr(s));
  const addr = Number(alloc(BigInt(8 + 8 * ptrs.length)));
  const view = new DataView(memory.buffer);
  view.setBigInt64(addr, BigInt(ptrs.length), true);
  for (let i = 0; i < ptrs.length; i++) {
    view.setBigInt64(addr + 8 + 8 * i, ptrs[i], true);
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

const imports = {
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
        const len = Number(view.getBigInt64(a, true));
        writeFileSync(readStr(pathAddr), new Uint8Array(memory.buffer, a + 8, len));
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

const bytes = await readFile(file);
const { instance } = await WebAssembly.instantiate(bytes, imports);
memory = instance.exports.memory;
alloc = instance.exports.alloc;

try {
  instance.exports.main();
} catch (e) {
  if (e instanceof WebAssembly.RuntimeError) {
    // the fail() import already printed the machino-level message
    process.exit(1);
  }
  throw e;
}
