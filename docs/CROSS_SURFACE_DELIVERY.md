# Cross-surface delivery

Verified **2026-08-06**.

## Surfaces

- Rust MASH web server: `apostille-me/apme-web-mash`
- Flutter Android/iOS, Flutter Web, and Flutter desktop: `apostille-me/apostille-flutter` — proposed/planned
- Rust desktop document helper: `apostille-me/apostille-desktop.rs` — proposed/planned
- Shared contracts: Apostille Me interfaces, generated clients, case/document/evidence schemas, routes, redacted fixtures, and conformance tests

Repository names are allocation targets until their remotes and builds are verified.

## Judgment-based propagation

Evaluate mobile, Flutter Web, Flutter desktop, Rust desktop, and shared contracts for every user-visible or contract-changing web change. Public information, SEO, and web-only case administration may remain web-only. Scanning, watched folders, local normalization/redaction, checksums, evidence packaging, secure storage, and offline preparation may be native-specific. Case/document state, appointments, deadlines, jurisdictions, permissions, errors, notifications, and navigation normally propagate or require an explicit rationale and parity issue.

## Deep links

```text
https://<verified-apostille-me-owned-host>/open/<route>?<bounded-query>
```

The host must be verified. A custom-scheme fallback requires a reviewed ADR and must not be guessed. All surfaces share versioned route types and redacted fixtures and support cold start, already-running delivery, authentication resume, replay/expiry rejection, browser fallback, and explicit confirmation before case, document, appointment, evidence, import/export, or destructive actions.

Never put identity-document data, case files, document images/PDFs, absolute local paths, credentials, access codes, legal notes, or bearer/refresh tokens in URLs. Use bounded identifiers or short-lived, single-use, audience-bound codes and validate route version, case/document/appointment/evidence IDs, action, authorization, jurisdiction, limits, and user intent.

## Review checklist

- [ ] Flutter Android/iOS impact evaluated.
- [ ] Flutter Web/mobile-web impact evaluated.
- [ ] Flutter desktop impact evaluated.
- [ ] Rust desktop impact evaluated.
- [ ] Shared case/client/route/fixture impact evaluated.
- [ ] Deep-link and document/evidence compatibility tested where relevant.
- [ ] Omitted surfaces have a rationale and follow-up when needed.

## Routing

- GitHub Project: [`apostille-me-project` — Project 1](https://github.com/orgs/apostille-me/projects/1)
- Linear project: [`github.com/apostille-me`](https://linear.app/denman/project/githubcomapostille-me-c884fbdbd637)
- Central policy: [`cross-surface-delivery.md`](https://github.com/ORESoftware/project-registry/blob/main/docs/cross-surface-delivery.md)
- Desktop registry: [`desktop-applications.json`](https://github.com/ORESoftware/project-registry/blob/main/registry/desktop-applications.json)
