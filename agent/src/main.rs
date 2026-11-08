//! dockbench-agent: the DockBench dev container's resident helper. With no argument it serves `GET /status` on :8080 for the dashboard (it replaces status_server.py); the subcommands answer the Telegram bot's `docker exec` probes.

mod check;
mod git;
mod http;
mod probes;
mod status;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "usage: dockbench-agent [serve | status | pin-info [dir] | activity <session-id> | title <session-id> | claude-key | --check | --version]";

fn home() -> PathBuf {
    status::Paths::from_env().home
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    match arg(0).unwrap_or("serve") {
        "serve" => {
            let listener = TcpListener::bind(("0.0.0.0", 8080)).unwrap_or_else(|e| {
                eprintln!("dockbench-agent: cannot listen on :8080: {e}");
                std::process::exit(1);
            });
            http::serve(listener, Arc::new(status::Server::new(status::Paths::from_env())));
        }
        "status" => println!("{}", serde_json::to_string(&status::Server::new(status::Paths::from_env()).status()).unwrap_or_default()),
        "pin-info" => {
            let dir = arg(1).map_or_else(|| status::Paths::from_env().workspace, PathBuf::from);
            println!("{}", probes::pin_info(&dir));
        }
        // the bot's claude.activity(): "<cpu seconds> <seconds since the transcript was written, -1 for none>", exit 3 when the run is not found
        "activity" => match arg(1).and_then(|sid| probes::activity(sid, Path::new("/proc"), &home())) {
            Some((cpu, age)) => println!("{cpu} {}", age.unwrap_or(-1)),
            None => std::process::exit(3),
        },
        "title" => match arg(1) {
            Some(sid) => print!("{}", probes::title(sid, &home())),
            None => fail(),
        },
        "claude-key" => println!("{}", probes::claude_key(&home())),
        "--check" => check::run(),
        "--version" => println!("dockbench-agent {}", env!("CARGO_PKG_VERSION")),
        _ => fail(),
    }
}

fn fail() {
    eprintln!("{USAGE}");
    std::process::exit(2);
}
