// Database host for Node WASM — mirrors native CLI/memory drivers.
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";

function which(bin) {
  const r = spawnSync("sh", ["-c", `command -v ${bin}`], { encoding: "utf8" });
  return r.status === 0;
}

function jerr(msg) {
  return JSON.stringify({ ok: false, error: msg });
}

export function createDbHost({ readStr, makeStr }) {
  const conns = new Map();
  let next = 1;

  function open(driver, conn) {
    if (driver === "memory") {
      return { kind: "memory", docs: new Map() };
    }
    if (driver === "sqlite") {
      if (!which("sqlite3")) throw new Error("sqlite3 CLI not found on PATH");
      const path = conn.replace(/^sqlite:\/\//, "");
      return { kind: "sqlite", path };
    }
    if (driver === "mysql") {
      if (!which("mysql")) throw new Error("mysql CLI not found on PATH");
      return { kind: "mysql", url: conn };
    }
    if (driver === "postgres" || driver === "pg") {
      if (!which("psql")) throw new Error("psql CLI not found on PATH");
      return { kind: "postgres", url: conn };
    }
    if (driver === "mongo" || driver === "mongodb") {
      if (!which("mongosh")) throw new Error("mongosh CLI not found on PATH");
      return { kind: "mongo", url: conn };
    }
    throw new Error(`unknown db driver '${driver}'`);
  }

  function memoryExec(c, sql) {
    const s = sql.trim();
    if (s.startsWith("PUT ")) {
      const rest = s.slice(4);
      const i = rest.indexOf(" ");
      const coll = i < 0 ? rest : rest.slice(0, i);
      const doc = i < 0 ? "{}" : rest.slice(i + 1);
      if (!c.docs.has(coll)) c.docs.set(coll, []);
      c.docs.get(coll).push(doc);
      return JSON.stringify({ ok: true, exec: true });
    }
    if (s.startsWith("CLEAR ")) {
      c.docs.set(s.slice(6).trim(), []);
      return JSON.stringify({ ok: true, exec: true });
    }
    return jerr("memory exec expects PUT/CLEAR");
  }

  function memoryQuery(c, sql) {
    const s = sql.trim();
    if (s.startsWith("GET ")) {
      const coll = s.slice(4).trim();
      const rows = c.docs.get(coll) || [];
      return JSON.stringify({ ok: true, rows: rows.map((r) => JSON.parse(r)) });
    }
    if (s.startsWith("COUNT ")) {
      const coll = s.slice(6).trim();
      return JSON.stringify({ ok: true, count: (c.docs.get(coll) || []).length });
    }
    return jerr("memory query expects GET/COUNT");
  }

  function runSqlCli(c, sql, asQuery) {
    if (c.kind === "sqlite") {
      const r = spawnSync("sqlite3", ["-json", c.path, sql], { encoding: "utf8" });
      if (r.status !== 0) return jerr(r.stderr || "sqlite3 failed");
      const s = (r.stdout || "").trim();
      if (!asQuery) return JSON.stringify({ ok: true, exec: true });
      return JSON.stringify({ ok: true, rows: s ? JSON.parse(s) : [] });
    }
    return jerr(`CLI driver ${c.kind} via WASM host: use machino run for full URL parsing`);
  }

  return {
    db_open(driverAddr, connAddr) {
      try {
        const c = open(readStr(driverAddr), readStr(connAddr));
        const h = BigInt(next++);
        conns.set(Number(h), c);
        return h;
      } catch (e) {
        throw new WebAssembly.RuntimeError(`db_open: ${e.message || e}`);
      }
    },
    db_close(h) {
      conns.delete(Number(h));
    },
    db_exec(h, sqlAddr) {
      const c = conns.get(Number(h));
      if (!c) return makeStr(jerr("invalid db handle"));
      const sql = readStr(sqlAddr);
      if (c.kind === "memory") return makeStr(memoryExec(c, sql));
      return makeStr(runSqlCli(c, sql, false));
    },
    db_query(h, sqlAddr) {
      const c = conns.get(Number(h));
      if (!c) return makeStr(jerr("invalid db handle"));
      const sql = readStr(sqlAddr);
      if (c.kind === "memory") return makeStr(memoryQuery(c, sql));
      if (c.kind === "mongo") {
        const r = spawnSync("mongosh", ["--quiet", c.url, "--eval", sql], {
          encoding: "utf8",
        });
        if (r.status !== 0) return makeStr(jerr(r.stderr || "mongosh failed"));
        return makeStr(JSON.stringify({ ok: true, result: (r.stdout || "").trim() }));
      }
      return makeStr(runSqlCli(c, sql, true));
    },
  };
}
