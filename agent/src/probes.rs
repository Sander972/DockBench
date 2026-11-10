//! What the Telegram bot asks of a dev container, answered by one native process instead of a `python3 -c` script, `git`, `claude auth status`, `sh` or `pkill` execs: the pin's git facts, a run's activity, a session's title, the Claude sign-in, a run's inbox and outbox folders and stopping a run.

use crate::git;
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// USER_HZ, the unit of the /proc/<pid>/stat CPU times, is 100 on Linux.
const CLK_TCK: f64 = 100.0;
/// Attachments older than this are deleted from the inbox before a run, so the home volume does not grow with every photo.
const INBOX_KEEP: Duration = Duration::from_secs(14 * 86400);

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
        if is_run(&cmd, Some(sid)) {
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

/// Whether `cmd` (a `/proc/<pid>/cmdline`) is the `timeout -k … claude -p …` wrapper of a bot run, of session `sid` when given: the bot's `claude_cmd()` starts its runs so and nothing else does (not its `/usage` pass or commands probe, not an interactive or VS Code `claude`), so these are the only processes the stop routes signal.
pub fn is_run(cmd: &[u8], sid: Option<&str>) -> bool {
    cmd.starts_with(b"timeout\0-k\0") && contains(cmd, b"\0claude\0-p\0") && sid.is_none_or(|s| contains(cmd, format!("\0{s}\0--\0").as_bytes()))
}

/// The pids under `proc_root` whose command line `is_run()`, sorted.
pub fn run_pids(proc_root: &Path, sid: Option<&str>) -> Vec<u32> {
    let Ok(rd) = fs::read_dir(proc_root) else { return Vec::new() };
    let mut pids: Vec<u32> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| fs::read(proc_root.join(pid.to_string()).join("cmdline")).is_ok_and(|cmd| is_run(&cmd, sid)))
        .collect();
    pids.sort();
    pids
}

/// `TERM` to each pid through one `kill` process (std can signal only its own children, and there is no `unsafe` here); `timeout` passes it on to the run's whole process group. Returns how many were signalled: 0 starts nothing.
pub fn stop(pids: &[u32]) -> usize {
    if pids.is_empty() {
        return 0;
    }
    let ok = Command::new("kill").arg("-TERM").args(pids.iter().map(u32::to_string)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if ok { pids.len() } else { 0 }
}

/// What the bot shows of the container's Claude sign-in, read from the files `claude auth status` reads instead of starting a whole Claude Code: whether the credentials file exists, the account email of `~/.claude.json` and the plan of the credentials file. No other field of either file is kept or returned, the token included.
pub fn account(home: &Path) -> Value {
    let read = |p: &str| fs::read(home.join(p)).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let text = |v: Option<Value>, ptr: &str| v.and_then(|v| v.pointer(ptr).and_then(Value::as_str).map(String::from));
    let creds = home.join(".claude/.credentials.json");
    json!({
        "loggedIn": creds.is_file(),
        "email": text(read(".claude.json"), "/oauthAccount/emailAddress"),
        "plan": text(read(".claude/.credentials.json"), "/claudeAiOauth/subscriptionType"),
    })
}

/// Before a run of session key `key`: its outbox emptied (it then holds only this run's images), the inbox and the outbox there, and inbox files older than `INBOX_KEEP` deleted.
pub fn prepare(home: &Path, key: &str) -> io::Result<()> {
    let (inbox, out) = (home.join(".telegram/inbox"), home.join(".telegram/outbox").join(key));
    remove(&out)?;
    fs::create_dir_all(&inbox)?;
    fs::create_dir_all(&out)?;
    let old = SystemTime::now() - INBOX_KEEP;
    for e in fs::read_dir(&inbox)?.flatten() {
        if e.file_type().is_ok_and(|t| t.is_file()) && e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < old) {
            fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

/// After a run's images were sent: session `key`'s outbox removed.
pub fn clear_outbox(home: &Path, key: &str) -> io::Result<()> {
    remove(&home.join(".telegram/outbox").join(key))
}

/// An upload from the bot (a prompt's attachment) as `~/.telegram/inbox/<name>`, mode 0644, the inbox created when missing. `name` is checked by the caller to name a file directly in it.
pub fn write_inbox(home: &Path, name: &str, data: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let inbox = home.join(".telegram/inbox");
    fs::create_dir_all(&inbox)?;
    fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o644).open(inbox.join(name))?.write_all(data)
}

/// `rm -rf`: a folder with everything in it, or a file or symlink itself (never what it points to); nothing there is fine.
fn remove(p: &Path) -> io::Result<()> {
    let r = match fs::symlink_metadata(p) {
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
    };
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
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

/// Changes when the container's Claude sign-in changes: the credentials file's size and modification time and the account email in `~/.claude.json`. Bots before the `/account` route reran `claude auth status` only when it changed; kept for them. The credentials file's content is not read.
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

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dockbench-agent-probes-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn account_returns_email_and_plan_never_the_token() {
        let home = scratch("account");
        assert_eq!(account(&home), json!({"loggedIn": false, "email": null, "plan": null}));
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/.credentials.json"), r#"{"claudeAiOauth":{"accessToken":"SECRET","subscriptionType":"max"}}"#).unwrap();
        fs::write(home.join(".claude.json"), r#"{"oauthAccount":{"emailAddress":"a@b.c"}}"#).unwrap();
        let a = account(&home);
        assert_eq!(a, json!({"loggedIn": true, "email": "a@b.c", "plan": "max"}));
        assert!(!a.to_string().contains("SECRET"));
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn prepare_empties_the_outbox_and_drops_old_inbox_files() {
        let home = scratch("prepare");
        let (inbox, out) = (home.join(".telegram/inbox"), home.join(".telegram/outbox/4518"));
        prepare(&home, "4518").unwrap();
        assert!(inbox.is_dir() && out.is_dir());
        fs::write(out.join("a.png"), "x").unwrap();
        fs::write(inbox.join("old.png"), "x").unwrap();
        fs::write(inbox.join("new.png"), "x").unwrap();
        fs::File::options().write(true).open(inbox.join("old.png")).unwrap().set_modified(SystemTime::now() - Duration::from_secs(15 * 86400)).unwrap();
        prepare(&home, "4518").unwrap();
        assert!(out.is_dir() && !out.join("a.png").exists() && !inbox.join("old.png").exists() && inbox.join("new.png").exists());
        fs::write(out.join("b.png"), "x").unwrap();
        clear_outbox(&home, "4518").unwrap();
        assert!(!out.exists());
        clear_outbox(&home, "4518").unwrap();
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn is_run_matches_only_the_bots_run_wrappers() {
        let run = b"timeout\0-k\x0030\x0014400\0nice\0-n\x0010\0claude\0-p\0--resume\0abc\0--\0hi\0";
        assert!(is_run(run, Some("abc")) && !is_run(run, Some("ab")) && is_run(run, None));
        assert!(!is_run(b"timeout\x0060\0nice\0-n\x0010\0claude\0-p\0--\0/usage\0", None));
        let proc_root = scratch("proc");
        for (pid, cmd) in [(7u32, &run[..]), (8, &b"claude\0--resume=abc\0"[..]), (9, &b"timeout\0-k\x0030\x0014400\0claude\0-p\0--session-id\0xyz\0--\0q\0"[..])] {
            fs::create_dir_all(proc_root.join(pid.to_string())).unwrap();
            fs::write(proc_root.join(pid.to_string()).join("cmdline"), cmd).unwrap();
        }
        fs::create_dir_all(proc_root.join("self")).unwrap();
        assert_eq!((run_pids(&proc_root, Some("abc")), run_pids(&proc_root, None), run_pids(&proc_root, Some("nope"))), (vec![7], vec![7, 9], vec![]));
        assert_eq!(stop(&[]), 0);
        let _ = fs::remove_dir_all(proc_root);
    }
}
