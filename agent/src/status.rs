//! The `/status` answer, field for field what `status_server.py` answered (contract: the dashboard's dev-container API reference). Git is read in-process; the two answers that still need a program (`code tunnel status`, `mise ls`) are cached, so a dashboard polling every 2 s starts almost nothing.

use crate::git;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Git facts are reread at most this often: the dashboard ticks every 2 s, the worktree walk is the costliest part left.
const GIT_TTL: Duration = Duration::from_secs(5);
/// `code tunnel status` while the tunnel is connected, and while it is not (waiting for a device-code login shows up within this).
const TUNNEL_TTL_UP: Duration = Duration::from_secs(300);
const TUNNEL_TTL_DOWN: Duration = Duration::from_secs(15);
/// `mise ls` reruns when a mise config file or the installs folder changes, and at the latest after this.
const MISE_TTL: Duration = Duration::from_secs(600);
/// Like status_server.py's subprocess timeout.
const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the answer is read from; `from_env` in the container, temp folders in `--check`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
    pub workspace: PathBuf,
    pub claude_dir: PathBuf,
    /// written by the entrypoint's background plugin sync, per container
    pub plugin_sync: PathBuf,
}

impl Paths {
    pub fn from_env() -> Paths {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/home/dev".into()));
        Paths { workspace: home.join("workspace"), claude_dir: home.join(".claude"), plugin_sync: "/var/lib/dockbench/claude-plugins.json".into(), home }
    }
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct LastCommit {
    pub sha: String,
    pub message: String,
    pub date: String,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
pub struct GitJson {
    pub branch: Option<String>,
    pub dirty: Option<usize>,
    pub last_commit: Option<LastCommit>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

/// Field order is status_server.py's.
#[derive(Serialize, Debug)]
pub struct Status {
    pub repo: Option<String>,
    pub tunnel_name: Option<String>,
    pub branch: Option<String>,
    pub dirty: Option<usize>,
    pub last_commit: Option<LastCommit>,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub tunnel: &'static str,
    pub mise: Map<String, Value>,
    pub workspace_file: Option<String>,
    pub claude_installed: bool,
    pub claude_logged_in: bool,
    pub claude_version: Option<String>,
    pub claude_plugins: Map<String, Value>,
    pub claude_plugin_sync: Option<Value>,
    pub uptime_seconds: u64,
}

struct Slot<T> {
    at: Option<Instant>,
    key: u64,
    val: T,
}

impl<T: Default> Default for Slot<T> {
    fn default() -> Self {
        Slot { at: None, key: 0, val: T::default() }
    }
}

/// One per process. Each cache's lock is held while it refills, so concurrent requests share one computation instead of each starting their own.
pub struct Server {
    pub paths: Paths,
    started: Instant,
    git: Mutex<Slot<GitJson>>,
    tunnel: Mutex<Slot<&'static str>>,
    mise: Mutex<Slot<Map<String, Value>>>,
}

impl Server {
    pub fn new(paths: Paths) -> Server {
        Server { paths, started: Instant::now(), git: Mutex::default(), tunnel: Mutex::default(), mise: Mutex::default() }
    }

    pub fn status(&self) -> Status {
        let git = self.git_cached();
        let (claude_version, claude_plugins, claude_plugin_sync) = claude_info(&self.paths);
        Status {
            repo: std::env::var("REPO_URL").ok(),
            tunnel_name: std::env::var("TUNNEL_NAME").ok(),
            branch: git.branch,
            dirty: git.dirty,
            last_commit: git.last_commit,
            ahead: git.ahead,
            behind: git.behind,
            tunnel: self.tunnel_cached(),
            mise: self.mise_cached(),
            workspace_file: workspace_file(&self.paths.workspace),
            claude_installed: which("claude").is_some(),
            claude_logged_in: self.paths.claude_dir.join(".credentials.json").exists(),
            claude_version,
            claude_plugins,
            claude_plugin_sync,
            uptime_seconds: self.started.elapsed().as_secs(),
        }
    }

    fn git_cached(&self) -> GitJson {
        let mut s = self.git.lock().unwrap_or_else(|e| e.into_inner());
        if s.at.is_none_or(|t| t.elapsed() >= GIT_TTL) {
            s.val = git_json(&self.paths.workspace);
            s.at = Some(Instant::now());
        }
        s.val.clone()
    }

    fn tunnel_cached(&self) -> &'static str {
        let mut s = self.tunnel.lock().unwrap_or_else(|e| e.into_inner());
        let ttl = if s.val == "connected" { TUNNEL_TTL_UP } else { TUNNEL_TTL_DOWN };
        if s.at.is_none_or(|t| t.elapsed() >= ttl) {
            s.val = tunnel_state(run(&["code", "tunnel", "status"], &self.paths.home).as_deref());
            s.at = Some(Instant::now());
        }
        s.val
    }

    fn mise_cached(&self) -> Map<String, Value> {
        let mut s = self.mise.lock().unwrap_or_else(|e| e.into_inner());
        let key = mise_fingerprint(&self.paths);
        if s.at.is_none_or(|t| t.elapsed() >= MISE_TTL) || s.key != key {
            s.val = mise_tools(run(&["mise", "ls", "--current", "--json"], &self.paths.workspace).as_deref());
            s.at = Some(Instant::now());
            s.key = key;
        }
        s.val.clone()
    }
}

/// status_server.py's git_info(): every field `null` when the workspace is not a repo or has no commit yet; the literal `HEAD` on a detached head.
pub fn git_json(workspace: &Path) -> GitJson {
    let Some(f) = git::facts(workspace, true) else { return GitJson::default() };
    if f.head.is_none() {
        return GitJson::default();
    }
    GitJson {
        branch: Some(f.branch.unwrap_or_else(|| "HEAD".into())),
        dirty: f.dirty,
        last_commit: f.last_commit.map(|(sha, message, date)| LastCommit { sha, message, date }),
        ahead: f.ab.map(|a| a.0),
        behind: f.ab.map(|a| a.1),
    }
}

/// stdout of a program, trimmed, `None` when it cannot start, fails or outlives RUN_TIMEOUT (it is killed then). A byte that is not UTF-8 is replaced, never an error.
pub fn run(args: &[&str], cwd: &Path) -> Option<String> {
    let mut child = Command::new(args[0]).args(&args[1..]).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    // read on a thread, so a child writing more than the pipe holds never blocks the wait below
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let end = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.filter(|s| s.success()).map(|_| String::from_utf8_lossy(&out).trim().to_string())
}

pub fn tunnel_state(out: Option<&str>) -> &'static str {
    let Some(out) = out else { return "unknown" };
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(out) else { return "unknown" };
    // Python's `if not t`: every falsy JSON value means no tunnel
    let falsy = |v: &Value| match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
    };
    match o.get("tunnel") {
        None => "down",
        Some(v) if falsy(v) => "down",
        Some(Value::Object(t)) if t.get("tunnel").and_then(Value::as_str) == Some("Connected") => "connected",
        Some(Value::Object(t)) => {
            if t.get("tunnel_id").is_none_or(Value::is_null) {
                "waiting_for_login"
            } else {
                "down"
            }
        }
        Some(_) => "unknown",
    }
}

/// Tool name to the first active version, `{}` when there is no output or it is not JSON.
pub fn mise_tools(out: Option<&str>) -> Map<String, Value> {
    let Some(Value::Object(data)) = out.filter(|o| !o.is_empty()).and_then(|o| serde_json::from_str(o).ok()) else { return Map::new() };
    data.into_iter()
        .filter_map(|(tool, entries)| match entries {
            Value::Array(e) if !e.is_empty() => Some((tool, e[0].get("version").cloned().unwrap_or(Value::Null))),
            _ => None,
        })
        .collect()
}

/// Changes when a file mise reads for the workspace's tools changes, or a tool version is installed or removed.
fn mise_fingerprint(p: &Paths) -> u64 {
    const LOCAL: [&str; 11] = ["mise.toml", ".mise.toml", "mise.local.toml", ".mise.local.toml", ".mise/config.toml", "mise/config.toml", ".config/mise.toml", ".config/mise/config.toml", ".config/mise/conf.d", ".tool-versions", ".mise"];
    let mut h = DefaultHasher::new();
    let mut stamp = |path: &Path| {
        if let Ok(m) = fs::metadata(path) {
            path.hash(&mut h);
            m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).hash(&mut h);
            m.len().hash(&mut h);
        }
    };
    for dir in p.workspace.ancestors() {
        for name in LOCAL {
            stamp(&dir.join(name));
        }
    }
    stamp(&p.home.join(".config/mise/config.toml"));
    stamp(&p.home.join(".config/mise/conf.d"));
    stamp(Path::new("/etc/mise/config.toml"));
    let data = std::env::var_os("MISE_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| p.home.join(".local/share/mise"));
    let installs = data.join("installs");
    stamp(&installs);
    if let Ok(rd) = fs::read_dir(&installs) {
        let mut tools: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        tools.sort();
        tools.iter().for_each(|t| stamp(t));
    }
    h.finish()
}

/// Python's str.isprintable() as far as it matters for a file name: no control, format or separator character other than the space. Unassigned code points are not known here and count as printable.
pub fn printable(s: &str) -> bool {
    s.chars().all(|c| {
        let u = c as u32;
        !(c.is_control()
            || matches!(u, 0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2 | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
            || matches!(u, 0xA0 | 0x1680 | 0x2000..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000)
            || matches!(u, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD | 0x100000..=0x10FFFD))
    })
}

/// Only the bare name, never the folder listing: the first `*.code-workspace` by name that is a regular file (no symlink, no directory) with a printable name.
pub fn workspace_file(workspace: &Path) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(workspace)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".code-workspace") && n != ".code-workspace" && printable(n))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// shutil.which(): the first executable regular file of that name on PATH.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// Files only, no subprocess: the native installer links ~/.local/bin/claude to ~/.local/share/claude/versions/<version>.
pub fn claude_info(p: &Paths) -> (Option<String>, Map<String, Value>, Option<Value>) {
    let version = which("claude").and_then(|c| fs::canonicalize(c).ok()).and_then(|r| r.file_name()?.to_str().map(String::from)).filter(|n| n.starts_with(|c: char| c.is_ascii_digit()));
    let mut plugins = Map::new();
    if let Some(Value::Object(all)) = read_json(&p.claude_dir.join("plugins/installed_plugins.json")).and_then(|v| v.get("plugins").cloned()) {
        for (plugin, entries) in all {
            let Value::Array(entries) = entries else { continue };
            if !entries.first().is_some_and(Value::is_object) {
                continue;
            }
            let user = entries.iter().find(|e| e.get("scope").and_then(Value::as_str) == Some("user"));
            plugins.insert(plugin, user.unwrap_or(&entries[0]).get("version").cloned().unwrap_or(Value::Null));
        }
    }
    let sync = read_json(&p.plugin_sync).filter(Value::is_object);
    (version, plugins, sync)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_states() {
        assert_eq!(tunnel_state(None), "unknown");
        assert_eq!(tunnel_state(Some("not json")), "unknown");
        assert_eq!(tunnel_state(Some("[]")), "unknown");
        assert_eq!(tunnel_state(Some(r#"{"tunnel":null}"#)), "down");
        assert_eq!(tunnel_state(Some(r#"{"tunnel":{"tunnel":"Connected","tunnel_id":"x"}}"#)), "connected");
        assert_eq!(tunnel_state(Some(r#"{"tunnel":{"tunnel":"Disconnected","tunnel_id":null}}"#)), "waiting_for_login");
        assert_eq!(tunnel_state(Some(r#"{"tunnel":{"tunnel":"Disconnected","tunnel_id":"x"}}"#)), "down");
    }

    #[test]
    fn mise_first_version_per_tool() {
        let m = mise_tools(Some(r#"{"node":[{"version":"22.1.0"},{"version":"20"}],"bun":[],"uv":[{"source":{}}]}"#));
        assert_eq!(Value::Object(m), serde_json::json!({"node": "22.1.0", "uv": null}));
        assert!(mise_tools(Some("")).is_empty() && mise_tools(None).is_empty() && mise_tools(Some("{bad")).is_empty());
    }

    #[test]
    fn printable_names() {
        assert!(printable("a b.code-workspace") && printable("aø.code-workspace"));
        assert!(!printable("\t.code-workspace") && !printable("a\u{202E}b") && !printable("a\u{A0}b"));
    }

    #[test]
    fn run_replaces_bad_bytes_and_times_out() {
        assert_eq!(run(&["printf", r"caff\350"], Path::new("/")).as_deref(), Some("caff\u{FFFD}"));
        assert_eq!(run(&["false"], Path::new("/")), None);
        assert_eq!(run(&["no-such-program-x"], Path::new("/")), None);
    }
}
