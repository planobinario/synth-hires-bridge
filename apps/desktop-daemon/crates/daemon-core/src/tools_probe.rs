//! User-tool capability probe (feature-free, std + tokio only).
//!
//! One-shot, process-cached detection of the tools ALREADY on the user's
//! machine: shell/CLI utilities, VCS, and — critically — the language
//! servers and debug adapters that `lsp_ops` / `dap_ops` need. The daemon
//! never downloads toolchains (see lsp_ops: "only servers already in PATH
//! are used"), so knowing what exists is the difference between the agent
//! offering `lsp_diagnostics` confidently and firing it into a
//! "server not found" error.
//!
//! Design:
//!  - every probe is a `--version`-style spawn with a short timeout;
//!    anything that fails to spawn or answer in time is simply ABSENT from
//!    the manifest (absent = "not confirmed", which the consumer must treat
//!    as "do not offer the dependent feature");
//!  - the whole sweep is bounded by TOTAL_BUDGET so a pathological PATH
//!    cannot stall the hello handshake;
//!  - results are cached for the process lifetime (tools don't appear
//!    mid-session often enough to justify re-probing per connection).
//!
//! The manifest rides HelloFrame.tools (protocol v1.1 — serde default, old
//! webs ignore it) and `desktop.tools.manifest` re-serves it on demand.
//! Wire types are the protocol crate's (`daemon_protocol::ToolProbe`) — the
//! protocol is the single type authority, like BridgeFrame.

use daemon_protocol::{ToolProbe, ToolsProbeKind};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;
use tokio::process::Command;

/// Per-probe timeout: version prints are instant; 750ms covers cold FS.
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
/// Whole-sweep budget. Probes run sequentially (cheap, and forks racing each
/// other on a loaded laptop is noise we don't need); anything past the
/// budget is skipped — an unprobed tool is reported as absent-confirmed.
const TOTAL_BUDGET: Duration = Duration::from_secs(8);

/// (bin, version args, kind). Everything the average productive workflow
/// touches — detection only, invocation stays shell-exec territory.
const PROBES: &[(&str, &[&str], ToolsProbeKind)] = &[
    ("git", &["--version"], ToolsProbeKind::Vcs),
    ("gh", &["--version"], ToolsProbeKind::Shell),
    ("node", &["--version"], ToolsProbeKind::Shell),
    ("npm", &["--version"], ToolsProbeKind::Shell),
    ("python3", &["--version"], ToolsProbeKind::Shell),
    ("python", &["--version"], ToolsProbeKind::Shell),
    ("pip3", &["--version"], ToolsProbeKind::Shell),
    ("cargo", &["--version"], ToolsProbeKind::Shell),
    ("rustc", &["--version"], ToolsProbeKind::Shell),
    ("go", &["version"], ToolsProbeKind::Shell),
    ("docker", &["--version"], ToolsProbeKind::Shell),
    ("jq", &["--version"], ToolsProbeKind::Shell),
    ("rg", &["--version"], ToolsProbeKind::Shell),
    ("fd", &["--version"], ToolsProbeKind::Shell),
    ("ffmpeg", &["-version"], ToolsProbeKind::Shell),
    ("psql", &["--version"], ToolsProbeKind::Shell),
    ("sqlite3", &["--version"], ToolsProbeKind::Shell),
    // Language servers (lsp_ops spawns these by name).
    ("rust-analyzer", &["--version"], ToolsProbeKind::Lsp),
    ("typescript-language-server", &["--version"], ToolsProbeKind::Lsp),
    ("pyright-langserver", &["--version"], ToolsProbeKind::Lsp),
    ("gopls", &["version"], ToolsProbeKind::Lsp),
    ("clangd", &["--version"], ToolsProbeKind::Lsp),
    // Debug adapters (dap_ops spawns these by name).
    ("dlv", &["version"], ToolsProbeKind::Dap),
    ("debugpy", &["--version"], ToolsProbeKind::Dap),
];

/// Extracts the version-looking token from a `--version` first line.
/// Handles: "git version 2.47.1", "node v22.11.0", "rg 14.1.0",
/// "Python 3.12.7", "dlv 1.23.0", "rust-analyzer 0.0.0 (…)".
pub fn parse_version(first_line: &str) -> Option<String> {
    let token = first_line.split_whitespace().find_map(|w| {
        let w = w.trim_start_matches(['v', 'V']).trim_end_matches([',', ';']);
        let mut dots = 0usize;
        let ok = !w.is_empty()
            && w.chars().enumerate().all(|(i, c)| {
                if c.is_ascii_digit() {
                    true
                } else if c == '.' && i > 0 {
                    dots += 1;
                    dots <= 2
                } else {
                    false
                }
            })
            && w.ends_with(|c: char| c.is_ascii_digit());
        ok.then(|| w.to_string())
    })?;
    Some(token)
}

async fn probe_one(bin: &str, args: &[&str], kind: ToolsProbeKind) -> Option<ToolProbe> {
    let child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let out = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    let first = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    Some(ToolProbe {
        name: bin.to_string(),
        version: parse_version(&first),
        kind,
    })
}

/// Runs the full sweep (bounded). Cached per process — the handshake calls
/// this once; later connections reuse it.
pub async fn detect_tools() -> Vec<ToolProbe> {
    static CACHE: Mutex<Option<Vec<ToolProbe>>> = Mutex::new(None);
    if let Some(hit) = CACHE.lock().unwrap().clone() {
        return hit;
    }
    let deadline = tokio::time::Instant::now() + TOTAL_BUDGET;
    let mut found = Vec::new();
    for (bin, args, kind) in PROBES {
        if tokio::time::Instant::now() >= deadline {
            break; // budget spent; remaining tools stay unconfirmed
        }
        if let Some(probe) = tokio::time::timeout(PROBE_TIMEOUT, probe_one(bin, args, *kind))
            .await
            .ok()
            .flatten()
        {
            found.push(probe);
        }
    }
    *CACHE.lock().unwrap() = Some(found.clone());
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_version_formats() {
        assert_eq!(parse_version("git version 2.47.1"), Some("2.47.1".into()));
        assert_eq!(parse_version("v22.11.0"), Some("22.11.0".into()));
        assert_eq!(parse_version("rg 14.1.0"), Some("14.1.0".into()));
        assert_eq!(parse_version("Python 3.12.7"), Some("3.12.7".into()));
        assert_eq!(parse_version("dlv 1.23.0"), Some("1.23.0".into()));
        assert_eq!(
            parse_version("rust-analyzer 0.0.0 (1a2b3c 2025-01-01)"),
            Some("0.0.0".into())
        );
        assert_eq!(parse_version("Docker version 27.3.1, build abc"), Some("27.3.1".into()));
        // Non-versions → None, but the tool is still reported (present).
        assert_eq!(parse_version("weird output"), None);
        assert_eq!(parse_version(""), None);
        // Rejects junk that only looks numeric at the edges.
        assert_eq!(parse_version("x.y.z"), None);
    }

    #[tokio::test]
    async fn sweep_is_bounded_and_well_formed() {
        // No count assertions: environment-dependent by design (a bare CI
        // container may have nothing; a dev shell has git + cargo). What
        // matters is that the sweep terminates and every hit is well-formed.
        let tools = detect_tools().await;
        for t in &tools {
            assert!(!t.name.is_empty());
        }
    }
}
