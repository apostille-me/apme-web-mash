# apme-web-mash

Maud + Axum + SeaORM + Supabase/PostgreSQL + HTMX + WebSocket web server for Apostille Me.

**Product:** Apostille Me — Case operations for visa and apostille consulting.

Track sanitized client references, document workflows, destination jurisdictions, appointments, deadlines, and case events for a visa and apostille consulting firm.

## Safety and production boundary

This software is an operational starter and does not provide legal advice. Keep identity documents and sensitive case files out of logs and this bootstrap data model; production use requires encryption, access controls, retention rules, auditability, and jurisdiction-specific professional review.

This repository is an executable bootstrap, not a production deployment. Before live
use, add authentication, tenant authorization, rate limits, durable migrations,
observability, backups, incident response, dependency review, and secret management.
## Stack

Maud renders escaped server-side HTML, Axum serves HTTP/WebSockets, HTMX handles
progressive updates, SeaORM connects to Supabase-compatible PostgreSQL, and the
browser refreshes fragments after WebSocket notifications.

`DATABASE_URL` and `SUPABASE_URL` are optional for the in-memory bootstrap. Never
expose a Supabase service-role key to browser code.

## Authentication and four data-plane avenues

The integration endpoint `GET /api/data-plane/{direct|http|tcp|nats}` requires both a
fresh `Authorization: Bearer ...` value and an exact `x-apme-tenant-id` UUID. The web
server authenticates through the official Shared Auth Rust client pinned to
`cc57a85b276bee81ad94decc87df2f48d49cab9f`. Protected introspection uses the strict
`IntrospectionRequest` envelope, an independent service credential, the
`apme:cases:read` scope, exact issuer/audience/lifetime checks, and a 64 KiB response
cap. Unknown response fields remain forward-compatible; authority failures, duplicate
requirements, and identity, scope, audience, or tenant mismatches fail closed.

The four avenues are real and deliberately bounded:

1. `direct` opens a SeaORM transaction, executes `SET TRANSACTION READ ONLY`, issues a
   literal tenant-predicated projection query, and rolls the transaction back. No
   migration or mutation surface is present.
2. `http` calls the API with the fresh end-user bearer and tenant header through a
   redirect-free client with connect/total timeouts, safe cleartext-host rules, and a
   256 KiB streamed response cap.
3. `tcp` uses a persistent length-delimited TLS channel with mandatory client
   certificate authentication, bounded frames, per-round-trip timeouts, exact request
   correlation, and a fresh bearer in every frame for API-side re-introspection.
4. `nats` publishes credential-free status signals to the durable
   `apme.web_api.outbox.status` subject and reads correlated responses from
   `apme.web_api.inbox.status.*`. It requires TLS, external credentials, pre-provisioned
   file-backed streams, work-queue request retention, broker deduplication, direct
   bounded status reads, and strict response schemas. No user or service bearer is put
   on the broker.

Ores structured logging is pinned to
`ca176fb6768a9750d262a536952268625ffd3a8a`. Authorization headers are marked sensitive;
credentials, tenant identifiers, URLs, request/response bodies, and document data are
not log fields. Optional transports are disabled when their enabling variable is
absent and fail startup on partial or unsafe configuration. The Zed manifest records
exact package-version intent and locked validation commands; `.zpkg.lock` remains
absent until a real successful resolver run produces it.

## Cross-surface delivery

User-visible, case, document, appointment, deadline, jurisdiction, evidence,
notification, permission, navigation, or deep-link changes in this Rust web
server must be evaluated for:

- `apostille-me/apostille-flutter` on Android, iOS, Flutter Web/mobile web, and
  Flutter desktop;
- `apostille-me/apostille-desktop.rs`, the planned native Rust document-helper
  application; and
- Apostille Me interfaces, generated clients, case/document/evidence schemas,
  route types, redacted fixtures, and conformance tests.

This is judgment-based coordination. Public information, SEO, and web-only case
administration may remain web-specific. Native scanning, watched folders,
local PDF/image normalization, redaction, checksums, evidence packaging, secure
storage, and offline preparation may be native-specific. Case/document state,
appointments, deadlines, permissions, errors, notifications, and navigation
normally require coordinated updates or an explicit no-change rationale and
parity follow-up.

Deep links are HTTPS-first:

```text
https://<verified-apostille-me-owned-host>/open/<route>?<bounded-query>
```

A custom-scheme fallback requires a reviewed ADR before registration; do not
invent one. Web, Flutter, and Rust desktop must share versioned route types and
fixtures and support cold start, already-running delivery, authentication
resume, replay/expiry rejection, and browser fallback. Identity-document data,
case files, document images/PDFs, absolute local paths, credentials, access
codes, legal notes, and bearer/refresh tokens are prohibited in URLs. Case,
document, appointment, and evidence handoffs use bounded identifiers or
short-lived, single-use, audience-bound codes and explicit confirmation.

See [`docs/CROSS_SURFACE_DELIVERY.md`](docs/CROSS_SURFACE_DELIVERY.md) and the
[portfolio policy](https://github.com/ORESoftware/project-registry/blob/main/docs/cross-surface-delivery.md).

## Environment secrets

Secrets live in this repo **encrypted** with [sops](https://github.com/getsops/sops) + [age](https://github.com/FiloSottile/age):
`env/enc/<dev|prod>.env.enc` is committed; `just env-use <name>` decrypts it to
`env/dec/<name>.env` (gitignored, mode 0600) and symlinks `./.env` to it. The
Nix dev shell provides the tooling, `just env-audit` runs keyless in CI, and
containers decrypt at `docker run` — never at build. See [`env/README.md`](env/README.md).
