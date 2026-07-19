//! Database capability helpers for the native runtime (`db_open` / `db_query` / …).
//!
//! Drivers:
//! - `memory` — in-process document store (always available; for tests)
//! - `sqlite` — shells out to `sqlite3` on PATH
//! - `mysql` — shells out to `mysql` on PATH
//! - `postgres` / `pg` — shells out to `psql` on PATH
//! - `mongo` / `mongodb` — shells out to `mongosh` on PATH
//!
//! All query/exec results are JSON strings: `{"ok":true,...}` or `{"ok":false,"error":"..."}`.

use std::collections::HashMap;
use std::process::Command;

pub enum DbConn {
    Memory {
        /// collection/table -> list of JSON document strings
        docs: HashMap<String, Vec<String>>,
    },
    Sqlite {
        path: String,
    },
    Mysql {
        url: String,
    },
    Postgres {
        url: String,
    },
    Mongo {
        url: String,
    },
}

fn json_err(msg: &str) -> String {
    format!(
        "{{\"ok\":false,\"error\":{}}}",
        serde_json_escape(msg)
    )
}

fn serde_json_escape(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn which(bin: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", bin)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn open(driver: &str, conn: &str) -> Result<DbConn, String> {
    match driver {
        "memory" => Ok(DbConn::Memory {
            docs: HashMap::new(),
        }),
        "sqlite" => {
            if !which("sqlite3") {
                return Err("sqlite3 CLI not found on PATH".into());
            }
            let path = conn
                .strip_prefix("sqlite://")
                .unwrap_or(conn)
                .to_string();
            Ok(DbConn::Sqlite { path })
        }
        "mysql" => {
            if !which("mysql") {
                return Err("mysql CLI not found on PATH".into());
            }
            Ok(DbConn::Mysql {
                url: conn.to_string(),
            })
        }
        "postgres" | "pg" => {
            if !which("psql") {
                return Err("psql CLI not found on PATH".into());
            }
            Ok(DbConn::Postgres {
                url: conn.to_string(),
            })
        }
        "mongo" | "mongodb" => {
            if !which("mongosh") {
                return Err("mongosh CLI not found on PATH".into());
            }
            Ok(DbConn::Mongo {
                url: conn.to_string(),
            })
        }
        other => Err(format!(
            "unknown db driver '{}'; use memory|sqlite|mysql|postgres|mongo",
            other
        )),
    }
}

pub fn exec(conn: &mut DbConn, sql: &str) -> String {
    match conn {
        DbConn::Memory { docs } => memory_exec(docs, sql),
        DbConn::Sqlite { path } => cli_capture(
            "sqlite3",
            &["-json", path, sql],
            "exec",
        ),
        DbConn::Mysql { url } => mysql_run(url, sql, false),
        DbConn::Postgres { url } => postgres_run(url, sql, false),
        DbConn::Mongo { url } => mongo_run(url, sql),
    }
}

pub fn query(conn: &mut DbConn, sql: &str) -> String {
    match conn {
        DbConn::Memory { docs } => memory_query(docs, sql),
        DbConn::Sqlite { path } => {
            let out = Command::new("sqlite3")
                .args(["-json", path, sql])
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() {
                        "{\"ok\":true,\"rows\":[]}".into()
                    } else {
                        format!("{{\"ok\":true,\"rows\":{}}}", s)
                    }
                }
                Ok(o) => json_err(&String::from_utf8_lossy(&o.stderr)),
                Err(e) => json_err(&e.to_string()),
            }
        }
        DbConn::Mysql { url } => mysql_run(url, sql, true),
        DbConn::Postgres { url } => postgres_run(url, sql, true),
        DbConn::Mongo { url } => mongo_run(url, sql),
    }
}

fn cli_capture(bin: &str, args: &[&str], kind: &str) -> String {
    match Command::new(bin).args(args).output() {
        Ok(o) if o.status.success() => format!(
            "{{\"ok\":true,\"{}\":true,\"stdout\":{}}}",
            kind,
            serde_json_escape(String::from_utf8_lossy(&o.stdout).trim())
        ),
        Ok(o) => json_err(&String::from_utf8_lossy(&o.stderr)),
        Err(e) => json_err(&e.to_string()),
    }
}

fn mysql_run(url: &str, sql: &str, as_query: bool) -> String {
    // url: mysql://user:pass@host:port/db  OR raw flags after --
    let mut cmd = Command::new("mysql");
    cmd.arg("-N").arg("-B");
    if let Some(rest) = url.strip_prefix("mysql://") {
        // user:pass@host:port/db
        if let Some((auth_host, db)) = rest.split_once('/') {
            let (auth, hostport) = auth_host.split_once('@').unwrap_or(("", auth_host));
            if let Some((user, pass)) = auth.split_once(':') {
                cmd.arg("-u").arg(user);
                if !pass.is_empty() {
                    cmd.arg(format!("-p{}", pass));
                }
            } else if !auth.is_empty() {
                cmd.arg("-u").arg(auth);
            }
            if let Some((host, port)) = hostport.split_once(':') {
                cmd.arg("-h").arg(host).arg("-P").arg(port);
            } else if !hostport.is_empty() {
                cmd.arg("-h").arg(hostport);
            }
            if !db.is_empty() {
                cmd.arg(db);
            }
        }
    } else {
        for part in url.split_whitespace() {
            cmd.arg(part);
        }
    }
    cmd.arg("-e").arg(sql);
    match cmd.output() {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if as_query {
                let rows: Vec<String> = stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let cols: Vec<String> = l
                            .split('\t')
                            .map(|c| serde_json_escape(c))
                            .collect();
                        format!("[{}]", cols.join(","))
                    })
                    .collect();
                format!("{{\"ok\":true,\"rows\":[{}]}}", rows.join(","))
            } else {
                format!("{{\"ok\":true,\"exec\":true}}")
            }
        }
        Ok(o) => json_err(&String::from_utf8_lossy(&o.stderr)),
        Err(e) => json_err(&e.to_string()),
    }
}

fn postgres_run(url: &str, sql: &str, as_query: bool) -> String {
    let mut cmd = Command::new("psql");
    cmd.arg("-v").arg("ON_ERROR_STOP=1").arg("-t").arg("-A");
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        cmd.arg(url);
    } else {
        for part in url.split_whitespace() {
            cmd.arg(part);
        }
    }
    cmd.arg("-c").arg(sql);
    match cmd.output() {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            if as_query {
                let rows: Vec<String> = stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let cols: Vec<String> =
                            l.split('|').map(|c| serde_json_escape(c.trim())).collect();
                        format!("[{}]", cols.join(","))
                    })
                    .collect();
                format!("{{\"ok\":true,\"rows\":[{}]}}", rows.join(","))
            } else {
                "{\"ok\":true,\"exec\":true}".into()
            }
        }
        Ok(o) => json_err(&String::from_utf8_lossy(&o.stderr)),
        Err(e) => json_err(&e.to_string()),
    }
}

fn mongo_run(url: &str, script: &str) -> String {
    // script is mongosh JS, e.g. `db.users.find().toArray()`
    let mut cmd = Command::new("mongosh");
    cmd.arg("--quiet").arg(url).arg("--eval").arg(script);
    match cmd.output() {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            format!("{{\"ok\":true,\"result\":{}}}", serde_json_escape(&s))
        }
        Ok(o) => json_err(&String::from_utf8_lossy(&o.stderr)),
        Err(e) => json_err(&e.to_string()),
    }
}

/// Memory driver mini-language:
/// - `PUT <coll> <json>` insert document
/// - `GET <coll>` list docs as JSON array
/// - `CLEAR <coll>` wipe collection
/// - `COUNT <coll>` count
fn memory_exec(docs: &mut HashMap<String, Vec<String>>, sql: &str) -> String {
    let s = sql.trim();
    if let Some(rest) = s.strip_prefix("PUT ") {
        let mut parts = rest.splitn(2, ' ');
        let coll = parts.next().unwrap_or("").to_string();
        let doc = parts.next().unwrap_or("{}").to_string();
        docs.entry(coll).or_default().push(doc);
        return "{\"ok\":true,\"exec\":true}".into();
    }
    if let Some(coll) = s.strip_prefix("CLEAR ") {
        docs.insert(coll.trim().to_string(), Vec::new());
        return "{\"ok\":true,\"exec\":true}".into();
    }
    json_err("memory exec expects: PUT <coll> <json> | CLEAR <coll>")
}

fn memory_query(docs: &mut HashMap<String, Vec<String>>, sql: &str) -> String {
    let s = sql.trim();
    if let Some(coll) = s.strip_prefix("GET ") {
        let coll = coll.trim();
        let rows = docs.get(coll).cloned().unwrap_or_default();
        return format!("{{\"ok\":true,\"rows\":[{}]}}", rows.join(","));
    }
    if let Some(coll) = s.strip_prefix("COUNT ") {
        let n = docs.get(coll.trim()).map(|v| v.len()).unwrap_or(0);
        return format!("{{\"ok\":true,\"count\":{}}}", n);
    }
    json_err("memory query expects: GET <coll> | COUNT <coll>")
}

