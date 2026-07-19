// Synchronous TCP host for Node WASM — mirrors native `tcp_*` externs.
// Net I/O runs on a helper worker thread; the WASM main thread blocks on
// Atomics.wait while the worker services node:net events.

import { Worker } from "node:worker_threads";

const MAX_READ = 65536;

function waitReady(ia) {
  Atomics.wait(ia, 0, 0);
}

export function createTcpHost({ readStr, makeStr, runtimeError }) {
  const worker = new Worker(new URL("./tcp_worker.mjs", import.meta.url), {
    type: "module",
  });
  worker.unref();

  worker.on("error", (e) => {
    console.error(`runtime error: tcp worker: ${e.message}`);
    process.exit(1);
  });

  function rpc(op, payload = {}) {
    const sab = new SharedArrayBuffer(16 + MAX_READ);
    const ia = new Int32Array(sab);
    worker.postMessage({ op, sab, ...payload });
    waitReady(ia);
    const status = Atomics.load(ia, 0);
    if (status === 2) {
      const len = Atomics.load(ia, 1);
      const msg = new TextDecoder().decode(new Uint8Array(sab, 16, len));
      runtimeError(`runtime error: ${msg}`);
    }
    return { ia, sab };
  }

  return {
    tcp_listen(port) {
      const { ia } = rpc("listen", { port: Number(port) });
      return BigInt(Atomics.load(ia, 1));
    },

    tcp_accept(listenerH) {
      const { ia } = rpc("accept", { handle: Number(listenerH) });
      return BigInt(Atomics.load(ia, 1));
    },

    tcp_read(connH) {
      const { ia, sab } = rpc("read", { handle: Number(connH) });
      const len = Atomics.load(ia, 1);
      const bytes = new Uint8Array(sab, 16, len);
      return makeStr(new TextDecoder().decode(bytes));
    },

    tcp_write(connH, dataAddr) {
      const data = readStr(dataAddr);
      const bytes = new TextEncoder().encode(data);
      const { ia } = rpc("write", { handle: Number(connH), bytes });
      return BigInt(Atomics.load(ia, 1));
    },

    tcp_close(handle) {
      rpc("close", { handle: Number(handle) });
    },
  };
}
