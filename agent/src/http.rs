//! HTTP on port 8080, one request per connection, each on its own thread so one slow or idle peer never holds the next poll. `GET /status` is open (the dashboard's contract); the bot's routes (the probes `GET /pin-info`, `/activity`, `/title`, `/claude-key`, `/account` and the run chores `POST /prepare`, `/outbox-clear`, `/stop`, `/stop-all`, and the two that take a body, `POST /inbox` and `POST /exec`) need `Authorization: Bearer <token>`, the token this process made at start and wrote to `~/.dockbench-agent-token`, so another container on the same network cannot read this one's git state or session titles, nor stop its runs.

use crate::status::Server;
use crate::{exec, probes};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Request line and headers above this are answered `400`.
const MAX_REQUEST: usize = 8 * 1024;
/// Largest upload `POST /inbox` takes: Telegram bots download at most 20 MB.
const MAX_INBOX: usize = 50 * 1024 * 1024;
/// Largest `POST /exec` body: an argv and an environment, the prompt included.
const MAX_EXEC: usize = 1024 * 1024;
/// A body that stops arriving for this long is dropped.
const BODY_STALL: Duration = Duration::from_secs(30);

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

/// The head up to the end of the headers, and the body bytes the same reads already took; `None` when the peer sends more than `MAX_REQUEST` before the end of the head, stalls past the 5 s timeout or closes first.
fn read_head(c: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            if end + 4 > MAX_REQUEST {
                return None;
            }
            let rest = buf.split_off(end + 4);
            return String::from_utf8(buf).ok().map(|h| (h, rest));
        }
        if buf.len() > MAX_REQUEST {
            return None;
        }
        let n = c.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// The request body: `already` (what `read_head` took past the head), then the rest up to `Content-Length`. `None` when the header is missing or not a number, above `max`, or the peer stalls `BODY_STALL` or closes first.
pub fn read_body(c: &mut TcpStream, head: &str, already: &[u8], max: usize) -> Option<Vec<u8>> {
    let len: usize = header(head, "content-length")?.parse().ok()?;
    if len > max || already.len() > len {
        return None;
    }
    let _ = c.set_read_timeout(Some(BODY_STALL));
    let mut body = Vec::with_capacity(len);
    body.extend_from_slice(already);
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < len {
        let n = c.read(&mut chunk[..(len - body.len()).min(64 * 1024)]).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(body)
}

/// The value of header `name` (lowercase) in a request head.
fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
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
    ("/inbox", "POST"),
    ("/exec", "POST"),
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

/// An inbox file name: 1 to 200 letters, digits, `.`, `_`, `-`, not starting with `.`, so it can only name a file directly in the inbox.
fn inbox_name(params: &[(String, String)]) -> Option<&str> {
    let n = params.iter().find(|(k, _)| k == "name").map(|(_, v)| v.as_str())?;
    (!n.is_empty() && n.len() <= 200 && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))).then_some(n)
}

fn authorized(head: &str, token: &str) -> bool {
    let Some(sent) = header(head, "authorization").map(|v| v.strip_prefix("Bearer ").unwrap_or("").trim().to_string()) else { return false };
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
        ("/stop", Some(sid)) => text(200, format!("{}\n", probes::stop(&probes::run_pids(proc_root, Some(sid))))),
        // /inbox and /exec with the token never get here, handle() serves them
        _ => error(404, "not found"),
    }
}

/// `POST /inbox` and `POST /exec` with the token, which need the body; `None` for every other request, which `answer()` serves. `Err` is the answer to send, `Ok` means the connection was used up by `/exec`'s stream.
fn with_body(c: &mut TcpStream, head: &str, extra: &[u8], server: &Server, token: &str) -> Option<Result<(), Answer>> {
    let target = head.lines().next()?.strip_prefix("POST ")?.split(' ').next()?;
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    if !matches!(path, "/inbox" | "/exec") || !authorized(head, token) {
        return None;
    }
    let Some(body) = read_body(c, head, extra, if path == "/inbox" { MAX_INBOX } else { MAX_EXEC }) else {
        return Some(Err(error(400, "a body with a Content-Length within the limit is needed")));
    };
    if path == "/inbox" {
        let params = query(q);
        let Some(name) = inbox_name(&params) else { return Some(Err(error(400, "name must be letters, digits, '.', '_' and '-', not starting with '.'"))) };
        return Some(Err(match probes::write_inbox(&server.paths.home, name, &body) {
            Ok(()) => text(200, "ok\n".into()),
            Err(e) => error(500, &e.to_string()),
        }));
    }
    match exec::parse(&body) {
        None => Some(Err(error(400, r#"the body must be {"cmd":[program, args...],"env":["K=V", ...]}"#))),
        Some(req) => {
            exec::run(c.try_clone().ok()?, req, &server.paths.workspace);
            Some(Ok(()))
        }
    }
}

fn handle(mut c: TcpStream, server: &Server, token: &str) {
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = c.set_write_timeout(Some(Duration::from_secs(5)));
    let a = match read_head(&mut c) {
        None => error(400, "bad request"),
        Some((head, extra)) => match with_body(&mut c, &head, &extra, server, token) {
            Some(Ok(())) => return,
            Some(Err(a)) => a,
            None => answer(&head, server, token),
        },
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
pub mod tests {
    use super::*;
    use crate::status::{Paths, Server};
    use std::io::{BufRead, BufReader};
    use std::net::SocketAddr;

    /// A real server on a loopback port over a scratch home (`ws/` its workspace, no proc root), and its token.
    pub fn live_server(tag: &str) -> (SocketAddr, String, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("dockbench-agent-live-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("ws")).unwrap();
        let paths = Paths { home: d.clone(), workspace: d.join("ws"), claude_dir: d.join(".claude"), plugin_sync: d.join("sync.json"), proc_root: d.join("no-proc") };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let token = new_token().expect("urandom");
        let tok: Arc<str> = token.clone().into();
        std::thread::spawn(move || serve(listener, Arc::new(Server::new(paths)), tok));
        (addr, token, d)
    }

    /// One request with an optional token and body (sent with its `Content-Length`): the status code and the answer body, read to the end.
    pub fn request(addr: SocketAddr, method: &str, target: &str, token: Option<&str>, body: Option<&[u8]>) -> (u16, Vec<u8>) {
        let mut c = TcpStream::connect(addr).unwrap();
        let mut head = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n");
        if let Some(t) = token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        if let Some(b) = body {
            head.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        head.push_str("\r\n");
        c.write_all(head.as_bytes()).unwrap();
        // a refused upload is answered before its body is read: the rest may not go through
        let _ = body.map(|b| c.write_all(b));
        let mut all = Vec::new();
        let _ = c.read_to_end(&mut all);
        let end = all.windows(4).position(|w| w == b"\r\n\r\n").expect("an answer head") + 4;
        let code = String::from_utf8_lossy(&all[9..12]).parse().unwrap();
        (code, all[end..].to_vec())
    }

    #[test]
    fn inbox_upload_lands_byte_for_byte_and_refuses_bad_names() {
        let (addr, token, d) = live_server("inbox");
        let mut data = vec![0u8; 3 * 1024 * 1024];
        std::fs::File::open("/dev/urandom").unwrap().read_exact(&mut data).unwrap();
        assert_eq!(request(addr, "POST", "/inbox?name=77-0.png", Some(&token), Some(&data)), (200, b"ok\n".to_vec()));
        let file = d.join(".telegram/inbox/77-0.png");
        assert!(std::fs::read(&file).unwrap() == data, "the upload changed on the way");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        for target in ["/inbox?name=..%2Fx", "/inbox?name=.hidden", "/inbox?name=", "/inbox", "/inbox?name=a%2Fb", &format!("/inbox?name={}", "a".repeat(201))] {
            assert_eq!(request(addr, "POST", target, Some(&token), Some(b"x")).0, 400, "{target}");
        }
        assert_eq!(request(addr, "POST", "/inbox?name=nolen", Some(&token), None).0, 400, "no Content-Length");
        // head and a binary body in one write: the bytes read_head takes past the head are the body's start
        let mut c = TcpStream::connect(addr).unwrap();
        let mut one = format!("POST /inbox?name=one.bin HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 4\r\n\r\n").into_bytes();
        one.extend_from_slice(b"\xff\x00\r\n");
        c.write_all(&one).unwrap();
        let mut answer = String::new();
        let _ = c.read_to_string(&mut answer);
        assert!(answer.starts_with("HTTP/1.1 200 OK") && std::fs::read(d.join(".telegram/inbox/one.bin")).unwrap() == b"\xff\x00\r\n", "{answer}");
        std::fs::remove_file(d.join(".telegram/inbox/one.bin")).unwrap();
        let mut c = TcpStream::connect(addr).unwrap();
        write!(c, "POST /inbox?name=big HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n", MAX_INBOX + 1).unwrap();
        let mut line = String::new();
        BufReader::new(c).read_line(&mut line).unwrap();
        assert_eq!(line, "HTTP/1.1 400 Bad Request\r\n", "an upload over the cap");
        assert_eq!(request(addr, "POST", "/inbox?name=x.png", None, Some(b"secret")).0, 401);
        assert_eq!(request(addr, "POST", "/inbox?name=x.png", Some("0123456789abcdef0123456789abcdef"), Some(b"secret")).0, 401);
        let names: Vec<_> = std::fs::read_dir(d.join(".telegram/inbox")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, ["77-0.png"], "a refused upload wrote something");
        let _ = std::fs::remove_dir_all(d);
    }

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
