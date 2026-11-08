//! What the Telegram bot asks a dev container through `docker exec`, answered by one native process instead of a `python3 -c` script or several `git` execs: the pin's git facts, a run's activity, a session's title and a fingerprint of the Claude sign-in.

use crate::git;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// USER_HZ, the unit of the /proc/<pid>/stat CPU times, is 100 on Linux.
const CLK_TCK: f64 = 100.0;

/// Last path part of the origin URL without `.git`, like the bot's `pins.repo_name()`: the URL may hold a credential, so only this part leaves the container.
pub fn repo_name(url: &str) -> String {
    let u = url.trim().trim_end_matches('/').replace(':', "/");
    let name = u.rsplit('/').next().unwrap_or("");
    match name.strip_suffix(".git") {
        Some(stripped) => stripped.into(),
        None if name.is_empty() => "workspace".into(),
        None => name.into(),
    }
}

/// The dicts the bot's `pins.git_info()` and `pins.subrepos()` build from git's output: branch line facts (`parse_status()`), the line counts of `git diff HEAD --numstat` and the repo name; `git` is `null` when the workspace is not a repository.
pub fn pin_info(workspace: &Path) -> Value {
    let git = git::facts(workspace, true).map(|f| {
        let (add, del, files) = if f.head.is_some() && f.tracked_changes > 0 { git::numstat(workspace) } else { (0, 0, 0) };
        let mut v = branch_line(&f);
        v["untracked"] = json!(f.untracked);
        v["add"] = json!(add);
        v["del"] = json!(del);
        v["files"] = json!(files);
        v["repo"] = json!(git::origin_url(workspace).map_or_else(|| "workspace".into(), |u| repo_name(&u)));
        v
    });
    let subs: Vec<Value> = subrepos(workspace)
        .into_iter()
        .filter_map(|d| {
            let f = git::facts(&workspace.join(&d), false)?;
            let mut v = branch_line(&f);
            v["dir"] = json!(d);
            Some(v)
        })
        .collect();
    json!({ "git": git, "subs": subs })
}

/// `parse_status()` of `git status --porcelain=v2 --branch --untracked-files=no`: the upstream counts as usable only while its ref exists, `gone` when it is configured but its ref is not (or there is no commit yet to compare).
fn branch_line(f: &git::Facts) -> Value {
    let oid = f.head.clone().unwrap_or_else(|| "(initial)".into());
    let branch = match &f.branch {
        Some(b) => b.clone(),
        None => format!("detached {}", &oid[..7.min(oid.len())]),
    };
    json!({
        "branch": branch,
        "oid": oid,
        "untracked": 0,
        "upstream": f.upstream_configured && f.ab.is_some(),
        "gone": f.upstream_configured && f.ab.is_none(),
        "ahead": f.ab.map_or(0, |a| a.0),
    })
}

/// Folder names one level inside the workspace that hold a `.git` (the repos of a multi-repo workspace), sorted; `find -mindepth 2 -maxdepth 2 -name .git` never follows a symlinked folder, nor does this.
pub fn subrepos(workspace: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(workspace) else { return Vec::new() };
    let mut dirs: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| fs::symlink_metadata(e.path().join(".git")).is_ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != ".git")
        .collect();
    dirs.sort();
    dirs
}

/// CPU seconds used so far by session `sid`'s run (the `timeout -k … claude -p` wrapper and every descendant, reaped children included), and seconds since its transcript or one of its subagents' was last written (`None`: no transcript yet). `None` when the run is not found.
pub fn activity(sid: &str, proc_root: &Path, home: &Path) -> Option<(f64, Option<i64>)> {
    let needle = format!("\0{sid}\0--\0").into_bytes();
    let mut procs: Vec<(u32, u32, u64)> = Vec::new();
    let mut top = None;
    for e in fs::read_dir(proc_root).ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let (Ok(cmd), Ok(stat)) = (fs::read(e.path().join("cmdline")), fs::read_to_string(e.path().join("stat"))) else { continue };
        let Some((_, rest)) = stat.rsplit_once(')') else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let num = |i: usize| f.get(i).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
        let ppid = num(1) as u32;
        // utime, stime, cutime, cstime
        procs.push((pid, ppid, num(11) + num(12) + num(13) + num(14)));
        if cmd.starts_with(b"timeout\0-k\0") && contains(&cmd, b"\0claude\0-p\0") && contains(&cmd, &needle) {
            top = Some(pid);
        }
    }
    let top = top?;
    let mut tree = vec![top];
    loop {
        let new: Vec<u32> = procs.iter().filter(|(p, pp, _)| tree.contains(pp) && !tree.contains(p)).map(|(p, _, _)| *p).collect();
        if new.is_empty() {
            break;
        }
        tree.extend(new);
    }
    let cpu = procs.iter().filter(|(p, _, _)| tree.contains(p)).map(|(_, _, t)| *t).sum::<u64>() as f64 / CLK_TCK;
    let newest = transcripts(home, sid).iter().filter_map(|f| fs::metadata(f).ok()?.modified().ok()).max();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let age = newest.map(|t| (now - t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()) as i64);
    Some((cpu, age))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `~/.claude/projects/*/<sid>.jsonl` and `~/.claude/projects/*/<sid>/subagents/*.jsonl`.
fn transcripts(home: &Path, sid: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(home.join(".claude/projects")) else { return out };
    for p in rd.flatten().map(|e| e.path()) {
        let main = p.join(format!("{sid}.jsonl"));
        if main.is_file() {
            out.push(main);
        }
        if let Ok(subs) = fs::read_dir(p.join(sid).join("subagents")) {
            out.extend(subs.flatten().map(|e| e.path()).filter(|f| f.extension().is_some_and(|x| x == "jsonl")));
        }
    }
    out
}

/// The title Claude Code gave session `sid`: the last `ai-title` line of its transcript, `""` when it has none yet.
pub fn title(sid: &str, home: &Path) -> String {
    let mut title = Value::String(String::new());
    let Ok(rd) = fs::read_dir(home.join(".claude/projects")) else { return String::new() };
    for p in rd.flatten().map(|e| e.path().join(format!("{sid}.jsonl"))) {
        let Ok(bytes) = fs::read(&p) else { continue };
        for line in bytes.split(|b| *b == b'\n') {
            if !contains(line, br#""type":"ai-title""#) {
                continue;
            }
            if let Ok(Value::Object(o)) = serde_json::from_slice::<Value>(line) {
                match o.get("aiTitle") {
                    None | Some(Value::Null) | Some(Value::Bool(false)) => {}
                    Some(Value::String(s)) if s.is_empty() => {}
                    Some(v) => title = v.clone(),
                }
            }
        }
    }
    title.as_str().unwrap_or("").to_string()
}

/// Changes when the container's Claude sign-in changes: the credentials file's size and modification time and the account email in `~/.claude.json`. The bot reruns `claude auth status` (a whole Claude Code start) only then. The token itself is never read.
pub fn claude_key(home: &Path) -> String {
    let creds = fs::metadata(home.join(".claude/.credentials.json")).ok().map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos())));
    let email = fs::read(home.join(".claude.json")).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()).and_then(|v| v.pointer("/oauthAccount/emailAddress").and_then(Value::as_str).map(String::from));
    match creds {
        Some((len, mtime)) => format!("{len}:{mtime}:{}", email.unwrap_or_default()),
        None => format!("signed-out:{}", email.unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_keeps_only_the_last_part() {
        assert_eq!(repo_name("https://x:tok@github.com/a/shop.git\n"), "shop");
        assert_eq!(repo_name("git@github.com:a/b"), "b");
        assert_eq!(repo_name("https://github.com/a/b/"), "b");
        assert_eq!(repo_name(""), "workspace");
    }
}
