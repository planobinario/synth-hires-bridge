//! Local capability enforcement.
//!
//! Defense in depth. The server validates capabilities too, but the
//! daemon is the last line of defense if the server is compromised
//! or the WS is replayed. Every action_request that arrives is
//! checked against `Scope` BEFORE any code runs.
//!
//! Path-prefix matching is intentionally identical to the TS
//! implementation in `src/lib/agent/bridge-codes.ts →
//! pathMatchesAlwaysAllow` so the three implementations never
//! disagree on whether `/home/u/workspace` matches
//! `/home/u/workspace/`.

use daemon_protocol::Scopes;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScopeSnapshot {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub always_allow_paths: Vec<PathBuf>,
}

impl From<&Scopes> for ScopeSnapshot {
    fn from(s: &Scopes) -> Self {
        ScopeSnapshot {
            capabilities: s.capabilities.clone(),
            always_allow_paths: s.always_allow_paths.iter().map(PathBuf::from).collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    RequireConsent,
    Deny,
}

#[derive(Debug, Clone)]
pub struct CapabilityGate {
    snapshot: ScopeSnapshot,
}

impl CapabilityGate {
    pub fn new(snapshot: ScopeSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn update(&mut self, snapshot: ScopeSnapshot) {
        self.snapshot = snapshot;
    }

    /// Clone with one extra path appended to the always-allow list.
    /// Used for single-action consent: the user approved one path, so
    /// exactly that operation runs under an augmented gate without
    /// mutating the connection-wide snapshot (unless they also said
    /// "remember").
    pub fn with_additional_path(&self, path: PathBuf) -> Self {
        let mut snapshot = self.snapshot.clone();
        snapshot.always_allow_paths.push(path);
        Self { snapshot }
    }

    pub fn allows(&self, capability: &str) -> bool {
        self.snapshot.capabilities.iter().any(|c| c == capability)
    }

    /// Returns Allow if the path is in the alwaysAllow list OR the
    /// capability is one that requires per-action consent regardless
    /// of path (e.g. shell.execute). Returns RequireConsent if the
    /// path is within an allowed scope but the user hasn't added it
    /// to alwaysAllowPaths. Returns Deny if the capability itself is
    /// not in the snapshot.
    pub fn check_path(&self, capability: &str, path: &Path) -> GateDecision {
        if !self.allows(capability) {
            return GateDecision::Deny;
        }
        if self.path_matches_any(path) {
            return GateDecision::Allow;
        }
        GateDecision::RequireConsent
    }

    fn path_matches_any(&self, path: &Path) -> bool {
        // Defensa 1: Bloquear traversal (..)
        if path.components().any(|c| matches!(c, Component::ParentDir)) {
            return false;
        }

        // Defensa 2: Verificar prefijo válido por componente
        for prefix in &self.snapshot.always_allow_paths {
            if path.starts_with(prefix) {
                return true;
            }
        }
        false
    }

    /// Defensa 3: symlink escape. El matching anterior es léxico; un
    /// directorio dentro del root puede ser un symlink que apunta fuera
    /// (`workspace/link -> /etc`), y un write a través de él aterrizaría
    /// fuera del workspace con un Allow lexical. Antes de conceder Allow
    /// para una acción que toca disco, se resuelve la ruta REAL del sistema
    /// y se vuelve a exigir que siga dentro de un prefix permitido. Sin
    /// symlinks en la cadena, la ruta resuelta es idéntica (coste: un
    /// syscall); con ellos, la decisión degrada a RequireConsent — el
    /// usuario ve el diálogo y aprueba (o no) la ruta externa concreta.
    pub fn check_path_real(&self, capability: &str, path: &Path) -> std::io::Result<GateDecision> {
        match self.check_path(capability, path) {
            GateDecision::Allow => {
                // El target puede no existir aún (write de fichero nuevo):
                // se resuelve el ancestro existente más profundo — sus
                // symlinks intermedios ya delatan el escape.
                let mut probe = path.to_path_buf();
                let real = loop {
                    match std::fs::canonicalize(&probe) {
                        Ok(p) => break p,
                        Err(_) => match probe.parent() {
                            Some(parent) => probe = parent.to_path_buf(),
                            None => return Ok(GateDecision::RequireConsent),
                        },
                    }
                };
                if self.path_matches_any(&real) {
                    Ok(GateDecision::Allow)
                } else {
                    tracing::warn!(
                        path = %path.display(),
                        real = %real.display(),
                        "symlink escape: resolved path outside scope — requiring consent"
                    );
                    Ok(GateDecision::RequireConsent)
                }
            }
            other => Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn gate_with_paths(prefixes: &[&str]) -> CapabilityGate {
        let snap = ScopeSnapshot {
            capabilities: vec!["desktop.fs.read".into(), "desktop.fs.write".into()],
            always_allow_paths: prefixes.iter().map(PathBuf::from).collect(),
        };
        CapabilityGate::new(snap)
    }

    #[test]
    fn allows_exact_match() {
        let g = gate_with_paths(&["/home/u/workspace"]);
        assert_eq!(
            g.check_path("desktop.fs.read", Path::new("/home/u/workspace")),
            GateDecision::Allow
        );
    }

    #[test]
    fn allows_with_trailing_slash() {
        let g = gate_with_paths(&["/home/u/workspace/"]);
        assert_eq!(
            g.check_path("desktop.fs.read", Path::new("/home/u/workspace")),
            GateDecision::Allow
        );
    }

    #[test]
    fn allows_under_prefix() {
        let g = gate_with_paths(&["/home/u/workspace"]);
        assert_eq!(
            g.check_path(
                "desktop.fs.read",
                Path::new("/home/u/workspace/sub/file.txt")
            ),
            GateDecision::Allow
        );
    }

    #[test]
    fn denies_prefix_collision() {
        // Without trailing-separator guard, "/home/u/workspace-evil"
        // would match "/home/u/workspace". The strip_trailing_sep logic
        // ensures we require a separator boundary.
        let g = gate_with_paths(&["/home/u/workspace"]);
        assert_eq!(
            g.check_path("desktop.fs.read", Path::new("/home/u/workspace-evil/file")),
            GateDecision::RequireConsent
        );
    }

    #[test]
    fn denies_path_traversal() {
        let g = gate_with_paths(&["/home/u/workspace"]);
        assert_eq!(
            g.check_path(
                "desktop.fs.read",
                Path::new("/home/u/workspace/../etc/passwd")
            ),
            GateDecision::RequireConsent
        );
    }

    #[test]
    fn denies_capability_not_in_scope() {
        let g = gate_with_paths(&["/home/u/workspace"]);
        assert_eq!(
            g.check_path("desktop.shell.execute", Path::new("/home/u/workspace/cmd")),
            GateDecision::Deny
        );
    }

    #[test]
    fn symlink_escape_inside_root_requires_consent() {
        let dir = std::env::temp_dir().join(format!("sh-gate-{}", uuid::Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("sh-gate-out-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&outside, dir.join("link")).unwrap();

        let g = gate_with_paths(&[dir.to_str().unwrap()]);
        // Léxico:Allow (la ruta pasa por dentro del root)…
        assert_eq!(
            g.check_path("desktop.fs.write", &dir.join("link/x.txt")),
            GateDecision::Allow
        );
        // …real: el destino cae FUERA del root → consentimiento.
        assert_eq!(
            g.check_path_real("desktop.fs.write", &dir.join("link/x.txt")).unwrap(),
            GateDecision::RequireConsent
        );
        // Sin symlink, la ruta resuelta sigue dentro → Allow intacto.
        assert_eq!(
            g.check_path_real("desktop.fs.write", &dir.join("plain.txt")).unwrap(),
            GateDecision::Allow
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }
}
