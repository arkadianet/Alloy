//! Environment scrubbing, hard-deny, and cargo quarantine (RFC-0005 §5.6 / §6).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::sandbox::types::{DenialReason, SandboxError};

/// Exact env names that are never forwarded (`env_allow` cannot override).
const HARD_DENY_EXACT: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "RUSTC_WRAPPER",
    "RUSTFLAGS",
    "CARGO",
    "CARGO_BUILD_RUSTC",
    "SSH_AUTH_SOCK",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

/// ASCII case-insensitive substrings denied on env **names**.
const HARD_DENY_SUBSTRINGS: &[&str] = &["api_key", "api-key", "secret", "password", "token"];

/// Validate an `env_allow` name (RFC-0005 §6.3).
pub(crate) fn validate_env_allow_name(name: &str) -> Result<(), SandboxError> {
    if name.is_empty() {
        return Err(SandboxError::Invalid("env_allow name is empty".into()));
    }
    if name.contains('=') || name.contains('\0') || name.contains('\r') || name.contains('\n') {
        return Err(SandboxError::Invalid(format!(
            "env_allow name `{name}` contains illegal characters"
        )));
    }
    if name.starts_with('#') {
        return Err(SandboxError::Invalid(format!(
            "env_allow name `{name}` must not start with #"
        )));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(SandboxError::Invalid("env_allow name is empty".into()));
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(SandboxError::Invalid(format!(
            "env_allow name `{name}` must start with letter or _"
        )));
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(SandboxError::Invalid(format!(
            "env_allow name `{name}` is not a portable identifier"
        )));
    }
    Ok(())
}

/// Returns true if the env name is hard-denied.
#[must_use]
pub(crate) fn is_hard_denied(name: &str) -> bool {
    if HARD_DENY_EXACT.contains(&name) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    HARD_DENY_SUBSTRINGS.iter().any(|sub| lower.contains(sub))
}

/// Homes resolved from the parent environment (RFC-0005 §5.5).
#[derive(Debug, Clone)]
pub(crate) struct OperatorHomes {
    /// Parent `HOME`.
    #[allow(dead_code)] // retained for future credential path derivation
    pub op_home: PathBuf,
    /// Operator cargo home (absolute).
    pub cargo_home: PathBuf,
    /// Operator rustup home (absolute).
    pub rustup_home: PathBuf,
}

impl OperatorHomes {
    /// Resolve from parent env; requires `HOME` on Unix.
    pub(crate) fn resolve() -> Result<Self, SandboxError> {
        let op_home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| SandboxError::Invalid("HOME unset".into()))?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| op_home.join(".cargo"));
        let rustup_home = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| op_home.join(".rustup"));
        Ok(Self {
            op_home,
            cargo_home,
            rustup_home,
        })
    }
}

/// Inputs for building the child environment map.
#[derive(Debug, Clone)]
pub(crate) struct ScrubInput<'a> {
    /// Per-exec sandbox home.
    pub child_home: &'a Path,
    /// Per-exec TMPDIR.
    pub child_tmpdir: &'a Path,
    /// Operator cargo home (native absolute).
    pub cargo_home: &'a Path,
    /// Operator rustup home (native absolute).
    pub rustup_home: &'a Path,
    /// Extra allowlisted names from the request.
    pub env_allow: &'a [String],
    /// Force `CARGO_NET_OFFLINE=true`.
    pub quarantine: bool,
    /// Trusted PATH string for native backends.
    pub path_value: Option<OsString>,
}

/// Scrub parent env into a deny-by-default child map (native backends).
///
/// Never parses `.env` files. Never logs values.
pub(crate) fn scrub_env(
    input: &ScrubInput<'_>,
) -> Result<BTreeMap<OsString, OsString>, SandboxError> {
    for name in input.env_allow {
        validate_env_allow_name(name)?;
        if is_hard_denied(name) {
            return Err(SandboxError::Denied(DenialReason::EnvDenied(name.clone())));
        }
    }

    let mut map = BTreeMap::new();

    // Base allowed names from parent (if set), then rewrites.
    for key in ["USER", "LANG", "LC_ALL", "TERM", "RUSTUP_TOOLCHAIN"] {
        if let Some(v) = std::env::var_os(key) {
            map.insert(OsString::from(key), v);
        }
    }

    if let Some(path) = &input.path_value {
        map.insert(OsString::from("PATH"), path.clone());
    } else if let Some(v) = std::env::var_os("PATH") {
        // Still insert only if caller didn't supply filtered PATH; broker should pass filtered.
        map.insert(OsString::from("PATH"), v);
    }

    map.insert(
        OsString::from("HOME"),
        input.child_home.as_os_str().to_os_string(),
    );
    map.insert(
        OsString::from("TMPDIR"),
        input.child_tmpdir.as_os_str().to_os_string(),
    );
    map.insert(
        OsString::from("CARGO_HOME"),
        input.cargo_home.as_os_str().to_os_string(),
    );
    map.insert(
        OsString::from("RUSTUP_HOME"),
        input.rustup_home.as_os_str().to_os_string(),
    );

    if input.quarantine {
        map.insert(OsString::from("CARGO_NET_OFFLINE"), OsString::from("true"));
    }

    for name in input.env_allow {
        if let Some(v) = std::env::var_os(name) {
            map.insert(OsString::from(name.as_str()), v);
        }
    }

    // Defence: strip any hard-denied that somehow got in.
    map.retain(|k, _| k.to_str().map(|s| !is_hard_denied(s)).unwrap_or(false));

    Ok(map)
}

/// Outcome of quarantine argv rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuarantineOutcome {
    /// No cargo detection — argv unchanged.
    Unchanged,
    /// Inserted `--offline` after subcommand.
    OfflineInserted,
    /// Only forced `CARGO_NET_OFFLINE` (no argv insert).
    OfflineEnvOnly,
}

/// Apply quarantine argv rewrite **after** grant match on original argv.
///
/// `authority_basename` is the security authority for "is this cargo", never
/// `argv[0]` on its own: for native backends it is
/// [`ResolvedBinary::authority_basename`], which prefers the invocation name
/// when a trusted-root shim renamed the target (`cargo` → `rustup`); for the
/// container backend it is the basename-form `argv[0]`, since the image — not
/// the host — supplies the tool.
///
/// [`ResolvedBinary::authority_basename`]: crate::sandbox::grant::ResolvedBinary::authority_basename
pub(crate) fn apply_quarantine(
    argv: &[String],
    authority_basename: Option<&str>,
    quarantine: bool,
) -> Result<(Vec<String>, QuarantineOutcome), SandboxError> {
    if !quarantine {
        return Ok((argv.to_vec(), QuarantineOutcome::Unchanged));
    }
    let Some("cargo") = authority_basename else {
        return Ok((argv.to_vec(), QuarantineOutcome::Unchanged));
    };

    let mut out = argv.to_vec();
    let sub = cargo_subcommand(&out);

    match sub.as_deref() {
        None => {
            tracing::info!(blocked = false, offline_inserted = false, "quarantine");
            Ok((out, QuarantineOutcome::OfflineEnvOnly))
        }
        Some(s) if matches!(s, "fetch" | "update" | "install" | "publish" | "search") => {
            tracing::info!(blocked = %s, "quarantine");
            Err(SandboxError::Denied(DenialReason::QuarantineBlocked(
                s.to_string(),
            )))
        }
        Some(s)
            if matches!(
                s,
                "check" | "test" | "build" | "clippy" | "tree" | "metadata"
            ) =>
        {
            if !out.iter().any(|a| a == "--offline") {
                // Insert immediately after subcommand.
                let idx = out
                    .iter()
                    .position(|a| a == s)
                    .ok_or_else(|| SandboxError::Internal("subcommand index missing".into()))?;
                out.insert(idx + 1, "--offline".into());
                tracing::info!(blocked = false, offline_inserted = true, sub = %s, "quarantine");
                Ok((out, QuarantineOutcome::OfflineInserted))
            } else {
                tracing::info!(blocked = false, offline_inserted = false, sub = %s, "quarantine");
                Ok((out, QuarantineOutcome::OfflineEnvOnly))
            }
        }
        Some(s) => {
            tracing::info!(blocked = false, offline_inserted = false, sub = %s, "quarantine");
            Ok((out, QuarantineOutcome::OfflineEnvOnly))
        }
    }
}

fn cargo_subcommand(argv: &[String]) -> Option<String> {
    // Skip argv[0]; skip optional +<toolchain>; skip leading flags until subcommand.
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if a.starts_with('+') {
            i += 1;
            continue;
        }
        if a.starts_with('-') {
            // Flag — for quarantine detection we skip flags until a non-flag token.
            // Cargo accepts `cargo --offline check`; treat non-option as sub.
            i += 1;
            // If flag takes a value we don't try to be perfect; MVP scans for first non -/+ token.
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// Compose container `--env-file` contents (RFC-0005 §5.5 container table).
pub(crate) fn compose_container_env(
    input: &ScrubInput<'_>,
) -> Result<BTreeMap<String, String>, SandboxError> {
    for name in input.env_allow {
        validate_env_allow_name(name)?;
        if is_hard_denied(name) {
            return Err(SandboxError::Denied(DenialReason::EnvDenied(name.clone())));
        }
    }

    let mut map = BTreeMap::new();
    // PATH / RUSTUP_HOME / RUSTUP_TOOLCHAIN intentionally not forwarded.
    for key in ["USER", "LANG", "LC_ALL", "TERM"] {
        if let Ok(v) = std::env::var(key) {
            if v.contains('\n') {
                return Err(SandboxError::Invalid(format!(
                    "env value for {key} contains newline"
                )));
            }
            map.insert(key.to_string(), v);
        }
    }
    map.insert("HOME".into(), input.child_home.display().to_string());
    map.insert("TMPDIR".into(), input.child_tmpdir.display().to_string());
    map.insert("CARGO_HOME".into(), input.cargo_home.display().to_string());
    if input.quarantine {
        map.insert("CARGO_NET_OFFLINE".into(), "true".into());
    }
    for name in input.env_allow {
        if let Ok(v) = std::env::var(name) {
            if v.contains('\n') {
                return Err(SandboxError::Invalid(format!(
                    "env value for {name} contains newline"
                )));
            }
            map.insert(name.clone(), v);
        }
    }
    Ok(map)
}

/// Serialize env map to docker `--env-file` format (mode 0600 applied by caller).
pub(crate) fn format_env_file(map: &BTreeMap<String, String>) -> Result<String, SandboxError> {
    let mut out = String::new();
    for (k, v) in map {
        validate_env_allow_name(k).or_else(|_| {
            // Base keys are known-good identifiers.
            if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                Ok(())
            } else {
                Err(SandboxError::Invalid(format!("bad env key {k}")))
            }
        })?;
        if v.contains('\n') || v.contains('\0') {
            return Err(SandboxError::Invalid(format!(
                "env value for {k} contains illegal characters"
            )));
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_scrub_strips_ld_preload() {
        let home = PathBuf::from("/tmp/sbx-home");
        let tmp = PathBuf::from("/tmp/sbx-tmp");
        let cargo = PathBuf::from("/tmp/cargo");
        let rustup = PathBuf::from("/tmp/rustup");
        let denied = vec!["LD_PRELOAD".to_string()];
        let input = ScrubInput {
            child_home: &home,
            child_tmpdir: &tmp,
            cargo_home: &cargo,
            rustup_home: &rustup,
            // Hard-denied names must never enter the child map via env_allow.
            env_allow: &denied,
            quarantine: false,
            path_value: Some(OsString::from("/usr/bin")),
        };
        let err = scrub_env(&input).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::EnvDenied(_))
        ));

        let input_ok = ScrubInput {
            env_allow: &[],
            ..input
        };
        let map = scrub_env(&input_ok).unwrap();
        assert!(!map.contains_key(OsStr::new("LD_PRELOAD")));
        assert_eq!(
            map.get(OsStr::new("HOME")).map(|s| s.as_os_str()),
            Some(home.as_os_str())
        );
    }

    use std::ffi::OsStr;

    #[test]
    fn env_substring_denies_registry_token() {
        assert!(is_hard_denied("CARGO_REGISTRY_TOKEN"));
        assert!(is_hard_denied("MY_API_KEY"));
        assert!(!is_hard_denied("USER"));
    }

    #[test]
    fn validate_env_allow_name_rejects() {
        assert!(validate_env_allow_name("").is_err());
        assert!(validate_env_allow_name("A=B").is_err());
        assert!(validate_env_allow_name("#FOO").is_err());
        assert!(validate_env_allow_name("1ABC").is_err());
        assert!(validate_env_allow_name("FOO_BAR").is_ok());
    }

    #[test]
    fn quarantine_rewrites_and_blocks_fetch() {
        let (out, oc) =
            apply_quarantine(&["cargo".into(), "check".into()], Some("cargo"), true).unwrap();
        assert_eq!(out, vec!["cargo", "check", "--offline"]);
        assert_eq!(oc, QuarantineOutcome::OfflineInserted);

        let err =
            apply_quarantine(&["cargo".into(), "fetch".into()], Some("cargo"), true).unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::QuarantineBlocked(_))
        ));
    }

    #[test]
    fn child_home_is_sandbox_home() {
        let home = PathBuf::from("/jail/.alloy-sbx/abc/home");
        let tmp = PathBuf::from("/jail/.alloy-sbx/abc/tmp");
        let cargo = PathBuf::from("/op/.cargo");
        let rustup = PathBuf::from("/op/.rustup");
        let input = ScrubInput {
            child_home: &home,
            child_tmpdir: &tmp,
            cargo_home: &cargo,
            rustup_home: &rustup,
            env_allow: &[],
            quarantine: true,
            path_value: Some(OsString::from("/usr/bin")),
        };
        let map = scrub_env(&input).unwrap();
        assert_eq!(
            map.get(OsStr::new("HOME")).unwrap().as_os_str(),
            home.as_os_str()
        );
        assert_eq!(map.get(OsStr::new("CARGO_NET_OFFLINE")).unwrap(), "true");
    }
}
