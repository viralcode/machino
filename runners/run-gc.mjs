#!/usr/bin/env node
// Runs a machino .wasm module built with the WASM-GC backend
// (machino build --gc). Requires a runtime with the WebAssembly GC
// proposal (Node 22+, modern browsers).
//
//   node runners/run-gc.mjs program.wasm
//
// Strings are GC byte arrays, opaque to JS; the module exports str_len and
// str_at accessors so the host can decode them for printing.

import { readFile } from "node:fs/promises";

const file = process.argv[2];
if (!file) {
  console.error("usage: node runners/run-gc.mjs <program.wasm>");
  process.exit(2);
}

let exports;

function readStr(ref) {
  const len = exports.str_len(ref);
  const bytes = new Uint8Array(len);
  for (let i = 0; i < len; i++) bytes[i] = exports.str_at(ref, i);
  return new TextDecoder().decode(bytes);
}

const imports = {
  env: {
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
  },
};

const bytes = await readFile(file);
const { instance } = await WebAssembly.instantiate(bytes, imports);
exports = instance.exports;

try {
  instance.exports.main();
} catch (e) {
  if (e instanceof WebAssembly.RuntimeError) {
    console.error("runtime error: trap (contract violation, assert failure, integer overflow, or out-of-bounds)");
    process.exit(1);
  }
  throw e;
}
