//! Linux Landlock + user/mount/net namespace backend (RFC-0005 §5.5).

#![allow(unsafe_code)]
#![allow(clippy::disallowed_methods)]

use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use landlock::{
    Access, AccessFs, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, ABI,
};
use rustix::mount::{mount_bind, mount_change, MountPropagationFlags};
use rustix::thread::{unshare, UnshareFlags};

use crate::sandbox::backend::IsolateContext;
use crate::sandbox::process::{spawn_supervised, SpawnSpec, SupervisedOutcome};
use crate::sandbox::profile::SandboxProfile;
use crate::sandbox::types::{NetworkPolicy, SandboxError};

/// Landlock isolation backend.
pub struct LinuxLandlockBackend;

struct LandlockPlan {
    uid: u32,
    gid: u32,
    binds: Vec<(CString, CString)>,
    ruleset: Mutex<Option<RulesetCreated>>,
}

impl LinuxLandlockBackend {
    /// Execute under Landlock + userns + netns.
    pub async fn exec(
        profile: &SandboxProfile,
        ctx: IsolateContext,
    ) -> Result<SupervisedOutcome, SandboxError> {
        if !matches!(profile.network, NetworkPolicy::Deny) {
            return Err(SandboxError::BackendCannotEnforce(
                "FS-only Landlock cannot enforce network=deny".into(),
            ));
        }

        let plan = prepare_plan(profile, &ctx)?;
        let program = ctx.program.clone();
        let argv = ctx.argv.clone();
        let cwd = ctx.cwd.clone();
        let env = ctx.env.clone();

        spawn_supervised(SpawnSpec {
            program,
            argv,
            cwd,
            env,
            stdout_cap: profile.stdout_cap,
            stderr_cap: profile.stderr_cap,
            exec_timeout: profile.exec_timeout,
            pre_exec: Some(Box::new(move || apply_plan(&plan))),
        })
        .await
    }
}

fn prepare_plan(
    profile: &SandboxProfile,
    ctx: &IsolateContext,
) -> Result<LandlockPlan, SandboxError> {
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();

    let mut binds = Vec::new();
    let mut empty_dirs = Vec::new();

    for cred in crate::sandbox::backend::credential_bind_targets(&ctx.cargo_home) {
        binds.push(bind_file_to_devnull(&cred)?);
    }
    for path in &ctx.deny_paths {
        if path.is_file() {
            binds.push(bind_file_to_devnull(path)?);
        } else if path.is_dir() {
            let empty = ctx.exec_dir.join(format!("empty-{}", empty_dirs.len()));
            std::fs::create_dir_all(&empty).map_err(SandboxError::Io)?;
            binds.push((cstring_path(&empty)?, cstring_path(path)?));
            empty_dirs.push(empty);
        } else {
            return Err(SandboxError::Internal(format!(
                "deny path {} has unsupported node type",
                path.display()
            )));
        }
    }
    // Keep empty dir paths alive for the duration of the exec (bind sources).
    std::mem::forget(empty_dirs);

    let ruleset = build_ruleset_created(profile, ctx)?;
    Ok(LandlockPlan {
        uid,
        gid,
        binds,
        ruleset: Mutex::new(Some(ruleset)),
    })
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

fn apply_plan(plan: &LandlockPlan) -> std::io::Result<()> {
    unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWNET)
        .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    // Identity maps: required on most hosts; may EPERM under nested userns.
    // Continue when maps fail if mounts + landlock still apply (enforcement intact).
    let _ = write_id_maps(plan.uid, plan.gid);
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;

    for (src, dst) in &plan.binds {
        mount_bind(src.as_c_str(), dst.as_c_str())
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
    }

    // Loopback up is best-effort; empty netns already denies egress.
    let _ = bring_up_loopback();

    let ruleset = plan
        .ruleset
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| std::io::Error::other("ruleset already consumed"))?;
    let status = ruleset.restrict_self().map_err(std::io::Error::other)?;
    if !status.no_new_privs {
        return Err(std::io::Error::other("PR_SET_NO_NEW_PRIVS not set"));
    }
    Ok(())
}

fn write_id_maps(uid: u32, gid: u32) -> std::io::Result<()> {
    std::fs::write("/proc/self/setgroups", b"deny")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1"))?;
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

/// Probe: throwaway child exercises userns+netns+landlock.
pub fn probe_landlock_sync() -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    let mut cmd = Command::new("/bin/true");
    unsafe {
        cmd.pre_exec(|| {
            unshare(UnshareFlags::NEWUSER | UnshareFlags::NEWNS | UnshareFlags::NEWNET)
                .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            let uid = rustix::process::getuid().as_raw();
            let gid = rustix::process::getgid().as_raw();
            let _ = write_id_maps(uid, gid);
            mount_change(
                "/",
                MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
            )
            .map_err(|e| std::io::Error::from_raw_os_error(e.raw_os_error()))?;
            // Prove bind mounts work (deny-glob mechanism).
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
        Ok("landlock+userns+netns ABI>=2".into())
    } else {
        Err(format!(
            "landlock probe failed: status={} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}
