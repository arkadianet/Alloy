//! Deny-glob defaults and matching helpers (RFC-0005 §3.6).

use alloy_runtime::Glob;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::sandbox::types::SandboxError;

/// Default credential / secret deny globs (RFC-0005).
#[must_use]
pub fn default_deny_globs() -> Vec<Glob> {
    const PATTERNS: &[&str] = &[
        ".env",
        ".env.*",
        "*.pem",
        "*.key",
        "id_rsa",
        "id_rsa.*",
        "id_ed25519",
        "id_ed25519.*",
        ".ssh/**",
        "**/.ssh/**",
        ".aws/**",
        "**/.aws/**",
        ".netrc",
    ];
    let mut out: Vec<Glob> = PATTERNS.iter().map(|p| Glob((*p).to_string())).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup();
    out
}

/// Compile deny globs into a [`GlobSet`].
///
/// On macOS matching is case-insensitive. Patterns with `/` are added as
/// jail-relative and also as `**/`+pattern when not already prefixed. Patterns
/// without `/` add both `pattern` and `**/`+pattern.
pub fn compile_deny_globs(globs: &[Glob]) -> Result<GlobSet, SandboxError> {
    let case_insensitive = cfg!(target_os = "macos");
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        add_deny_pattern(&mut builder, &g.0, case_insensitive)?;
    }
    builder
        .build()
        .map_err(|e| SandboxError::Invalid(format!("compile deny globs: {e}")))
}

fn add_deny_pattern(
    builder: &mut GlobSetBuilder,
    pattern: &str,
    case_insensitive: bool,
) -> Result<(), SandboxError> {
    let add = |builder: &mut GlobSetBuilder, pat: &str| -> Result<(), SandboxError> {
        let g = GlobBuilder::new(pat)
            .literal_separator(true)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|e| SandboxError::Invalid(format!("deny glob `{pat}`: {e}")))?;
        builder.add(g);
        Ok(())
    };

    if pattern.contains('/') {
        add(builder, pattern)?;
        if !pattern.starts_with("**/") {
            add(builder, &format!("**/{pattern}"))?;
        }
    } else {
        add(builder, pattern)?;
        add(builder, &format!("**/{pattern}"))?;
    }
    Ok(())
}

/// Match a jail-relative path (no leading `/`, `/` separators) against deny set.
#[must_use]
pub fn deny_matches(set: &GlobSet, jail_relative: &str) -> bool {
    set.is_match(jail_relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn rel(jail: &Path, path: &Path) -> String {
        path.strip_prefix(jail)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn deny_globs_env_and_keys() {
        let set = compile_deny_globs(&default_deny_globs()).unwrap();
        let jail = Path::new("/workspace");
        for p in [
            "/workspace/.env",
            "/workspace/nested/.env",
            "/workspace/.env.local",
            "/workspace/certs/key.pem",
            "/workspace/id_rsa",
            "/workspace/.ssh/id_ed25519",
            "/workspace/.aws/credentials",
            "/workspace/.netrc",
        ] {
            let r = rel(jail, Path::new(p));
            assert!(deny_matches(&set, &r), "expected deny for {r}");
        }
        assert!(!deny_matches(&set, "src/main.rs"));
        assert!(!deny_matches(&set, "example.env"));
    }
}
