use lambda_runtime::{run as run_lambda, service_fn, tracing, Error, LambdaEvent};
use serde_json::{json, Map, Value};

pub const DEFAULT_MAX_EVENT_BYTES: usize = 1_048_576;

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
        return Self {
            handler,
            method,
            path,
            required_field,
            max_event_bytes: DEFAULT_MAX_EVENT_BYTES,
        };
    }
}

pub async fn run(spec: RouteSpec) -> Result<(), Error> {
    tracing::init_default_subscriber();
    return run_lambda(service_fn(move |event| handle(event, spec))).await;
}

async fn handle(event: LambdaEvent<Value>, spec: RouteSpec) -> Result<Value, Error> {
    let (payload, context) = event.into_parts();
    return Ok(dispatch(&payload, &context.request_id, spec));
}

pub fn dispatch(payload: &Value, request_id: &str, spec: RouteSpec) -> Value {
    let event_bytes = serde_json::to_vec(payload)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if event_bytes > spec.max_event_bytes {
        return response(
            413,
            json!({"error":"event_too_large","max_bytes":spec.max_event_bytes}),
            None,
        );
    }

    if payload
        .get("isBase64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return response(415, json!({"error":"base64_body_not_supported"}), None);
    }

    let path = payload
        .get("rawPath")
        .or_else(|| payload.get("path"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if path != spec.path {
        return response(404, json!({"error":"route_not_found"}), None);
    }

    let method = payload
        .pointer("/requestContext/http/method")
        .or_else(|| payload.get("httpMethod"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !method.eq_ignore_ascii_case(spec.method) {
        return response(
            405,
            json!({"error":"method_not_allowed"}),
            Some(spec.method),
        );
    }

    let body = decode_body(payload.get("body"));
    if !body
        .as_object()
        .and_then(|object| object.get(spec.required_field))
        .is_some_and(|value| !value.is_null())
    {
        return response(
            422,
            json!({
                "error":"missing_required_field",
                "field":spec.required_field,
            }),
            None,
        );
    }

    tracing::info!(
        handler = spec.handler,
        route = spec.path,
        method = spec.method,
        request_id,
        event_bytes,
        "accepted isolated heavy-route invocation"
    );

    return response(
        202,
        json!({
            "accepted":true,
            "handler":spec.handler,
            "request_id":request_id,
            "route":spec.path,
            "input":body,
        }),
        None,
    );
}

fn decode_body(body: Option<&Value>) -> Value {
    return match body {
        Some(Value::String(text)) => serde_json::from_str(text)
            .unwrap_or_else(|_| Value::String(text.clone())),
        Some(value) => value.clone(),
        None => Value::Object(Map::new()),
    };
}

fn response(status_code: u16, body: Value, allow: Option<&str>) -> Value {
    let mut headers = Map::new();
    headers.insert("content-type".into(), Value::String("application/json".into()));
    headers.insert("cache-control".into(), Value::String("no-store".into()));
    if let Some(method) = allow {
        headers.insert("allow".into(), Value::String(method.into()));
    }

    return json!({
        "statusCode":status_code,
        "headers":headers,
        "isBase64Encoded":false,
        "body":serde_json::to_string(&body)
            .unwrap_or_else(|_| "{\"error\":\"serialization_failed\"}".into()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: RouteSpec = RouteSpec::new(
        "heavy_document_render",
        "POST",
        "/api/heavy/document-render",
        "document_id",
    );

    fn status(response: &Value) -> u64 {
        return response["statusCode"].as_u64().expect("numeric status");
    }

    fn payload(method: &str, path: &str, body: Value) -> Value {
        return json!({
            "rawPath":path,
            "requestContext":{"http":{"method":method}},
            "body":body.to_string(),
            "isBase64Encoded":false,
        });
    }

    #[test]
    fn accepts_exact_api_gateway_v2_route() {
        let response = dispatch(
            &payload("POST", SPEC.path, json!({"document_id":"doc-1"})),
            "request-1",
            SPEC,
        );
        assert_eq!(status(&response), 202);
        assert!(response["body"]
            .as_str()
            .expect("response body")
            .contains("heavy_document_render"));
    }

    #[test]
    fn accepts_api_gateway_v1_shape() {
        let response = dispatch(
            &json!({
                "path":SPEC.path,
                "httpMethod":"POST",
                "body":"{\"document_id\":\"doc-1\"}",
            }),
            "request-2",
            SPEC,
        );
        assert_eq!(status(&response), 202);
    }

    #[test]
    fn rejects_route_method_and_missing_field() {
        assert_eq!(
            status(&dispatch(
                &payload("POST", "/wrong", json!({"document_id":"doc-1"})),
                "r",
                SPEC,
            )),
            404,
        );
        assert_eq!(
            status(&dispatch(
                &payload("GET", SPEC.path, json!({"document_id":"doc-1"})),
                "r",
                SPEC,
            )),
            405,
        );
        assert_eq!(
            status(&dispatch(&payload("POST", SPEC.path, json!({})), "r", SPEC)),
            422,
        );
    }

    #[test]
    fn rejects_base64_and_oversized_events() {
        let mut encoded = payload("POST", SPEC.path, json!({"document_id":"doc-1"}));
        encoded["isBase64Encoded"] = Value::Bool(true);
        assert_eq!(status(&dispatch(&encoded, "r", SPEC)), 415);

        let tiny = RouteSpec {
            max_event_bytes: 8,
            ..SPEC
        };
        assert_eq!(
            status(&dispatch(
                &payload("POST", SPEC.path, json!({"document_id":"doc-1"})),
                "r",
                tiny,
            )),
            413,
        );
    }
}
