# Portable Rust serverless entrypoints

This nested crate isolates heavy and infrequent routes from
`apostille-me/apme-web-mash` without changing the normal Mash server
entrypoint. It does not initialize Mash, bind the primary listener, create
WebSockets, or construct the main server/database graph.

The domain validation lives in one provider-neutral `FunctionRequest` →
`FunctionResponse` dispatcher. Thin runtime adapters expose that dispatcher
through three deliberately different execution contracts:

- AWS Lambda Runtime API custom-runtime ZIPs;
- ordinary stateless HTTP/CloudEvents containers for Google Cloud Run, Azure
  Functions custom handlers, Knative, OpenFaaS, and IBM Cloud Code Engine; and
- Oracle Functions/Fn Project `http-stream` over the runtime-provided Unix
  socket.

An AWS `bootstrap.zip` is **not** relabeled as portable. Each adapter has its
own entrypoint and artifact verification.

## Entry points

| AWS Runtime API | Portable HTTP | Oracle/Fn | Route | Required body field |
|---|---|---|---|---|
| `heavy_document_render` | `heavy_document_render_http` | `heavy_document_render_fn` | `POST /api/heavy/document-render` | `document_id` |
| `heavy_case_export` | `heavy_case_export_http` | `heavy_case_export_fn` | `POST /api/heavy/case-export` | `case_id` |

Each route remains independently deployable, so memory, timeout, concurrency,
CPU, and ephemeral-storage settings remain route-specific on every platform.

## Seven-platform target matrix

| Platform | Deployment artifact | Architecture |
|---|---|---|
| AWS Lambda | root-level `bootstrap.zip`, runtime `provided.al2023` | x86_64 and arm64 |
| Google Cloud Run | shell-free stateless HTTP OCI image | x86_64 |
| Azure Functions | custom-handler ZIP with `enableProxyingHttpRequest` | Linux x86_64 |
| Knative Serving/Eventing | OCI HTTP/CloudEvents image | x86_64 or arm64, matching cluster nodes |
| OpenFaaS | OCI HTTP image exposing port 8080 and health endpoints | x86_64 or arm64, matching cluster nodes |
| IBM Cloud Code Engine | stateless HTTP application/container | x86_64 |
| Oracle Functions | Fn Project `http-stream` OCI function | x86_64, arm64, or a verified multi-architecture image |

The common-denominator artifact is Linux x86_64. Arm64 is additionally
published for AWS Lambda, Oracle Functions, and arm64 Knative/OpenFaaS
clusters. Cloud Run remains x86_64-only for bring-your-own executables.

## Portable HTTP and CloudEvents contract

The portable HTTP binary:

- binds `0.0.0.0`;
- prefers `FUNCTIONS_CUSTOMHANDLER_PORT`, then `PORT`, then defaults to `8080`;
- exposes `/healthz` and `/readyz`;
- accepts exact-route JSON requests;
- accepts CloudEvents 1.0 structured and binary modes;
- maps `/` to the route at the function-adapter boundary for event delivery;
- caps request bodies at 1 MiB;
- emits sanitized correlation metadata but never request bodies; and
- returns ordinary HTTP status codes and JSON, not an AWS envelope.

This same process is used for Cloud Run, Azure custom handlers, Knative,
OpenFaaS, and Code Engine.

## AWS Lambda contract

The AWS adapter consumes API Gateway v1/v2 event envelopes through the Lambda
Runtime API and returns the expected `statusCode`/`headers`/`body` envelope.
Each function ZIP contains exactly one executable named `bootstrap` at the
archive root. One architecture is selected per Lambda function version.

## Oracle Functions/Fn Project contract

The Oracle adapter requires:

```text
FN_FORMAT=http-stream
FN_LISTENER=unix:///absolute/runtime/socket
```

It serves HTTP/1.1 directly on that Unix-domain socket. For HTTP-triggered
invocations it reads `Fn-Http-Method`, `Fn-Http-Request-Url`, and
`Fn-Call-Id`. Recoverable function outcomes use outer HTTP `200` and carry the
logical status in `Fn-Http-Status`, matching the Fn FDK protocol. The entrypoint
is a static Rust executable; no experimental Rust FDK or shell-based HotWrap
process is required.

## Validate locally

```bash
cargo +1.88.0 test \
  --manifest-path src/lambdas/Cargo.toml \
  --locked \
  --all-targets
```

CI verifies the committed lockfile, then reuses that exact dependency graph for
both architectures and every adapter build. The combined machine-readable
contract is `platforms/serverless-platforms.v2.json`.

## Build examples

AWS ZIP:

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

Portable HTTP executable:

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

Oracle/Fn executable:

```bash
cargo +1.88.0 build \
  --manifest-path src/lambdas/Cargo.toml \
  --release \
  --locked \
  --no-default-features \
  --features http \
  --target x86_64-unknown-linux-musl \
  --bin heavy_document_render_fn
```

The workflows build and start the portable OCI images with a read-only root
filesystem and writable `/tmp`, exercise direct HTTP plus both CloudEvents
modes, invoke the Fn adapter over a real Unix socket, validate every ELF
architecture, and publish provider manifests, OCI archives, checksums, and
machine-readable evidence.

## Production boundary

The current handlers deliberately stop at a narrow `202 Accepted` seam.
Before production traffic, connect the provider-neutral dispatcher to a
framework-independent domain/service crate. Do not link the Mash UI or primary
server startup graph into these binaries.

- Authenticate before invoking domain work.
- Require a persistent idempotency key before writes or retried work.
- Keep secrets out of ZIPs and OCI layers.
- Embed only small immutable assets; place large generated/media data in object storage.
- Propagate the sanitized request ID into OpenTelemetry.
- Keep the executable entrypoint direct; never interpolate request data through a shell.
