//! `POST /exec`: the bot's processes (`claude -p`, `git push`, `claude -p /usage`, the commands probe) started by the agent instead of `docker exec`, so the bot needs only read access to the Docker daemon. The output goes back as it comes in Docker's own exec frame format, so the bot reads it as it read an exec, ending with one frame of stream 3 that carries the exit code.

use serde::Deserialize;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A client that takes no output for this long is dropped; the child keeps running and its output is read to its end.
const WRITE_STALL: Duration = Duration::from_secs(30);

/// The body: the argv (one element per argument, never a shell string) and `K=V` pairs added to the agent's own environment.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Req {
    cmd: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
}

/// The body as a `Req`, `None` when it is not one: not that JSON, an empty `cmd`, an `env` entry without `=`.
pub fn parse(body: &[u8]) -> Option<Req> {
    let r: Req = serde_json::from_slice(body).ok()?;
    (!r.cmd.is_empty() && r.env.iter().all(|e| e.split_once('=').is_some_and(|(k, _)| !k.is_empty()))).then_some(r)
}

/// One frame: stream byte, three zero bytes, the big-endian length, the bytes.
fn frame(kind: u8, data: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + data.len());
    f.extend_from_slice(&[kind, 0, 0, 0]);
    f.extend_from_slice(&(data.len() as u32).to_be_bytes());
    f.extend_from_slice(data);
    f
}

/// The client's side of the stream: `None` once a write failed, after which output is still read but dropped.
type Out = Arc<Mutex<Option<TcpStream>>>;

fn send(out: &Out, bytes: &[u8]) {
    let mut g = out.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_mut().is_some_and(|c| c.write_all(bytes).is_err()) {
        *g = None;
    }
}

/// Runs `req` in `workspace` with stdin closed and streams its stdout (1) and stderr (2) as frames, then the exit frame (3): the child's code, `128 + signal` when a signal ended it, `127` when the program was not found, `126` when it could not be started otherwise. A lost client never kills the child.
pub fn run(c: TcpStream, req: Req, workspace: &Path) {
    let _ = c.set_write_timeout(Some(WRITE_STALL));
    let out: Out = Arc::new(Mutex::new(Some(c)));
    send(&out, b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: close\r\n\r\n");
    let env = req.env.iter().filter_map(|e| e.split_once('='));
    let spawned = Command::new(&req.cmd[0]).args(&req.cmd[1..]).envs(env).current_dir(workspace).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let code = match spawned {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 127,
        Err(_) => 126,
        Ok(mut child) => {
            let pipes: [(u8, Option<Box<dyn Read + Send>>); 2] = [(1, child.stdout.take().map(|p| Box::new(p) as _)), (2, child.stderr.take().map(|p| Box::new(p) as _))];
            let readers: Vec<_> = pipes
                .into_iter()
                .filter_map(|(kind, pipe)| {
                    let (mut pipe, out) = (pipe?, out.clone());
                    Some(std::thread::spawn(move || {
                        let mut buf = vec![0u8; 32 * 1024];
                        while let Ok(n) = pipe.read(&mut buf) {
                            if n == 0 {
                                break;
                            }
                            send(&out, &frame(kind, &buf[..n]));
                        }
                    }))
                })
                .collect();
            for r in readers {
                let _ = r.join();
            }
            match child.wait() {
                Ok(s) => s.code().or_else(|| s.signal().map(|n| 128 + n)).unwrap_or(126),
                Err(_) => 126,
            }
        }
    };
    send(&out, &frame(3, code.to_string().as_bytes()));
    if let Some(c) = out.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = c.shutdown(Shutdown::Write);
    }
}

#[cfg(test)]
mod tests {
    use crate::http::tests::{live_server, request};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// The frames of an answer body.
    fn frames(mut b: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        while b.len() >= 8 {
            let n = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
            out.push((b[0], b[8..8 + n].to_vec()));
            b = &b[8 + n..];
        }
        assert!(b.is_empty(), "a cut frame");
        out
    }

    fn exec(addr: std::net::SocketAddr, token: &str, body: &str) -> (u16, Vec<(u8, Vec<u8>)>) {
        let (code, b) = request(addr, "POST", "/exec", Some(token), Some(body.as_bytes()));
        (code, if code == 200 { frames(&b) } else { Vec::new() })
    }

    fn joined(frames: &[(u8, Vec<u8>)], kind: u8) -> Vec<u8> {
        frames.iter().filter(|(k, _)| *k == kind).flat_map(|(_, d)| d.clone()).collect()
    }

    #[test]
    fn streams_output_and_exit_codes_and_refuses_bad_bodies() {
        let (addr, token, d) = live_server("exec");
        let (code, f) = exec(addr, &token, r#"{"cmd":["sh","-c","printf out; printf err >&2; exit 7"]}"#);
        assert_eq!((code, joined(&f, 1), joined(&f, 2), f.last().cloned()), (200, b"out".to_vec(), b"err".to_vec(), Some((3, b"7".to_vec()))));
        let (_, f) = exec(addr, &token, r#"{"cmd":["sh","-c","printf %s \"$X\""],"env":["X=a b"]}"#);
        assert_eq!((joined(&f, 1), f.last().cloned()), (b"a b".to_vec(), Some((3, b"0".to_vec()))));
        // the prompt's path: one argv element, a leading dash, quotes, a newline, emoji, 20 000 characters
        for arg in ["-n \"q\"\nü😀".to_string(), "-".to_string() + &"x".repeat(20_000)] {
            let (_, f) = exec(addr, &token, &serde_json::json!({"cmd": ["printf", "%s", arg]}).to_string());
            assert_eq!((joined(&f, 1), f.last().cloned()), (arg.as_bytes().to_vec(), Some((3, b"0".to_vec()))));
        }
        assert_eq!(exec(addr, &token, r#"{"cmd":["/no/such/program"]}"#).1.last().cloned(), Some((3, b"127".to_vec())));
        assert_eq!(exec(addr, &token, r#"{"cmd":["sh","-c","kill -TERM $$"]}"#).1.last().cloned(), Some((3, b"143".to_vec())));
        let (_, f) = exec(addr, &token, r#"{"cmd":["pwd"]}"#);
        assert_eq!(joined(&f, 1), format!("{}\n", d.join("ws").display()).into_bytes(), "runs in the workspace");
        for body in [r#"{"cmd":[]}"#, "{}", r#"{"cmd":["true"],"env":["NOEQUALS"]}"#, r#"{"cmd":"true"}"#, "not json"] {
            assert_eq!(exec(addr, &token, body).0, 400, "{body}");
        }
        assert_eq!(request(addr, "POST", "/exec", None, Some(br#"{"cmd":["true"]}"#)).0, 401);
        assert_eq!(request(addr, "GET", "/exec", Some(&token), None).0, 405);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_lost_client_never_stops_the_child() {
        let (addr, token, d) = live_server("exec-lost");
        let file = d.join("done");
        let body = serde_json::json!({"cmd": ["sh", "-c", "echo a; sleep 1; echo b; echo done > \"$0\"", file.to_str().unwrap()]}).to_string();
        let mut c = TcpStream::connect(addr).unwrap();
        write!(c, "POST /exec HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut first = [0u8; 1];
        c.read_exact(&mut first).unwrap();
        drop(c);
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(std::fs::read_to_string(&file).unwrap_or_default(), "done\n");
        let _ = std::fs::remove_dir_all(d);
    }
}
