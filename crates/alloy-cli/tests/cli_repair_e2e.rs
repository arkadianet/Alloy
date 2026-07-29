//! RFC-0015 Appendix C — the M7 acceptance walkthrough, offline, through
//! the real `alloy` binary (AC70).
//!
//! A scripted OpenAI-compatible HTTP responder on loopback stands in for the
//! model (the eval `ScriptedProvider` cannot cross the process boundary; see
//! the spec-defect notes on Appendix D.3). The repair template runs to a
//! verified patch: index → run (gate, `--no-input` → exit 7) → approve from
//! a second process → resume → Succeeded, with the fix on disk.
//!
//! Skip policy mirrors `scheduler_repair_e2e.rs`: without a working
//! Landlock jail (exit `EX_SANDBOX`) the tests skip unless
//! `ALLOY_REQUIRE_LANDLOCK=1`.
//!
//! Author: arkadianet

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

// --- scripted HTTP responder ------------------------------------------------

/// The diff the scripted `edit` completion returns; applies cleanly to the
/// fixture's `src/main.rs`.
const FIX_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    let x: i32 = \"not a number\";\n+    let x: i32 = 42;\n     println!(\"{}\", x);\n }\n";

fn repair_response_json() -> serde_json::Value {
    serde_json::json!({
        "summary": "the literal is a &str but the binding is typed i32; replace it with an integer literal",
        "target_files": ["src/main.rs"],
        "steps": [{
            "file": "src/main.rs",
            "rationale": "replace the string literal with an i32 literal so the annotation holds",
            "anchor_line": 2,
        }],
        "needs_replan": false,
        "confidence": 0.9,
    })
}

fn edit_response_json() -> serde_json::Value {
    serde_json::json!({
        "patch": FIX_DIFF,
        "summary": "replace the string literal with 42",
        "confidence": 0.85,
    })
}

/// Serve OpenAI-compatible chat completions on a loopback port, keyed on
/// the capability-owned system instruction in the request body. Runs on a
/// plain std thread for the whole test.
fn start_scripted_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Some(body) = read_http_request(&mut stream) else {
                continue;
            };
            let content = if body.contains("You analyse Rust compiler diagnostics") {
                repair_response_json()
            } else if body.contains("You produce a minimal unified diff") {
                edit_response_json()
            } else {
                serde_json::json!({"unexpected": true})
            };
            let doc = serde_json::json!({
                "id": "scripted-1",
                "choices": [{
                    "message": { "content": content.to_string() },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 200, "completion_tokens": 60 },
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                doc.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

/// Minimal HTTP/1.1 request reader: headers, then `content-length` bytes.
fn read_http_request(stream: &mut std::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let content_length: usize = headers
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buf[header_end..]).into_owned())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

// --- workspace / environment ------------------------------------------------

struct E2e {
    ws: TempDir,
    homes: TempDir,
    port: u16,
}

fn router_toml(port: u16) -> String {
    format!(
        r#"
[policy]
default_tier = "standard"
max_in_flight = 2
shutdown_grace_ms = 50

[[providers]]
id = "provider"
kind = "openai_compatible"
base_url = "http://127.0.0.1:{port}/v1/"
api_key_env = "ALLOY_API_KEY"

[[providers.endpoints]]
id = "endpoint"
display_name = "Endpoint"
model = "operator-configured"
tiers = ["standard"]
supports_structured_output = true
max_context = 65536
input_usd_per_mtok = 2.0
output_usd_per_mtok = 4.0

[capability_tiers]
repair = "standard"
edit = "standard"
review = "standard"
planning = "standard"
"#
    )
}

fn which_cargo() -> Option<PathBuf> {
    [
        std::env::var_os("CARGO").map(PathBuf::from),
        Some(PathBuf::from("/usr/bin/cargo")),
        std::env::var_os("CARGO_HOME").map(|h| PathBuf::from(h).join("bin/cargo")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin/cargo")),
    ]
    .into_iter()
    .flatten()
    .find(|p| p.is_file())
}

/// A `CARGO_HOME` holding only a copy of the real `cargo`, so the jail's
/// deny on `config.toml` cannot fail the sandboxed check (mirrors
/// `scheduler_repair_e2e.rs::hermetic_cargo_home`).
fn hermetic_cargo_home(root: &Path) -> Option<()> {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).ok()?;
    std::fs::copy(which_cargo()?, bin.join("cargo")).ok()?;
    Some(())
}

fn setup() -> Option<E2e> {
    setup_with(start_scripted_server())
}

fn setup_with(port: u16) -> Option<E2e> {
    if which_cargo().is_none() {
        eprintln!("skip: cargo not found");
        return None;
    }
    let ws = tempfile::tempdir().unwrap();
    common::copy_dir_all(&common::fixture_crate_source(), ws.path()).unwrap();
    common::write_profiles(ws.path());
    std::fs::write(ws.path().join("router.toml"), router_toml(port)).unwrap();
    std::fs::write(ws.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();
    common::git_init_commit(ws.path());

    let homes = tempfile::tempdir().unwrap();
    hermetic_cargo_home(homes.path())?;
    Some(E2e { ws, homes, port })
}

impl E2e {
    fn alloy(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_alloy"));
        cmd.current_dir(self.ws.path())
            .env("ALLOY_API_KEY", "scripted-key")
            .env("CARGO_HOME", self.homes.path())
            .env_remove("ALLOY_DATA_DIR")
            .env_remove("ALLOY_PROFILE")
            .env_remove("ALLOY_ROUTER");
        let _ = self.port;
        cmd
    }

    fn run_json(&self, args: &[&str]) -> (Option<i32>, serde_json::Value, String) {
        let out = self.alloy().args(args).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let doc = serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
        (out.status.code(), doc, stderr)
    }
}

fn require_landlock() -> bool {
    std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_some()
}

/// Returns true when `code`/`stderr` indicate the environment cannot run
/// the sandboxed flow (skip), panicking instead under ALLOY_REQUIRE_LANDLOCK.
fn is_environment_skip(code: Option<i32>, stderr: &str) -> bool {
    if code == Some(4) {
        assert!(
            !require_landlock(),
            "ALLOY_REQUIRE_LANDLOCK=1 but sandbox unavailable: {stderr}"
        );
        eprintln!("skip: sandbox unavailable ({stderr})");
        return true;
    }
    false
}

// --- the walkthrough ---------------------------------------------------------

/// Appendix C steps 2–7: index, gate via --no-input (GA5, exit 7, gate id on
/// stdout), approve from a second process (SQ9), resume to Succeeded, fix on
/// disk, workspace compiles via the events log, `.env` never created.
#[test]
fn appendix_c_offline_walkthrough() {
    let Some(e2e) = setup() else { return };

    // Step 2 — index (IX1/IX2).
    let (code, doc, stderr) = e2e.run_json(&["index", "--json"]);
    assert_eq!(code, Some(0), "index failed: {stderr}");
    assert!(doc["report"]["version"].is_number());

    // Step 3 — plan without executing (CL12).
    let (code, doc, stderr) = e2e.run_json(&[
        "run",
        "fix the compile error in src/main.rs",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(code, Some(0), "dry-run failed: {stderr}");
    assert_eq!(doc["template_id"], "RepairLocalDiagnostic");

    // Step 4b — the real run in CI mode: gate → exit 7 with the gate id.
    let (code, doc, stderr) = e2e.run_json(&[
        "run",
        "fix the compile error in src/main.rs",
        "--no-input",
        "--json",
    ]);
    if is_environment_skip(code, &stderr) {
        return;
    }
    assert_eq!(code, Some(7), "expected EX_GATE_REQUIRED: {stderr}");
    let gate = doc["gate_required"]
        .as_str()
        .expect("gate id in JSON")
        .to_owned();
    let run = doc["run"].as_str().unwrap().to_owned();
    let session = doc["session"].as_str().unwrap().to_owned();

    // The fix is already applied (edit precedes the gate) but the run is
    // durable in waiting_approval and resumable.
    let main_rs = std::fs::read_to_string(e2e.ws.path().join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains("let x: i32 = 42;"),
        "patch not applied: {main_rs}"
    );

    // Approve out of band from a second process (SQ9).
    let (code, _, stderr) = e2e.run_json(&[
        "approve",
        "--run",
        &run,
        "--gate",
        &gate,
        "--decision",
        "allow",
        "--json",
    ]);
    assert_eq!(code, Some(0), "approve failed: {stderr}");

    // Resume re-dispatches and completes (SQ13; no replan — SQ14).
    let (code, doc, stderr) = e2e.run_json(&["resume", "--session", &session, "--json"]);
    assert_eq!(code, Some(0), "resume failed: {stderr}\n{doc}");

    // Step 5 — inspect: the event log tells the whole story (OUT5).
    let out = e2e
        .alloy()
        .args(["events", "--session", &session])
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&out.stdout).into_owned();
    for needle in [
        "session_created",
        "goal_submitted",
        "plan_produced",
        "node_state",
        "approval_requested",
        "approval_resolved",
        "run_completed",
        "dag_state succeeded",
    ] {
        assert!(log.contains(needle), "events missing {needle}: {log}");
    }

    // Step 7 — nothing was written that should not have been (SEC1/SEC5).
    assert!(!e2e.ws.path().join(".env").exists());
    assert!(e2e.ws.path().join(".alloy").is_dir());
}

/// GA6 — a denied gate is a legitimate outcome: the run terminalizes and a
/// later resume reports it rather than re-running (exit 8 when the resume
/// observes the denial as the run outcome, or EX_STATE when the deny had
/// already terminalized the run row).
#[test]
fn deny_exits_gate_denied() {
    let Some(e2e) = setup() else { return };

    let (code, doc, stderr) = e2e.run_json(&[
        "run",
        "fix the compile error in src/main.rs",
        "--no-input",
        "--json",
    ]);
    if is_environment_skip(code, &stderr) {
        return;
    }
    assert_eq!(code, Some(7), "expected EX_GATE_REQUIRED: {stderr}");
    let gate = doc["gate_required"].as_str().unwrap().to_owned();
    let run = doc["run"].as_str().unwrap().to_owned();
    let session = doc["session"].as_str().unwrap().to_owned();

    let (code, _, stderr) = e2e.run_json(&[
        "approve",
        "--run",
        &run,
        "--gate",
        &gate,
        "--decision",
        "deny",
    ]);
    assert_eq!(code, Some(0), "deny approve failed: {stderr}");

    let (code, _, _stderr) = e2e.run_json(&["resume", "--session", &session, "--json"]);
    match code {
        Some(8) => {}
        Some(14) => {
            // Already terminal: the denial must be durable in the log.
            let out = e2e
                .alloy()
                .args(["events", "--session", &session])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stdout).into_owned();
            assert!(log.contains("approval_resolved"), "no denial in log: {log}");
        }
        other => panic!("expected exit 8 or 14 after deny, got {other:?}"),
    }
}

/// CR18/CR14 — SIGTERM during a run exits EX_CANCELLED (6) and names the
/// run so `alloy resume` can be used; the data dir stays intact.
#[test]
fn run_sigterm_drains_and_exits_cancelled() {
    let Some(e2e) = setup() else { return };

    let mut child = e2e
        .alloy()
        .args(["run", "fix the compile error in src/main.rs", "--no-input"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // Signal early enough to land mid-run (the offline flow reaches its
    // gate in well under a second).
    std::thread::sleep(Duration::from_millis(250));
    if child.try_wait().unwrap().is_some() {
        // Finished (or refused) before the signal — nothing to assert here;
        // environment skips land in the other tests.
        eprintln!("skip: run exited before SIGTERM could be delivered");
        return;
    }
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&child),
        rustix::process::Signal::Term,
    )
    .unwrap();

    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    if out.status.code() == Some(7) {
        // The run reached its gate before the signal landed; GA5 already
        // owns that path. Nothing further to assert about cancellation.
        eprintln!("skip: run reached the gate before SIGTERM");
        return;
    }
    if out.status.code().is_none() {
        // Killed by the raw signal: it landed during assembly, before the
        // CR14 handler was armed (slow CI). Default disposition applies
        // there by design — nothing was running yet.
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            out.status.signal(),
            Some(rustix::process::Signal::Term as i32),
            "{stderr}"
        );
        eprintln!("skip: SIGTERM landed before the handler was armed");
        return;
    }
    assert_eq!(
        out.status.code(),
        Some(6),
        "expected EX_CANCELLED: {stderr}"
    );
    assert!(
        stderr.contains("resume"),
        "cancel must name the resume path: {stderr}"
    );
    assert!(e2e.ws.path().join(".alloy").is_dir());
}

/// GA4 — `--yes` auto-answers the gate with Allow, prints the approval
/// block it answered, and the run completes in one invocation.
#[test]
fn yes_auto_approves_and_completes() {
    let Some(e2e) = setup() else { return };

    let out = e2e
        .alloy()
        .args(["run", "fix the compile error in src/main.rs", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    assert_eq!(out.status.code(), Some(0), "run --yes failed: {stderr}");
    // The block it auto-answered is in the log (GA4).
    assert!(
        stderr.contains("approval required"),
        "approval block missing: {stderr}"
    );
    assert!(stderr.contains("(--yes)"), "auto-answer marker: {stderr}");
    // OUT6 — measured cost line, no savings claims.
    assert!(stderr.contains("measured"), "cost line missing: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("succeeded"), "stdout: {stdout}");
    let main_rs = std::fs::read_to_string(e2e.ws.path().join("src/main.rs")).unwrap();
    assert!(main_rs.contains("let x: i32 = 42;"));
}

/// AM-0013-1: the line-ops form of `edit_response_json` — no hunk headers,
/// just the 1-based line numbers of the fixture's `src/main.rs` with the
/// replaced line repeated in `expect` as the honesty guard.
fn edit_ops_response_json() -> serde_json::Value {
    serde_json::json!({
        "ops": [{
            "op": "replace_lines",
            "path": "src/main.rs",
            "start": 2,
            "end": 2,
            "expect": ["    let x: i32 = \"not a number\";"],
            "new": ["    let x: i32 = 42;"],
        }],
        "summary": "replace the string literal with 42",
        "confidence": 0.85,
    })
}

/// Scripted server that answers the edit capability with line ops instead
/// of a unified diff (same shape as `start_scripted_server` otherwise).
fn start_ops_scripted_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Some(body) = read_http_request(&mut stream) else {
                continue;
            };
            let content = if body.contains("You analyse Rust compiler diagnostics") {
                repair_response_json()
            } else if body.contains("You produce a minimal unified diff") {
                edit_ops_response_json()
            } else {
                serde_json::json!({"unexpected": true})
            };
            let doc = serde_json::json!({
                "id": "scripted-1",
                "choices": [{
                    "message": { "content": content.to_string() },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 200, "completion_tokens": 60 },
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                doc.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

/// AM-0013-1 through the real binary: the model answers the edit turn with
/// a line-ops array; the worker reads the current file, compiles the ops to
/// the canonical `PatchSet`, and the run completes with the fix applied —
/// same downstream machinery as the unified-diff walkthrough.
#[test]
fn ops_edit_response_completes_the_run_with_the_fix_applied() {
    let Some(e2e) = setup_with(start_ops_scripted_server()) else {
        return;
    };
    let out = e2e
        .alloy()
        .args(["run", "fix the compile error in src/main.rs", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    assert_eq!(out.status.code(), Some(0), "ops run failed: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("succeeded"), "stdout: {stdout}");
    let main_rs = std::fs::read_to_string(e2e.ws.path().join("src/main.rs")).unwrap();
    assert!(
        main_rs.contains("let x: i32 = 42;"),
        "ops patch not applied: {main_rs}"
    );
    assert!(
        !main_rs.contains("not a number"),
        "original line survived: {main_rs}"
    );
}

/// A server whose *first* edit response is a wrong fix (it introduces a
/// fresh type error); every later edit response patches the *original*
/// broken line — which only applies if the retry loop rolled the wrong
/// patch back first. Analyze responses are shared.
fn start_wrong_first_server() -> u16 {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let edits = AtomicUsize::new(0);
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Some(body) = read_http_request(&mut stream) else {
                continue;
            };
            let content = if body.contains("You analyse Rust compiler diagnostics") {
                repair_response_json()
            } else if body.contains("You produce a minimal unified diff") {
                let n = edits.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    serde_json::json!({
                        "patch": WRONG_DIFF,
                        "summary": "replace the string literal with true",
                        "confidence": 0.4,
                    })
                } else {
                    serde_json::json!({
                        "patch": SECOND_FIX_DIFF,
                        "summary": "replace the string literal with 42",
                        "confidence": 0.9,
                    })
                }
            } else {
                serde_json::json!({"unexpected": true})
            };
            let doc = serde_json::json!({
                "id": "scripted-1",
                "choices": [{
                    "message": { "content": content.to_string() },
                    "finish_reason": "stop",
                }],
                "usage": { "prompt_tokens": 200, "completion_tokens": 60 },
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                doc.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    port
}

const WRONG_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    let x: i32 = \"not a number\";\n+    let x: i32 = true;\n     println!(\"{}\", x);\n }\n";
/// Deliberately anchored on the *original* `"not a number"` line: it can
/// only apply if the failed attempt's `let x: i32 = true;` was rolled back.
const SECOND_FIX_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    let x: i32 = \"not a number\";\n+    let x: i32 = 42;\n     println!(\"{}\", x);\n }\n";

/// The bounded retry loop: a wrong first patch fails verify, the driver
/// rolls the failed attempt's edit transaction back so the workspace is the
/// original broken tree again, re-checks it, seeds those diagnostics, and
/// the second attempt patches the *original* content. Exit 0 overall.
#[test]
fn retry_rolls_back_the_wrong_first_patch_before_retrying() {
    let Some(e2e) = setup_with(start_wrong_first_server()) else {
        return;
    };
    let out = e2e
        .alloy()
        .args(["run", "fix the compile error in src/main.rs", "--yes"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    assert_eq!(out.status.code(), Some(0), "retry run failed: {stderr}");
    assert!(
        stderr.contains("retrying"),
        "retry marker missing: {stderr}"
    );
    // The rollback is announced, not silent — and it undid the one
    // transaction the failed attempt applied, with nothing refused.
    assert!(
        stderr.contains("rolled back 1/1 edit transaction(s)"),
        "rollback marker missing: {stderr}"
    );
    assert!(
        !stderr.contains("was refused"),
        "rollback should not have been refused: {stderr}"
    );
    // Only reachable if the second attempt patched the restored original
    // content: `SECOND_FIX_DIFF` does not apply to the wrong-patched tree.
    let main_rs = std::fs::read_to_string(e2e.ws.path().join("src/main.rs")).unwrap();
    assert!(main_rs.contains("let x: i32 = 42;"), "{main_rs}");
    assert!(!main_rs.contains("true"), "wrong patch survived: {main_rs}");
}
