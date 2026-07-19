// Helper worker for synchronous TCP imports (see tcp_host.mjs).

import { parentPort } from "node:worker_threads";
import net from "node:net";

const MAX_READ = 65536;
const listeners = new Map();
const conns = new Map();
let nextHandle = 1;

function allocHandle() {
  return nextHandle++;
}

function finish(ia, sab, status, a = 0, b = 0, bytes = null) {
  Atomics.store(ia, 0, status);
  Atomics.store(ia, 1, a);
  if (bytes !== null) {
    const n = Math.min(bytes.length, MAX_READ);
    new Uint8Array(sab, 16, n).set(bytes.subarray(0, n));
    Atomics.store(ia, 1, n);
  } else {
    Atomics.store(ia, 2, b);
  }
  Atomics.notify(ia, 0);
}

function fail(ia, sab, msg) {
  const bytes = new TextEncoder().encode(msg);
  const n = Math.min(bytes.length, MAX_READ);
  new Uint8Array(sab, 16, n).set(bytes.subarray(0, n));
  finish(ia, sab, 2, n);
}

parentPort.on("message", ({ op, sab, port, handle, bytes }) => {
  const ia = new Int32Array(sab);
  try {
    if (op === "listen") {
      const server = net.createServer();
      server.once("error", (e) => fail(ia, sab, `tcp_listen: cannot bind port ${port}: ${e.message}`));
      server.listen(port, "0.0.0.0", () => {
        const h = allocHandle();
        listeners.set(h, server);
        finish(ia, sab, 1, h);
      });
      return;
    }
    if (op === "accept") {
      const server = listeners.get(handle);
      if (!server) {
        fail(ia, sab, `tcp_accept: invalid listener handle ${handle}`);
        return;
      }
      const onConn = (socket) => {
        server.removeListener("connection", onConn);
        server.removeListener("error", onErr);
        const h = allocHandle();
        conns.set(h, socket);
        finish(ia, sab, 1, h);
      };
      const onErr = (e) => {
        server.removeListener("connection", onConn);
        server.removeListener("error", onErr);
        fail(ia, sab, `tcp_accept: ${e.message}`);
      };
      server.once("connection", onConn);
      server.once("error", onErr);
      return;
    }
    if (op === "read") {
      const socket = conns.get(handle);
      if (!socket) {
        fail(ia, sab, `tcp_read: invalid connection handle ${handle}`);
        return;
      }
      const done = (chunk) => {
        socket.removeListener("data", onData);
        socket.removeListener("end", onEnd);
        socket.removeListener("error", onErr);
        const buf = chunk ? chunk.subarray(0, MAX_READ) : Buffer.alloc(0);
        finish(ia, sab, 1, 0, 0, buf);
      };
      const onData = (data) => done(data);
      const onEnd = () => done(null);
      const onErr = (e) => {
        socket.removeListener("data", onData);
        socket.removeListener("end", onEnd);
        socket.removeListener("error", onErr);
        fail(ia, sab, `tcp_read: ${e.message}`);
      };
      if (socket.readableLength > 0) {
        done(socket.read(Math.min(socket.readableLength, MAX_READ)));
        return;
      }
      socket.once("data", onData);
      socket.once("end", onEnd);
      socket.once("error", onErr);
      return;
    }
    if (op === "write") {
      const socket = conns.get(handle);
      if (!socket) {
        fail(ia, sab, `tcp_write: invalid connection handle ${handle}`);
        return;
      }
      const buf = Buffer.from(bytes);
      const finishOk = () => finish(ia, sab, 1, buf.length);
      const ok = socket.write(buf, (err) => {
        if (err) fail(ia, sab, `tcp_write: ${err.message}`);
      });
      if (ok) finishOk();
      else socket.once("drain", finishOk);
      socket.once("error", (e) => fail(ia, sab, `tcp_write: ${e.message}`));
      return;
    }
    if (op === "close") {
      const socket = conns.get(handle);
      if (socket) {
        socket.destroy();
        conns.delete(handle);
        finish(ia, sab, 1);
        return;
      }
      const server = listeners.get(handle);
      if (server) {
        server.close(() => finish(ia, sab, 1));
        listeners.delete(handle);
        return;
      }
      finish(ia, sab, 1);
      return;
    }
    fail(ia, sab, `tcp worker: unknown op '${op}'`);
  } catch (e) {
    fail(ia, sab, String(e?.message ?? e));
  }
});
