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
