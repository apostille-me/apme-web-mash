mod http_stream;
mod socket;

use http_stream::ReadRequestError;
use scintilla_route_lambdas::{
    dispatch_request, FunctionRequest, FunctionResponse, RouteSpec,
};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    env,
    error::Error,
    io::{self, BufReader},
    os::unix::net::UnixStream,
    time::{SystemTime, UNIX_EPOCH},
};

const FN_FORMAT_HTTP_STREAM: &str = "http-stream";

pub fn run(spec: RouteSpec) -> Result<(), Box<dyn Error + Send + Sync>> {
    let format = env::var("FN_FORMAT").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FN_FORMAT is required for Oracle Functions/Fn Project",
        )
    })?;
    if format.trim() != FN_FORMAT_HTTP_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "FN_FORMAT must equal http-stream",
        )
        .into());
    }

    let listener_value = env::var("FN_LISTENER").map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "FN_LISTENER is required for Oracle Functions/Fn Project",
        )
    })?;
    let socket_path = socket::parse_listener_uri(&listener_value)?;
    let listener = socket::bind(&socket_path)?;
    println!(
        "{{\"level\":\"info\",\"event\":\"oracle_fn_listening\",\"handler\":\"{}\",\"format\":\"{}\"}}",
        spec.handler, FN_FORMAT_HTTP_STREAM
    );

    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if let Err(error) = handle_stream(stream, spec) {
                    eprintln!(
                        "{{\"level\":\"error\",\"event\":\"oracle_fn_connection_failed\",\"handler\":\"{}\",\"error\":\"{}\"}}",
                        spec.handler,
                        escape_log_value(&error.to_string())
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "{{\"level\":\"error\",\"event\":\"oracle_fn_accept_failed\",\"handler\":\"{}\",\"error\":\"{}\"}}",
                    spec.handler,
                    escape_log_value(&error.to_string())
                );
            }
        }
    }
    Ok(())
}

fn handle_stream(mut stream: UnixStream, spec: RouteSpec) -> io::Result<()> {
    let read_stream = stream.try_clone()?;
    let mut reader = BufReader::new(read_stream);
    let request = match http_stream::read_request(&mut reader, spec.max_event_bytes) {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(ReadRequestError::TooLarge) => {
            return http_stream::write_fn_response(
                &mut stream,
                200,
                error_response(
                    413,
                    json!({
                        "error":"event_too_large",
                        "max_bytes":spec.max_event_bytes,
                    }),
                    &generated_request_id(),
                ),
            );
        }
        Err(ReadRequestError::BadRequest(error)) => {
            return http_stream::write_fn_response(
                &mut stream,
                200,
                error_response(400, json!({"error":error}), &generated_request_id()),
            );
        }
        Err(ReadRequestError::Io(error)) => {
            eprintln!(
                "{{\"level\":\"error\",\"event\":\"oracle_fn_request_read_failed\",\"handler\":\"{}\",\"error\":\"{}\"}}",
                spec.handler,
                escape_log_value(&error.to_string())
            );
            return http_stream::write_fn_response(
                &mut stream,
                502,
                error_response(
                    500,
                    json!({"error":"fn_request_read_failed"}),
                    &generated_request_id(),
                ),
            );
        }
    };

    let request_id = first_header(
        &request.headers,
        &[
            "fn-call-id",
            "ce-id",
            "fn-http-h-ce-id",
            "x-request-id",
            "fn-http-h-x-request-id",
            "traceparent",
            "fn-http-h-traceparent",
        ],
    )
    .and_then(sanitize_request_id)
    .unwrap_or_else(generated_request_id);
    let method = first_header(&request.headers, &["fn-http-method"])
        .unwrap_or(spec.method)
        .to_owned();
    let path = first_header(&request.headers, &["fn-http-request-url"])
        .map(request_path)
        .unwrap_or_else(|| spec.path.to_owned());
    let content_type = first_header(
        &request.headers,
        &["content-type", "fn-http-h-content-type"],
    )
    .unwrap_or_default()
    .to_ascii_lowercase();

    let payload = match decode_payload(&request.headers, &content_type, &request.body) {
        Ok(payload) => payload,
        Err(error) => {
            return http_stream::write_fn_response(
                &mut stream,
                200,
                error_response(400, json!({"error":error}), &request_id),
            );
        }
    };

    let normalized = FunctionRequest {
        method,
        path,
        body: payload,
        request_id,
        event_bytes: request.body.len(),
    };
    http_stream::write_fn_response(&mut stream, 200, dispatch_request(&normalized, spec))
}

fn decode_payload(
    headers: &BTreeMap<String, String>,
    content_type: &str,
    body: &[u8],
) -> Result<Value, String> {
    if body.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|_| "invalid_json".to_owned())?;

    if content_type.starts_with("application/cloudevents+json") {
        validate_structured_cloud_event(&value)?;
        return Ok(value
            .get("data")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())));
    }

    if let Some(version) = first_header(
        headers,
        &["ce-specversion", "fn-http-h-ce-specversion"],
    ) {
        if version != "1.0" {
            return Err("unsupported_cloud_event_version".to_owned());
        }
        for (logical, candidates) in [
            ("id", ["ce-id", "fn-http-h-ce-id"]),
            ("source", ["ce-source", "fn-http-h-ce-source"]),
            ("type", ["ce-type", "fn-http-h-ce-type"]),
        ] {
            if first_header(headers, &candidates).is_none() {
                return Err(format!("missing_cloud_event_{logical}"));
            }
        }
    }

    Ok(value)
}

fn validate_structured_cloud_event(value: &Value) -> Result<(), String> {
    if value.get("specversion").and_then(Value::as_str) != Some("1.0") {
        return Err("unsupported_cloud_event_version".to_owned());
    }
    for field in ["id", "source", "type"] {
        if value
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .is_none()
        {
            return Err(format!("missing_cloud_event_{field}"));
        }
    }
    Ok(())
}

fn request_path(value: &str) -> String {
    let trimmed = value.trim();
    let path_and_query = if let Some(scheme) = trimmed.find("://") {
        let authority = &trimmed[scheme + 3..];
        authority
            .find('/')
            .map(|index| &authority[index..])
            .unwrap_or("/")
    } else {
        trimmed
    };
    let path = path_and_query
        .split(['?', '#'])
        .next()
        .unwrap_or("/")
        .trim();
    if path.is_empty() {
        "/".to_owned()
    } else if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn error_response(status_code: u16, body: Value, request_id: &str) -> FunctionResponse {
    FunctionResponse {
        status_code,
        headers: default_headers(request_id),
        body,
    }
}

fn default_headers(request_id: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("cache-control".to_owned(), "no-store".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
        ("x-request-id".to_owned(), request_id.to_owned()),
    ])
}

fn first_header<'a>(
    headers: &'a BTreeMap<String, String>,
    names: &[&str],
) -> Option<&'a str> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .map(String::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn sanitize_request_id(value: &str) -> Option<String> {
    let sanitized: String = value
        .trim()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | ':' | '/')
        })
        .take(128)
        .collect();
    (!sanitized.is_empty()).then_some(sanitized)
}

fn generated_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("scintilla-fn-{}-{nanos}", std::process::id())
}

fn escape_log_value(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}
