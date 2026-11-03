import json
import os
import shutil
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOME = os.path.expanduser("~")
WORKSPACE = f"{HOME}/workspace"
CLAUDE_DIR = f"{HOME}/.claude"
# written by the entrypoint's background plugin sync, per container
PLUGIN_SYNC = "/var/lib/dockbench/claude-plugins.json"
START = time.time()


def run(args, cwd=None):
    try:
        # errors="replace": a commit subject in Latin-1 would otherwise raise UnicodeDecodeError and drop the request
        r = subprocess.run(args, cwd=cwd or WORKSPACE, capture_output=True, text=True, errors="replace", timeout=5)
    except (OSError, subprocess.TimeoutExpired):
        return None
    return r.stdout.strip() if r.returncode == 0 else None


def git_info():
    info = {"branch": None, "dirty": None, "last_commit": None, "ahead": None, "behind": None}
    branch = run(["git", "rev-parse", "--abbrev-ref", "HEAD"])
    if branch is None:
        return info
    info["branch"] = branch
    status = run(["git", "--no-optional-locks", "status", "--porcelain"])  # never take index.lock under the user's own git commands
    info["dirty"] = len(status.splitlines()) if status is not None else None
    log = run(["git", "log", "-1", "--format=%h%n%s%n%cI"])
    if log:
        sha, msg, date = (log.split("\n") + [None, None, None])[:3]
        info["last_commit"] = {"sha": sha, "message": msg, "date": date}
    counts = run(["git", "rev-list", "--left-right", "--count", "@{u}...HEAD"])
    if counts:
        behind, ahead = counts.split()
        info["behind"], info["ahead"] = int(behind), int(ahead)
    return info


def tunnel_state():
    out = run(["code", "tunnel", "status"], cwd=HOME)
    if out is None:
        return "unknown"
    try:
        t = json.loads(out).get("tunnel")
    except ValueError:
        return "unknown"
    if not t:
        return "down"
    if t.get("tunnel") == "Connected":
        return "connected"
    return "waiting_for_login" if t.get("tunnel_id") is None else "down"


def mise_tools():
    out = run(["mise", "ls", "--current", "--json"])
    if not out:
        return {}
    try:
        data = json.loads(out)
    except ValueError:
        return {}
    return {tool: entries[0].get("version") for tool, entries in data.items() if entries}


def workspace_file():
    # only the bare name, never the folder listing: the dashboard needs nothing more to link the workspace. Regular files only (no symlink, no directory); any printable name, the dashboard URL-encodes it
    try:
        with os.scandir(WORKSPACE) as entries:
            names = sorted(e.name for e in entries if e.name.endswith(".code-workspace") and e.name != ".code-workspace" and e.name.isprintable() and e.is_file(follow_symlinks=False))
    except OSError:
        return None
    return names[0] if names else None


def read_json(path):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def claude_info():
    # files only, no subprocess: the native installer links ~/.local/bin/claude to ~/.local/share/claude/versions/<version>
    path = shutil.which("claude")
    name = os.path.basename(os.path.realpath(path)) if path else ""
    installed = read_json(f"{CLAUDE_DIR}/plugins/installed_plugins.json")
    plugins = {}
    if isinstance(installed, dict) and isinstance(installed.get("plugins"), dict):
        for plugin, entries in installed["plugins"].items():
            if isinstance(entries, list) and entries and isinstance(entries[0], dict):
                user = [e for e in entries if isinstance(e, dict) and e.get("scope") == "user"]
                plugins[plugin] = (user or entries)[0].get("version")
    sync = read_json(PLUGIN_SYNC)
    return {
        "claude_version": name if name[:1].isdigit() else None,
        "claude_plugins": plugins,
        "claude_plugin_sync": sync if isinstance(sync, dict) else None,
    }


def status():
    return {
        "repo": os.environ.get("REPO_URL"),
        "tunnel_name": os.environ.get("TUNNEL_NAME"),
        **git_info(),
        "tunnel": tunnel_state(),
        "mise": mise_tools(),
        "workspace_file": workspace_file(),
        "claude_installed": shutil.which("claude") is not None,
        "claude_logged_in": os.path.exists(f"{HOME}/.claude/.credentials.json"),
        **claude_info(),
        "uptime_seconds": int(time.time() - START),
    }


class Handler(BaseHTTPRequestHandler):
    timeout = 5  # a peer that connects and sends nothing is dropped instead of holding a thread forever

    def do_GET(self):
        if self.path != "/status":
            self.send_error(404)
            return
        body = json.dumps(status()).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def self_check():
    import tempfile
    global WORKSPACE, CLAUDE_DIR, PLUGIN_SYNC
    with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as c:
        WORKSPACE = d
        CLAUDE_DIR = c
        PLUGIN_SYNC = f"{c}/sync.json"
        os.mkdir(f"{c}/plugins")
        with open(f"{c}/plugins/installed_plugins.json", "w") as f:
            json.dump({"version": 2, "plugins": {"a@m": [{"scope": "project", "version": "0.1"}, {"scope": "user", "version": "1.0"}], "b@m": [{"scope": "local", "version": "2.0"}], "bad@m": []}}, f)
        with open(PLUGIN_SYNC, "w") as f:
            f.write("{broken")
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=d, check=True)
        subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "first"], cwd=d, check=True)
        open(f"{d}/x", "w").close()
        open(f"{d}/a b.code-workspace", "w").close()
        os.mkdir(f"{d}/a.code-workspace")
        open(f"{d}/aø.code-workspace", "w").close()
        open(f"{d}/b.code-workspace", "w").close()
        # Windows rejects a tab in a file name and needs privileges for a symlink
        if os.name != "nt":
            os.symlink("b.code-workspace", f"{d}/0.code-workspace")
            open(f"{d}/\t.code-workspace", "w").close()
        s = status()
        assert run(["printf", r"caff\xe8"]) == "caff�", "a Latin-1 byte in git output must be replaced, not raise"
    assert s["branch"] == "main", s
    assert s["dirty"] == (4 if os.name == "nt" else 6), s
    assert s["workspace_file"] == "a b.code-workspace", s
    assert s["last_commit"]["message"] == "first", s
    assert s["ahead"] is None and s["behind"] is None, s
    assert s["claude_plugins"] == {"a@m": "1.0", "b@m": "2.0"}, s
    assert s["claude_plugin_sync"] is None, s
    assert set(s) == {"repo", "tunnel_name", "branch", "dirty", "last_commit", "ahead", "behind", "tunnel", "mise", "workspace_file", "claude_installed", "claude_logged_in", "claude_version", "claude_plugins", "claude_plugin_sync", "uptime_seconds"}, s
    print("ok", json.dumps(s))


if __name__ == "__main__":
    if "--check" in sys.argv:
        self_check()
    else:
        # threads: one stuck or slow request (git can take 5 s each) must not queue every later poll behind it
        ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
