#[cfg(feature = "http")]
use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode},
    response::Response,
    routing::get,
    Router,
};
#[cfg(feature = "aws")]
use lambda_runtime::{run as run_lambda, service_fn, tracing, Error, LambdaEvent};
use serde_json::{json, Map, Value};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_MAX_EVENT_BYTES: usize = 1_048_576;
pub const DEFAULT_HTTP_PORT: u16 = 8080;

#[derive(Clone, Copy, Debug)]
pub struct RouteSpec {
    pub handler: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    pub required_field: &'static str,
    pub max_event_bytes: usize,
}

impl RouteSpec {
    pub const fn new(
        handler: &'static str,
        method: &'static str,
        path: &'static str,
        required_field: &'static str,
    ) -> Self {
        Self {
            handler,
            method,
            path,
            required_field,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionRequest {
    pub method: String,
    pub path: String,
    pub body: Value,
    pub request_id: String,
    pub event_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FunctionResponse {
    pub status_code: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Value,
}

/// Backwards-compatible AWS entrypoint used by the existing route binaries.
#[cfg(feature = "aws")]
pub async fn run(spec: RouteSpec) -> Result<(), Error> {
    run_aws(spec).await
}

/// AWS Lambda Runtime API adapter. The provider-neutral domain dispatch remains
/// in `dispatch_request`; only the invocation envelope is AWS-specific.
#[cfg(feature = "aws")]
pub async fn run_aws(spec: RouteSpec) -> Result<(), Error> {
    tracing::init_default_subscriber();
    run_lambda(service_fn(move |event| handle_aws(event, spec))).await
}

#[cfg(feature = "aws")]
async fn handle_aws(event: LambdaEvent<Value>, spec: RouteSpec) -> Result<Value, Error> {
    let (payload, context) = event.into_parts();
    Ok(dispatch(&payload, &context.request_id, spec))
}

/// Translate API Gateway v1/v2 envelopes into the provider-neutral request.
pub fn dispatch(payload: &Value, request_id: &str, spec: RouteSpec) -> Value {
    let request_id = sanitize_request_id(request_id).unwrap_or_else(generated_request_id);
    if payload
        .get("isBase64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return aws_response(with_request_id(
            function_response(415, json!({"error":"base64_body_not_supported"}), None),
            &request_id,
        ));
    }

    let event_bytes = serde_json::to_vec(payload)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    let request = FunctionRequest {
        method: payload
            .pointer("/requestContext/http/method")
            .or_else(|| payload.get("httpMethod"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        path: payload
            .get("rawPath")
            .or_else(|| payload.get("path"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        body: decode_json_value(payload.get("body")),
        request_id,
        event_bytes,
    };
    aws_response(dispatch_request(&request, spec))
}

/// Provider-neutral dispatch shared by AWS Lambda, Google Cloud Run, Azure
/// Functions custom handlers, Knative, OpenFaaS, and IBM Cloud Code Engine.
pub fn dispatch_request(request: &FunctionRequest, spec: RouteSpec) -> FunctionResponse {
    let response = if request.event_bytes > spec.max_event_bytes {
        function_response(
            413,
            json!({"error":"event_too_large","max_bytes":spec.max_event_bytes}),
            None,
        )
    } else if request.path != spec.path {
        function_response(404, json!({"error":"route_not_found"}), None)
    } else if !request.method.eq_ignore_ascii_case(spec.method) {
        function_response(
            405,
            json!({"error":"method_not_allowed"}),
            Some(spec.method),
        )
    } else if !has_required_value(&request.body, spec.required_field) {
        function_response(
            422,
            json!({
                "error":"missing_required_field",
                "field":spec.required_field,
            }),
            None,
        )
    } else {
        emit_accept_log(request, spec);
        function_response(
            202,
            json!({
                "accepted":true,
                "handler":spec.handler,
                "request_id":request.request_id.as_str(),
                "route":spec.path,
            }),
            None,
        )
    };

    with_request_id(response, &request.request_id)
}

#[cfg(feature = "aws")]
fn emit_accept_log(request: &FunctionRequest, spec: RouteSpec) {
    tracing::info!(
        handler = spec.handler,
        route = spec.path,
        method = spec.method,
        request_id = request.request_id.as_str(),
        event_bytes = request.event_bytes,
        "accepted isolated heavy-route invocation"
    );
}

#[cfg(all(feature = "http", not(feature = "aws")))]
fn emit_accept_log(request: &FunctionRequest, spec: RouteSpec) {
    println!(
        "{{\"level\":\"info\",\"event\":\"accepted_heavy_route\",\"handler\":\"{}\",\"route\":\"{}\",\"method\":\"{}\",\"request_id\":\"{}\",\"event_bytes\":{}}}",
        spec.handler,
        spec.path,
        spec.method,
        request.request_id,
        request.event_bytes
    );
}

#[cfg(not(any(feature = "aws", feature = "http")))]
fn emit_accept_log(_request: &FunctionRequest, _spec: RouteSpec) {}

/// Portable HTTP adapter used by Cloud Run, Azure Functions custom handlers,
/// Knative, OpenFaaS, IBM Code Engine, and ordinary container runtimes.
#[cfg(feature = "http")]
pub async fn run_http(
    spec: RouteSpec,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(feature = "aws")]
    tracing::init_default_subscriber();

    let port = configured_port()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(health))
        .fallback(http_handler)
        .with_state(spec);
    let address = std::net::SocketAddr::new(
        std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        port,
    );
    let listener = tokio::net::TcpListener::bind(address).await?;

    #[cfg(feature = "aws")]
    tracing::info!(port, handler = spec.handler, "portable HTTP function listening");
    #[cfg(not(feature = "aws"))]
    println!(
        "{{\"level\":\"info\",\"event\":\"portable_http_listening\",\"handler\":\"{}\",\"port\":{}}}",
        spec.handler,
        port
    );

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(feature = "http")]
pub fn configured_port() -> Result<u16, String> {
    let azure = read_optional_env("FUNCTIONS_CUSTOMHANDLER_PORT")?;
    let generic = read_optional_env("PORT")?;
    resolve_port(azure.as_deref(), generic.as_deref())
}

pub fn resolve_port(azure: Option<&str>, generic: Option<&str>) -> Result<u16, String> {
    for (key, candidate) in [
        ("FUNCTIONS_CUSTOMHANDLER_PORT", azure),
        ("PORT", generic),
    ] {
        if let Some(value) = candidate.map(str::trim).filter(|value| !value.is_empty()) {
            let port = value
                .parse::<u16>()
                .map_err(|_| format!("{key} must be an integer TCP port from 1 through 65535"))?;
            if port == 0 {
                return Err(format!("{key} must be an integer TCP port from 1 through 65535"));
            }
            return Ok(port);
        }
    }
    Ok(DEFAULT_HTTP_PORT)
}

#[cfg(feature = "http")]
async fn health() -> Response {
    http_response(function_response(200, json!({"ok":true}), None))
}

#[cfg(feature = "http")]
async fn http_handler(State(spec): State<RouteSpec>, request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let original_path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let body = match to_bytes(request.into_body(), spec.max_event_bytes.saturating_add(1)).await {
        Ok(body) => body,
        Err(_) => {
            return http_response(function_response(
                413,
                json!({"error":"event_too_large","max_bytes":spec.max_event_bytes}),
                None,
            ));
        }
    };
    if body.len() > spec.max_event_bytes {
        return http_response(function_response(
            413,
            json!({"error":"event_too_large","max_bytes":spec.max_event_bytes}),
            None,
        ));
    }

    let (payload, cloud_event_id) = match decode_http_payload(&headers, &body) {
        Ok(value) => value,
        Err(error) => {
            return http_response(function_response(400, json!({"error":error}), None));
        }
    };
    let request_id = cloud_event_id
        .and_then(|value| sanitize_request_id(&value))
        .or_else(|| request_id_from_headers(&headers))
        .unwrap_or_else(generated_request_id);
    let path = if original_path == "/" {
        spec.path.to_owned()
    } else {
        original_path
    };
    let normalized = FunctionRequest {
        method,
        path,
        body: payload,
        request_id,
        event_bytes: body.len(),
    };
    http_response(dispatch_request(&normalized, spec))
}

#[cfg(feature = "http")]
fn decode_http_payload(headers: &HeaderMap, bytes: &[u8]) -> Result<(Value, Option<String>), String> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if content_type.starts_with("application/cloudevents+json") {
        let envelope: Value =
            serde_json::from_slice(bytes).map_err(|_| "invalid_cloud_event_json".to_owned())?;
        if envelope.get("specversion").and_then(Value::as_str) != Some("1.0") {
            return Err("unsupported_cloud_event_version".to_owned());
        }
        for field in ["id", "source", "type"] {
            if envelope
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(format!("missing_cloud_event_{field}"));
            }
        }
        let id = envelope.get("id").and_then(Value::as_str).map(ToOwned::to_owned);
        let data = envelope
            .get("data")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        return Ok((data, id));
    }

    let binary_version = first_header(headers, &["ce-specversion"]);
    if let Some(version) = binary_version.as_deref() {
        if version != "1.0" {
            return Err("unsupported_cloud_event_version".to_owned());
        }
        for name in ["ce-id", "ce-source", "ce-type"] {
            if first_header(headers, &[name]).is_none() {
                return Err(format!("missing_cloud_event_{}", name.trim_start_matches("ce-")));
            }
        }
    }
    let id = first_header(headers, &["ce-id"]);
    if bytes.is_empty() {
        return Ok((Value::Object(Map::new()), id));
    }
    serde_json::from_slice(bytes)
        .map(|value| (value, id))
        .map_err(|_| "invalid_json".to_owned())
}

#[cfg(feature = "http")]
fn request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    first_header(
        headers,
        &[
            "x-request-id",
            "x-correlation-id",
            "x-ms-request-id",
            "x-cloud-trace-context",
            "traceparent",
            "x-openfaas-request-id",
        ],
    )
    .and_then(|value| sanitize_request_id(&value))
}

#[cfg(feature = "http")]
fn first_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
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
    format!("scintilla-{}-{nanos}", std::process::id())
}

#[cfg(feature = "http")]
fn read_optional_env(key: &str) -> Result<Option<String>, String> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{key} is not valid Unicode")),
    }
}

fn has_required_value(body: &Value, field: &str) -> bool {
    body.as_object()
        .and_then(|object| object.get(field))
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::String(text) => !text.trim().is_empty(),
            Value::Array(items) => !items.is_empty(),
            Value::Object(fields) => !fields.is_empty(),
            Value::Bool(_) | Value::Number(_) => true,
        })
}

fn decode_json_value(body: Option<&Value>) -> Value {
    match body {
        Some(Value::String(text)) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Some(value) => value.clone(),
        None => Value::Object(Map::new()),
    }
}

fn function_response(status_code: u16, body: Value, allow: Option<&str>) -> FunctionResponse {
    let mut headers = BTreeMap::from([
        ("cache-control".to_owned(), "no-store".to_owned()),
        ("content-type".to_owned(), "application/json".to_owned()),
    ]);
    if let Some(method) = allow {
        headers.insert("allow".to_owned(), method.to_owned());
    }
    FunctionResponse {
        status_code,
        headers,
        body,
    }
}

fn with_request_id(mut response: FunctionResponse, request_id: &str) -> FunctionResponse {
    response
        .headers
        .insert("x-request-id".to_owned(), request_id.to_owned());
    response
}

fn aws_response(response: FunctionResponse) -> Value {
    json!({
        "statusCode":response.status_code,
        "headers":response.headers,
        "isBase64Encoded":false,
        "body":serde_json::to_string(&response.body)
            .unwrap_or_else(|_| "{\"error\":\"serialization_failed\"}".into()),
    })
}

#[cfg(feature = "http")]
fn http_response(response: FunctionResponse) -> Response {
    let status =
        StatusCode::from_u16(response.status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let bytes = serde_json::to_vec(&response.body)
        .unwrap_or_else(|_| b"{\"error\":\"serialization_failed\"}".to_vec());
    builder.body(Body::from(bytes)).unwrap_or_else(|_| {
        Response::new(Body::from(
            "{\"error\":\"response_build_failed\"}",
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "http")]
    use axum::http::{header, HeaderMap};

    const SPEC: RouteSpec = RouteSpec::new(
        "heavy_document_render",
        "POST",
        "/api/heavy/document-render",
        "document_id",
    );

    fn status(response: &Value) -> u64 {
        response["statusCode"].as_u64().expect("numeric status")
    }

    fn aws_payload(method: &str, path: &str, body: Value) -> Value {
        json!({
            "rawPath":path,
            "requestContext":{"http":{"method":method}},
            "body":body.to_string(),
            "isBase64Encoded":false,
        })
    }

    fn valid_body() -> Value {
        json!({"document_id":"ci-document-1"})
    }

    #[test]
    fn accepts_aws_api_gateway_v1_and_v2() {
        assert_eq!(
            status(&dispatch(
                &aws_payload(SPEC.method, SPEC.path, valid_body()),
                "request-1",
                SPEC,
            )),
            202,
        );
        assert_eq!(
            status(&dispatch(
                &json!({
                    "path":SPEC.path,
                    "httpMethod":SPEC.method,
                    "body":valid_body().to_string(),
                }),
                "request-2",
                SPEC,
            )),
            202,
        );
    }

    #[test]
    fn provider_neutral_dispatch_requires_exact_route() {
        let response = dispatch_request(
            &FunctionRequest {
                method: SPEC.method.to_owned(),
                path: SPEC.path.to_owned(),
                body: valid_body(),
                request_id: "request-3".to_owned(),
                event_bytes: 64,
            },
            SPEC,
        );
        assert_eq!(response.status_code, 202);
        assert_eq!(response.body["handler"], SPEC.handler);
        assert_eq!(response.headers["x-request-id"], "request-3");

        let wrong = dispatch_request(
            &FunctionRequest {
                method: SPEC.method.to_owned(),
                path: "/".to_owned(),
                body: valid_body(),
                request_id: "request-4".to_owned(),
                event_bytes: 64,
            },
            SPEC,
        );
        assert_eq!(wrong.status_code, 404);
    }

    #[cfg(feature = "http")]
    #[test]
    fn decodes_structured_and_binary_cloud_events() {
        let mut structured = HeaderMap::new();
        structured.insert(
            header::CONTENT_TYPE,
            "application/cloudevents+json; charset=utf-8"
                .parse()
                .expect("content type"),
        );
        let structured_event = json!({
            "specversion":"1.0",
            "id":"ce-1",
            "source":"urn:scintilla:test",
            "type":"scintilla.test",
            "data":valid_body(),
        });
        let structured_bytes = structured_event.to_string();
        let (body, id) = decode_http_payload(&structured, structured_bytes.as_bytes())
            .expect("structured cloud event");
        assert_eq!(id.as_deref(), Some("ce-1"));
        assert_eq!(body, valid_body());

        let mut binary = HeaderMap::new();
        binary.insert("ce-id", "ce-2".parse().expect("event id"));
        binary.insert("ce-source", "urn:scintilla:test".parse().expect("event source"));
        binary.insert("ce-type", "scintilla.test".parse().expect("event type"));
        binary.insert(
            "ce-specversion",
            "1.0".parse().expect("event version"),
        );
        let direct = valid_body().to_string();
        let (body, id) =
            decode_http_payload(&binary, direct.as_bytes()).expect("binary cloud event");
        assert_eq!(id.as_deref(), Some("ce-2"));
        assert_eq!(body, valid_body());
    }

    #[test]
    fn rejects_route_method_missing_field_base64_and_oversize() {
        assert_eq!(
            dispatch_request(
                &FunctionRequest {
                    method: SPEC.method.to_owned(),
                    path: "/wrong".to_owned(),
                    body: valid_body(),
                    request_id: "r".to_owned(),
                    event_bytes: 10,
                },
                SPEC,
            )
            .status_code,
            404,
        );
        assert_eq!(
            dispatch_request(
                &FunctionRequest {
                    method: "GET".to_owned(),
                    path: SPEC.path.to_owned(),
                    body: valid_body(),
                    request_id: "r".to_owned(),
                    event_bytes: 10,
                },
                SPEC,
            )
            .status_code,
            405,
        );
        assert_eq!(
            dispatch_request(
                &FunctionRequest {
                    method: SPEC.method.to_owned(),
                    path: SPEC.path.to_owned(),
                    body: json!({}),
                    request_id: "r".to_owned(),
                    event_bytes: 10,
                },
                SPEC,
            )
            .status_code,
            422,
        );

        let mut encoded = aws_payload(SPEC.method, SPEC.path, valid_body());
        encoded["isBase64Encoded"] = Value::Bool(true);
        assert_eq!(status(&dispatch(&encoded, "r", SPEC)), 415);

        let tiny = RouteSpec {
            max_event_bytes: 8,
            ..SPEC
        };
        assert_eq!(
            dispatch_request(
                &FunctionRequest {
                    method: SPEC.method.to_owned(),
                    path: SPEC.path.to_owned(),
                    body: valid_body(),
                    request_id: "r".to_owned(),
                    event_bytes: 9,
                },
                tiny,
            )
            .status_code,
            413,
        );
    }

    #[test]
    fn resolves_azure_port_before_generic_port() {
        assert_eq!(
            resolve_port(Some("8082"), Some("8081")).expect("azure port"),
            8082
        );
        assert_eq!(resolve_port(None, Some("8081")).expect("generic port"), 8081);
        assert_eq!(resolve_port(None, None).expect("default port"), 8080);
        assert!(resolve_port(Some("0"), None).is_err());
        assert!(resolve_port(None, Some("not-a-port")).is_err());
    }

    #[test]
    fn sanitizes_untrusted_correlation_ids() {
        assert_eq!(
            sanitize_request_id(" request\r\nid/1 "),
            Some("requestid/1".to_owned())
        );
        assert_eq!(sanitize_request_id("  "), None);
    }
}
