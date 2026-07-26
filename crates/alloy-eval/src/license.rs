//! R17 fixture license and provenance validation.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// Exact Day-1 SPDX allowlist accepted by RFC-0016.
pub const PERMITTED_SPDX: [&str; 5] = [
    "MIT",
    "Apache-2.0",
    "MIT OR Apache-2.0",
    "CC0-1.0",
    "Alloy-Original",
];

/// Manifest license class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenseClass {
    /// Subject to the exact five-value SPDX allowlist.
    Permitted,
    /// Always rejected at fixture load.
    Forbidden,
}

/// License and source-provenance metadata from a fixture manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LicenseMeta {
    /// Permit/deny classification for the fixture corpus.
    pub class: LicenseClass,
    /// Exact SPDX id or `Alloy-Original`.
    pub spdx: String,
    /// Human provenance note; must contain non-whitespace text.
    pub source_note: String,
}

/// Validate R17 license metadata and the fixture-local `LICENSE` file.
///
/// Failure: [`EvalError::LicenseForbidden`] for class/SPDX/provenance failures,
/// missing or invalid `LICENSE`, non-UTF-8 contents, and symlink escapes.
pub fn validate_license(fixture_dir: &Path, license: &LicenseMeta) -> Result<(), EvalError> {
    validate_meta(license)?;
    validate_license_file(fixture_dir)
}

fn validate_meta(license: &LicenseMeta) -> Result<(), EvalError> {
    if license.class != LicenseClass::Permitted {
        return Err(EvalError::LicenseForbidden(
            "license class must be permitted".into(),
        ));
    }
    if !PERMITTED_SPDX.contains(&license.spdx.as_str()) {
        return Err(EvalError::LicenseForbidden(format!(
            "spdx not permitted: {}",
            license.spdx
        )));
    }
    if license.source_note.trim().is_empty() {
        return Err(EvalError::LicenseForbidden(
            "license source_note must be non-empty".into(),
        ));
    }
    Ok(())
}

fn validate_license_file(fixture_dir: &Path) -> Result<(), EvalError> {
    let fixture_dir = fixture_dir
        .canonicalize()
        .map_err(|err| EvalError::LicenseForbidden(format!("LICENSE fixture root: {err}")))?;
    let path = fixture_dir.join("LICENSE");
    let symlink_meta = std::fs::symlink_metadata(&path)
        .map_err(|err| EvalError::LicenseForbidden(format!("LICENSE: {err}")))?;
    if !symlink_meta.file_type().is_file() {
        return Err(EvalError::LicenseForbidden(
            "LICENSE must be a regular file".into(),
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|err| EvalError::LicenseForbidden(format!("LICENSE: {err}")))?;
    if !canonical.starts_with(&fixture_dir) {
        return Err(EvalError::LicenseForbidden(
            "LICENSE must stay within fixture directory".into(),
        ));
    }
    let bytes = std::fs::read(&canonical)
        .map_err(|err| EvalError::LicenseForbidden(format!("LICENSE: {err}")))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| EvalError::LicenseForbidden("LICENSE must be valid UTF-8".into()))?;
    if text.trim().is_empty() {
        return Err(EvalError::LicenseForbidden(
            "LICENSE must contain non-whitespace text".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn meta(spdx: &str) -> LicenseMeta {
        LicenseMeta {
            class: LicenseClass::Permitted,
            spdx: spdx.into(),
            source_note: "original test fixture".into(),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("LICENSE"), "license text\n").unwrap();
        dir
    }

    #[test]
    fn license_exact_allowlist() {
        for spdx in PERMITTED_SPDX {
            let dir = fixture();
            validate_license(dir.path(), &meta(spdx)).unwrap();
        }
    }

    #[test]
    fn license_rejects_forbidden_or_unknown() {
        let dir = fixture();
        let mut forbidden = meta("MIT");
        forbidden.class = LicenseClass::Forbidden;
        assert!(matches!(
            validate_license(dir.path(), &forbidden),
            Err(EvalError::LicenseForbidden(_))
        ));

        for spdx in ["mit", " MIT", "MIT ", "(MIT)", "Apache 2.0", "BSD-3-Clause"] {
            let dir = fixture();
            assert!(matches!(
                validate_license(dir.path(), &meta(spdx)),
                Err(EvalError::LicenseForbidden(_))
            ));
        }
    }

    #[test]
    fn license_file_integrity() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));

        fs::write(dir.path().join("LICENSE"), "").unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));

        fs::write(dir.path().join("LICENSE"), "  \n\t").unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));

        fs::write(dir.path().join("LICENSE"), [0xff, 0xfe]).unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));

        fs::remove_file(dir.path().join("LICENSE")).unwrap();
        fs::create_dir(dir.path().join("LICENSE")).unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn license_file_symlink_escape_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_license = outside.path().join("LICENSE");
        fs::write(&outside_license, "license text\n").unwrap();
        symlink(&outside_license, dir.path().join("LICENSE")).unwrap();
        assert!(matches!(
            validate_license(dir.path(), &meta("MIT")),
            Err(EvalError::LicenseForbidden(_))
        ));
    }

    #[test]
    fn license_source_note_required() {
        for spdx in ["MIT", "Alloy-Original"] {
            let dir = fixture();
            let mut license = meta(spdx);
            license.source_note = " \n\t".into();
            assert!(matches!(
                validate_license(dir.path(), &license),
                Err(EvalError::LicenseForbidden(_))
            ));
        }
    }
}
