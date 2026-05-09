# Security policy

## Reporting a vulnerability

**Do not open a public GitHub issue for security-sensitive reports.**

Two private channels are accepted:

1. **GitHub Private Vulnerability Reporting** — preferred. On the
   [avifenesh/FlowFabric](https://github.com/avifenesh/FlowFabric)
   repo, click **Security → Report a vulnerability**. This creates a
   private advisory that only the maintainer sees; GitHub coordinates
   CVE assignment and embargoed disclosure from the same surface.
2. **Email** — `aviarchi1994@gmail.com`. Use this as a backup or if
   you cannot use GitHub's flow. Subject line prefix: `[FlowFabric
   SECURITY]`. PGP is not currently offered.

Please include:

- A description of the vulnerability and its impact.
- Steps to reproduce (minimal Rust example, curl invocation, or test
  case preferred).
- Affected version(s) — tag, commit SHA, or `cargo.lock` entry for
  `flowfabric` / any `ff-*` crate.
- Whether you have already disclosed this anywhere.
- Your preferred credit attribution for the advisory (or "anonymous").

## Response expectations

FlowFabric is maintained by a single primary maintainer. Best-effort
commitments:

| Step | Target |
| --- | --- |
| Initial acknowledgement | 72 hours |
| Triage + severity assessment | 7 days |
| Fix released (high/critical) | 30 days from triage |
| Fix released (medium/low) | Next scheduled release |
| Public advisory | After fix is available on crates.io |

If an issue is declined (not a vulnerability, already known, out of
scope), you will receive a written rationale.

## Scope

**In scope.** Code shipped from this repository on crates.io under any
of:

- `flowfabric`
- `ff-core`, `ff-engine`, `ff-scheduler`, `ff-sdk`, `ff-server`,
  `ff-script`
- `ff-backend-valkey`, `ff-backend-postgres`, `ff-backend-sqlite`
- `ff-observability`, `ff-observability-http`
- `ff-test`, `ff-readiness-tests`
- The in-tree `ferriskey` fork of glide-core.

**Out of scope.**

- Vulnerabilities in upstream Valkey, Redis, Postgres, or SQLite.
  Report those to the relevant upstream projects. FlowFabric patches
  for an upstream CVE are in scope if they can mitigate exploitation
  against a FlowFabric deployment — say so in the report.
- Misconfigured deployments where the operator skipped documented
  requirements (missing `FF_API_TOKEN`, no reverse proxy, permissive
  `FF_CORS_ORIGINS=*`, etc.). See
  [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md).
- Issues that require local-process access or physical access to the
  Valkey/Postgres instance.
- Rate-limiting gaps in `ff-server` — documented as "no built-in
  rate limiting" (see [`docs/DEPLOYMENT.md §4`](docs/DEPLOYMENT.md)).
  A reverse proxy is the expected rate-limit point.

## Supported versions

Pre-1.0. Security fixes land on the most recent minor line only.
Older lines may receive fixes at the maintainer's discretion; there is
no LTS.

| Version | Supported |
| --- | --- |
| latest minor (e.g. 0.15.x) | yes |
| prior minors | best-effort only |

## Safe harbour

Good-faith security research conducted under this policy will not be
met with legal action. Specifically, we will not pursue action against
researchers who:

- Make a good-faith effort to avoid privacy violations, data
  destruction, denial of service, and degradation of the live service
  of any user.
- Report the issue through the private channels above and do not
  disclose publicly before a fix is released.
- Use only test accounts / test data they control.

If unsure whether something is in scope or would trigger safe harbour,
ask first via the private channels.

## Hardening-related attacks

FlowFabric depends on several security controls being configured
correctly at deploy time. If you find a FlowFabric-level issue that
breaks the documented guarantee even when those controls *are*
configured, it is in scope. Examples:

- HMAC waitpoint token forgery without knowledge of
  `FF_WAITPOINT_HMAC_SECRET`.
- Authentication bypass on `ff-server` when `FF_API_TOKEN` is set.
- Cross-tenant data leakage via `ScannerFilter` subscriptions.
- Lua FCALL argument injection or privilege escalation in `ff-script`.
- Supply-chain concerns in the `ferriskey` fork or other bundled code.

Out-of-scope because they are documented: deploying `ff-server`
without `FF_API_TOKEN`, running `ff-server` without a reverse proxy,
using `FF_DEV_MODE=1 FF_BACKEND=sqlite` outside of local dev.
