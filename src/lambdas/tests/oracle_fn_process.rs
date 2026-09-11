#![cfg(unix)]

use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::{fs::FileTypeExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DOCUMENT_BINARY: &str = env!("CARGO_BIN_EXE_heavy_document_render_fn");
const CASE_BINARY: &str = env!("CARGO_BIN_EXE_heavy_case_export_fn");

struct ChildGuard {
    child: Child,
    root: PathBuf,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let public = self.root.join("listener.sock");
        let phony = self.root.join("phonylistener.sock");
        let _ = fs::remove_file(public);
        let _ = fs::remove_file(phony);
        let _ = fs::remove_dir(&self.root);
    }
}

#[derive(Debug)]
struct ParsedResponse {
    outer_status: u16,
    headers: BTreeMap<String, String>,
    body: Value,
}

#[test]
fn oracle_fn_processes_direct_and_cloudevent_invocations() {
    for case in [
        (
            DOCUMENT_BINARY,
            "heavy_document_render",
            "/api/heavy/document-render",
            json!({"document_id":"document-1"}),
        ),
        (
            CASE_BINARY,
            "heavy_case_export",
            "/api/heavy/case-export",
            json!({"case_id":"case-1"}),
        ),
    ] {
        exercise_binary(case.0, case.1, case.2, &case.3);
    }
}

fn exercise_binary(binary: &str, handler: &str, path: &str, payload: &Value) {
    let root = unique_temp_dir(handler);
    fs::create_dir_all(&root).expect("create Fn test directory");
    let socket = root.join("listener.sock");
    let listener = format!("unix://{}", socket.display());
    let child = Command::new(binary)
        .env("FN_FORMAT", "http-stream")
        .env("FN_LISTENER", listener)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Oracle/Fn adapter");
    let mut guard = ChildGuard { child, root };

    wait_for_socket(&socket, &mut guard.child);

    let direct = invoke(
        &socket,
        &[
            ("Fn-Intent", "httprequest"),
            ("Fn-Call-Id", "direct-call-1"),
            ("Fn-Http-Method", "POST"),
            ("Fn-Http-Request-Url", &format!("https://functions.example{path}")),
            ("Content-Type", "application/json"),
        ],
        &serde_json::to_vec(payload).expect("serialize direct payload"),
    );
    assert_accepted(&direct, handler, "direct-call-1");

    let structured_id = "structured-event-1";
    let structured = json!({
        "specversion":"1.0",
        "id":structured_id,
        "source":"urn:scintilla:test",
        "type":"scintilla.route.requested",
        "data":payload,
    });
    let structured_response = invoke(
        &socket,
        &[
            ("Fn-Intent", "httprequest"),
            ("Fn-Call-Id", "structured-call-1"),
            ("Fn-Http-Method", "POST"),
            ("Fn-Http-Request-Url", &format!("https://functions.example{path}")),
            ("Content-Type", "application/cloudevents+json"),
        ],
        &serde_json::to_vec(&structured).expect("serialize structured CloudEvent"),
    );
    assert_accepted(&structured_response, handler, "structured-call-1");

    let binary_id = "binary-event-1";
    let binary_response = invoke(
        &socket,
        &[
            ("Fn-Intent", "httprequest"),
            ("Fn-Http-Method", "POST"),
            ("Fn-Http-Request-Url", &format!("https://functions.example{path}")),
            ("Content-Type", "application/json"),
            ("Fn-Http-H-Ce-Specversion", "1.0"),
            ("Fn-Http-H-Ce-Id", binary_id),
            ("Fn-Http-H-Ce-Source", "urn:scintilla:test"),
            ("Fn-Http-H-Ce-Type", "scintilla.route.requested"),
        ],
        &serde_json::to_vec(payload).expect("serialize binary CloudEvent data"),
    );
    assert_accepted(&binary_response, handler, binary_id);
}

fn wait_for_socket(socket: &Path, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if fs::metadata(socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false)
        {
            return;
        }
        if let Some(status) = child.try_wait().expect("query Fn child status") {
            let mut stderr = String::new();
            if let Some(mut stream) = child.stderr.take() {
                let _ = stream.read_to_string(&mut stderr);
            }
            panic!("Fn adapter exited before binding: {status}; stderr={stderr}");
        }
        assert!(Instant::now() < deadline, "Fn adapter did not bind its socket");
        thread::sleep(Duration::from_millis(25));
    }
}

fn invoke(socket: &Path, headers: &[(&str, &str)], body: &[u8]) -> ParsedResponse {
    let mut stream = UnixStream::connect(socket).expect("connect to Fn listener");
    write!(stream, "POST /call HTTP/1.1\r\nHost: localhost\r\n")
        .expect("write request line");
    for (name, value) in headers {
        assert!(!name.contains(['\r', '\n']));
        assert!(!value.contains(['\r', '\n']));
        write!(stream, "{name}: {value}\r\n").expect("write request header");
    }
    write!(stream, "Content-Length: {}\r\nConnection: close\r\n\r\n", body.len())
        .expect("write content length");
    stream.write_all(body).expect("write request body");
    stream.shutdown(std::net::Shutdown::Write).expect("finish request");

    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read Fn response");
    parse_response(&response)
}

fn parse_response(bytes: &[u8]) -> ParsedResponse {
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response header terminator");
    let head = std::str::from_utf8(&bytes[..split]).expect("UTF-8 response headers");
    let body_bytes = &bytes[split + 4..];
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("HTTP status line");
    let outer_status = status_line
        .split_whitespace()
        .nth(1)
        .expect("HTTP status code")
        .parse::<u16>()
        .expect("numeric HTTP status");
    let mut headers = BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').expect("valid response header");
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let declared = headers
        .get("content-length")
        .expect("response content-length")
        .parse::<usize>()
        .expect("numeric response content-length");
    assert_eq!(declared, body_bytes.len(), "response content-length mismatch");
    let body = serde_json::from_slice(body_bytes).expect("JSON response body");
    ParsedResponse {
        outer_status,
        headers,
        body,
    }
}

fn assert_accepted(response: &ParsedResponse, handler: &str, request_id: &str) {
    assert_eq!(response.outer_status, 200, "Fn outer status must remain 200");
    assert_eq!(
        response.headers.get("fn-http-status").map(String::as_str),
        Some("202"),
    );
    assert_eq!(
        response.headers.get("x-request-id").map(String::as_str),
        Some(request_id),
    );
    assert_eq!(response.body["accepted"], true);
    assert_eq!(response.body["handler"], handler);
    assert_eq!(response.body["request_id"], request_id);
}

fn unique_temp_dir(handler: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "scintilla-oracle-fn-{handler}-{}-{nonce}",
        std::process::id()
    ))
}
