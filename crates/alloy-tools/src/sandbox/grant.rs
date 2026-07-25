//! Exec grant matching and trusted-root binary resolution (RFC-0005 §5.3).

use std::path::{Component, Path, PathBuf};

use alloy_runtime::{ExecAllow, Grant, PermissionToken};
use globset::GlobBuilder;

use crate::sandbox::types::{DenialReason, SandboxBackend, SandboxError};

const MAX_ARGV: usize = 256;
const MAX_ARGV_BYTES: usize = 64 * 1024;

/// Resolved native executable after canonicalize-before-auth.
#[derive(Debug, Clone)]
pub struct ResolvedBinary {
    /// Canonical absolute path of the executable.
    pub resolved: PathBuf,
    /// Original `argv[0]` preserved for child argv0-dispatch.
    pub original_argv0: String,
    /// Basename of the resolved path (security authority).
    pub resolved_basename: String,
}

/// Validate argv size caps and reject `..` in argv0.
pub fn validate_argv(argv: &[String]) -> Result<(), SandboxError> {
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
/// Returns the first matching allow, or a typed denial.
pub fn match_exec_grant<'a>(
    perms: &'a PermissionToken,
    argv: &[String],
    backend: SandboxBackend,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<&'a ExecAllow, SandboxError> {
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
        if a.binary.contains("..")
            || Path::new(&a.binary)
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
            return Ok(allow);
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
    /// Native: canonical path + basename.
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
                // Path-form: host canonicalize under trusted roots.
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
                let allow_canon = allow_path.canonicalize().map_err(|e| {
                    SandboxError::Invalid(format!(
                        "canonicalize ExecAllow.binary {}: {e}",
                        allow.binary
                    ))
                })?;
                Ok(res.resolved == allow_canon)
            } else {
                Ok(res.resolved_basename == allow.binary)
            }
        }
    }
}

/// Test `args_glob` against `argv[1..]` space-joined (RFC-0005 §5.3 table).
pub fn exec_allow_matches(allow: &ExecAllow, argv: &[String]) -> Result<bool, SandboxError> {
    // Used by unit tests for the normative examples table (binary already assumed matched).
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
pub fn resolve_executable(
    argv0: &str,
    cwd: &Path,
    trusted_path: &[PathBuf],
) -> Result<ResolvedBinary, SandboxError> {
    let chosen = if Path::new(argv0).is_absolute() {
        PathBuf::from(argv0)
    } else if argv0.contains('/') {
        cwd.join(argv0)
    } else {
        find_on_trusted_path(argv0, trusted_path)?
    };

    let resolved = chosen.canonicalize().map_err(|e| {
        SandboxError::Invalid(format!("canonicalize binary {}: {e}", chosen.display()))
    })?;

    if !trusted_path.iter().any(|root| {
        root.canonicalize()
            .map(|r| resolved.starts_with(r))
            .unwrap_or(false)
            || resolved.starts_with(root)
    }) {
        // Also accept if resolved is under any trusted root after canonicalize of root.
        let ok = trusted_path.iter().any(|root| {
            let r = root.canonicalize().unwrap_or_else(|_| root.clone());
            resolved.starts_with(&r)
        });
        if !ok {
            return Err(SandboxError::Invalid(format!(
                "binary {} is outside trusted immutable roots",
                resolved.display()
            )));
        }
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
    })
}

fn find_on_trusted_path(name: &str, trusted_path: &[PathBuf]) -> Result<PathBuf, SandboxError> {
    for dir in trusted_path {
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
    // Fallback: which within filtered PATH string for convenience when dirs listed.
    Err(SandboxError::Invalid(format!(
        "binary not found on trusted PATH: {name}"
    )))
}

/// Build trusted immutable PATH directories from system roots + cargo/rustup.
pub fn trusted_path_dirs(cargo_home: Option<&Path>, rustup_home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for p in ["/usr/bin", "/usr/sbin", "/bin", "/sbin", "/usr/local/bin"] {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            dirs.push(pb);
        }
    }
    // Also allow /usr as root for absolute tools under /usr/...
    for p in ["/usr", "/bin", "/sbin"] {
        let pb = PathBuf::from(p);
        if pb.is_dir() && !dirs.iter().any(|d| d == &pb) {
            // Used as trusted root for absolute-path grants; PATH search uses bin dirs above.
            let _ = pb;
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
            // Each toolchain bin is trusted; walk one level.
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
pub fn trusted_roots(cargo_home: Option<&Path>, rustup_home: Option<&Path>) -> Vec<PathBuf> {
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
}
