//! Precise host OS information for the local status endpoint.
//!
//! The browser cannot distinguish Linux distributions — the user-agent only
//! says "Linux (X11)". The daemon, running ON the host, is the authoritative
//! source: it parses the standard `/etc/os-release` (os-release(5)), the
//! kernel release, machine architecture and libc, and serves them via
//! `GET /status` so the web UI can show "NixOS 26.05" instead of a generic
//! "Linux".
//!
//! Std-only (no new dependencies): file reads + `Command` probes, results
//! cached for the lifetime of the process.

use serde::Serialize;
use std::sync::OnceLock;

/// Precise, host-side OS identification served on `/status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OsInfo {
    /// Coarse family: "windows" | "macos" | "linux" | "android" | "ios".
    pub family: String,
    /// Lower-case distro id from os-release: "nixos", "ubuntu", "fedora", "arch"…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Comma-separated distro family from os-release: "debian", "rhel", "suse", "arch"…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id_like: Option<String>,
    /// Human distro name: "NixOS", "Ubuntu", "Fedora Linux"…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Distro version id: "26.05", "24.04", "41"…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Full pretty name from os-release: "NixOS 26.05 (Yarara)".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pretty_name: Option<String>,
    /// Variant id when present ("workstation" on Fedora, "server"…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,
    /// Kernel release: "6.12.7", "6.1.0-18-amd64"…
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    /// Machine architecture: "x86_64", "aarch64", "armv7"… (Rust ARCH naming).
    pub arch: String,
    /// "glibc 2.39" | "musl" | "bionic" when determinable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libc: Option<String>,
}

impl OsInfo {
    /// Collect host OS info (cheap: file reads + at most one process probe).
    pub fn collect() -> Self {
        match std::env::consts::OS {
            "linux" => Self::linux(),
            "macos" => Self::macos(),
            "android" => Self::android(),
            other => Self::fallback(other),
        }
    }

    /// Cached collect — safe to call on every `/status` poll.
    pub fn cached() -> &'static Self {
        static CACHE: OnceLock<OsInfo> = OnceLock::new();
        CACHE.get_or_init(Self::collect)
    }

    fn fallback(family: &str) -> Self {
        Self {
            family: family.to_string(),
            id: None,
            id_like: None,
            name: match family {
                "windows" => Some("Windows".into()),
                "macos" => Some("macOS".into()),
                _ => None,
            },
            version: None,
            pretty_name: None,
            variant_id: None,
            kernel: None,
            arch: std::env::consts::ARCH.to_string(),
            libc: None,
        }
    }

    fn linux() -> Self {
        // os-release(5): /etc/os-release first, /usr/lib/os-release as fallback.
        let release_src = std::fs::read_to_string("/etc/os-release")
            .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
            .unwrap_or_default();
        let rel = parse_os_release(&release_src);

        // Kernel release without spawning a process.
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());

        let libc = detect_libc();

        Self {
            family: "linux".into(),
            id: rel.id,
            id_like: rel.id_like,
            name: rel.name,
            version: rel.version_id,
            pretty_name: rel.pretty_name,
            variant_id: rel.variant_id,
            kernel,
            arch: std::env::consts::ARCH.to_string(),
            libc,
        }
    }

    fn macos() -> Self {
        let sw_vers = |arg: &str| {
            std::process::Command::new("sw_vers")
                .arg(arg)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let version = sw_vers("-productVersion");
        let pretty_name = version
            .as_ref()
            .map(|v| format!("macOS {v}"));
        let kernel = probe_kernel_uname();
        Self {
            family: "macos".into(),
            id: Some("macos".into()),
            id_like: None,
            name: Some("macOS".into()),
            version,
            pretty_name,
            variant_id: None,
            kernel,
            arch: std::env::consts::ARCH.to_string(),
            libc: None,
        }
    }

    fn android() -> Self {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty());
        Self {
            family: "android".into(),
            id: Some("android".into()),
            id_like: None,
            name: Some("Android".into()),
            version: std::env::var("ANDROID_VERSION").ok(),
            pretty_name: None,
            variant_id: None,
            kernel,
            arch: std::env::consts::ARCH.to_string(),
            libc: Some("bionic".into()),
        }
    }
}

/// Parsed subset of os-release(5) we care about. Values are unquoted.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct OsReleaseFields {
    pub id: Option<String>,
    pub id_like: Option<String>,
    pub name: Option<String>,
    pub version_id: Option<String>,
    pub pretty_name: Option<String>,
    pub variant_id: Option<String>,
}

/// Pure, unit-testable os-release parser (KEY=VALUE, optional quotes,
/// `#` comments, simple backslash escapes per the spec).
pub fn parse_os_release(src: &str) -> OsReleaseFields {
    let mut out = OsReleaseFields::default();
    for raw_line in src.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let unescape = |s: &str| {
            s.replace("\\\\", "\u{0}")
                .replace("\\\"", "\"")
                .replace("\\$", "$")
                .replace("\\`", "`")
                .replace('\\', "")
                .replace('\u{0}', "\\")
        };
        let value = unescape(value.trim().trim_matches('"').trim_matches('\''));
        if value.is_empty() {
            continue;
        }
        match key {
            "ID" => out.id = Some(value),
            "ID_LIKE" => out.id_like = Some(value),
            "NAME" => out.name = Some(value),
            "VERSION_ID" => out.version_id = Some(value),
            "PRETTY_NAME" => out.pretty_name = Some(value),
            "VARIANT_ID" => out.variant_id = Some(value),
            _ => {}
        }
    }
    out
}

fn probe_kernel_uname() -> Option<String> {
    std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Distinguish glibc vs musl on Linux. Best-effort; None when unknown.
fn detect_libc() -> Option<String> {
    // getconf is the cheapest authoritative probe on glibc systems.
    if let Ok(out) = std::process::Command::new("getconf").arg("GNU_LIBC_VERSION").output() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !s.is_empty() {
            return Some(s);
        }
    }
    // Musl systems: ldd --version first line mentions musl.
    if let Ok(out) = std::process::Command::new("ldd").arg("--version").output() {
        let s = String::from_utf8_lossy(&out.stdout);
        if s.to_lowercase().contains("musl") {
            return Some("musl".into());
        }
    }
    if std::path::Path::new("/lib/ld-musl-x86_64.so.1").exists()
        || std::path::Path::new("/lib/ld-musl-aarch64.so.1").exists()
    {
        return Some("musl".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nixos_os_release() {
        let src = "ANSI_COLOR=\"0;38;2;126;186;228\"\nBUILD_ID=\"26.05.9729\"\nID=nixos\nNAME=NixOS\nPRETTY_NAME=\"NixOS 26.05 (Yarara)\"\nVERSION_ID=\"26.05\"\n# a comment line\nVERSION_CODENAME=yarara\n";
        let rel = parse_os_release(src);
        assert_eq!(rel.id.as_deref(), Some("nixos"));
        assert_eq!(rel.name.as_deref(), Some("NixOS"));
        assert_eq!(rel.version_id.as_deref(), Some("26.05"));
        assert_eq!(rel.pretty_name.as_deref(), Some("NixOS 26.05 (Yarara)"));
    }

    #[test]
    fn parses_debian_with_id_like() {
        let src = "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nID=ubuntu\nID_LIKE=debian\n";
        let rel = parse_os_release(src);
        assert_eq!(rel.id.as_deref(), Some("ubuntu"));
        assert_eq!(rel.id_like.as_deref(), Some("debian"));
    }

    #[test]
    fn handles_escaped_quotes_and_empty_values() {
        let src = "NAME=\"Fedora \\\"Rawhide\\\"\"\nEMPTY=\nVARIANT_ID=\nID=fedora\n";
        let rel = parse_os_release(src);
        assert_eq!(rel.name.as_deref(), Some("Fedora \"Rawhide\""));
        assert_eq!(rel.variant_id, None);
        assert_eq!(rel.id.as_deref(), Some("fedora"));
    }
}
