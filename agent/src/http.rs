//! HTTP on port 8080, one request per connection, each on its own thread so one slow or idle peer never holds the next poll. `GET /status` is open (the dashboard's contract); the bot's routes (the probes `GET /pin-info`, `/activity`, `/title`, `/claude-key`, `/account` and the run chores `POST /prepare`, `/outbox-clear`, `/stop`, `/stop-all`) need `Authorization: Bearer <token>`, the token this process made at start and wrote to `~/.dockbench-agent-token`, so another container on the same network cannot read this one's git state or session titles, nor stop its runs.

use crate::probes;
use crate::status::Server;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Request line and headers above this are answered `400`.
const MAX_REQUEST: usize = 8 * 1024;

pub fn serve(listener: TcpListener, server: Arc<Server>, token: Arc<str>) {
    for conn in listener.incoming().flatten() {
        let (s, t) = (server.clone(), token.clone());
        std::thread::spawn(move || handle(conn, &s, &t));
    }
}

/// 32 hex characters from the kernel's random source, or `None` when it cannot be read (the probes then answer `401` to everyone and the bot keeps its execs).
pub fn new_token() -> Option<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom").ok()?.read_exact(&mut b).ok()?;
    Some(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The token file, mode 0600, readable by the container's own user and by the bot through the Docker archive API.
pub fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    f.write_all(token.as_bytes())?;
    f.write_all(b"\n")
}

/// Up to the end of the headers; `None` when the peer sends more than `MAX_REQUEST`, stalls past the 5 s timeout or closes first.
fn read_head(c: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = c.read(&mut chunk).ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).ok()
}

struct Answer {
    code: u16,
    json: bool,
    body: String,
}

fn json(code: u16, body: String) -> Answer {
    Answer { code, json: true, body }
}

fn text(code: u16, body: String) -> Answer {
    Answer { code, json: false, body }
}

fn error(code: u16, msg: &str) -> Answer {
    json(code, serde_json::json!({ "error": msg }).to_string())
}

/// Every route and its one method; a known path asked with another is `405`, an unknown one `404`, both before the token is checked.
const ROUTES: &[(&str, &str)] = &[
    ("/status", "GET"),
    ("/pin-info", "GET"),
    ("/activity", "GET"),
    ("/title", "GET"),
    ("/claude-key", "GET"),
    ("/account", "GET"),
    ("/prepare", "POST"),
    ("/outbox-clear", "POST"),
    ("/stop", "POST"),
    ("/stop-all", "POST"),
];

const BAD_SESSION: &str = "session must be letters, digits and dashes";

/// The `name=value` pairs of a query string, percent-decoded.
fn query(q: &str) -> Vec<(String, String)> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        (unescape(k), unescape(v))
    }).collect()
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(x) => { out.push(x); i += 3; }
                    None => { out.push(b'%'); i += 1; }
                }
            }
            b'+' => { out.push(b' '); i += 1; }
            x => { out.push(x); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A Claude Code session id: letters, digits and dashes only, so it can never name a path outside the projects folder.
fn session_id(params: &[(String, String)]) -> Option<&str> {
    let sid = params.iter().find(|(k, _)| k == "session").map(|(_, v)| v.as_str())?;
    (!sid.is_empty() && sid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')).then_some(sid)
}

fn authorized(head: &str, token: &str) -> bool {
    let Some(sent) = head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("authorization").then(|| v.trim().strip_prefix("Bearer ").unwrap_or("").trim().to_string())
    }) else { return false };
    // same length and a fold over every byte, so the comparison does not stop at the first difference
    sent.len() == token.len() && !token.is_empty() && sent.bytes().zip(token.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// The answer to one request head. `/status` is open; the other routes need the token, a wrong or missing one is `401` whatever the route, so a scan learns nothing.
pub fn answer(head: &str, server: &Server, token: &str) -> Answer {
    let mut parts = head.lines().next().unwrap_or("").split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return error(400, "bad request") };
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let Some(&(_, want)) = ROUTES.iter().find(|(p, _)| *p == path) else { return error(404, "not found") };
    if method != want {
        return error(405, "method not allowed");
    }
    if path == "/status" {
        return json(200, serde_json::to_string(&server.status()).unwrap_or_default());
    }
    if !authorized(head, token) {
        return error(401, "unauthorized");
    }
    let params = query(q);
    let (home, proc_root) = (&server.paths.home, &server.paths.proc_root);
    let done = |r: std::io::Result<()>| match r {
        Ok(()) => text(200, "ok\n".into()),
        Err(e) => error(500, &e.to_string()),
    };
    match (path, session_id(&params)) {
        ("/pin-info", _) => {
            let dir = params.iter().find(|(k, _)| k == "dir").map_or_else(|| server.paths.workspace.clone(), |(_, v)| v.into());
            json(200, probes::pin_info(&dir).to_string())
        }
        ("/claude-key", _) => text(200, format!("{}\n", probes::claude_key(home))),
        ("/account", _) => json(200, probes::account(home).to_string()),
        ("/stop-all", _) => text(200, format!("{}\n", probes::stop(&probes::run_pids(proc_root, None)))),
        (_, None) => error(400, BAD_SESSION),
        // exit 3 of the subcommand: the run is not in the container
        ("/activity", Some(sid)) => match probes::activity(sid, proc_root, home) {
            Some((cpu, age)) => text(200, format!("{cpu} {}\n", age.unwrap_or(-1))),
            None => text(204, String::new()),
        },
        ("/title", Some(sid)) => text(200, probes::title(sid, home)),
        ("/prepare", Some(key)) => done(probes::prepare(home, key)),
        ("/outbox-clear", Some(key)) => done(probes::clear_outbox(home, key)),
        (_, Some(sid)) => text(200, format!("{}\n", probes::stop(&probes::run_pids(proc_root, Some(sid))))),
    }
}

fn handle(mut c: TcpStream, server: &Server, token: &str) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
    let a = match read_head(&mut c) {
        None => error(400, "bad request"),
        Some(head) => answer(&head, server, token),
    };
    let reason = match a.code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Not Found",
    };
    let ctype = if a.json { "application/json" } else { "text/plain; charset=utf-8" };
    let head = format!("HTTP/1.1 {} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", a.code, a.body.len());
    let _ = c.write_all(head.as_bytes()).and_then(|_| c.write_all(a.body.as_bytes()));
    // drain what the peer still sends, so closing does not reset the connection before it reads the answer
    let _ = c.shutdown(Shutdown::Write);
    let _ = c.set_read_timeout(Some(Duration::from_millis(200)));
    let mut sink = [0u8; 1024];
    while matches!(c.read(&mut sink), Ok(n) if n > 0) {}
}

/// The probe routes against a server on `paths` (a scratch home and workspace: `/status` is left out, it would start `code` and `mise`), as the image build's `--check` and `cargo test` run it. Panics on the first wrong answer.
pub fn self_test(server: &Server) {
    let token = "0123456789abcdef0123456789abcdef";
    let req = |method: &str, target: &str, auth: Option<&str>| {
        let mut h = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
        if let Some(a) = auth {
            h.push_str(&format!("Authorization: {a}\r\n"));
        }
        h.push_str("\r\n");
        h
    };
    let head = |target: &str, auth: Option<&str>| req("GET", target, auth);
    let ok = Some(format!("Bearer {token}"));
    let a = answer(&head("/pin-info", ok.as_deref()), server, token);
    assert_eq!((a.code, a.body.as_str()), (200, r#"{"git":null,"subs":[]}"#), "pin-info of a non-repo workspace");
    let a = answer(&head("/pin-info?dir=%2Fnonexistent%2Fdir", ok.as_deref()), server, token);
    assert_eq!((a.code, a.body.as_str()), (200, r#"{"git":null,"subs":[]}"#), "pin-info of a missing dir");
    let a = answer(&head("/activity?session=no-such-run", ok.as_deref()), server, token);
    assert_eq!((a.code, a.body.as_str()), (204, ""), "activity of a run that is not there");
    let a = answer(&head("/title?session=s9", ok.as_deref()), server, token);
    assert_eq!((a.code, a.body.as_str()), (200, ""), "title of an unknown session");
    let a = answer(&head("/claude-key", ok.as_deref()), server, token);
    assert!(a.code == 200 && a.body.starts_with("signed-out:"), "{} {}", a.code, a.body);
    for (target, auth, want) in [
        ("/pin-info", None, 401),
        ("/pin-info", Some("Bearer 0123456789abcdef0123456789abcdee"), 401),
        ("/pin-info", Some("Bearer short"), 401),
        ("/title?session=..%2F..%2Fetc", ok.as_deref(), 400),
        ("/title", ok.as_deref(), 400),
        ("/nothing", None, 404),
        ("/nothing", ok.as_deref(), 404),
    ] {
        let a = answer(&head(target, auth), server, token);
        assert_eq!(a.code, want, "{target} {auth:?}: {}", a.body);
    }
    let a = answer("POST /status HTTP/1.1\r\n\r\n", server, token);
    assert_eq!(a.code, 405);

    // the run chores: the sign-in, the run's folders, stopping runs (the server's proc root holds none, so nothing is signalled)
    let a = answer(&head("/account", ok.as_deref()), server, token);
    assert_eq!((a.code, a.body.as_str()), (200, r#"{"loggedIn":false,"email":null,"plan":null}"#), "account of a signed-out home");
    let post = |target: &str| answer(&req("POST", target, ok.as_deref()), server, token);
    let a = post("/prepare?session=4518");
    let (inbox, out) = (server.paths.home.join(".telegram/inbox"), server.paths.home.join(".telegram/outbox/4518"));
    assert!(a.code == 200 && a.body == "ok\n" && inbox.is_dir() && out.is_dir(), "{} {}", a.code, a.body);
    std::fs::write(out.join("a.png"), "x").unwrap();
    for _ in 0..2 {
        let a = post("/outbox-clear?session=4518");
        assert!(a.code == 200 && a.body == "ok\n" && !out.exists(), "{} {}", a.code, a.body);
    }
    let a = post("/stop?session=no-such-run");
    assert_eq!((a.code, a.body.as_str()), (200, "0\n"));
    let a = post("/stop-all");
    assert!(a.code == 200 && a.body.trim().parse::<usize>().is_ok(), "{} {}", a.code, a.body);
    for (target, want) in [("/prepare?session=..%2Fx", 400), ("/prepare", 400), ("/stop", 400), ("/outbox-clear?session=", 400)] {
        assert_eq!(post(target).code, want, "{target}");
    }
    for (m, target) in [("GET", "/prepare?session=1"), ("POST", "/account"), ("GET", "/stop-all"), ("POST", "/pin-info")] {
        assert_eq!(answer(&req(m, target, ok.as_deref()), server, token).code, 405, "{m} {target}");
    }
    for (m, target) in [("GET", "/account"), ("POST", "/prepare?session=1"), ("POST", "/outbox-clear?session=1"), ("POST", "/stop?session=1"), ("POST", "/stop-all")] {
        assert_eq!(answer(&req(m, target, None), server, token).code, 401, "{m} {target} without the token");
    }
    assert!(!server.paths.home.join(".telegram/outbox/1").exists(), "an unauthorized prepare did something");
    assert!(!authorized(&head("/pin-info", Some("Bearer ")), ""), "an empty token never authorizes");
    assert_eq!(unescape("a%20b+c%2F%zz"), "a b c/%zz");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Paths, Server};
    use std::io::{BufRead, BufReader};

    fn scratch_server() -> (Server, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("dockbench-agent-http-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("ws")).unwrap();
        let paths = Paths { home: d.clone(), workspace: d.join("ws"), claude_dir: d.join(".claude"), plugin_sync: d.join("sync.json"), proc_root: d.join("no-proc") };
        (Server::new(paths), d)
    }

    #[test]
    fn probe_routes_answer_as_the_subcommands_do() {
        let (server, d) = scratch_server();
        self_test(&server);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn token_file_is_private_and_a_real_request_round_trips() {
        let (server, d) = scratch_server();
        let token = new_token().expect("urandom");
        assert_eq!(token.len(), 32);
        let file = d.join(".dockbench-agent-token");
        write_token(&file, &token).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{token}\n"));

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let tok: Arc<str> = token.clone().into();
        std::thread::spawn(move || serve(listener, Arc::new(server), tok));
        let mut c = TcpStream::connect(addr).unwrap();
        write!(c, "GET /title?session=abc HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n").unwrap();
        let mut r = BufReader::new(c);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 200 OK\r\n");
        let mut rest = String::new();
        r.read_to_string(&mut rest).unwrap();
        assert!(rest.contains("Content-Length: 0\r\n") && rest.contains("text/plain"), "{rest}");

        let mut c = TcpStream::connect(addr).unwrap();
        write!(c, "GET /title?session=abc HTTP/1.1\r\n\r\n").unwrap();
        let mut line = String::new();
        BufReader::new(c).read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 401 Unauthorized\r\n");
        let _ = std::fs::remove_dir_all(d);
    }
}
