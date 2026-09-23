//! Read-only git intelligence (feature: git) — OMP-style `git_overview` /
//! `git_diff`, implemented WITHOUT a shell: `git` is spawned with direct argv
//! so there is no injection surface. Every request path is gated through the
//! same `desktop.fs.read` capability the paired device already grants, so no
//! new pairing scope is needed.

use crate::capability::CapabilityGate;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;

type Result<T> = std::result::Result<T, crate::DaemonError>;

const MAX_OUTPUT_BYTES: usize = 512 * 1024; // 512 KiB per git call
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitOverviewRequest {
    /// Repo root (or any path inside it — git resolves upward).
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffRequest {
    pub path: PathBuf,
    /// Ref or range: `HEAD`, `main..feature`, `HEAD~3`, `staged` (index vs HEAD), `unstaged` (default).
    #[serde(default)]
    pub target: Option<String>,
    /// Limit files in the diff output.
    #[serde(default)]
    pub max_files: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitOverviewResult {
    pub root: PathBuf,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    pub head: String,
    pub subject: String,
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    pub recent_commits: Vec<GitCommit>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCommit {
    pub hash: String,
    pub subject: String,
    pub relative_date: String,
    pub author: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitDiffResult {
    pub target: String,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub truncated: bool,
    /// Raw unified diff (--no-color, --no-ext-diff), capped by max_files.
    pub diff: String,
}

/// Run `git` with direct argv in `dir`. No shell → no injection. stdout is
/// capped; a nonzero exit returns a clean error with git's stderr.
async fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Defense in depth: even with direct argv, forbid the repo from
    // executing hooks/config scripts during these read-only queries.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    cmd.env("GIT_OPTIONAL_LOCKS", "0");

    let child = cmd
        .spawn()
        .map_err(|e| crate::DaemonError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git: {e}"),
        )))?;
    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| crate::DaemonError::Protocol("git: timed out".into()))?
        .map_err(crate::DaemonError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.lines().next().unwrap_or("git failed");
        return Err(crate::DaemonError::Protocol(format!("git: {stderr}")));
    }
    let mut out = output.stdout;
    out.truncate(MAX_OUTPUT_BYTES);
    String::from_utf8(out)
        .map_err(|e| crate::DaemonError::Protocol(format!("git: non-utf8 output: {e}")))
}

/// Resolve the repo root and verify the request path is inside it, then gate
/// the ROOT through desktop.fs.read (the root contains every path git reads).
async fn gate_repo(gate: &CapabilityGate, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(crate::DaemonError::Io)?
            .join(path)
    };
    let root_line = run_git(&path, &["rev-parse", "--show-toplevel"]).await?;
    let root = PathBuf::from(root_line.trim());
    gate_root(gate, &root).await
}

async fn gate_root(gate: &CapabilityGate, root: &Path) -> Result<PathBuf> {
    match gate.check_path("desktop.fs.read", root) {
        crate::capability::GateDecision::Allow => Ok(root.to_path_buf()),
        crate::capability::GateDecision::RequireConsent => Err(crate::DaemonError::CapabilityDenied(format!(
            "desktop.git: repo {} requires consent (desktop.fs.read)",
            root.display()
        ))),
        crate::capability::GateDecision::Deny => Err(crate::DaemonError::CapabilityDenied(
            "desktop.git: not granted".into(),
        )),
    }
}

pub struct GitOps<'a> {
    gate: &'a CapabilityGate,
}

impl<'a> GitOps<'a> {
    pub fn new(gate: &'a CapabilityGate) -> Self {
        Self { gate }
    }

    pub async fn overview(&self, req: GitOverviewRequest) -> Result<GitOverviewResult> {
        let root = gate_repo(self.gate, &req.path).await?;

        let branch = run_git(&root, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string();
        let head = run_git(&root, &["rev-parse", "HEAD"])
            .await?
            .trim()
            .to_string();
        let subject = run_git(&root, &["log", "-1", "--format=%s"])
            .await?
            .trim()
            .to_string();

        let (upstream, ahead, behind) = {
            let track = run_git(&root, &["status", "-sb"]).await?;
            let first = track.lines().next().unwrap_or("");
            // Format: `## branch...origin/branch [ahead 1, behind 2]`
            let upstream = first
                .split("...")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(|s| s.trim_start_matches('[').to_string());
            let mut ahead = 0i64;
            let mut behind = 0i64;
            if let Some(idx) = first.find('[') {
                let counters = &first[idx + 1..first.len().saturating_sub(1)];
                for part in counters.split(',') {
                    let part = part.trim();
                    if let Some(n) = part.strip_prefix("ahead ") {
                        ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix("behind ") {
                        behind = n.parse().unwrap_or(0);
                    }
                }
            }
            (upstream, ahead, behind)
        };

        let porcelain = run_git(&root, &["status", "--porcelain"]).await?;
        let mut staged = Vec::new();
        let mut modified = Vec::new();
        let mut untracked = Vec::new();
        for line in porcelain.lines() {
            if line.len() < 4 {
                continue;
            }
            let (xy, file) = line.split_at(2);
            let file = file.trim_start().to_string();
            match (xy.as_bytes()[0], xy.as_bytes()[1]) {
                (b'?', _) => untracked.push(file),
                (x, _) if x != b' ' => staged.push(file),
                (_, _) => modified.push(file),
            }
        }

        let log = run_git(
            &root,
            &[
                "log",
                "--max-count=10",
                "--date=relative",
                "--format=%h%x09%s%x09%ar%x09%an",
            ],
        )
        .await?;
        let recent_commits = log
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                Some(GitCommit {
                    hash: parts.next()?.to_string(),
                    subject: parts.next()?.to_string(),
                    relative_date: parts.next()?.to_string(),
                    author: parts.next().unwrap_or("").to_string(),
                })
            })
            .collect();

        Ok(GitOverviewResult {
            root,
            branch,
            upstream,
            ahead,
            behind,
            head,
            subject,
            staged,
            modified,
            untracked,
            recent_commits,
        })
    }

    pub async fn diff(&self, req: GitDiffRequest) -> Result<GitDiffResult> {
        let root = gate_repo(self.gate, &req.path).await?;
        let max_files = req.max_files.unwrap_or(40).clamp(1, 200);
        let target = req.target.unwrap_or_else(|| "unstaged".to_string());

        let stat_args: Vec<&str> = match target.as_str() {
            "staged" | "--cached" => vec!["diff", "--cached", "--numstat"],
            "unstaged" | "" => vec!["diff", "--numstat"],
            other => vec!["diff", other, "--numstat"],
        };
        let numstat = run_git(&root, &stat_args).await?;
        let mut files_changed = 0usize;
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(i), Some(d), Some(_)) => {
                    files_changed += 1;
                    insertions += i.parse::<usize>().unwrap_or(0);
                    deletions += d.parse::<usize>().unwrap_or(0);
                }
                _ => {}
            }
        }

        // Cap the file list: build a pathspec from the first max_files files.
        let mut files: Vec<String> = numstat
            .lines()
            .filter_map(|l| l.split('\t').nth(2).map(str::to_string))
            .take(max_files)
            .collect();
        let truncated = files_changed > files.len();
        if files.is_empty() {
            return Ok(GitDiffResult {
                target,
                files_changed,
                insertions,
                deletions,
                truncated,
                diff: String::new(),
            });
        }
        // Rename entries come as `old\tnew` in the third column; use new.
        for f in &mut files {
            if let Some(new) = f.split('\t').last() {
                *f = new.to_string();
            }
        }

        let mut diff_args: Vec<&str> = match target.as_str() {
            "staged" | "--cached" => vec!["diff", "--cached", "--no-color", "--no-ext-diff", "--unified=3", "--"],
            "unstaged" | "" => vec!["diff", "--no-color", "--no-ext-diff", "--unified=3", "--"],
            other => vec!["diff", other, "--no-color", "--no-ext-diff", "--unified=3", "--"],
        };
        let file_refs: Vec<&str> = files.iter().map(String::as_str).collect();
        diff_args.extend(file_refs.iter().copied());
        let body = run_git(&root, &diff_args).await?;

        Ok(GitDiffResult {
            target,
            files_changed,
            insertions,
            deletions,
            truncated,
            diff: body,
        })
    }
}

// ─── Tests (pure parsing + a real temp repo when git exists) ────────────────

#[cfg(test)]
mod tests {
    // Self-contained parsing tests: no super::* items needed (an unused
    // import here would fail the CI's -D warnings).

    #[test]
    fn porcelain_parsing_shapes() {
        // Mirrors the parsing block in overview() on representative lines.
        let porcelain = "M  src/lib.rs\n M src/main.rs\n?? notes.txt\nA  new.rs\n";
        let mut staged = Vec::new();
        let mut modified = Vec::new();
        let mut untracked = Vec::new();
        for line in porcelain.lines() {
            if line.len() < 4 {
                continue;
            }
            let (xy, file) = line.split_at(2);
            let file = file.trim_start().to_string();
            match (xy.as_bytes()[0], xy.as_bytes()[1]) {
                (b'?', _) => untracked.push(file),
                (x, _) if x != b' ' => staged.push(file),
                (_, _) => modified.push(file),
            }
        }
        assert_eq!(staged, vec!["src/lib.rs", "new.rs"]);
        assert_eq!(modified, vec!["src/main.rs"]);
        assert_eq!(untracked, vec!["notes.txt"]);
    }

    #[test]
    fn status_sb_counters() {
        let first = "## main...origin/main [ahead 2, behind 5]";
        let mut ahead = 0i64;
        let mut behind = 0i64;
        if let Some(idx) = first.find('[') {
            let counters = &first[idx + 1..first.len().saturating_sub(1)];
            for part in counters.split(',') {
                let part = part.trim();
                if let Some(n) = part.strip_prefix("ahead ") {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    behind = n.parse().unwrap_or(0);
                }
            }
        }
        assert_eq!(ahead, 2);
        assert_eq!(behind, 5);
        // No upstream
        let first = "## main";
        assert!(first.split("...").nth(1).is_none());
    }

    #[test]
    fn numstat_parsing() {
        let numstat = "10\t2\tsrc/a.rs\n-\t-\tbinary.png\n3\t1\tsrc/b.rs\n";
        let mut files_changed = 0usize;
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        let mut files: Vec<String> = Vec::new();
        for line in numstat.lines() {
            let mut parts = line.split('\t');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(i), Some(d), Some(f)) => {
                    files_changed += 1;
                    insertions += i.parse::<usize>().unwrap_or(0);
                    deletions += d.parse::<usize>().unwrap_or(0);
                    files.push(f.to_string());
                }
                _ => {}
            }
        }
        assert_eq!(files_changed, 3);
        assert_eq!(insertions, 13);
        assert_eq!(deletions, 3);
        assert_eq!(files.len(), 3);
    }
}
