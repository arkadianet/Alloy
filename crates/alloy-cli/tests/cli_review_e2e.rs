//! `alloy review` end to end, offline, through the real `alloy` binary.
//!
//! A scripted OpenAI-compatible HTTP responder on loopback stands in for the
//! model (same device as `cli_repair_e2e.rs`), keyed on the `review`
//! capability's own system instruction (`REVIEW_SYSTEM`). The diff arrives on
//! stdin or from a file — the CLI spawns no process to obtain it (RFC-0015
//! rule B7 / boundary grep T1).
//!
//! Skip policy mirrors `cli_repair_e2e.rs`: without a working Landlock jail
//! (exit `EX_SANDBOX`) the tests skip unless `ALLOY_REQUIRE_LANDLOCK=1`.
//!
//! Author: arkadianet

#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use tempfile::TempDir;

/// The diff under review in these tests.
const REVIEW_DIFF: &str = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,4 +1,4 @@\n fn main() {\n-    let x: i32 = 1;\n+    let x: i32 = \"not a number\";\n     println!(\"{}\", x);\n }\n";

fn request_changes_json() -> serde_json::Value {
    serde_json::json!({
        "verdict": "request_changes",
        "findings": [
            {
                "severity": "blocker",
                "file": "src/main.rs",
                "line": 2,
                "message": "the literal is a &str but the binding is typed i32",
            },
            {
                "severity": "info",
                "file": "src/main.rs",
                "message": "consider a test covering this binding",
            },
        ],
        "summary": "the diff does not compile",
        "confidence": 0.8,
    })
}

fn approve_json() -> serde_json::Value {
    serde_json::json!({
        "verdict": "approve",
        "findings": [],
        "summary": "no issues found",
        "confidence": 0.7,
    })
}

/// Serve OpenAI-compatible chat completions on loopback, answering the
/// review instruction with `body` and anything else with an obvious marker.
fn start_scripted_server(body: serde_json::Value) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Some(request) = read_http_request(&mut stream) else {
                continue;
            };
            let content = if request.contains("You review a diff for correctness and risk") {
                body.clone()
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

struct E2e {
    ws: TempDir,
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
tiers = ["economy", "standard"]
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

fn setup(body: serde_json::Value) -> E2e {
    let port = start_scripted_server(body);
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(ws.path().join("src")).unwrap();
    std::fs::write(ws.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    common::write_profiles(ws.path());
    std::fs::write(ws.path().join("router.toml"), router_toml(port)).unwrap();
    std::fs::write(ws.path().join("example.env"), "ALLOY_API_KEY=\n").unwrap();
    common::git_init_commit(ws.path());
    E2e { ws }
}

impl E2e {
    fn alloy(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_alloy"));
        cmd.current_dir(self.ws.path())
            .env("ALLOY_API_KEY", "scripted-key")
            .env_remove("ALLOY_DATA_DIR")
            .env_remove("ALLOY_PROFILE")
            .env_remove("ALLOY_ROUTER");
        cmd
    }

    fn path(&self) -> &Path {
        self.ws.path()
    }
}

fn is_environment_skip(code: Option<i32>, stderr: &str) -> bool {
    if code == Some(4) {
        assert!(
            std::env::var_os("ALLOY_REQUIRE_LANDLOCK").is_none(),
            "ALLOY_REQUIRE_LANDLOCK=1 but sandbox unavailable: {stderr}"
        );
        eprintln!("skip: sandbox unavailable ({stderr})");
        return true;
    }
    false
}

/// `request_changes` renders one line per finding, then the summary and the
/// verdict, and exits with the dedicated review exit code (16).
#[test]
fn review_from_a_file_renders_findings_and_exits_review_changes() {
    let e2e = setup(request_changes_json());
    let diff_path = e2e.path().join("pr.diff");
    std::fs::write(&diff_path, REVIEW_DIFF).unwrap();

    let out = e2e
        .alloy()
        .args(["review", "--diff", diff_path.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.status.code(),
        Some(16),
        "expected EX_REVIEW_CHANGES: {stderr}\n{stdout}"
    );
    assert!(
        stdout.contains("blocker src/main.rs:2 the literal is a &str"),
        "finding line missing: {stdout}"
    );
    assert!(
        stdout.contains("info src/main.rs consider a test"),
        "line-less finding missing: {stdout}"
    );
    assert!(
        stdout.contains("summary: the diff does not compile"),
        "summary missing: {stdout}"
    );
    assert!(
        stdout.contains("verdict: request_changes"),
        "verdict missing: {stdout}"
    );
}

/// The diff may arrive on stdin (`--diff -`), so `git diff | alloy review
/// --diff -` needs no process spawning inside the CLI (B7/T1). `--json`
/// emits exactly one envelope carrying the typed findings.
#[test]
fn review_from_stdin_json_envelope() {
    let e2e = setup(request_changes_json());

    let mut child = e2e
        .alloy()
        .args(["review", "--diff", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(REVIEW_DIFF.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let doc: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not one JSON doc: {e}\n{stdout}"));
    assert_eq!(out.status.code(), Some(16), "{stderr}");
    assert_eq!(doc["command"], "review");
    assert_eq!(doc["exit_name"], "EX_REVIEW_CHANGES");
    assert_eq!(doc["verdict"], "request_changes");
    assert_eq!(doc["summary"], "the diff does not compile");
    assert_eq!(doc["findings"][0]["severity"], "blocker");
    assert_eq!(doc["findings"][0]["file"], "src/main.rs");
    assert_eq!(doc["findings"][0]["line"], 2);
    assert!(doc["session"].is_string());
    assert!(doc["run"].is_string());
}

/// `approve` is exit 0, and the review leaves the workspace untouched: the
/// planned template has no Edit node at all.
#[test]
fn approve_exits_ok_and_touches_nothing() {
    let e2e = setup(approve_json());
    let diff_path = e2e.path().join("pr.diff");
    std::fs::write(&diff_path, REVIEW_DIFF).unwrap();

    let out = e2e
        .alloy()
        .args(["review", "--diff", diff_path.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if is_environment_skip(out.status.code(), &stderr) {
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{stderr}\n{stdout}");
    assert!(stdout.contains("verdict: approve"), "{stdout}");
    assert!(stdout.contains("summary: no issues found"), "{stdout}");
    // The review never writes to the workspace.
    assert_eq!(
        std::fs::read_to_string(e2e.path().join("src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
}

/// An empty diff is a usage error naming the flag, not a model call.
#[test]
fn empty_diff_is_a_usage_error() {
    let e2e = setup(approve_json());
    let diff_path = e2e.path().join("empty.diff");
    std::fs::write(&diff_path, "").unwrap();

    let out = e2e
        .alloy()
        .args(["review", "--diff", diff_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--diff"), "{stderr}");
}

/// A missing diff file is a usage error naming the path (EX3).
#[test]
fn missing_diff_file_names_the_path() {
    let e2e = setup(approve_json());
    let out = e2e
        .alloy()
        .args(["review", "--diff", "no/such/file.diff"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no/such/file.diff"), "{stderr}");
}
