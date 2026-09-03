# Portable Rust serverless entrypoints

This nested crate isolates heavy and infrequent routes from `apostille-me/apme-web-mash` without changing the
normal Mash server entrypoint. It does not initialize Mash, bind the
main web listener, create WebSockets, or construct the primary server/database graph.

The domain validation lives in one provider-neutral `FunctionRequest` →
`FunctionResponse` dispatcher. Thin adapters expose it as:

- an AWS Lambda Runtime API process for custom-runtime ZIPs; and
- a stateless HTTP process for Google Cloud Run, Azure Functions custom handlers,
  Knative, OpenFaaS, and IBM Cloud Code Engine applications.

## Entry points

| AWS Runtime API binary | Portable HTTP binary | Route | Required body field | Intended workload |
|---|---|---|---|---|
| `heavy_document_render` | `heavy_document_render_http` | `POST /api/heavy/document-render` | `document_id` | Render a document without starting the Mash web server. |
| `heavy_case_export` | `heavy_case_export_http` | `POST /api/heavy/case-export` | `case_id` | Export a case bundle in an isolated function. |

AWS deployment units:

- `heavy_document_render`
- `heavy_case_export`

Portable HTTP deployment units:

- `heavy_document_render_http`
- `heavy_case_export_http`

Each route remains independently deployable, so memory, timeout, concurrency, CPU, and
ephemeral-storage settings stay route-specific.

## Six-platform target matrix

| Platform | Deployment artifact | Architecture |
|---|---|---|
| AWS Lambda | `bootstrap.zip`, runtime `provided.al2023` | x86_64 and arm64 |
| Google Cloud Run | shell-free OCI HTTP image | x86_64 |
| Azure Functions | custom-handler ZIP with `enableProxyingHttpRequest` | Linux x86_64 |
| Knative | OCI HTTP/CloudEvents image | x86_64 or arm64, matching cluster nodes |
| OpenFaaS | OCI HTTP image listening on port 8080 | x86_64 or arm64, matching cluster nodes |
| IBM Cloud Code Engine | application/container image | x86_64 |

The six-platform common denominator is **Linux x86_64**. Arm64 is retained for AWS Lambda
and for Knative/OpenFaaS clusters that actually schedule arm64 nodes; it is not advertised
for Google Cloud Run, Azure Functions, or IBM Code Engine.

## HTTP and event contract

The portable binary:

- binds `0.0.0.0`;
- prefers `FUNCTIONS_CUSTOMHANDLER_PORT`, then `PORT`, then defaults to `8080`;
- exposes `/healthz` and `/readyz`;
- accepts exact-route JSON requests;
- accepts CloudEvents 1.0 structured and binary modes;
- maps `/` to the route only at the HTTP adapter boundary for Eventarc/Knative-style delivery;
- caps request bodies at 1 MiB;
- emits only sanitized correlation metadata, never request bodies; and
- returns ordinary HTTP status codes and JSON rather than an AWS envelope.

The AWS adapter continues to accept API Gateway v1/v2 envelopes and returns the expected
`statusCode`/`headers`/`body` response shape.

## Validate locally

```bash
cargo +1.88.0 test \
  --manifest-path src/lambdas/Cargo.toml \
  --all-targets

cargo +1.88.0 build \
  --manifest-path src/lambdas/Cargo.toml \
  --release \
  --bins
```

## Build one AWS ZIP

```bash
cargo lambda build \
  --manifest-path src/lambdas/Cargo.toml \
  --release \
  --locked \
  --no-default-features \
  --features aws \
  --output-format zip \
  --bin heavy_document_render
```

Add `--arm64` for the arm64 artifact.

## Build one portable HTTP executable

```bash
cargo +1.88.0 build \
  --manifest-path src/lambdas/Cargo.toml \
  --release \
  --locked \
  --no-default-features \
  --features http \
  --target x86_64-unknown-linux-musl \
  --bin heavy_document_render_http
```

CI packages both artifact families, builds and starts the OCI images with a read-only root
filesystem and writable `/tmp`, exercises direct HTTP plus both CloudEvents modes, validates
ELF architecture, and publishes provider manifests and checksums.

## Production boundary

The current handlers deliberately stop at a narrow `202 Accepted` seam. Before production
traffic, connect the provider-neutral dispatcher to a framework-independent domain/service
crate. Do not link the Mash UI or primary server startup graph into these binaries.

- Authenticate before invoking domain work.
- Require a persistent idempotency key before writes or retried work.
- Keep secrets out of ZIPs and OCI layers.
- Embed only small immutable assets; place large generated/media data in object storage.
- Propagate the sanitized request ID into OpenTelemetry.
- Keep the executable entrypoint direct; never interpolate request data through a shell.
