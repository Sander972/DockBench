//! `dockbench-agent --check`: the self-check status_server.py had (a scratch repo and Claude folder, then the whole `/status` answer), plus the bot probes as subcommands and over HTTP. It needs `git` on PATH and runs in the image build, so a broken binary never ships.

use crate::{probes, status};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("dockbench-agent-check-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(dir).status().is_ok_and(|s| s.success());
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

pub fn run() {
    let (d, c, home) = (scratch("ws"), scratch("claude"), scratch("home"));
    fs::create_dir_all(c.join("plugins")).unwrap();
    fs::write(c.join("plugins/installed_plugins.json"), r#"{"version":2,"plugins":{"a@m":[{"scope":"project","version":"0.1"},{"scope":"user","version":"1.0"}],"b@m":[{"scope":"local","version":"2.0"}],"bad@m":[]}}"#).unwrap();
    fs::write(c.join("sync.json"), "{broken").unwrap();
    git(&d, &["init", "-q", "-b", "main"]);
    git(&d, &["commit", "-q", "--allow-empty", "-m", "first\ncontinued\n\nbody"]);
    for f in ["x", "a b.code-workspace", "aø.code-workspace", "b.code-workspace", "\t.code-workspace"] {
        fs::write(d.join(f), "").unwrap();
    }
    fs::create_dir(d.join("a.code-workspace")).unwrap();
    std::os::unix::fs::symlink("b.code-workspace", d.join("0.code-workspace")).unwrap();

    let paths = status::Paths { home: home.clone(), workspace: d.clone(), claude_dir: c.clone(), plugin_sync: c.join("sync.json"), proc_root: home.join("no-proc") };
    let s = serde_json::to_value(status::Server::new(paths).status()).unwrap();
    assert_eq!(s["branch"], "main", "{s}");
    assert_eq!(s["dirty"], 6, "{s}");
    assert_eq!(s["workspace_file"], "a b.code-workspace", "{s}");
    // %s folds the subject paragraph onto one line
    assert_eq!(s["last_commit"]["message"], "first continued", "{s}");
    assert!(s["last_commit"]["sha"].as_str().is_some_and(|h| h.len() >= 7) && s["last_commit"]["date"].as_str().is_some_and(|t| t.len() == 25), "{s}");
    assert!(s["ahead"].is_null() && s["behind"].is_null(), "{s}");
    assert_eq!(s["claude_plugins"], json!({"a@m": "1.0", "b@m": "2.0"}), "{s}");
    assert!(s["claude_plugin_sync"].is_null() && s["claude_logged_in"] == false, "{s}");
    let keys: Vec<&str> = s.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["repo", "tunnel_name", "branch", "dirty", "last_commit", "ahead", "behind", "tunnel", "mise", "workspace_file", "claude_installed", "claude_logged_in", "claude_version", "claude_plugins", "claude_plugin_sync", "uptime_seconds"], "{s}");
    assert_eq!(status::run(&["printf", r"caff\350"], Path::new("/")).as_deref(), Some("caff\u{FFFD}"), "a Latin-1 byte in a program's output must be replaced");

    // not a repository: every git field null
    let empty = scratch("empty");
    assert_eq!(status::git_json(&empty), status::GitJson::default());

    // pin: an upstream ahead by one, a subrepo with its upstream gone, a non-repo folder
    git(&d, &["remote", "add", "origin", "https://u:secret@example.com/a/shop.git"]);
    git(&d, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&d, &["branch", "-q", "--set-upstream-to=origin/main"]);
    git(&d, &["commit", "-q", "--allow-empty", "-m", "second"]);
    fs::create_dir(d.join("sub")).unwrap();
    git(&d.join("sub"), &["init", "-q", "-b", "feat"]);
    git(&d.join("sub"), &["commit", "-q", "--allow-empty", "-m", "s"]);
    git(&d.join("sub"), &["config", "branch.feat.remote", "origin"]);
    git(&d.join("sub"), &["config", "branch.feat.merge", "refs/heads/feat"]);
    git(&d.join("sub"), &["config", "remote.origin.url", "https://example.com/x.git"]);
    git(&d.join("sub"), &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
    fs::create_dir(d.join("plain")).unwrap();
    fs::write(d.join("x"), "one\ntwo\n").unwrap();
    git(&d, &["add", "x"]);
    let p = probes::pin_info(&d);
    let g = &p["git"];
    assert_eq!((g["branch"].as_str(), g["upstream"].as_bool(), g["gone"].as_bool(), g["ahead"].as_u64()), (Some("main"), Some(true), Some(false), Some(1)), "{p}");
    // sub/ counts as an untracked folder, like git shows it
    assert_eq!((g["untracked"].as_u64(), g["add"].as_u64(), g["files"].as_u64(), g["repo"].as_str()), (Some(6), Some(2), Some(1), Some("shop")), "{p}");
    assert_eq!(p["subs"], json!([{"branch": "feat", "oid": p["subs"][0]["oid"], "untracked": 0, "upstream": false, "gone": true, "ahead": 0, "dir": "sub"}]), "{p}");
    assert!(!p.to_string().contains("secret"), "the origin URL's credential must never leave the container");
    assert_eq!(probes::pin_info(&empty), json!({"git": null, "subs": []}));

    // a session's title is its transcript's last ai-title; activity sums the run's process tree
    let proj = home.join(".claude/projects/-home-dev-workspace");
    fs::create_dir_all(proj.join("s1/subagents")).unwrap();
    fs::write(proj.join("s1.jsonl"), "{\"type\":\"user\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"Old\"}\nnot json \"type\":\"ai-title\"\n{\"type\":\"ai-title\",\"aiTitle\":\"Fix login\"}\n").unwrap();
    fs::write(proj.join("s1/subagents/a.jsonl"), "{}\n").unwrap();
    assert_eq!((probes::title("s1", &home), probes::title("s2", &home)), ("Fix login".to_string(), String::new()));
    let proc_root = scratch("proc");
    let pid = |pid: u32, ppid: u32, cmd: &str, times: [u64; 4]| {
        let dir = proc_root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cmdline"), cmd.replace(' ', "\0") + "\0").unwrap();
        fs::write(dir.join("stat"), format!("{pid} (x) y) S {ppid} 1 1 0 -1 0 0 0 0 0 {} {} {} {} 20 0 1 0 100 0 0\n", times[0], times[1], times[2], times[3])).unwrap();
    };
    pid(10, 1, "timeout -k 30 14400 claude -p --model m --session-id s1 -- hi", [1, 1, 0, 0]);
    pid(11, 10, "claude", [100, 50, 20, 30]);
    pid(12, 11, "bash", [5, 5, 0, 0]);
    pid(13, 1, "timeout -k 30 14400 claude -p --session-id s2 -- hi", [999, 0, 0, 0]);
    let (cpu, age) = probes::activity("s1", &proc_root, &home).expect("run of s1 found");
    assert!((cpu - 2.12).abs() < 1e-9 && age.is_some_and(|a| (0..60).contains(&a)), "{cpu} {age:?}");
    assert_eq!(probes::activity("s3", &proc_root, &home), None);
    assert!(probes::claude_key(&home).starts_with("signed-out:"));

    // the same probes over HTTP, token and all, against the scratch home (its workspace is not a repo)
    crate::http::self_test(&status::Server::new(status::Paths { home: home.clone(), workspace: home.join("no-repo"), claude_dir: c.clone(), plugin_sync: c.join("sync.json"), proc_root: home.join("no-proc") }));

    for dir in [d, c, home, empty, proc_root] {
        let _ = fs::remove_dir_all(dir);
    }
    println!("ok {}", Value::from(s));
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_check_passes() {
        super::run();
    }
}
