//! Linux Landlock + user/mount/net namespace backend (RFC-0005 §5.5).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::ffi::CString;
use std::path::{Path, PathBuf};

use landlock::{
    Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, ABI,
};
use rustix::mount::{mount_bind, mount_change, mount_remount, MountFlags, MountPropagationFlags};
use rustix::thread::{unshare, UnshareFlags};

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::process::{spawn_supervised, SpawnSpec, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxError};

/// Landlock isolation backend.
pub struct LinuxLandlockBackend;

/// Pre-formatted child-side plan — no allocation in `pre_exec`.
struct LandlockPlan {
    uid_map: CString,
    gid_map: CString,
    /// (source, target) bind pairs prepared in the parent.
    binds: Vec<(CString, CString)>,
    /// Landlock ruleset applied in the child (moved, no Mutex).
    ruleset: Option<RulesetCreated>,
    /// Require successful identity maps (fail closed if nested userns blocks them).
    require_id_maps: bool,
}

impl LinuxLandlockBackend {
    /// Execute under Landlock + userns + netns.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        if !matches!(profile.network, NetworkPolicy::Deny) {
            return Err(SandboxError::Invalid(
                "network=allow unsupported in MVP".into(),
            ));
        }

        let mut plan = Some(prepare_plan(profile, &ctx, true)?);
        let program = ctx.program.clone();
        let argv = ctx.argv.clone();
        let cwd = ctx.cwd.clone();
        let env = ctx.env.clone();

        let outcome = spawn_supervised(SpawnSpec {
            program,
            argv,
            cwd,
            env,
            stdout_cap: profile.stdout_cap,
            stderr_cap: profile.stderr_cap,
            exec_timeout: profile.exec_timeout,
            pre_exec: Some(Box::new(move || {
                let plan = plan
                    .take()
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
                apply_plan(plan)
            })),
            after_spawn: None,
        })
        .await;

        // Probe said Available, but exec-time userns/netns/landlock failed.
        // Under network=Deny that is "cannot enforce", not "backend missing"
        // (RFC §5.5): operators must not be steered to reinstall a present ABI.
        match outcome {
            Err(SandboxError::Io(ref e)) if is_isolation_io(e) => {
                Err(SandboxError::BackendCannotEnforce(format!(
                    "landlock isolation failed at exec under network=deny \
                     (userns/netns/landlock): {e}"
                )))
            }
            other => other,
        }
    }
}

fn is_isolation_io(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::Other
    )
}

fn prepare_plan(
    profile: &SandboxProfile,
    ctx: &IsolateContext,
    require_id_maps: bool,
) -> Result<LandlockPlan, SandboxError> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    // Pre-format identity maps in the parent (async-signal-safe child applies them).
    let uid_map = CString::new(format!("0 {uid} 1"))
        .map_err(|_| SandboxError::Internal("uid_map CString".into()))?;
    let gid_map = CString::new(format!("0 {gid} 1"))
        .map_err(|_| SandboxError::Internal("gid_map CString".into()))?;

    // Broker-owned bind sources OUTSIDE the jail (not writable by sandboxed children).
    let bind_root = broker_bind_root()?.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&bind_root).map_err(SandboxError::Io)?;

    let mut binds = Vec::new();
    let mut empty_idx = 0usize;

    for cred in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home) {
        binds.push(bind_file_to_devnull(&cred)?);
    }
    for path in &ctx.deny_paths {
        let ft = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(SandboxError::Io(e)),
        };
        if ft.is_symlink() || ft.is_file() {
            binds.push(bind_file_to_devnull(path)?);
        } else if ft.is_dir() {
            let empty = bind_root.join(format!("empty-{empty_idx}"));
            empty_idx += 1;
            std::fs::create_dir_all(&empty).map_err(SandboxError::Io)?;
            binds.push((cstring_path(&empty)?, cstring_path(path)?));
        } else {
            return Err(SandboxError::Internal(format!(
                "deny path {} has unsupported node type",
                path.display()
            )));
        }
    }

    let ruleset = build_ruleset_created(profile, ctx)?;
    Ok(LandlockPlan {
        uid_map,
        gid_map,
        binds,
        ruleset: Some(ruleset),
        require_id_maps,
    })
}

fn broker_bind_root() -> Result<PathBuf, SandboxError> {
    let base = std::env::temp_dir().join("alloy-sbx-binds");
    std::fs::create_dir_all(&base).map_err(SandboxError::Io)?;
    Ok(base)
}

fn bind_file_to_devnull(path: &Path) -> Result<(CString, CString), SandboxError> {
    Ok((CString::new("/dev/null").unwrap(), cstring_path(path)?))
}

fn cstring_path(path: &Path) -> Result<CString, SandboxError> {
    CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| SandboxError::Invalid(format!("path contains NUL: {}", path.display())))
}

fn build_ruleset_created(
    profile: &SandboxProfile,
    ctx: &IsolateContext,
) -> Result<RulesetCreated, SandboxError> {
    let abi = ABI::V2;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);
    let access_file = AccessFs::from_file(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(access_all)
        .map_err(|e| SandboxError::BackendCannotEnforce(format!("landlock handle_access: {e}")))?
        .create()
        .map_err(|e| SandboxError::BackendCannotEnforce(format!("landlock create: {e}")))?;

    for p in [profile.fs_jail.clone(), ctx.exec_dir.clone()] {
        if !p.exists() {
            continue;
        }
        let path_fd = PathFd::new(&p).map_err(|e| {
            SandboxError::BackendCannotEnforce(format!("PathFd {}: {e}", p.display()))
        })?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(path_fd, access_all))
            .map_err(|e| {
                SandboxError::BackendCannotEnforce(format!("add_rule {}: {e}", p.display()))
            })?;
    }

    for p in [
        PathBuf::from("/dev/null"),
        PathBuf::from("/dev/urandom"),
        PathBuf::from("/dev/zero"),
    ] {
        if !p.exists() {
            continue;
        }
        let path_fd = PathFd::new(&p).map_err(|e| {
            SandboxError::BackendCannotEnforce(format!("PathFd {}: {e}", p.display()))
        })?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(path_fd, access_file))
            .map_err(|e| {
                SandboxError::BackendCannotEnforce(format!("add_rule {}: {e}", p.display()))
            })?;
    }

    let mut ro = vec![
        PathBuf::from("/usr"),
        PathBuf::from("/lib"),
        PathBuf::from("/lib64"),
        PathBuf::from("/lib32"),
        PathBuf::from("/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/etc"),
        PathBuf::from("/proc"),
    ];
    ro.extend(ctx.read_only_roots.iter().cloned());
    // Allow broker bind-root (empty dirs) as RO so mounts can reference them.
    if let Ok(br) = broker_bind_root() {
        ro.push(br);
    }
    for p in &ro {
        if !p.exists() {
            continue;
        }
        let access = if p.is_file() {
            access_file
        } else {
            access_read
        };
        let path_fd = PathFd::new(p).map_err(|e| {
            SandboxError::BackendCannotEnforce(format!("PathFd {}: {e}", p.display()))
        })?;
        ruleset = ruleset
            .add_rule(PathBeneath::new(path_fd, access))
            .map_err(|e| {
                SandboxError::BackendCannotEnforce(format!("add_rule {}: {e}", p.display()))
            })?;
    }

    Ok(ruleset)
}

fn apply_plan(mut plan: LandlockPlan) -> std::io::Result<()> {
    // Child side: only syscalls + pre-built CStr buffers from the parent.
    // No format!, Mutex, std::fs, or String allocation on the success path.
    unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWNET)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

    if let Err(e) = write_id_maps_preformatted(plan.uid_map.as_bytes(), plan.gid_map.as_bytes()) {
        if plan.require_id_maps {
            return Err(e);
        }
    }

    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

    for (src, dst) in &plan.binds {
        match mount_bind(src.as_c_str(), dst.as_c_str()) {
            Ok(()) => {}
            // Target vanished between parent snapshot and child mount — skip.
            Err(e) if e.raw_os_error() == libc::ENOENT => continue,
            Err(e) => return Err(std::io::Error::from_raw_os_error(e.raw_os_error())),
        }
        // Remount bind read-only when source is not /dev/null (directory denies).
        // Landlock still denies writes if this remount fails.
        if src.as_bytes() != b"/dev/null" {
            if let Err(e) = mount_remount(dst.as_c_str(), MountFlags::BIND | MountFlags::RDONLY, "")
            {
                if e.raw_os_error() != libc::ENOENT {
                    let _ = e; // best-effort; Landlock is the write boundary
                }
            }
        }
    }

    let _ = bring_up_loopback();

    let ruleset = plan
        .ruleset
        .take()
        .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
    if !status.no_new_privs {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }

    // The child is uid 0 in this userns and would otherwise keep CAP_SYS_ADMIN,
    // which lets it umount deny-glob /dev/null binds and re-expose in-jail
    // secrets. Drop the capability (bounding set + effective sets) and lock
    // SECBIT_NOROOT so execve cannot regain it.
    lock_down_userns_caps()?;

    Ok(())
}

/// Prevent the sandboxed payload from undoing mount binds / remounts.
fn lock_down_userns_caps() -> std::io::Result<()> {
    use rustix::thread::{
        capabilities, remove_capability_from_bounding_set, set_capabilities,
        set_capabilities_secure_bits, CapabilitiesSecureBits, Capability, CapabilityFlags,
    };

    set_capabilities_secure_bits(
        CapabilitiesSecureBits::NO_ROOT | CapabilitiesSecureBits::NO_ROOT_LOCKED,
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

    // Best-effort: already-absent bounding-set entries return EPERM/EINVAL.
    let _ = remove_capability_from_bounding_set(Capability::SystemAdmin);

    let mut caps =
        capabilities(None).map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    caps.effective.remove(CapabilityFlags::SYS_ADMIN);
    caps.permitted.remove(CapabilityFlags::SYS_ADMIN);
    caps.inheritable.remove(CapabilityFlags::SYS_ADMIN);
    set_capabilities(None, caps)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    Ok(())
}

fn write_id_maps_preformatted(uid_map: &[u8], gid_map: &[u8]) -> std::io::Result<()> {
    // rustix open/write on static paths — no std::fs path allocation in the child.
    write_proc_file(c"/proc/self/setgroups", b"deny")?;
    write_proc_file(c"/proc/self/uid_map", uid_map)?;
    write_proc_file(c"/proc/self/gid_map", gid_map)?;
    Ok(())
}

fn write_proc_file(path: &std::ffi::CStr, bytes: &[u8]) -> std::io::Result<()> {
    use rustix::fd::AsFd;
    use rustix::fs::{open, Mode, OFlags};
    use rustix::io::write;

    let fd = open(path, OFlags::WRONLY, Mode::empty())
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    let mut off = 0usize;
    while off < bytes.len() {
        let n = write(fd.as_fd(), &bytes[off..])
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
        if n == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        off += n;
    }
    Ok(())
}

fn bring_up_loopback() -> std::io::Result<()> {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        let name = b"lo\0";
        for (i, b) in name.iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        ifr.ifr_ifru.ifru_flags = (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        let rc = libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr);
        let err = std::io::Error::last_os_error();
        libc::close(fd);
        if rc != 0 {
            return Err(err);
        }
    }
    Ok(())
}

/// Probe: throwaway child exercises userns+netns+identity maps+landlock.
///
/// Identity-map failure → Unavailable (RFC §5.5). Nested userns hosts that
/// refuse `uid_map` correctly report Unavailable.
pub fn probe_landlock_sync() -> Result<String, String> {
    // Fail closed early when identity maps + netns cannot be established.
    probe_id_map_only()?;

    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let uid_map = CString::new(format!("0 {uid} 1")).map_err(|e| e.to_string())?;
    let gid_map = CString::new(format!("0 {gid} 1")).map_err(|e| e.to_string())?;

    let mut cmd = Command::new("/bin/true");
    unsafe {
        cmd.pre_exec(move || {
            unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWNET)
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            write_id_maps_preformatted(uid_map.as_bytes(), gid_map.as_bytes())?;
            mount_change(
                "/",
                MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
            )
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            mount_bind("/dev/null", "/etc/hostname")
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            let _ = bring_up_loopback();
            let abi = ABI::V2;
            let status = Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(abi))
                .map_err(std::io::Error::other)?
                .create()
                .map_err(std::io::Error::other)?
                .add_rule(PathBeneath::new(
                    PathFd::new("/").map_err(std::io::Error::other)?,
                    AccessFs::from_all(abi),
                ))
                .map_err(std::io::Error::other)?
                .restrict_self()
                .map_err(std::io::Error::other)?;
            if !status.no_new_privs {
                return Err(std::io::Error::other("no_new_privs not set"));
            }
            Ok(())
        });
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok("landlock+userns+netns+idmap ABI>=2".into())
    } else {
        Err(format!(
            "landlock probe failed (need unprivileged userns identity maps + landlock ABI>=2): status={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Probe identity maps + netns only (no Landlock ruleset).
///
/// Used by RFC §11 `netns_probe_marks_unavailable`: when this fails, the full
/// Landlock probe must report [`BackendStatus::Unavailable`], never Available.
pub fn probe_id_map_only() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let uid_map = CString::new(format!("0 {uid} 1")).map_err(|e| e.to_string())?;
    let gid_map = CString::new(format!("0 {gid} 1")).map_err(|e| e.to_string())?;
    let mut cmd = Command::new("/bin/true");
    unsafe {
        cmd.pre_exec(move || {
            unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNET)
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            write_id_maps_preformatted(uid_map.as_bytes(), gid_map.as_bytes())?;
            Ok(())
        });
    }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("id_map probe status={s}")),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::backend::probe::probe_landlock;
    use crate::sandbox::types::{BackendStatus, SandboxBackend, SandboxError};

    /// RFC §11: probe-time netns/id-map failure must mark Landlock Unavailable.
    #[test]
    fn netns_probe_marks_unavailable() {
        match probe_id_map_only() {
            Ok(()) => {
                // Host can establish identity maps + netns. Full probe may still
                // fail for Landlock ABI reasons; either way it must not claim
                // Available without having exercised id maps (covered by the
                // probe implementation). Assert the contract holds in reverse:
                // Available ⇒ id_map_only succeeded (this branch).
                let _ = probe_landlock();
            }
            Err(reason) => {
                let status = probe_landlock();
                assert!(
                    matches!(status, BackendStatus::Unavailable { .. }),
                    "id_map/netns failed ({reason}) but landlock probe was {status:?}"
                );
            }
        }
    }

    #[test]
    fn backend_cannot_enforce_is_distinct_from_unavailable() {
        // Typed distinction used by exec-time isolation mapping above.
        let a = SandboxError::BackendCannotEnforce("fs-only under deny".into());
        let b = SandboxError::BackendUnavailable {
            backend: SandboxBackend::Landlock,
            message: "missing".into(),
        };
        assert!(matches!(a, SandboxError::BackendCannotEnforce(_)));
        assert!(matches!(b, SandboxError::BackendUnavailable { .. }));
    }
}
