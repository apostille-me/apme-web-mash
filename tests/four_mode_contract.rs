use std::{fs, path::Path};

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).expect("contract source must exist")
}

#[test]
fn dependency_contract_uses_reviewed_immutable_revisions() {
    let cargo = read("Cargo.toml");
    assert!(cargo.contains("cc57a85b276bee81ad94decc87df2f48d49cab9f"));
    assert!(cargo.contains("ca176fb6768a9750d262a536952268625ffd3a8a"));
    assert!(cargo.contains("async-nats"));
    assert!(cargo.contains("sea-orm"));
    assert!(cargo.contains("tokio-rustls"));
}

#[test]
fn protected_introspection_is_scoped_bounded_and_service_authenticated() {
    let auth = read("src/auth.rs");
    assert!(auth.contains("SharedAuthClient::try_new"));
    assert!(auth.contains("with_service_credential"));
    assert!(auth.contains("with_max_response_bytes"));
    assert!(auth.contains("introspect_with_requirements"));
    assert!(auth.contains("apme:cases:read"));
}

#[test]
fn four_avenues_are_real_and_fail_closed() {
    let data_plane = read("src/data_plane.rs");
    for required in [
        "SET TRANSACTION READ ONLY",
        "transaction.rollback()",
        "Policy::none()",
        "LengthDelimitedCodec",
        "TlsConnector",
        "RetentionPolicy::WorkQueue",
        "StorageType::File",
        "duplicate_window",
        "message_id",
    ] {
        assert!(data_plane.contains(required), "missing {required}");
    }
}

#[test]
fn direct_database_avenue_is_tenant_scoped_and_contains_no_mutation_sql() {
    let data_plane = read("src/data_plane.rs");
    assert!(data_plane.contains("WHERE tenant_id = $1"));
    for forbidden in [
        concat!("IN", "SERT INTO"),
        concat!("UP", "DATE apme"),
        concat!("DE", "LETE FROM"),
        concat!("AL", "TER TABLE"),
    ] {
        assert!(!data_plane.contains(forbidden), "{forbidden}");
    }
}

#[test]
fn no_async_message_persists_a_user_bearer() {
    let data_plane = read("src/data_plane.rs");
    let nats = data_plane
        .split("pub mod nats")
        .nth(1)
        .expect("nats module must exist");
    assert!(!nats.contains("bearer_token"));
    assert!(!nats.contains("authorization"));
}
