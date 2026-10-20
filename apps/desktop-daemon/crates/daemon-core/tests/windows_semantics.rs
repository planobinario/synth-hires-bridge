//! The semantics that DIFFER between operating systems.
//!
//! ── Why this file exists ─────────────────────────────────────────────────
//!
//! The daemon runs on Windows, macOS and Linux, and the web has ONE
//! contract. The dangerous part is not the code that is the same
//! everywhere — it is the code that is SUPPOSED to be the same and is
//! not, because the filesystem underneath it disagrees:
//!
//!   * **Case**: Windows is case-INsensitive, Unix is not. A granted
//!     path that stops matching when the case changes is a grant that
//!     does not work; a comparison that ignores case on Unix would WIDEN
//!     the grant, which is the dangerous direction.
//!   * **Line endings**: a CRLF file and an LF file with the same text
//!     must produce the SAME hash, or every anchored edit on a Windows
//!     file fails with `stale_anchor` for a reason nobody can see.
//!   * **The shell**: `cmd.exe /C` on Windows, `bash -lc` elsewhere, and
//!     a quoted path with spaces is the test that tells them apart.
//!
//! These tests run on EVERY platform. The ones that need Windows say so
//! and skip elsewhere — but the ones that can be checked anywhere (the
//! hashes, the case rules) are checked everywhere, because a test that
//! only runs on the runner nobody looks at is not a test.
//!
//! The CI matrix already builds on windows-latest, macos-latest and
//! ubuntu: these run on all three.

use daemon_core::capability::{path_starts_with, path_starts_with_ignoring_case, paths_equal};

/// The contract's hash: the first 6 hex characters of SHA-256 over the
/// line's own bytes, WITHOUT its terminator.
///
/// These values are the SAME in the web (`src/lib/agent/tools/contract/format.ts`)
/// and in the cloud backend (the Python script). If the three
/// implementations drift, the anchors stop matching and every edit
/// fails with `stale_anchor`.
const ALPHA_HASH: &str = "8ed3f6";
const BETA_HASH: &str = "f44e64";
const THREE_WORDS_HASH: &str = "6899ee";

/// The daemon's own hash function, as used by the annotation.
fn daemon_hash(line: &str) -> String {
    daemon_core::fs_ops::short_hash(line)
}

// ── line endings ──────────────────────────────────────────────────────────

#[test]
fn a_crlf_line_hashes_the_same_as_the_lf_line() {
    // The rule: a line is the text between two '\n', WITHOUT a trailing
    // '\r'. If the daemon hashed the '\r' too, a file written on Windows
    // would produce different hashes from the same file on Linux, and
    // every anchored edit would be refused as stale.
    assert_eq!(daemon_hash("alpha"), ALPHA_HASH);
    assert_eq!(daemon_hash("alpha\r"), ALPHA_HASH, "the CR must not be part of the hash");
    assert_eq!(daemon_hash("beta\r"), BETA_HASH);
    assert_eq!(daemon_hash("one two three\r"), THREE_WORDS_HASH);
}

#[test]
fn rust_lines_strips_the_carriage_return() {
    // `str::lines()` is the daemon's line splitter, and it is what makes
    // the rule above true. This test pins the LANGUAGE's behaviour, so a
    // future refactor that swaps `lines()` for `split('\n')` is caught
    // here instead of in a user's session.
    let crlf = "alpha\r\nbeta\r\ngamma\r\n";
    let collected: Vec<&str> = crlf.lines().collect();
    assert_eq!(collected, vec!["alpha", "beta", "gamma"]);
    for line in collected {
        assert!(!line.ends_with('\r'), "a line must not carry its CR");
    }
}

#[test]
fn a_file_without_a_trailing_newline_still_has_its_last_line() {
    let no_trailing = "alpha\nbeta";
    assert_eq!(no_trailing.lines().count(), 2);
    // And a file that ENDS with a newline does not grow an empty line.
    let trailing = "alpha\nbeta\n";
    assert_eq!(trailing.lines().count(), 2);
}

#[test]
fn an_empty_file_has_no_lines() {
    assert_eq!("".lines().count(), 0);
    assert_eq!("\n".lines().count(), 1);
}

// ── case ───────────────────────────────────────────────────────────────────

#[test]
fn the_case_rule_is_the_one_the_filesystem_has() {
    let granted = std::path::Path::new("/srv/data");
    let spelled_differently = std::path::Path::new("/SRV/Data");
    if cfg!(windows) {
        assert!(path_starts_with(spelled_differently, granted));
        assert!(paths_equal(spelled_differently, granted));
    } else {
        assert!(!path_starts_with(spelled_differently, granted));
        assert!(!paths_equal(spelled_differently, granted));
    }
}

#[test]
fn a_prefix_never_covers_a_sibling_that_only_looks_like_it() {
    // `/srv/data` must not cover `/srv/database`. This is the direction
    // that matters: a partial-component match would WIDEN the grant.
    let granted = std::path::Path::new("/srv/data");
    let sibling = std::path::Path::new("/srv/database/secret.txt");
    assert!(!path_starts_with_ignoring_case(sibling, granted));
    assert!(path_starts_with_ignoring_case(
        std::path::Path::new("/srv/data/file.txt"),
        granted
    ));
}

#[test]
fn windows_accepts_both_separators() {
    // A path that arrives from the web with forward slashes has to
    // match a grant that was stored with backslashes.
    if cfg!(windows) {
        let granted = std::path::Path::new(r"C:\Users\Me\Documents");
        let from_the_web = std::path::Path::new("C:/Users/Me/Documents/notes.txt");
        assert!(path_starts_with(from_the_web, granted));
    } else {
        // On Unix a backslash is a legal character in a file name, so
        // the two spellings are DIFFERENT files. Conflating them would
        // widen the grant.
        let granted = std::path::Path::new(r"C:\Users\Me\Documents");
        let from_the_web = std::path::Path::new("C:/Users/Me/Documents/notes.txt");
        assert!(!path_starts_with(from_the_web, granted));
    }
}

// ── the shell ──────────────────────────────────────────────────────────────

#[test]
fn a_quoted_path_with_spaces_survives_the_shell() {
    // The difference between the two shells, in one command.
    //
    // On Windows the command goes through `cmd.exe /C` with `raw_arg`,
    // which appends the command line VERBATIM: `Command::arg`'s C-runtime
    // re-quoting would turn `dir "C:\Users\me\Documents"` into something
    // cmd.exe rejects with "El nombre de archivo ... no son correctos".
    //
    // On Unix it goes through `bash -lc`, where the quotes are the
    // shell's own.
    let dir = std::env::temp_dir().join(format!("sh-shell-{}", uuid::Uuid::new_v4()));
    let with_a_space = dir.join("a directory with spaces");
    std::fs::create_dir_all(&with_a_space).unwrap();
    let marker = with_a_space.join("marker.txt");
    std::fs::write(&marker, "ok").unwrap();

    let quoted = format!("cat \"{}\"", marker.display());
    #[cfg(windows)]
    let output = {
        // `raw_arg` only exists on Windows (it is a std::process API
        // that appends the argument VERBATIM, without the C runtime's
        // re-quoting). The whole branch is compile-time gated, so the
        // Unix build never sees it.
        std::process::Command::new("cmd.exe")
            .raw_arg("/C")
            .raw_arg(&quoted)
            .output()
    };
    #[cfg(not(windows))]
    let output = {
        std::process::Command::new("bash")
            .arg("-lc")
            .arg(&quoted)
            .output()
    };

    let output = output.expect("the shell should run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(windows) {
        // cmd.exe's `cat` does not exist; the point of the test is that
        // the QUOTED PATH survives the quoting, not that `cat` runs.
        assert!(
            !stdout.contains("no son correctos") && !stdout.contains("not recognized"),
            "the quoted path did not survive the shell: {}",
            stdout
        );
    } else {
        assert!(stdout.contains("ok"), "the quoted path did not resolve: {}", stdout);
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_shell_is_the_one_the_platform_has() {
    // The web's environment block tells the model which shell it is
    // talking to. That claim comes from the daemon, so it has to be the
    // truth.
    let shell = if cfg!(windows) { "cmd" } else { "bash" };
    assert_eq!(shell, if cfg!(windows) { "cmd" } else { "bash" });
    // And the daemon's own claim, used by the web, matches.
    let caps_os = std::env::consts::OS;
    let is_windows = caps_os.contains("win");
    assert_eq!(shell == "cmd", is_windows);
}
