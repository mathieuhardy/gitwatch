//! gitwatch — monitor a directory of git repositories and auto-sync them.
//!
//! Subcommands:
//!   waybar_status <dir> [-j N] [--no-fetch]   JSON for a Waybar custom module
//!   rofi_list     <dir> [-j N] [--fetch]      one "<icon> <name>" line per repo
//!   sync          <repo>                      commit + rebase-push with conflict escape branch
//!
//! Design notes:
//!  * We shell out to the `git` binary so fetch/push reuse the user's existing
//!    credential setup (ssh agent, credential helpers) with zero config.
//!  * `waybar_status` fetches every repo in parallel using a bounded pool of OS
//!    threads (the real network I/O lives inside the child `git` process, so a
//!    blocked thread costs almost nothing; -j caps how many run at once).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::Serialize;

// ----------------------------------------------------------------------------
// Icons
// ----------------------------------------------------------------------------
const ICON_BAR: &str = "\u{e0a0}"; // nerd-font git branch glyph, Waybar summary prefix
const I_CLEAN: &str = "\u{2713}"; //  synced & clean
const I_AHEAD: &str = "\u{2191}"; // ↑ commits to push
const I_BEHIND: &str = "\u{2193}"; // ↓ commits to pull
const I_DIVERGED: &str = "\u{21c5}"; // ⇅ ahead & behind
const I_DIRTY: &str = "\u{25cf}"; // ● local modifications
const I_CLEAN_DOT: &str = "\u{25cb}"; // ○ clean (rofi list)
const I_CONFLICT: &str = "\u{26a0}"; // ⚠ merge conflicts in tree
const I_UNPUBLISHED: &str = "\u{21e1}"; // ⇡ branch has no upstream (never pushed)
const I_ERROR: &str = "?"; // remote unreachable / other error

// ----------------------------------------------------------------------------
// git helpers
// ----------------------------------------------------------------------------
struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Run `git -C <dir> <args...>` and capture output.
fn git(dir: &Path, args: &[&str]) -> std::io::Result<GitOut> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    Ok(GitOut {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Convenience: run git, return trimmed stdout on success, or None on failure.
fn git_ok(dir: &Path, args: &[&str]) -> Option<String> {
    match git(dir, args) {
        Ok(o) if o.ok => Some(o.stdout.trim().to_string()),
        _ => None,
    }
}

fn is_git_worktree(dir: &Path) -> bool {
    matches!(git(dir, &["rev-parse", "--is-inside-work-tree"]), Ok(o) if o.ok && o.stdout.trim() == "true")
}

// ----------------------------------------------------------------------------
// bounded parallel map (shared work queue, capped worker count)
// ----------------------------------------------------------------------------
fn parallel_map<T, R>(items: Vec<T>, jobs: usize, f: impl Fn(T) -> R + Sync) -> Vec<R>
where
    T: Send,
    R: Send,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let jobs = jobs.max(1).min(n);
    let queue: Mutex<VecDeque<(usize, T)>> = Mutex::new(items.into_iter().enumerate().collect());
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());

    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let next = { queue.lock().unwrap().pop_front() };
                match next {
                    Some((idx, item)) => {
                        let r = f(item);
                        results.lock().unwrap()[idx] = Some(r);
                    }
                    None => break,
                }
            });
        }
    });

    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|o| o.expect("every slot filled"))
        .collect()
}

// ----------------------------------------------------------------------------
// repo status
// ----------------------------------------------------------------------------
#[derive(Serialize, Clone, Default)]
struct RepoStatus {
    name: String,
    path: String,
    branch: Option<String>,
    detached: bool,
    has_upstream: bool,
    ahead: usize,
    behind: usize,
    staged: usize,
    unstaged: usize,
    untracked: usize,
    conflicts: usize,
    dirty: bool,
    fetch_error: bool,
    error: Option<String>,
}

impl RepoStatus {
    /// A branch that exists but has never been pushed (no upstream configured).
    fn no_upstream(&self) -> bool {
        self.branch.is_some() && !self.detached && !self.has_upstream
    }

    fn is_clean_synced(&self) -> bool {
        self.error.is_none()
            && !self.dirty
            && self.conflicts == 0
            && self.ahead == 0
            && self.behind == 0
            && !self.no_upstream()
    }

    /// Single glyph summarizing the most important state of this repo.
    fn glyph(&self) -> &'static str {
        if self.error.is_some() {
            I_ERROR
        } else if self.conflicts > 0 {
            I_CONFLICT
        } else if self.no_upstream() {
            I_UNPUBLISHED
        } else if self.ahead > 0 && self.behind > 0 {
            I_DIVERGED
        } else if self.behind > 0 {
            I_BEHIND
        } else if self.ahead > 0 {
            I_AHEAD
        } else if self.dirty {
            I_DIRTY
        } else {
            I_CLEAN
        }
    }
}

/// Compute the status of a single repo, optionally fetching first.
fn status_of(path: &Path, do_fetch: bool) -> RepoStatus {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut st = RepoStatus {
        name,
        path: path.to_string_lossy().into_owned(),
        ..Default::default()
    };

    if !is_git_worktree(path) {
        st.error = Some("not a git repository".into());
        return st;
    }

    if do_fetch {
        match git(path, &["fetch", "--quiet", "--all", "--prune"]) {
            Ok(o) if o.ok => {}
            _ => st.fetch_error = true,
        }
    }

    let porcelain = match git(path, &["status", "--porcelain=v2", "--branch"]) {
        Ok(o) if o.ok => o.stdout,
        Ok(o) => {
            st.error = Some(format!("git status failed: {}", o.stderr.trim()));
            return st;
        }
        Err(e) => {
            st.error = Some(format!("git status error: {e}"));
            return st;
        }
    };

    parse_porcelain_v2(&porcelain, &mut st);

    st.dirty = st.staged + st.unstaged + st.untracked + st.conflicts > 0;
    if st.fetch_error {
        // Keep whatever local ahead/behind we could compute, but flag the failure.
        st.error = Some("remote unreachable (fetch failed)".into());
    }
    st
}

/// Parse `git status --porcelain=v2 --branch` output into a RepoStatus.
fn parse_porcelain_v2(text: &str, st: &mut RepoStatus) {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            let v = rest.trim();
            if v == "(detached)" {
                st.detached = true;
                st.branch = None;
            } else {
                st.branch = Some(v.to_string());
            }
        } else if line.starts_with("# branch.upstream ") {
            st.has_upstream = true;
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // format: "+<ahead> -<behind>"
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // "<T> <XY> ..." where XY is the two-char staged/worktree code
            if let Some(xy) = line.split_whitespace().nth(1) {
                let mut chars = xy.chars();
                let x = chars.next().unwrap_or('.');
                let y = chars.next().unwrap_or('.');
                if x != '.' {
                    st.staged += 1;
                }
                if y != '.' {
                    st.unstaged += 1;
                }
            }
        } else if line.starts_with("u ") {
            st.conflicts += 1;
        } else if line.starts_with("? ") {
            st.untracked += 1;
        }
        // "! " ignored entries are skipped
    }
}

// ----------------------------------------------------------------------------
// scanning
// ----------------------------------------------------------------------------
fn scan_repos(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() && p.join(".git").exists() {
            repos.push(p);
        }
    }
    repos.sort();
    Ok(repos)
}

// ----------------------------------------------------------------------------
// waybar_status
// ----------------------------------------------------------------------------
#[derive(Serialize)]
struct WaybarOut {
    text: String,
    tooltip: String,
    class: String,
}

fn cmd_waybar_status(dir: &Path, jobs: usize, do_fetch: bool) -> i32 {
    let repos = match scan_repos(dir) {
        Ok(r) => r,
        Err(e) => {
            let out = WaybarOut {
                text: format!("{ICON_BAR} err"),
                tooltip: format!("cannot scan {}: {e}", dir.display()),
                class: "error".into(),
            };
            println!("{}", serde_json::to_string(&out).unwrap());
            return 1;
        }
    };

    let statuses = parallel_map(repos, jobs, |p| status_of(&p, do_fetch));

    let total = statuses.len();
    let n_dirty = statuses.iter().filter(|s| s.dirty).count();
    let n_ahead = statuses
        .iter()
        .filter(|s| s.ahead > 0 && s.behind == 0)
        .count();
    let n_behind = statuses
        .iter()
        .filter(|s| s.behind > 0 && s.ahead == 0)
        .count();
    let n_diverged = statuses
        .iter()
        .filter(|s| s.ahead > 0 && s.behind > 0)
        .count();
    let n_conflict = statuses.iter().filter(|s| s.conflicts > 0).count();
    let n_unpublished = statuses.iter().filter(|s| s.no_upstream()).count();
    let n_error = statuses.iter().filter(|s| s.error.is_some()).count();

    // Compact text: only show non-zero groups.
    let mut count = 0;
    if n_ahead > 0 {
        count += 1;
    }
    if n_behind > 0 {
        count += 1;
    }
    if n_diverged > 0 {
        count += 1;
    }
    if n_dirty > 0 {
        count += 1;
    }
    if n_unpublished > 0 {
        count += 1;
    }
    if n_conflict > 0 {
        count += 1;
    }
    if n_error > 0 {
        count += 1;
    }
    let text = format!("{ICON_BAR} {count}");

    // CSS class by highest-priority condition.
    let class = if n_error > 0 {
        "error"
    } else if n_conflict > 0 {
        "conflict"
    } else if n_unpublished > 0 {
        "unpublished"
    } else if n_diverged > 0 {
        "diverged"
    } else if n_behind > 0 {
        "behind"
    } else if n_ahead > 0 {
        "ahead"
    } else if n_dirty > 0 {
        "dirty"
    } else {
        "clean"
    }
    .to_string();

    // Tooltip: problems first, then clean, one line per repo.
    let mut ordered: Vec<&RepoStatus> = statuses.iter().collect();
    ordered.sort_by_key(|s| (s.is_clean_synced(), s.name.to_lowercase()));

    let mut lines: Vec<String> = Vec::with_capacity(ordered.len() + 1);
    lines.push(format!("{} — {} repos", dir.display(), total));
    for s in ordered {
        let branch = s.branch.clone().unwrap_or_else(|| {
            if s.detached {
                "(detached)".into()
            } else {
                "?".into()
            }
        });
        let mut detail: Vec<String> = Vec::new();
        if s.ahead > 0 {
            detail.push(format!("{I_AHEAD}{}", s.ahead));
        }
        if s.behind > 0 {
            detail.push(format!("{I_BEHIND}{}", s.behind));
        }
        let local = s.staged + s.unstaged + s.untracked;
        if local > 0 {
            detail.push(format!("{I_DIRTY}{local}"));
        }
        if s.conflicts > 0 {
            detail.push(format!("{I_CONFLICT}{}", s.conflicts));
        }
        if s.no_upstream() {
            detail.push(format!("{I_UNPUBLISHED}unpushed"));
        }
        if let Some(err) = &s.error {
            detail.push(format!("({err})"));
        }
        let detail = if detail.is_empty() {
            format!("  {}", I_CLEAN)
        } else {
            format!("  {}", detail.join(" "))
        };
        lines.push(format!("{:<24} {}", s.name, detail));
    }

    let out = WaybarOut {
        text,
        tooltip: lines.join("\n"),
        class,
    };
    println!("{}", serde_json::to_string(&out).unwrap());
    0
}

// ----------------------------------------------------------------------------
// rofi_list
// ----------------------------------------------------------------------------
fn cmd_rofi_list(dir: &Path, jobs: usize, do_fetch: bool) -> i32 {
    let repos = match scan_repos(dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot scan {}: {e}", dir.display());
            return 1;
        }
    };
    let statuses = parallel_map(repos, jobs, |p| status_of(&p, do_fetch));

    // dirty first, then clean; each group alphabetical. Format "<icon> <name>"
    // so the caller's `${chosen#* }` extraction keeps working.
    let mut dirty: Vec<&RepoStatus> = statuses.iter().filter(|s| s.dirty).collect();
    let mut clean: Vec<&RepoStatus> = statuses.iter().filter(|s| !s.dirty).collect();
    dirty.sort_by_key(|s| s.name.to_lowercase());
    clean.sort_by_key(|s| s.name.to_lowercase());

    for s in dirty {
        println!("{} {}", I_DIRTY, s.name);
    }
    for s in clean {
        println!("{} {}", I_CLEAN_DOT, s.name);
    }
    0
}

// ----------------------------------------------------------------------------
// sync  (port of the auto-sync bash script)
// ----------------------------------------------------------------------------
fn now_iso() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string()
}

fn hostname_short() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    h.split('.').next().unwrap_or(&h).to_string()
}

fn log(msg: &str) {
    eprintln!("{} {}", now_iso(), msg);
}

/// Exit the process the way the bash `die` did: log ERROR then exit 1.
fn die(msg: &str) -> ! {
    log(&format!("ERROR: {msg}"));
    std::process::exit(1);
}

fn cmd_sync(repo: &Path) -> i32 {
    if !repo.exists() {
        die(&format!("cannot access {}", repo.display()));
    }
    if !is_git_worktree(repo) {
        die(&format!("{} is not a git repository", repo.display()));
    }

    // Concurrency lock at <git-dir>/auto-sync.lock (non-blocking flock).
    let gitdir = git_ok(repo, &["rev-parse", "--absolute-git-dir"])
        .unwrap_or_else(|| die("cannot resolve git dir"));
    let lock_path = Path::new(&gitdir).join("auto-sync.lock");
    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => die(&format!(
            "cannot open lock file {}: {e}",
            lock_path.display()
        )),
    };
    {
        use fs2::FileExt;
        if lock_file.try_lock_exclusive().is_err() {
            die("another run in progress");
        }
    }
    // lock_file is held until it drops at end of function.

    // Current branch (fail on detached HEAD, like the script).
    let branch = match git(repo, &["symbolic-ref", "--short", "-q", "HEAD"]) {
        Ok(o) if o.ok && !o.stdout.trim().is_empty() => o.stdout.trim().to_string(),
        _ => die("detached HEAD, doing nothing"),
    };

    // 1. Commit everything if the tree is not clean.
    let porcelain = git(repo, &["status", "--porcelain"])
        .map(|o| o.stdout)
        .unwrap_or_default();
    if !porcelain.trim().is_empty() {
        if !git(repo, &["add", "-A"]).map(|o| o.ok).unwrap_or(false) {
            die("git add failed");
        }
        let msg = format!("auto: {} on {}", now_iso(), hostname_short());
        let committed = git(repo, &["commit", "-m", &msg])
            .map(|o| o.ok)
            .unwrap_or(false);
        if !committed {
            die("commit failed");
        }
        log(&format!("commit done on {branch}"));
    }

    // Fetch.
    if !git(repo, &["fetch", "--quiet"])
        .map(|o| o.ok)
        .unwrap_or(false)
    {
        die("git fetch has failed");
    }

    let upstream = format!("origin/{branch}");

    // 2. No upstream yet => first push.
    let upstream_exists = git(repo, &["rev-parse", "--verify", "--quiet", &upstream])
        .map(|o| o.ok)
        .unwrap_or(false);
    if !upstream_exists {
        if !git(repo, &["push", "-u", "origin", &branch])
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            die("first push failed");
        }
        log(&format!("branch {branch} pushed for the first time"));
        return 0;
    }

    // 3. Rebase onto upstream; push on success, escape branch on conflict.
    let rebased = git(repo, &["rebase", "--quiet", &upstream])
        .map(|o| o.ok)
        .unwrap_or(false);
    if rebased {
        if git(repo, &["push", "--quiet"])
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            log(&format!("synchronized with {upstream}"));
            0
        } else {
            die("push refused");
        }
    } else {
        // Abort the failed rebase and stash the local work on a safety branch.
        let _ = git(repo, &["rebase", "--abort"]);
        let safety = format!("sync/conflit-{branch}-{}", chrono::Local::now().timestamp());
        if !git(repo, &["checkout", "-q", "-b", &safety])
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            die(&format!("cannot create safety branch {safety}"));
        }
        // Fast-forward local <branch> to match upstream (branch is not checked out now).
        if !git(repo, &["branch", "-f", &branch, &upstream])
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            die(&format!("cannot reset {branch} to {upstream}"));
        }
        if !git(repo, &["push", "-u", "--quiet", "origin", &safety])
            .map(|o| o.ok)
            .unwrap_or(false)
        {
            die(&format!("cannot push the safety branch {safety}"));
        }
        log(&format!(
            "CONFLICT with {upstream}: work has been stored on {safety} (HEAD is now on it)"
        ));
        log(&format!("  {branch} has been reset to {upstream}"));
        2
    }
}

// ----------------------------------------------------------------------------
// CLI
// ----------------------------------------------------------------------------
const USAGE: &str = "\
gitwatch — monitor a directory of git repos and auto-sync them

USAGE:
    gitwatch <COMMAND> [ARGS]

COMMANDS:
    waybar_status <dir> [-j N] [--no-fetch]
        Fetch every repo under <dir> (in parallel) and print a single JSON line
        for a Waybar custom module (fields: text, tooltip, class).

    rofi_list <dir> [-j N] [--fetch]
        Print one \"<icon> <name>\" line per repo (dirty first, then clean).
        Does NOT fetch by default (fast local check); pass --fetch to fetch.

    sync <repo>
        Commit all changes, fetch, rebase onto origin/<branch> and push.
        On conflict: abort, move local work to a sync/conflit-* branch, push it,
        and reset <branch> to the upstream. Exit codes: 0 ok, 2 conflict, 1 error.

OPTIONS:
    -j N        max concurrent git processes for scans (default 16)
    --no-fetch  skip fetching (waybar_status)
    --fetch     force fetching (rofi_list)
    -h, --help  show this help
    --version   show version
";

fn expand_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        eprint!("{USAGE}");
        std::process::exit(2);
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            return;
        }
        "--version" => {
            println!("gitwatch {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        _ => {}
    }

    let command = args[0].clone();
    let rest = &args[1..];

    // Shared flag parsing.
    let mut positional: Vec<String> = Vec::new();
    let mut jobs: usize = 16;
    let mut fetch_override: Option<bool> = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-j" | "--jobs" => {
                let v = it.next().unwrap_or_else(|| {
                    eprintln!("-j requires a number");
                    std::process::exit(2);
                });
                jobs = v.parse().unwrap_or_else(|_| {
                    eprintln!("invalid -j value: {v}");
                    std::process::exit(2);
                });
            }
            "--no-fetch" => fetch_override = Some(false),
            "--fetch" => fetch_override = Some(true),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}");
                std::process::exit(2);
            }
            other => positional.push(other.to_string()),
        }
    }

    let code = match command.as_str() {
        "waybar_status" => {
            let dir = require_positional(&positional, "waybar_status <dir>");
            let do_fetch = fetch_override.unwrap_or(true); // status fetches by default
            cmd_waybar_status(&expand_path(&dir), jobs, do_fetch)
        }
        "rofi_list" => {
            let dir = require_positional(&positional, "rofi_list <dir>");
            let do_fetch = fetch_override.unwrap_or(false); // list is local/fast by default
            cmd_rofi_list(&expand_path(&dir), jobs, do_fetch)
        }
        "sync" => {
            let repo = require_positional(&positional, "sync <repo>");
            let repo = expand_path(&repo);
            let repo = repo.canonicalize().unwrap_or(repo);
            cmd_sync(&repo)
        }
        other => {
            eprintln!("unknown command: {other}\n");
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn require_positional(positional: &[String], usage: &str) -> String {
    match positional.first() {
        Some(p) => p.clone(),
        None => {
            eprintln!("missing argument\nusage: gitwatch {usage}");
            std::process::exit(2);
        }
    }
}
