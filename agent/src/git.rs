//! Git facts of a repository read in-process with gix: no `git` program is started, except `git diff HEAD --numstat` for the bot's pin while tracked files changed (`numstat()`).

use gix::bstr::{BString, ByteSlice};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

/// What `/status` and the bot's pin need, computed in one pass.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Facts {
    /// Short branch name, `None` on a detached HEAD.
    pub branch: Option<String>,
    /// Full hex id of HEAD, `None` on an unborn branch (no commit yet).
    pub head: Option<String>,
    /// Abbreviated id, subject line and committer date (ISO 8601 with offset) of HEAD.
    pub last_commit: Option<(String, String, String)>,
    /// Whether the branch has an upstream configured (`branch.<name>.merge`), whether or not its ref exists.
    pub upstream_configured: bool,
    /// `(ahead, behind)` against the upstream's local tracking ref, `None` when there is none or it is gone.
    pub ab: Option<(u32, u32)>,
    /// Lines `git status --porcelain` would print: changed paths (staged, unstaged, conflicted) plus untracked entries.
    pub dirty: Option<usize>,
    /// Untracked entries (a wholly untracked folder counts once, like git).
    pub untracked: usize,
    /// Paths with tracked changes, staged or not.
    pub tracked_changes: usize,
}

/// Opens the repository at `dir` (its root, not a subfolder); `None` when it is not a git repository.
pub fn open(dir: &Path) -> Option<gix::Repository> {
    // default options read the system and user config like git does: status.showUntrackedFiles or a global excludesFile can live there
    gix::open(dir).ok()
}

/// Facts of the repository at `dir`. `with_status` false skips the worktree walk (`dirty`, `untracked`, `tracked_changes` stay at their defaults), for repos where only the branch line matters.
pub fn facts(dir: &Path, with_status: bool) -> Option<Facts> {
    let repo = open(dir)?;
    let mut f = Facts::default();
    let head_name = repo.head_name().ok()?;
    f.branch = head_name.as_ref().map(|n| n.shorten().to_string());
    let head_id = repo.head_id().ok();
    if let Some(id) = &head_id {
        f.head = Some(id.to_string());
        if let Ok(commit) = id.object().and_then(|o| Ok(o.into_commit())) {
            let short = id.shorten_or_id().to_string();
            let subject = commit.message().map(|m| m.summary().to_str_lossy().into_owned()).unwrap_or_default();
            let date = commit.committer().ok().map(|c| c.time().ok()).flatten().map(iso8601).unwrap_or_default();
            f.last_commit = Some((short, subject, date));
        }
    }
    if let Some(name) = &head_name {
        f.upstream_configured = repo.branch_remote_ref_name(name.as_ref(), gix::remote::Direction::Fetch).is_some_and(|r| r.is_ok());
        let tracking = repo.branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch).and_then(|r| r.ok());
        let up = tracking.and_then(|t| repo.find_reference(t.as_ref()).ok()).and_then(|mut r| r.peel_to_id().ok().map(|i| i.detach()));
        if let (Some(up), Some(head)) = (up, &head_id) {
            let head = head.detach();
            let ahead = repo.rev_walk([head]).with_hidden([up]).all().ok().map(|w| w.filter(|c| c.is_ok()).count() as u32);
            let behind = repo.rev_walk([up]).with_hidden([head]).all().ok().map(|w| w.filter(|c| c.is_ok()).count() as u32);
            f.ab = ahead.zip(behind);
        }
    }
    if with_status {
        if let Some((dirty, untracked, tracked)) = worktree(&repo) {
            f.dirty = Some(dirty);
            f.untracked = untracked;
            f.tracked_changes = tracked;
        }
    }
    Some(f)
}

/// `(porcelain lines, untracked entries, paths with tracked changes)`. A path changed both in the index and the worktree is one line, like in git.
fn worktree(repo: &gix::Repository) -> Option<(usize, usize, usize)> {
    static NEVER: AtomicBool = AtomicBool::new(false);
    let iter = repo.status(gix::progress::Discard).ok()?.should_interrupt_shared(&NEVER).into_iter(Vec::<BString>::new()).ok()?;
    let (mut lines, mut tracked) = (BTreeSet::<BString>::new(), BTreeSet::<BString>::new());
    let mut untracked = 0;
    for item in iter {
        let item = item.ok()?;
        match &item {
            gix::status::Item::IndexWorktree(it) => {
                let Some(summary) = it.summary() else { continue };
                if matches!(it, gix::status::index_worktree::Item::DirectoryContents { .. }) {
                    if matches!(summary, gix::status::index_worktree::iter::Summary::Added) {
                        untracked += 1;
                        lines.insert(it.rela_path().to_owned());
                    }
                    continue;
                }
                tracked.insert(it.rela_path().to_owned());
                lines.insert(it.rela_path().to_owned());
            }
            gix::status::Item::TreeIndex(change) => {
                tracked.insert(change.location().to_owned());
                lines.insert(change.location().to_owned());
            }
        }
    }
    Some((lines.len(), untracked, tracked.len()))
}

/// `remote.origin.url`, `None` without an origin.
pub fn origin_url(dir: &Path) -> Option<String> {
    let repo = open(dir)?;
    let remote = repo.find_remote("origin").ok()?;
    remote.url(gix::remote::Direction::Fetch).map(|u| u.to_bstring().to_str_lossy().into_owned())
}

/// Added lines, deleted lines and changed files of `git diff HEAD --numstat` (a binary file counts as a file with no lines); zeros when git fails (no HEAD yet). The one place a `git` program runs: line diffs with git's exact rename and filter rules are not worth reimplementing for a pin.
pub fn numstat(dir: &Path) -> (u64, u64, u64) {
    let out = Command::new("git").args(["--no-optional-locks", "diff", "HEAD", "--numstat"]).current_dir(dir).output();
    let Ok(out) = out else { return (0, 0, 0) };
    if !out.status.success() {
        return (0, 0, 0);
    }
    parse_numstat(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_numstat(out: &str) -> (u64, u64, u64) {
    let (mut add, mut del, mut files) = (0, 0, 0);
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 || !parts[..2].iter().all(|p| *p == "-" || (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))) {
            continue;
        }
        files += 1;
        add += parts[0].parse::<u64>().unwrap_or(0);
        del += parts[1].parse::<u64>().unwrap_or(0);
    }
    (add, del, files)
}

/// Strict ISO 8601 like git's `%cI`: local time of the signature with its offset, `2026-11-08T12:34:56+01:00`.
pub fn iso8601(t: gix::date::Time) -> String {
    let local = t.seconds + t.offset as i64;
    let (days, secs) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    let sign = if t.offset < 0 { '-' } else { '+' };
    let off = t.offset.unsigned_abs();
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}", secs / 3600, secs % 3600 / 60, secs % 60, off / 3600, off % 3600 / 60)
}

/// Days since 1970-01-01 to (year, month, day), Howard Hinnant's algorithm.
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_matches_git() {
        assert_eq!(iso8601(gix::date::Time { seconds: 0, offset: 0 }), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso8601(gix::date::Time { seconds: 1_794_150_896, offset: 3600 }), "2026-11-08T16:14:56+01:00");
        assert_eq!(iso8601(gix::date::Time { seconds: 951_782_400, offset: -19_800 }), "2000-02-28T18:30:00-05:30");
    }

    #[test]
    fn numstat_counts_binary_files_without_lines() {
        assert_eq!(parse_numstat("3\t1\ta.py\n-\t-\timg.png\n10\t0\tb.py\nwarning: CRLF\n"), (13, 1, 3));
    }
}
