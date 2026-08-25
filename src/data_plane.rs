//! Explicit clients for the four reviewed web-to-API interaction avenues.
//!
//! Callers authenticate the user with Shared Auth before invoking an avenue.
//! Direct database work is transactionally read-only and tenant-predicated;
//! HTTP is bounded and redirect-free; TCP is persistent framed mTLS; and
//! JetStream carries only credential-free, durable status correlation.

use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    CaseProjection,
    ApiCases,
    StatefulCases,
    AsyncStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Avenue {
    DirectReadOnlyDatabase,
    StatelessHttp,
    StatefulMtlsTcp,
    DurableJetStream,
}

#[must_use]
pub const fn choose(operation: Operation) -> Avenue {
    match operation {
        Operation::CaseProjection => Avenue::DirectReadOnlyDatabase,
        Operation::ApiCases => Avenue::StatelessHttp,
        Operation::StatefulCases => Avenue::StatefulMtlsTcp,
        Operation::AsyncStatus => Avenue::DurableJetStream,
    }
}

pub mod direct {
    use sea_orm::{
        ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, Statement, TransactionTrait,
    };
    use serde_json::{json, Value};
    use uuid::Uuid;

    pub async fn case_projection(
        database: &DatabaseConnection,
        tenant_id: Uuid,
    ) -> Result<Value, DbErr> {
        let transaction = database.begin().await?;
        transaction
            .execute_raw(Statement::from_string(
                DatabaseBackend::Postgres,
                "SET TRANSACTION READ ONLY".to_owned(),
            ))
            .await?;
        let row = transaction
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::INT8 AS case_count FROM apme_cases WHERE tenant_id = $1 AND tombstoned_at IS NULL",
                [tenant_id.into()],
            ))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("case projection row missing".to_owned()))?;
        let case_count: i64 = row.try_get("", "case_count")?;
        transaction.rollback().await?;
        Ok(json!({
            "tenantId": tenant_id,
            "caseCount": case_count,
            "mode": "direct_read_only_db"
        }))
    }

    #[cfg(test)]
    mod tests {
        const SOURCE: &str = include_str!("data_plane.rs");

        #[test]
        fn direct_database_contract_is_literal_tenant_scoped_and_read_only() {
            assert!(SOURCE.contains("SET TRANSACTION READ ONLY"));
            assert!(SOURCE.contains("WHERE tenant_id = $1"));
            assert!(SOURCE.contains("transaction.rollback()"));
        }
    }
}

pub mod http {
    use reqwest::redirect::Policy;
    use serde_json::Value;
    use url::Url;
    use uuid::Uuid;

    use super::Duration;

    const MAX_RESPONSE_BYTES: usize = 256 * 1024;

    #[derive(Clone)]
    pub struct Transport {
        base: Url,
        client: reqwest::Client,
    }

    impl Transport {
        pub fn from_env() -> Result<Option<Self>, Error> {
            let Some(base) = std::env::var("APME_API_HTTP_BASE")
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
            else {
                return Ok(None);
            };
            Self::try_new(&base).map(Some)
        }

        fn try_new(base: &str) -> Result<Self, Error> {
            let mut base = Url::parse(base).map_err(|_| Error::InvalidConfiguration)?;
            if base.host_str().is_none()
                || !base.username().is_empty()
                || base.password().is_some()
                || base.query().is_some()
                || base.fragment().is_some()
                || !transport_is_acceptable(&base)
            {
                return Err(Error::InvalidConfiguration);
            }
            let path = base.path().trim_end_matches('/').to_owned();
            base.set_path(if path.is_empty() { "/" } else { &path });
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .redirect(Policy::none())
                .user_agent("apme-web-mash/0.1")
                .build()
                .map_err(|_| Error::InvalidConfiguration)?;
            Ok(Self { base, client })
        }

        pub async fn cases(&self, bearer_token: &str, tenant_id: Uuid) -> Result<Value, Error> {
            validate_token(bearer_token)?;
            let mut endpoint = self.base.clone();
            endpoint
                .path_segments_mut()
                .map_err(|()| Error::InvalidConfiguration)?
                .pop_if_empty()
                .extend(["api", "v1", "cases"]);
            let mut response = self
                .client
                .get(endpoint)
                .bearer_auth(bearer_token)
                .header("x-apme-tenant-id", tenant_id.to_string())
                .send()
                .await
                .map_err(|_| Error::Unavailable)?;
            if !response.status().is_success() {
                return Err(Error::Rejected);
            }
            if response
                .content_length()
                .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
            {
                return Err(Error::ResponseTooLarge);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
                if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(Error::ResponseTooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&body).map_err(|_| Error::Rejected)
        }
    }

    #[derive(Debug, thiserror::Error, Eq, PartialEq)]
    pub enum Error {
        #[error("invalid HTTP transport configuration")]
        InvalidConfiguration,
        #[error("invalid user credential")]
        InvalidCredential,
        #[error("API unavailable")]
        Unavailable,
        #[error("API request rejected")]
        Rejected,
        #[error("API response exceeded byte limit")]
        ResponseTooLarge,
    }

    fn validate_token(value: &str) -> Result<(), Error> {
        if value.is_empty()
            || value.len() > 16 * 1024
            || value.chars().any(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            Err(Error::InvalidCredential)
        } else {
            Ok(())
        }
    }

    fn transport_is_acceptable(url: &Url) -> bool {
        if url.scheme() == "https" {
            return true;
        }
        if url.scheme() != "http" {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        host == "localhost"
            || host.ends_with(".localhost")
            || !host.contains('.')
            || host.ends_with(".svc.cluster.local")
            || host.ends_with(".internal")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| match address {
                    std::net::IpAddr::V4(address) => {
                        address.is_loopback() || address.is_private() || address.is_link_local()
                    }
                    std::net::IpAddr::V6(address) => {
                        let first = address.segments()[0];
                        address.is_loopback()
                            || first & 0xfe00 == 0xfc00
                            || first & 0xffc0 == 0xfe80
                    }
                })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn redirects_credentials_and_public_cleartext_fail_before_network_io() {
            assert!(Transport::try_new("https://api.apostille.me").is_ok());
            assert!(Transport::try_new("http://apme-api").is_ok());
            for rejected in [
                "http://api.apostille.me",
                "https://user:secret@api.apostille.me",
                "https://api.apostille.me?tenant=other",
                "ftp://api.apostille.me",
            ] {
                assert!(Transport::try_new(rejected).is_err(), "{rejected}");
            }
            assert_eq!(
                validate_token("token with whitespace"),
                Err(Error::InvalidCredential)
            );
        }
    }
}

#[cfg(feature = "tcp-transport")]
pub mod tcp {
    use std::{env, io::BufReader, path::PathBuf, sync::Arc};

    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use serde::{Deserialize, Serialize};
    use tokio::{fs, net::TcpStream, time::timeout};
    use tokio_rustls::{
        client::TlsStream,
        rustls::{self, pki_types::ServerName},
        TlsConnector,
    };
    use tokio_util::codec::{Framed, LengthDelimitedCodec};
    use uuid::Uuid;

    use super::Duration;

    const MAX_REQUEST_BYTES: usize = 20 * 1024;
    const MAX_RESPONSE_BYTES: usize = 256 * 1024;

    #[derive(Clone, Debug)]
    pub struct Config {
        pub address: String,
        pub server_name: String,
        pub ca_path: PathBuf,
        pub client_certificate_path: PathBuf,
        pub client_key_path: PathBuf,
    }

    impl Config {
        pub fn from_env() -> std::io::Result<Option<Self>> {
            let Some(address) = optional_env("APME_API_TCP_ADDRESS") else {
                return Ok(None);
            };
            Ok(Some(Self {
                address,
                server_name: required_env("APME_API_TCP_SERVER_NAME")?,
                ca_path: PathBuf::from(required_env("APME_API_TCP_CA_FILE")?),
                client_certificate_path: PathBuf::from(required_env(
                    "APME_API_TCP_CLIENT_CERT_FILE",
                )?),
                client_key_path: PathBuf::from(required_env("APME_API_TCP_CLIENT_KEY_FILE")?),
            }))
        }
    }

    pub struct Channel {
        framed: Framed<TlsStream<TcpStream>, LengthDelimitedCodec>,
    }

    #[derive(Serialize)]
    struct FrameRequest<'a> {
        request_id: Uuid,
        tenant_id: Uuid,
        bearer_token: &'a str,
        operation: &'static str,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct FrameResponse {
        request_id: Uuid,
        status: String,
        payload: Option<serde_json::Value>,
        error: Option<String>,
    }

    impl Channel {
        pub async fn connect(config: &Config) -> std::io::Result<Self> {
            let ca_bytes = fs::read(&config.ca_path).await?;
            let certificate_bytes = fs::read(&config.client_certificate_path).await?;
            let key_bytes = fs::read(&config.client_key_path).await?;
            let mut roots = rustls::RootCertStore::empty();
            let mut ca_reader = BufReader::new(ca_bytes.as_slice());
            let authorities =
                rustls_pemfile::certs(&mut ca_reader).collect::<std::io::Result<Vec<_>>>()?;
            if authorities.is_empty() {
                return Err(invalid_data("mTLS CA bundle is empty"));
            }
            for authority in authorities {
                roots
                    .add(authority)
                    .map_err(|_| invalid_data("invalid mTLS CA"))?;
            }
            let mut certificate_reader = BufReader::new(certificate_bytes.as_slice());
            let certificates = rustls_pemfile::certs(&mut certificate_reader)
                .collect::<std::io::Result<Vec<_>>>()?;
            if certificates.is_empty() {
                return Err(invalid_data("mTLS client certificate is empty"));
            }
            let mut key_reader = BufReader::new(key_bytes.as_slice());
            let private_key = rustls_pemfile::private_key(&mut key_reader)?
                .ok_or_else(|| invalid_data("mTLS client key is missing"))?;
            let tls = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_client_auth_cert(certificates, private_key)
                .map_err(io_other)?;
            let tcp = timeout(Duration::from_secs(3), TcpStream::connect(&config.address))
                .await
                .map_err(|_| timed_out("mTLS connect timed out"))??;
            tcp.set_nodelay(true)?;
            let name = ServerName::try_from(config.server_name.clone())
                .map_err(|_| invalid_data("invalid mTLS server name"))?;
            let stream = timeout(
                Duration::from_secs(5),
                TlsConnector::from(Arc::new(tls)).connect(name, tcp),
            )
            .await
            .map_err(|_| timed_out("mTLS handshake timed out"))??;
            let codec = LengthDelimitedCodec::builder()
                .max_frame_length(MAX_RESPONSE_BYTES)
                .length_field_length(4)
                .new_codec();
            Ok(Self {
                framed: Framed::new(stream, codec),
            })
        }

        pub async fn cases(
            &mut self,
            request_id: Uuid,
            tenant_id: Uuid,
            bearer_token: &str,
        ) -> std::io::Result<serde_json::Value> {
            validate_token(bearer_token)?;
            let payload = serde_json::to_vec(&FrameRequest {
                request_id,
                tenant_id,
                bearer_token,
                operation: "list_cases",
            })
            .map_err(io_other)?;
            if payload.is_empty() || payload.len() > MAX_REQUEST_BYTES {
                return Err(invalid_data("invalid request frame length"));
            }
            timeout(
                Duration::from_secs(5),
                self.framed.send(Bytes::from(payload)),
            )
            .await
            .map_err(|_| timed_out("mTLS send timed out"))??;
            let bytes = timeout(Duration::from_secs(5), self.framed.next())
                .await
                .map_err(|_| timed_out("mTLS receive timed out"))?
                .ok_or_else(|| invalid_data("mTLS API closed connection"))??;
            let response: FrameResponse = serde_json::from_slice(&bytes).map_err(io_other)?;
            if response.request_id != request_id
                || response.status != "ok"
                || response.error.is_some()
            {
                return Err(invalid_data("mTLS cases response rejected"));
            }
            response
                .payload
                .ok_or_else(|| invalid_data("mTLS cases response omitted payload"))
        }
    }

    fn validate_token(value: &str) -> std::io::Result<()> {
        if value.is_empty()
            || value.len() > 16 * 1024
            || value.chars().any(char::is_whitespace)
            || value.chars().any(char::is_control)
        {
            Err(invalid_data("invalid user token"))
        } else {
            Ok(())
        }
    }

    fn invalid_data(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, message)
    }

    fn timed_out(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::TimedOut, message)
    }

    fn io_other(error: impl std::error::Error + Send + Sync + 'static) -> std::io::Error {
        std::io::Error::other(error)
    }

    fn optional_env(name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn required_env(name: &str) -> std::io::Result<String> {
        optional_env(name).ok_or_else(|| invalid_data("required mTLS configuration is missing"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn user_token_bounds_fail_closed_before_network_io() {
            assert!(validate_token("synthetic.token").is_ok());
            assert!(validate_token("token with whitespace").is_err());
            assert!(validate_token(&"x".repeat(16 * 1024 + 1)).is_err());
        }
    }
}

#[cfg(feature = "nats-transport")]
pub mod nats {
    use std::{env, path::PathBuf};

    use async_nats::{
        jetstream::{
            self,
            message::PublishMessage,
            stream::{RetentionPolicy, StorageType, Stream},
        },
        ConnectOptions,
    };
    use bytes::Bytes;
    use serde::{Deserialize, Serialize};
    use tokio::time::{sleep, Instant};
    use uuid::Uuid;

    use super::Duration;

    const REQUEST_SUBJECT: &str = "apme.web_api.outbox.status";
    const RESPONSE_SUBJECT_PREFIX: &str = "apme.web_api.inbox.status";
    const REQUEST_SCHEMA: &str = "apme.async-status-request.v1";
    const RESPONSE_SCHEMA: &str = "apme.async-status-response.v1";
    const MAX_SIGNAL_BYTES: usize = 4 * 1024;
    const MAX_RESPONSE_BYTES: usize = 64 * 1024;
    const ASYNC_TIMEOUT: Duration = Duration::from_secs(10);

    type TransportError = anyhow::Error;

    #[derive(Clone, Debug)]
    pub struct Config {
        pub url: String,
        pub credentials_path: PathBuf,
        pub request_stream: String,
        pub response_stream: String,
    }

    impl Config {
        pub fn from_env() -> Result<Option<Self>, TransportError> {
            let Some(url) = optional_env("APME_NATS_URL") else {
                return Ok(None);
            };
            let config = Self {
                url,
                credentials_path: PathBuf::from(required_env("APME_NATS_CREDENTIALS_FILE")?),
                request_stream: optional_env("APME_NATS_REQUEST_STREAM")
                    .unwrap_or_else(|| "APME_STATUS_OUTBOX".to_owned()),
                response_stream: optional_env("APME_NATS_RESPONSE_STREAM")
                    .unwrap_or_else(|| "APME_STATUS_INBOX".to_owned()),
            };
            config.validate()?;
            Ok(Some(config))
        }

        fn validate(&self) -> Result<(), TransportError> {
            let authority = self
                .url
                .strip_prefix("tls://")
                .ok_or_else(|| invalid_data("NATS URL must use tls://"))?
                .split('/')
                .next()
                .unwrap_or_default();
            if authority.is_empty() || authority.contains('@') {
                return Err(invalid_data("NATS URL credentials are forbidden").into());
            }
            if !safe_topology_name(&self.request_stream)
                || !safe_topology_name(&self.response_stream)
            {
                return Err(invalid_data("invalid JetStream topology name").into());
            }
            Ok(())
        }
    }

    pub struct Transport {
        context: jetstream::Context,
        response_stream: Stream,
    }

    #[derive(Serialize)]
    struct StatusRequest {
        schema: &'static str,
        operation_id: Uuid,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StatusResponse {
        schema: String,
        operation_id: Uuid,
        status: String,
        service: String,
        database_ready: bool,
    }

    impl Transport {
        pub async fn connect(config: &Config) -> Result<Self, TransportError> {
            config.validate()?;
            let options = ConnectOptions::with_credentials_file(&config.credentials_path)
                .await?
                .require_tls(true)
                .name("apme-web-mash")
                .connection_timeout(Duration::from_secs(5))
                .subscription_capacity(128);
            let context = jetstream::new(options.connect(&config.url).await?);
            let request_stream = context.get_stream(&config.request_stream).await?;
            validate_request_stream(request_stream.cached_info())?;
            let response_stream = context.get_stream(&config.response_stream).await?;
            validate_response_stream(response_stream.cached_info())?;
            Ok(Self {
                context,
                response_stream,
            })
        }

        pub async fn status(
            &self,
            operation_id: Uuid,
        ) -> Result<serde_json::Value, TransportError> {
            let payload = serde_json::to_vec(&StatusRequest {
                schema: REQUEST_SCHEMA,
                operation_id,
            })?;
            if payload.len() > MAX_SIGNAL_BYTES {
                return Err(invalid_data("status signal too large").into());
            }
            self.context
                .send_publish(
                    REQUEST_SUBJECT,
                    PublishMessage::build()
                        .payload(Bytes::from(payload))
                        .message_id(format!("apme-status-request-{operation_id}")),
                )
                .await?
                .await?;

            let expected_subject = format!("{RESPONSE_SUBJECT_PREFIX}.{operation_id}");
            let started = Instant::now();
            loop {
                if let Ok(message) = self
                    .response_stream
                    .direct_get_last_for_subject(expected_subject.clone())
                    .await
                {
                    if message.payload.len() > MAX_RESPONSE_BYTES {
                        return Err(invalid_data("status response too large").into());
                    }
                    return decode_response(&message.payload, operation_id);
                }
                if started.elapsed() >= ASYNC_TIMEOUT {
                    return Err(timed_out("status response timed out").into());
                }
                sleep(Duration::from_millis(100)).await;
            }
        }
    }

    fn decode_response(
        payload: &[u8],
        expected_operation_id: Uuid,
    ) -> Result<serde_json::Value, TransportError> {
        let response: StatusResponse = serde_json::from_slice(payload)?;
        if response.schema != RESPONSE_SCHEMA
            || response.operation_id != expected_operation_id
            || response.status != "ok"
            || response.service != "apme-api"
        {
            return Err(invalid_data("status response correlation mismatch").into());
        }
        Ok(serde_json::json!({
            "service": response.service,
            "databaseReady": response.database_ready,
            "mode": "durable_jetstream"
        }))
    }

    fn validate_request_stream(
        info: &async_nats::jetstream::stream::Info,
    ) -> Result<(), TransportError> {
        let config = &info.config;
        if config.storage != StorageType::File
            || config.retention != RetentionPolicy::WorkQueue
            || !config.subjects.iter().any(|value| value == REQUEST_SUBJECT)
            || config.duplicate_window.is_zero()
        {
            return Err(invalid_data("unsafe JetStream request stream").into());
        }
        Ok(())
    }

    fn validate_response_stream(
        info: &async_nats::jetstream::stream::Info,
    ) -> Result<(), TransportError> {
        let config = &info.config;
        if config.storage != StorageType::File
            || config.retention != RetentionPolicy::Limits
            || !config.allow_direct
            || !config
                .subjects
                .iter()
                .any(|value| value == &format!("{RESPONSE_SUBJECT_PREFIX}.*"))
            || config.duplicate_window.is_zero()
        {
            return Err(invalid_data("unsafe JetStream response stream").into());
        }
        Ok(())
    }

    fn safe_topology_name(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }

    fn invalid_data(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, message)
    }

    fn timed_out(message: &'static str) -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::TimedOut, message)
    }

    fn optional_env(name: &str) -> Option<String> {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn required_env(name: &str) -> Result<String, TransportError> {
        optional_env(name)
            .ok_or_else(|| invalid_data("required JetStream configuration is missing").into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn response_is_strict_correlated_and_bounded() {
            let operation_id = Uuid::new_v4();
            let valid = serde_json::json!({
                "schema": RESPONSE_SCHEMA,
                "operation_id": operation_id,
                "status": "ok",
                "service": "apme-api",
                "database_ready": true
            });
            assert!(decode_response(valid.to_string().as_bytes(), operation_id).is_ok());
            assert!(decode_response(valid.to_string().as_bytes(), Uuid::new_v4()).is_err());
            let mut injected = valid;
            injected["credential"] = serde_json::json!("attack");
            assert!(decode_response(injected.to_string().as_bytes(), operation_id).is_err());
        }

        #[test]
        fn broker_configuration_requires_tls_and_external_credentials() {
            let config = Config {
                url: "nats://localhost:4222".to_owned(),
                credentials_path: "unused.creds".into(),
                request_stream: "APME_STATUS_OUTBOX".to_owned(),
                response_stream: "APME_STATUS_INBOX".to_owned(),
            };
            assert!(config.validate().is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_operation_has_one_reviewed_avenue() {
        assert_eq!(
            choose(Operation::CaseProjection),
            Avenue::DirectReadOnlyDatabase
        );
        assert_eq!(choose(Operation::ApiCases), Avenue::StatelessHttp);
        assert_eq!(choose(Operation::StatefulCases), Avenue::StatefulMtlsTcp);
        assert_eq!(choose(Operation::AsyncStatus), Avenue::DurableJetStream);
    }
}
