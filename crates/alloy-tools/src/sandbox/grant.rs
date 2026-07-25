//! Exec grant matching and trusted-root binary resolution (RFC-0005 §5.3).

use std::path::{Component, Path, PathBuf};

use alloy_runtime::{ExecAllow, Grant, PermissionToken};
use globset::GlobBuilder;

use crate::sandbox::types::{DenialReason, SandboxBackend, SandboxError};

const MAX_ARGV: usize = 256;
const MAX_ARGV_BYTES: usize = 64 * 1024;

/// Resolved native executable after canonicalize-before-auth.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedBinary {
    /// Canonical absolute path of the executable (spawn target).
    pub resolved: PathBuf,
    /// Original `argv[0]` preserved for child argv0-dispatch.
    pub original_argv0: String,
    /// Basename of the canonical target (never sole authority for shims).
    pub resolved_basename: String,
    /// Basename used at PATH lookup / invocation (e.g. `cargo` before rustup symlink).
    ///
    /// When the *pre-canonicalization* path sits under a trusted immutable root,
    /// grant matching and quarantine may also accept this name (RFC §5.3 rustup
    /// shim semantics). Jail-writable symlink shadows must not.
    pub invocation_basename: String,
    /// True iff the pre-canonicalization `chosen` path lives under a trusted root.
    pub invocation_from_trusted: bool,
}

impl ResolvedBinary {
    /// Security name for quarantine cargo detection.
    ///
    /// Prefers the invocation basename only when that name came from a trusted
    /// root path (rustup shim). Jail-writable symlink shadows fall back to the
    /// canonical basename so they cannot reclassify `/bin/sh` as `cargo`.
    #[must_use]
    pub fn authority_basename(&self) -> &str {
        if self.invocation_from_trusted && self.invocation_basename != self.resolved_basename {
            &self.invocation_basename
        } else {
            &self.resolved_basename
        }
    }
}

/// Result of a successful exec-grant match.
#[derive(Debug)]
pub(crate) struct MatchedExec<'a> {
    /// Matching allow entry.
    pub allow: &'a ExecAllow,
    /// Native resolution (None for container basename-form).
    pub resolved: Option<ResolvedBinary>,
}

/// Validate argv size caps and reject `..` in argv0.
pub(crate) fn validate_argv(argv: &[String]) -> Result<(), SandboxError> {
    if argv.is_empty() {
        return Err(SandboxError::Invalid("empty argv".into()));
    }
    if argv.len() > MAX_ARGV {
        return Err(SandboxError::Invalid(format!(
            "argv length {} exceeds {MAX_ARGV}",
            argv.len()
        )));
    }
    let total: usize = argv.iter().map(|s| s.len()).sum();
    if total > MAX_ARGV_BYTES {
        return Err(SandboxError::Invalid(format!(
            "argv total bytes {total} exceeds {MAX_ARGV_BYTES}"
        )));
    }
    for (i, a) in argv.iter().enumerate() {
        if a.contains('\0') {
            return Err(SandboxError::Invalid(format!("argv[{i}] contains NUL")));
        }
    }
    if argv[0].split(['/', '\\']).any(|s| s == "..")
        || Path::new(&argv[0])
            .components()
            .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(SandboxError::Invalid(
            "argv[0] must not contain `..` segments".into(),
        ));
    }
    Ok(())
}

/// Match `Grant::Exec` allows against **pre-quarantine** argv.
///
/// Returns the matching allow and the authorized resolved binary (when native).
pub(crate) fn match_exec_grant<'a>(
    perms: &'a PermissionToken,
    argv: &[String],
    backend: SandboxBackend,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<MatchedExec<'a>, SandboxError> {
    validate_argv(argv)?;

    let allows: Vec<&ExecAllow> = perms
        .grants
        .iter()
        .filter_map(|g| match g {
            Grant::Exec(a) => Some(a),
            _ => None,
        })
        .collect();
    if allows.is_empty() {
        return Err(SandboxError::Denied(DenialReason::MissingExecGrant));
    }

    for a in &allows {
        if Path::new(&a.binary)
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(SandboxError::Invalid(
                "ExecAllow.binary must not contain `..`".into(),
            ));
        }
    }

    let binary_subject = binary_match_subject(argv, backend, cwd, trusted_path)?;

    let mut binary_matched = false;
    let mut args_failed = false;
    for allow in &allows {
        if !binary_matches(allow, &binary_subject, cwd)? {
            continue;
        }
        binary_matched = true;
        if exec_allow_matches_args(allow, argv)? {
            let resolved = match &binary_subject {
                BinarySubject::Native(r) => Some(r.clone()),
                BinarySubject::ContainerBasename(_) => None,
            };
            return Ok(MatchedExec { allow, resolved });
        }
        args_failed = true;
    }

    if !binary_matched {
        Err(SandboxError::Denied(DenialReason::ExecNotAllowlisted))
    } else if args_failed {
        Err(SandboxError::Denied(DenialReason::ArgsNotAllowlisted))
    } else {
        Err(SandboxError::Denied(DenialReason::ExecNotAllowlisted))
    }
}

#[derive(Debug)]
enum BinarySubject {
    /// Native: canonical path + invocation basename.
    Native(ResolvedBinary),
    /// Container basename-form: basename only (image supplies tool).
    ContainerBasename(String),
}

fn binary_match_subject(
    argv: &[String],
    backend: SandboxBackend,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<BinarySubject, SandboxError> {
    match backend {
        SandboxBackend::Container => {
            let argv0 = &argv[0];
            if Path::new(argv0).is_absolute() || argv0.contains('/') {
                let resolved = resolve_executable(argv0, cwd, trusted_path)?;
                Ok(BinarySubject::Native(resolved))
            } else {
                Ok(BinarySubject::ContainerBasename(argv0.clone()))
            }
        }
        SandboxBackend::Landlock | SandboxBackend::Seatbelt => {
            let resolved = resolve_executable(&argv[0], cwd, trusted_path)?;
            Ok(BinarySubject::Native(resolved))
        }
    }
}

fn binary_matches(
    allow: &ExecAllow,
    subject: &BinarySubject,
    cwd: &Path,
) -> Result<bool, SandboxError> {
    let path_form = allow.binary.contains('/');
    match subject {
        BinarySubject::ContainerBasename(name) => {
            if path_form {
                Ok(false)
            } else {
                Ok(name == &allow.binary)
            }
        }
        BinarySubject::Native(res) => {
            if path_form {
                let allow_path = if Path::new(&allow.binary).is_absolute() {
                    PathBuf::from(&allow.binary)
                } else {
                    cwd.join(&allow.binary)
                };
                let allow_canon = match allow_path.canonicalize() {
                    Ok(p) => p,
                    // Stale path grant: no match, keep scanning other grants.
                    Err(_) => return Ok(false),
                };
                Ok(res.resolved == allow_canon)
            } else {
                // Canonical basename, or trusted-root invocation basename only
                // (rustup shim). Jail-writable symlink shadows must not count.
                Ok(res.resolved_basename == allow.binary
                    || (res.invocation_from_trusted && res.invocation_basename == allow.binary))
            }
        }
    }
}

/// Test `args_glob` against `argv[1..]` space-joined (RFC-0005 §5.3 table).
#[cfg(test)]
pub(crate) fn exec_allow_matches(allow: &ExecAllow, argv: &[String]) -> Result<bool, SandboxError> {
    exec_allow_matches_args(allow, argv)
}

fn exec_allow_matches_args(allow: &ExecAllow, argv: &[String]) -> Result<bool, SandboxError> {
    match &allow.args_glob {
        None => Ok(true),
        Some(pat) if pat.is_empty() => Ok(false),
        Some(pat) => {
            let subject = argv.get(1..).unwrap_or(&[]).join(" ");
            let glob = GlobBuilder::new(pat)
                .literal_separator(true)
                .case_insensitive(false)
                .backslash_escape(true)
                .build()
                .map_err(|e| SandboxError::Invalid(format!("args_glob `{pat}`: {e}")))?;
            Ok(glob.compile_matcher().is_match(&subject))
        }
    }
}

/// Resolve executable under trusted immutable PATH roots only.
pub(crate) fn resolve_executable(
    argv0: &str,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<ResolvedBinary, SandboxError> {
    let invocation_basename = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0)
        .to_string();

    // Canonicalize trusted roots once; reuse for membership and final checks.
    let canon_roots: Vec<PathBuf> = trusted_path
        .iter()
        .map(|root| root.canonicalize().unwrap_or_else(|_| root.clone()))
        .collect();

    // PATH hits are always under a trusted bin dir. Absolute/relative path forms
    // are trusted for *invocation authority* only when the pre-canonicalization
    // path itself sits under a trusted root (rustup shim). A jail-writable
    // symlink named `cargo` → `/bin/sh` must not inherit the `cargo` grant.
    let (chosen, invocation_from_trusted) = if Path::new(argv0).is_absolute() {
        let chosen = PathBuf::from(argv0);
        let trusted = path_under_trusted(&chosen, &canon_roots);
        (chosen, trusted)
    } else if argv0.contains('/') {
        let chosen = cwd.join(argv0);
        let trusted = path_under_trusted(&chosen, &canon_roots);
        (chosen, trusted)
    } else {
        (find_on_trusted_path(argv0, trusted_path)?, true)
    };

    let resolved = chosen.canonicalize().map_err(|e| {
        SandboxError::Invalid(format!("canonicalize binary {}: {e}", chosen.display()))
    })?;

    let under_trusted = canon_roots.iter().any(|r| resolved.starts_with(r));
    if !under_trusted {
        return Err(SandboxError::Invalid(format!(
            "binary {} is outside trusted immutable roots",
            resolved.display()
        )));
    }

    let resolved_basename = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| SandboxError::Invalid("binary basename invalid utf-8".into()))?
        .to_string();

    Ok(ResolvedBinary {
        resolved,
        original_argv0: argv0.to_string(),
        resolved_basename,
        invocation_basename,
        invocation_from_trusted,
    })
}

/// Whether `path` (pre-canonicalization) lives under already-canonical trusted roots.
fn path_under_trusted(path: &Path, canon_roots: &[PathBuf]) -> bool {
    canon_roots.iter().any(|r| {
        if path.starts_with(r) {
            return true;
        }
        // Relative joins: compare the canonical parent directory.
        path.parent()
            .and_then(|p| p.canonicalize().ok())
            .is_some_and(|parent| parent.starts_with(r))
    })
}

fn find_on_trusted_path(name: &str, trusted_path: &[PathBuf]) -> Result<PathBuf, SandboxError> {
    for dir in trusted_path {
        // Only search directory entries that look like PATH bins (…/bin), not bare roots.
        let meta = match dir.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        // Skip broad roots like /usr used only for absolute-path membership.
        let file_name = dir.file_name().and_then(|s| s.to_str()).unwrap_or("");
        let is_bin_dir = matches!(file_name, "bin" | "sbin") || dir.ends_with("sbin");
        if !is_bin_dir {
            continue;
        }
        let candidate = dir.join(name);
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = candidate
                    .metadata()
                    .map(|m| m.permissions().mode())
                    .unwrap_or(0);
                if mode & 0o111 == 0 {
                    continue;
                }
            }
            return Ok(candidate);
        }
    }
    Err(SandboxError::Invalid(format!(
        "binary not found on trusted PATH: {name}"
    )))
}

/// Build trusted immutable PATH directories from system roots + cargo/rustup.
pub(crate) fn trusted_path_dirs(
    cargo_home: Option<&Path>,
    rustup_home: Option<&Path>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for p in ["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/local/bin"] {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            dirs.push(pb);
        }
    }
    if let Some(ch) = cargo_home {
        let bin = ch.join("bin");
        if bin.is_dir() {
            dirs.push(bin);
        }
    }
    if let Some(rh) = rustup_home {
        let toolchains = rh.join("toolchains");
        if toolchains.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&toolchains) {
                for ent in rd.flatten() {
                    let bin = ent.path().join("bin");
                    if bin.is_dir() {
                        dirs.push(bin);
                    }
                }
            }
        }
    }
    dirs
}

/// Trusted roots for absolute-path membership (broader than PATH search dirs).
pub(crate) fn trusted_roots(cargo_home: Option<&Path>, rustup_home: Option<&Path>) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/usr", "/bin", "/sbin"]
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect();
    if let Some(ch) = cargo_home {
        let bin = ch.join("bin");
        if bin.is_dir() {
            roots.push(bin);
        }
    }
    if let Some(rh) = rustup_home {
        let toolchains = rh.join("toolchains");
        if toolchains.is_dir() {
            roots.push(toolchains);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_runtime::{ExecAllow, Glob, Grant, ProfileId, RunId};

    fn token(grants: Vec<Grant>) -> PermissionToken {
        PermissionToken {
            profile: ProfileId::new("default").unwrap(),
            grants,
            expires: None,
            run_id: RunId::new(),
        }
    }

    #[test]
    fn exec_allow_examples_table() {
        let cases: &[(&str, &[&str], bool)] = &[
            ("check", &["check"], true),
            ("check", &["check", "--workspace"], false),
            ("check*", &["check", "--workspace"], true),
            ("check*", &["test"], false),
            ("check --workspace", &["check", "--workspace"], true),
            ("+nightly check*", &["+nightly", "check"], true),
        ];
        for (pat, args, expect) in cases {
            let allow = ExecAllow {
                binary: "cargo".into(),
                args_glob: Some((*pat).into()),
            };
            let mut argv = vec!["cargo".to_string()];
            argv.extend(args.iter().map(|s| (*s).to_string()));
            let got = exec_allow_matches(&allow, &argv).unwrap();
            assert_eq!(got, *expect, "pat={pat:?} args={args:?}");
        }
        let empty = ExecAllow {
            binary: "cargo".into(),
            args_glob: Some(String::new()),
        };
        assert!(!exec_allow_matches(&empty, &["cargo".into(), "check".into()]).unwrap());
        let any = ExecAllow {
            binary: "cargo".into(),
            args_glob: None,
        };
        assert!(exec_allow_matches(&any, &["cargo".into(), "x".into()]).unwrap());
    }

    #[test]
    fn binary_resolution_rejects_dotdot() {
        let err = validate_argv(&["../bin/evil".into()]).unwrap_err();
        assert!(matches!(err, SandboxError::Invalid(_)));
    }

    #[test]
    fn missing_exec_grant() {
        let t = token(vec![Grant::FsRead(Glob("**/*.rs".into()))]);
        let err = match_exec_grant(
            &t,
            &["true".into()],
            SandboxBackend::Landlock,
            Path::new("/"),
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            err,
            SandboxError::Denied(DenialReason::MissingExecGrant)
        ));
    }

    #[test]
    fn binary_resolution_path_basename_shim() {
        // Simulate rustup shim: PATH entry named `cargo` that resolves to a
        // different basename under a trusted root.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let target = bin.join("rustup");
        std::fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&target).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&target, p).unwrap();
            std::os::unix::fs::symlink(&target, bin.join("cargo")).unwrap();
        }
        let roots = vec![bin.clone()];
        let resolved = resolve_executable("cargo", dir.path(), &roots).unwrap();
        assert_eq!(resolved.invocation_basename, "cargo");
        assert_eq!(resolved.resolved_basename, "rustup");
        assert!(resolved.invocation_from_trusted);
        assert_eq!(resolved.authority_basename(), "cargo");

        let allow = ExecAllow {
            binary: "cargo".into(),
            args_glob: Some("check*".into()),
        };
        let subject = BinarySubject::Native(resolved.clone());
        assert!(binary_matches(&allow, &subject, dir.path()).unwrap());

        // Workspace shadow rejected: binary under non-trusted cwd.
        let shadow = dir.path().join("cargo-shadow");
        std::fs::write(&shadow, b"evil").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&shadow).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&shadow, p).unwrap();
        }
        let err = resolve_executable("./cargo-shadow", dir.path(), &roots).unwrap_err();
        assert!(matches!(err, SandboxError::Invalid(_)));
    }

    /// Jail-writable symlink named `cargo` → `/bin/sh` must not inherit a
    /// basename `cargo` grant (RFC §5.3: authority from resolved, never the
    /// pre-canonicalization basename alone when the link is untrusted).
    #[test]
    #[cfg(unix)]
    fn binary_resolution_rejects_symlink_shadow() {
        let dir = tempfile::tempdir().unwrap();
        let jail = dir.path();
        let sh = ["/bin/sh", "/usr/bin/sh"]
            .into_iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .expect("sh");
        std::os::unix::fs::symlink(&sh, jail.join("cargo")).unwrap();

        let roots = trusted_path_dirs(None, None);
        let resolved = resolve_executable("./cargo", jail, &roots).unwrap();
        assert_eq!(resolved.invocation_basename, "cargo");
        assert!(!resolved.invocation_from_trusted);
        assert_eq!(
            resolved.authority_basename(),
            resolved.resolved_basename.as_str()
        );
        assert_ne!(resolved.authority_basename(), "cargo");

        let allow = ExecAllow {
            binary: "cargo".into(),
            args_glob: None,
        };
        assert!(!binary_matches(&allow, &BinarySubject::Native(resolved), jail).unwrap());

        let tok = token(vec![Grant::Exec(ExecAllow {
            binary: "cargo".into(),
            args_glob: None,
        })]);
        let err = match_exec_grant(
            &tok,
            &["./cargo".into(), "-c".into(), "echo pwned".into()],
            SandboxBackend::Landlock,
            jail,
            &roots,
        )
        .unwrap_err();
        assert!(
            matches!(err, SandboxError::Denied(DenialReason::ExecNotAllowlisted)),
            "got {err:?}"
        );
    }
}
