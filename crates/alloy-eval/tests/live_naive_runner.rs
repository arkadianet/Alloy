//! End-to-end run of the real `alloy-eval-live-naive` binary against a
//! loopback stub server, asserting the tool-free single-request contract
//! and hidden-oracle isolation (E1 three-arm holdout, arm B).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_eval::NaiveRunTelemetry;

const BINARY: &str = env!("CARGO_BIN_EXE_alloy-eval-live-naive");

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
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
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
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Serve exactly one OpenAI-compatible chat-completions response and record
/// every request body received on this port.
fn start_stub_server(response_body: &str) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let doc = response_body.to_owned();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let Some(body) = read_http_request(&mut stream) else {
                continue;
            };
            recorded.lock().unwrap().push(body);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{doc}",
                doc.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (port, requests)
}

fn stub_response() -> String {
    serde_json::json!({
        "id": "naive-1",
        "choices": [{
            "message": {
                "content": "{\"replacement\":\"pub fn repaired() -> i32 { 42 }\\n\"}"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20
        }
    })
    .to_string()
}

struct Workspace {
    dir: tempfile::TempDir,
    diagnostics: PathBuf,
    result: PathBuf,
}

fn workspace() -> Workspace {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    // A sibling `.post` reference file and an oracle-tests directory, as a
    // real holdout fixture workspace would carry — the driver must never
    // read either.
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn broken() { missing }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs.post"),
        "pub fn repaired() -> i32 { 42 }\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("oracle-tests")).unwrap();
    std::fs::write(
        dir.path().join("oracle-tests/semantic.rs"),
        "// hidden oracle assertions\n",
    )
    .unwrap();
    let diagnostics = dir.path().join("diagnostics.txt");
    std::fs::write(&diagnostics, "error[E0425]: cannot find value `missing`\n").unwrap();
    let result = dir.path().join("result.json");
    Workspace {
        dir,
        diagnostics,
        result,
    }
}

fn run_binary(ws: &Workspace, base_url: &str) -> std::process::Output {
    Command::new(BINARY)
        .arg("--workspace")
        .arg(ws.dir.path())
        .args(["--target", "src/lib.rs"])
        .arg("--diagnostics")
        .arg(&ws.diagnostics)
        .args(["--goal", "fix the compile error"])
        .args(["--model", "stub-model"])
        .args(["--temperature", "0.6"])
        .args(["--base-url", base_url])
        .arg("--result")
        .arg(&ws.result)
        .env("ALLOY_API_KEY", "test-key-not-real")
        .output()
        .expect("run alloy-eval-live-naive")
}

#[test]
fn one_shot_naive_run_sends_exactly_one_tool_free_request_and_writes_the_replacement() {
    let (port, requests) = start_stub_server(&stub_response());
    let ws = workspace();
    let base_url = format!("http://127.0.0.1:{port}/v1/");

    let output = run_binary(&ws, &base_url);
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let bodies = requests.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1, "exactly one request must be sent");
    let body = &bodies[0];

    let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
    assert!(
        parsed
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "request must carry no tools: {body}"
    );
    assert!(
        !body.contains(".post"),
        "request leaked a .post sentinel: {body}"
    );
    assert!(
        !body.contains("oracle-tests"),
        "request leaked the oracle-tests directory: {body}"
    );

    let replaced = std::fs::read_to_string(ws.dir.path().join("src/lib.rs")).unwrap();
    assert_eq!(replaced, "pub fn repaired() -> i32 { 42 }\n");

    let telemetry: NaiveRunTelemetry =
        serde_json::from_str(&std::fs::read_to_string(&ws.result).unwrap()).unwrap();
    assert_eq!(telemetry.model_calls, 1);
    assert_eq!(telemetry.tokens_in, Some(100));
    assert_eq!(telemetry.tokens_out, Some(20));
    assert_eq!(telemetry.finish_reason.as_deref(), Some("stop"));

    // Never print or leak the API key.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("test-key-not-real"));
    assert!(!stderr.contains("test-key-not-real"));
}

#[test]
fn missing_api_key_fails_closed_without_a_request() {
    let (port, requests) = start_stub_server(&stub_response());
    let ws = workspace();
    let base_url = format!("http://127.0.0.1:{port}/v1/");
    let output = Command::new(BINARY)
        .arg("--workspace")
        .arg(ws.dir.path())
        .args(["--target", "src/lib.rs"])
        .arg("--diagnostics")
        .arg(&ws.diagnostics)
        .args(["--goal", "fix the compile error"])
        .args(["--model", "stub-model"])
        .args(["--temperature", "0.6"])
        .args(["--base-url", &base_url])
        .arg("--result")
        .arg(&ws.result)
        .env_remove("ALLOY_API_KEY")
        .output()
        .expect("run alloy-eval-live-naive");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("ALLOY_API_KEY"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "no request should have been sent"
    );
    assert!(!ws.result.exists());
}
