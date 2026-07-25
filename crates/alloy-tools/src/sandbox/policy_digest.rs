//! Portable policy digest (RFC-0005 §9).

use alloy_runtime::Digest;
use serde_json::json;

use crate::sandbox::profile::SandboxProfile;

/// Digest over canonical policy JSON with sorted keys; excludes absolute `fs_jail`.
#[must_use]
pub fn compute_policy_digest(profile: &SandboxProfile) -> Digest {
    let mut deny: Vec<&str> = profile.deny_globs.iter().map(|g| g.0.as_str()).collect();
    deny.sort_unstable();
    deny.dedup();

    let check = serde_name(profile.check_backend);
    let test = serde_name(profile.test_backend);
    let network = match profile.network {
        crate::sandbox::types::NetworkPolicy::Deny => "deny",
        crate::sandbox::types::NetworkPolicy::Allow => "allow",
    };

    // serde_json::Map iterates in sorted key order for string keys?
    // Use json! with a BTreeMap for deterministic key order.
    let mut map = serde_json::Map::new();
    map.insert("check_backend".into(), json!(check));
    map.insert("container_image".into(), json!(profile.container_image));
    map.insert("deny_globs".into(), json!(deny));
    map.insert(
        "exec_timeout_secs".into(),
        json!(profile.exec_timeout.as_secs()),
    );
    map.insert("network".into(), json!(network));
    map.insert("quarantine_deps".into(), json!(profile.quarantine_deps));
    map.insert("stderr_cap".into(), json!(profile.stderr_cap));
    map.insert("stdout_cap".into(), json!(profile.stdout_cap));
    map.insert("test_backend".into(), json!(test));

    let bytes =
        serde_json::to_vec(&serde_json::Value::Object(map)).expect("policy JSON serialization");
    Digest::sha256(&bytes)
}

fn serde_name(b: crate::sandbox::types::SandboxBackend) -> &'static str {
    match b {
        crate::sandbox::types::SandboxBackend::Landlock => "landlock",
        crate::sandbox::types::SandboxBackend::Seatbelt => "seatbelt",
        crate::sandbox::types::SandboxBackend::Container => "container",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::profile::SandboxProfile;

    #[test]
    fn policy_digest_stable_and_jail_excluded() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let p1 = SandboxProfile::default_for_jail(d1.path().to_path_buf()).unwrap();
        let mut p2 = SandboxProfile::default_for_jail(d2.path().to_path_buf()).unwrap();
        // Same policy fields aside from jail path.
        p2.check_backend = p1.check_backend;
        p2.test_backend = p1.test_backend;
        p2.network = p1.network;
        p2.quarantine_deps = p1.quarantine_deps;
        p2.deny_globs = p1.deny_globs.clone();
        p2.exec_timeout = p1.exec_timeout;
        p2.stdout_cap = p1.stdout_cap;
        p2.stderr_cap = p1.stderr_cap;
        p2.container_image = p1.container_image.clone();
        assert_eq!(compute_policy_digest(&p1), compute_policy_digest(&p2));
        assert_ne!(p1.fs_jail, p2.fs_jail);
    }
}
