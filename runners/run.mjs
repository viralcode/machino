#!/usr/bin/env node
// Runs a machino-compiled .wasm module in Node (or Deno/Bun with minor tweaks).
//
//   node runners/run.mjs program.wasm
//
// This file is the reference implementation of the machino host interface.
// Any WebAssembly host can run machino programs by providing these imports.

import { readFile } from "node:fs/promises";

const file = process.argv[2];
if (!file) {
  console.error("usage: node runners/run.mjs <program.wasm>");
  process.exit(2);
}

let memory; // set after instantiation

// str/[T] layout in linear memory: [len: i64 little-endian][payload]
function readStr(addr) {
  const a = Number(addr);
  const view = new DataView(memory.buffer);
  const len = Number(view.getBigInt64(a, true));
  return new TextDecoder().decode(new Uint8Array(memory.buffer, a + 8, len));
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
    // built-in extern; declare it in machino as: extern fn clock_ms() -> int
    clock_ms() {
      return BigInt(Date.now());
    },
  },
};

const bytes = await readFile(file);
const { instance } = await WebAssembly.instantiate(bytes, imports);
memory = instance.exports.memory;

try {
  instance.exports.main();
} catch (e) {
  if (e instanceof WebAssembly.RuntimeError) {
    // the fail() import already printed the machino-level message
    process.exit(1);
  }
  throw e;
}
