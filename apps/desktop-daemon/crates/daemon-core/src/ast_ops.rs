//! Structural code search and edit (feature: ast) — OMP-style ast_grep/ast_edit.
//!
//! Powered by `ast-grep-core` + `ast-grep-language` (tree-sitter grammars).
//! The model writes patterns with meta-variables (`$NAME`, `$$$REST`) and gets
//! SYNTACTIC matches — immune to formatting, comments and whitespace — instead
//! of fragile regex. `ast_edit` rewrites through the AST, reusing matched
//! meta-variables in the replacement, and verifies the write like fs.write.
//!
//! Security model: both actions run through the same CapabilityGate path
//! gating as fs.read/fs.write (grep is read-gated, edit is write-gated) and
//! composite scopes map to their parents (`desktop.code.ast_*` →
//! `desktop.fs.read`/`desktop.fs.write`), so paired devices need no re-scope.

use crate::{capability::CapabilityGate, DaemonError, Result};
use ast_grep_core::matcher::Pattern;
use ast_grep_core::AstGrep;
use ast_grep_language::SupportLang;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

const MAX_MATCHES: usize = 100;
const MAX_MATCH_TEXT: usize = 500;

#[derive(Debug, Clone, Deserialize)]
pub struct AstGrepRequest {
    pub path: PathBuf,
    /// ast-grep pattern with meta-variables, e.g. `console.log($MSG)` or
    /// `fn $NAME($$$ARGS) { $$$BODY }`.
    pub pattern: String,
    /// Explicit language name (rust, typescript, python, go…). When absent,
    /// inferred from the file extension.
    #[serde(default)]
    pub language: Option<String>,
    /// Cap the number of reported matches (server cap: MAX_MATCHES).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AstMatch {
    /// 1-indexed line/column (column measured in bytes, tree-sitter Point).
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AstGrepResult {
    pub language: String,
    pub matches: Vec<AstMatch>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AstEditRequest {
    pub path: PathBuf,
    /// Pattern whose matches are rewritten.
    pub pattern: String,
    /// Replacement template; `$META` from the pattern is substituted per match.
    pub replacement: String,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AstEditResult {
    pub replacements: usize,
    pub bytes_written: u64,
    pub verified: bool,
}

pub struct AstOps<'a> {
    gate: &'a CapabilityGate,
}

impl<'a> AstOps<'a> {
    pub fn new(gate: &'a CapabilityGate) -> Self {
        Self { gate }
    }

    /// Syntactic search: returns every match of `pattern` with positions.
    pub async fn grep(&self, req: AstGrepRequest) -> Result<AstGrepResult> {
        ensure_path(self.gate, "desktop.fs.read", &req.path)?;
        let lang = resolve_lang(req.language.as_deref(), &req.path)?;
        let pattern = compile_pattern(&req.pattern, lang)?;
        let source = fs::read_to_string(&req.path).await.map_err(DaemonError::Io)?;
        let grep = AstGrep::new(&source, lang);
        let cap = req.limit.unwrap_or(MAX_MATCHES).min(MAX_MATCHES);
        let mut matches: Vec<AstMatch> = Vec::new();
        let mut truncated = false;
        for m in grep.root().find_all(&pattern) {
            if matches.len() >= cap {
                truncated = true;
                break;
            }
            let start = m.start_pos();
            let end = m.end_pos();
            let mut text = m.text().to_string();
            if text.len() > MAX_MATCH_TEXT {
                text.truncate(MAX_MATCH_TEXT);
                text.push('…');
            }
            matches.push(AstMatch {
                start_line: start.line() + 1,
                start_column: start.column(&m) + 1,
                end_line: end.line() + 1,
                end_column: end.column(&m) + 1,
                text,
            });
        }
        Ok(AstGrepResult {
            language: lang.to_string().to_lowercase(),
            matches,
            truncated,
        })
    }

    /// Structural rewrite: replaces every AST match of `pattern` with the
    /// replacement template (meta-variables substituted), then writes the
    /// result atomically and verifies the read-back, like fs.write.
    pub async fn edit(&self, req: AstEditRequest) -> Result<AstEditResult> {
        ensure_path(self.gate, "desktop.fs.write", &req.path)?;
        let lang = resolve_lang(req.language.as_deref(), &req.path)?;
        let pattern = compile_pattern(&req.pattern, lang)?;
        let source = fs::read_to_string(&req.path).await.map_err(DaemonError::Io)?;
        let grep = AstGrep::new(&source, lang);
        let mut edits = grep.root().replace_all(&pattern, req.replacement.as_str());
        let replacements = edits.len();
        // Zero matches (including lenient "garbage" patterns that compile
        // but match nothing): leave the file untouched instead of rewriting
        // identical content through a temp file.
        if replacements == 0 {
            return Ok(AstEditResult {
                replacements: 0,
                bytes_written: source.len() as u64,
                verified: true,
            });
        }

        // Edit offsets are BYTE offsets into the original source (see
        // ContentExt for String in ast-grep-core). Rebuild the output by
        // walking matches in ascending byte order — no offset invalidation.
        edits.sort_by_key(|e| e.position);
        let mut out = String::with_capacity(source.len());
        let mut last = 0usize;
        for edit in edits {
            let start = edit.position;
            let end = start + edit.deleted_length;
            if start < last || end > source.len() {
                return Err(DaemonError::Protocol(
                    "ast_edit: overlapping or out-of-bounds edit computed".into(),
                ));
            }
            out.push_str(&source[last..start]);
            out.push_str(&String::from_utf8_lossy(&edit.inserted_text));
            last = end;
        }
        out.push_str(&source[last..]);

        let tmp = req.path.with_extension("synthhires-tmp");
        fs::write(&tmp, out.as_bytes()).await.map_err(DaemonError::Io)?;
        if fs::rename(&tmp, &req.path).await.is_err() {
            let _ = fs::remove_file(&tmp).await;
            return Err(DaemonError::Protocol("ast_edit: rename failed".into()));
        }
        let written = fs::read(&req.path).await.map_err(DaemonError::Io)?;
        let verified = written == out.as_bytes();
        Ok(AstEditResult {
            replacements,
            bytes_written: written.len() as u64,
            verified,
        })
    }
}

fn ensure_path(gate: &CapabilityGate, capability: &str, path: &Path) -> Result<()> {
    match gate.check_path(capability, path) {
        crate::capability::GateDecision::Allow => Ok(()),
        crate::capability::GateDecision::RequireConsent => Err(DaemonError::CapabilityDenied(
            format!("{} requires consent for {}", capability, path.display()),
        )),
        crate::capability::GateDecision::Deny => {
            Err(DaemonError::CapabilityDenied(capability.into()))
        }
    }
}

fn compile_pattern(pattern: &str, lang: SupportLang) -> Result<Pattern> {
    Pattern::try_new(pattern, lang)
        .map_err(|e| DaemonError::Protocol(format!("ast: invalid pattern {pattern:?}: {e}")))
}

/// Resolve the tree-sitter language: explicit name first (case-insensitive
/// via FromStr), then file extension, then a helpful error.
fn resolve_lang(explicit: Option<&str>, path: &Path) -> Result<SupportLang> {
    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(lang) = name.parse::<SupportLang>() {
            return Ok(lang);
        }
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if let Some(lang) = lang_from_extension(ext) {
        return Ok(lang);
    }
    Err(DaemonError::Protocol(format!(
        "ast: cannot determine language for {} (passed language {:?}); pass 'language' explicitly (rust, typescript, tsx, javascript, python, go, java, kotlin, swift, c, cpp, csharp, ruby, php, lua, json, yaml, html, css, scala, bash, nix, solidity, elixir, haskell)",
        path.display(),
        explicit.unwrap_or("none")
    )))
}

fn lang_from_extension(ext: &str) -> Option<SupportLang> {
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => SupportLang::Rust,
        "ts" => SupportLang::TypeScript,
        "tsx" => SupportLang::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => SupportLang::JavaScript,
        "py" => SupportLang::Python,
        "go" => SupportLang::Go,
        "java" => SupportLang::Java,
        "kt" | "kts" => SupportLang::Kotlin,
        "swift" => SupportLang::Swift,
        "c" | "h" => SupportLang::C,
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => SupportLang::Cpp,
        "cs" => SupportLang::CSharp,
        "rb" => SupportLang::Ruby,
        "php" => SupportLang::Php,
        "lua" => SupportLang::Lua,
        "json" => SupportLang::Json,
        "yaml" | "yml" => SupportLang::Yaml,
        "html" | "htm" => SupportLang::Html,
        "css" => SupportLang::Css,
        "scala" => SupportLang::Scala,
        "sh" | "bash" | "zsh" => SupportLang::Bash,
        "nix" => SupportLang::Nix,
        "sol" => SupportLang::Solidity,
        "ex" | "exs" => SupportLang::Elixir,
        "hs" => SupportLang::Haskell,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityGate, ScopeSnapshot};

    fn test_gate(dir: &std::path::Path) -> CapabilityGate {
        let snap = ScopeSnapshot {
            capabilities: vec!["desktop.fs.read".into(), "desktop.fs.write".into()],
            always_allow_paths: vec![dir.to_path_buf()],
        };
        CapabilityGate::new(snap)
    }

    #[tokio::test]
    async fn grep_finds_syntactic_matches() {
        let dir = std::env::temp_dir().join(format!("sh-ast-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("lib.rs");
        tokio::fs::write(
            &path,
            "fn zero() {}\nfn one(x: u8) -> u8 { x }\nfn two() {}\n",
        )
        .await
        .unwrap();
        let gate = test_gate(&dir);
        let ops = AstOps::new(&gate);
        let res = ops
            .grep(AstGrepRequest {
                path: path.clone(),
                pattern: "fn $NAME() {}".into(),
                language: None,
                limit: None,
            })
            .await
            .unwrap();
        // one(x: u8) has args → not a match; zero/two match (signature text
        // is matched structurally, body differences don't matter).
        assert_eq!(res.matches.len(), 2, "{:?}", res.matches);
        assert_eq!(res.matches[0].start_line, 1);
        assert_eq!(res.matches[1].start_line, 3);
        assert_eq!(res.language, "rust");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn edit_rewrites_with_metavars() {
        let dir = std::env::temp_dir().join(format!("sh-ast-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("app.rs");
        tokio::fs::write(
            &path,
            "fn main() {\n    println!(\"hola\");\n    println!(\"adiós ñ\");\n}\n",
        )
        .await
        .unwrap();
        let gate = test_gate(&dir);
        let ops = AstOps::new(&gate);
        let res = ops
            .edit(AstEditRequest {
                path: path.clone(),
                pattern: "println!($MSG)".into(),
                replacement: "eprintln!($MSG)".into(),
                language: None,
            })
            .await
            .unwrap();
        assert_eq!(res.replacements, 2);
        assert!(res.verified);
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            after,
            "fn main() {\n    eprintln!(\"hola\");\n    eprintln!(\"adiós ñ\");\n}\n"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn edit_with_no_matches_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("sh-ast-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("x.rs");
        tokio::fs::write(&path, "fn a() {}").await.unwrap();
        let gate = test_gate(&dir);
        let ops = AstOps::new(&gate);
        // A lenient pattern that compiles but matches nothing must NOT touch
        // the file (and mtime stays stable for build systems).
        let res = ops
            .edit(AstEditRequest {
                path: path.clone(),
                pattern: "nonexistent_fn_call($X)".into(),
                replacement: "x".into(),
                language: Some("rust".into()),
            })
            .await
            .unwrap();
        assert_eq!(res.replacements, 0);
        let unchanged = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(unchanged, "fn a() {}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn unknown_language_is_a_clean_error() {
        let dir = std::env::temp_dir().join(format!("sh-ast-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("data.weird");
        tokio::fs::write(&path, "whatever").await.unwrap();
        let gate = test_gate(&dir);
        let ops = AstOps::new(&gate);
        let res = ops
            .grep(AstGrepRequest {
                path: path.clone(),
                pattern: "$A".into(),
                language: None,
                limit: None,
            })
            .await;
        assert!(matches!(res, Err(DaemonError::Protocol(msg)) if msg.contains("cannot determine language")));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
