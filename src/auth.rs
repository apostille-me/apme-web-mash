//! Fail-closed protected introspection for Apostille Me web routes.
//!
//! The end-user bearer and the Shared Auth service credential are independent.
//! Only the official client owns the service credential; neither credential is
//! rendered, persisted, forwarded through JetStream, or logged.

use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};

use shared_auth_client::{ClientError, Introspection, SharedAuthClient};
use thiserror::Error;
use uuid::Uuid;

pub const CASES_READ_SCOPE: &str = "apme:cases:read";
const MAX_INTROSPECTION_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Clone)]
pub struct SharedAuthVerifier {
    client: SharedAuthClient,
    audience: String,
    issuer: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedIdentity {
    pub subject: Uuid,
    pub tenant_id: Uuid,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AuthError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("authentication authority unavailable")]
    Unavailable,
    #[error("invalid Shared Auth configuration")]
    Configuration,
}

impl SharedAuthVerifier {
    pub fn from_env() -> Result<Self, AuthError> {
        let base = required_env("SHARED_AUTH_BASE_URL")?;
        let service_credential = required_env("SHARED_AUTH_INTROSPECTION_CREDENTIAL")?;
        let audience = required_env("SHARED_AUTH_AUDIENCE")?;
        let issuer = required_env("SHARED_AUTH_ISSUER")?;
        Self::try_new(base, Some(service_credential), audience, issuer)
    }

    fn try_new(
        base: impl Into<String>,
        service_credential: Option<String>,
        audience: impl Into<String>,
        issuer: impl Into<String>,
    ) -> Result<Self, AuthError> {
        let audience = audience.into();
        let issuer = issuer.into();
        validate_claim(&audience, 128).map_err(|()| AuthError::Configuration)?;
        validate_claim(&issuer, 256).map_err(|()| AuthError::Configuration)?;
        let mut client = SharedAuthClient::try_new(base.into())
            .map_err(|_| AuthError::Configuration)?
            .with_max_response_bytes(MAX_INTROSPECTION_RESPONSE_BYTES);
        if let Some(credential) = service_credential {
            client = client.with_service_credential(credential);
        }
        Ok(Self {
            client,
            audience,
            issuer,
        })
    }

    pub async fn verify_user(
        &self,
        token: &str,
        required_scopes: &[&str],
    ) -> Result<VerifiedIdentity, AuthError> {
        let identity = self
            .client
            .introspect_with_requirements(token, &self.audience, required_scopes)
            .await
            .map_err(|error| map_client_error(&error))?;
        validate_identity(&identity, &self.audience, &self.issuer, required_scopes)
    }
}

fn validate_identity(
    identity: &Introspection,
    audience: &str,
    issuer: &str,
    required_scopes: &[&str],
) -> Result<VerifiedIdentity, AuthError> {
    if !identity.active
        || identity.aud.as_deref() != Some(audience)
        || identity.iss.as_deref() != Some(issuer)
        || identity.exp.is_none_or(|expiry| expiry <= unix_timestamp())
        || identity
            .nbf
            .is_some_and(|not_before| not_before > unix_timestamp())
        || required_scopes
            .iter()
            .any(|required| !identity.has_scope(required))
    {
        return Err(AuthError::Unauthorized);
    }
    let subject = parse_uuid_claim(identity.sub.as_deref()).ok_or(AuthError::Unauthorized)?;
    let tenant = identity
        .rest
        .get("tenant_id")
        .and_then(serde_json::Value::as_str)
        .or(identity.provider_tenant.as_deref());
    let tenant_id = parse_uuid_claim(tenant).ok_or(AuthError::Unauthorized)?;
    Ok(VerifiedIdentity { subject, tenant_id })
}

fn parse_uuid_claim(value: Option<&str>) -> Option<Uuid> {
    validate_claim(value?, 128).ok()?.parse().ok()
}

fn map_client_error(error: &ClientError) -> AuthError {
    match error {
        ClientError::Unauthorized | ClientError::InvalidInput(_) => AuthError::Unauthorized,
        _ => AuthError::Unavailable,
    }
}

fn required_env(name: &'static str) -> Result<String, AuthError> {
    let value = env::var(name).map_err(|_| AuthError::Configuration)?;
    validate_claim(&value, 16 * 1024).map_err(|()| AuthError::Configuration)?;
    Ok(value)
}

fn validate_claim(value: &str, maximum: usize) -> Result<&str, ()> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(value)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Duration,
    };

    use serde_json::{json, Value};

    use super::*;

    const SERVICE_CREDENTIAL: &str = "independent-apme-web-service-credential-0001";
    const USER_ID: &str = "11111111-1111-4111-8111-111111111111";
    const TENANT_ID: &str = "22222222-2222-4222-8222-222222222222";
    const AUDIENCE: &str = "apostille-me";
    const ISSUER: &str = "https://auth.example.invalid/realms/apostille-me";

    fn verifier(base: String, credential: Option<&str>) -> SharedAuthVerifier {
        SharedAuthVerifier::try_new(base, credential.map(str::to_owned), AUDIENCE, ISSUER).unwrap()
    }

    fn response(audience: &str, scope: &str) -> String {
        json!({
            "active": true,
            "sub": USER_ID,
            "tenant_id": TENANT_ID,
            "iss": ISSUER,
            "aud": audience,
            "exp": 4_102_444_800_u64,
            "scope": scope,
            "futureEnvelopeField": {"safe": true}
        })
        .to_string()
    }

    fn read_request(stream: &mut std::net::TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let count = stream.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
            let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            });
            if content_length.is_none_or(|length| bytes.len() >= header_end + 4 + length) {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn spawn_provider(body: String) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            sender.send(read_request(&mut stream)).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        (format!("http://{address}"), receiver, handle)
    }

    #[tokio::test]
    async fn sends_strict_scoped_envelope_with_independent_service_auth() {
        let (base, requests, handle) =
            spawn_provider(response(AUDIENCE, "apme:cases:read apme:cases:write"));
        let identity = verifier(base, Some(SERVICE_CREDENTIAL))
            .verify_user("signed-browser-token", &[CASES_READ_SCOPE])
            .await
            .unwrap();
        assert_eq!(identity.subject.to_string(), USER_ID);
        assert_eq!(identity.tenant_id.to_string(), TENANT_ID);

        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.starts_with("POST /auth/introspect HTTP/1.1"));
        assert!(request.lines().any(|line| {
            line.eq_ignore_ascii_case(&format!("authorization: Bearer {SERVICE_CREDENTIAL}"))
        }));
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({
                "contract": "IntrospectionRequest",
                "payload": {
                    "token": "signed-browser-token",
                    "audience": AUDIENCE,
                    "requiredScopes": [CASES_READ_SCOPE]
                }
            })
        );
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn unknown_fields_and_omitted_scopes_are_compatible() {
        let (base, requests, handle) = spawn_provider(response(AUDIENCE, ""));
        verifier(base, Some(SERVICE_CREDENTIAL))
            .verify_user("signed-browser-token", &[])
            .await
            .unwrap();
        let request = requests.recv_timeout(Duration::from_secs(2)).unwrap();
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["payload"]["requiredScopes"], json!([]));
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn audience_scope_and_duplicate_requirements_fail_closed() {
        let (base, _requests, handle) =
            spawn_provider(response("other-audience", CASES_READ_SCOPE));
        assert_eq!(
            verifier(base, Some(SERVICE_CREDENTIAL))
                .verify_user("signed-browser-token", &[CASES_READ_SCOPE])
                .await,
            Err(AuthError::Unauthorized)
        );
        handle.join().unwrap();

        assert_eq!(
            verifier("http://127.0.0.1:9".to_owned(), Some(SERVICE_CREDENTIAL))
                .verify_user(
                    "signed-browser-token",
                    &[CASES_READ_SCOPE, CASES_READ_SCOPE]
                )
                .await,
            Err(AuthError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn service_auth_is_independent_from_user_token_parsing() {
        assert_eq!(
            verifier("http://127.0.0.1:9".to_owned(), None)
                .verify_user("invalid browser token with spaces", &[])
                .await,
            Err(AuthError::Unavailable)
        );
    }
}
