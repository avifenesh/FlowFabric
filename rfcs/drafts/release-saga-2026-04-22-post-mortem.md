# v0.3.x release saga post-mortem (2026-04-22)

Author: Worker JJ. Scope: single-session retrospective of the v0.3.0 → v0.3.3
release cycle on 2026-04-22, its three yanks, and the infrastructure added
in response.

## Timeline

All times Europe/Jerusalem (commit `%ci` timestamps).

| Event | Time | Ref |
|---|---|---|
| v0.3.0 tagged (first attempt) | prior to 16:35 | tag `v0.3.0` |
| v0.3.0 publish run fails at `ff-engine` (ff-observability missing from crates.io); 3 of 7 crates published (`ferriskey`, `ff-core`, `ff-script`) | prior to 16:35 | `.github/workflows/release.yml:23-27` (post-fix comment documents the failure); CHANGELOG.md:86-93 |
| v0.3.0 yank of the 3 partial crates | — | CHANGELOG.md:91 |
| PR #130 "Hotfix v0.3.1: release workflow + bridge-event gaps" merged, adds `ff-observability` publish step | 17:42:57 | commit 68c1b5b |
| v0.3.1 tagged (second attempt) | 17:42–18:17 | tag `v0.3.1`; CHANGELOG.md:68-75 |
| v0.3.1 publish run fails at `ff-sdk` (ff-backend-valkey missing); 6 of 9 crates published (`ferriskey`, `ff-core`, `ff-script`, `ff-observability`, `ff-engine`, `ff-scheduler`) | 18:17–18:35 | CHANGELOG.md:62-65 |
| v0.3.1 yank of the 6 partial crates | — | CHANGELOG.md:62-65 |
| PR #131 "Hotfix v0.3.2: add ff-backend-valkey to publish set" merged | 18:35:12 | commit da89fa9 |
| v0.3.2 tagged (third attempt) — all 9 crates publish clean | 18:35–19:29 | tag `v0.3.2`; CHANGELOG.md:48-66 |
| v0.3.2 published-artifact smoke (Worker X) finds `ScannerFilter` unconstructible + `CompletionBackend` unreachable through `FlowFabricWorker` | 19:29 window | CHANGELOG.md:8-29; PR #134 body |
| PR #132 "add publish-list drift check against workspace" merged (preventive, catches failure class of v0.3.0/0.3.1 at PR time) | 19:29:19 | commit ba7459d |
| PR #134 "Hotfix v0.3.3: ScannerFilter builder + CompletionBackend accessor" merged | 19:53:50 | commit 23d3ce3 |
| v0.3.3 tagged (fourth attempt) — clean | 19:53+ | tag `v0.3.3` |
| PR #136 follow-on: drop unused `BackendTimeouts.keepalive` placeholder | 21:16:35 | commit 37a1ea0 |

Yank incantation (from in-workflow error blocks at `.github/workflows/release.yml:164-168, 246-255`):
`cargo yank --version ${VER} <crate>` for each partial-published crate.

## Root causes

1. **Publish-list drift between workspace and release workflow.** New
   publishable crates added to the workspace were not added to the explicit
   `cargo publish -p <name>` list in `.github/workflows/release.yml`. v0.3.0
   failed because `ff-observability` (introduced in PR #94 — observability
   metrics endpoint) was never added to the publish set. v0.3.1 failed the
   same way because `ff-backend-valkey` (introduced in RFC-012 Stage 1a,
   PR #114) was also missing. Both are called out in the workflow header
   comments (`release.yml:23-31`) and in `feedback_release_publish_list_drift.md`.
   The topological publish order in the workflow (ferriskey → ff-core →
   ff-script → ff-observability → ff-backend-valkey → ff-engine → ff-scheduler
   → ff-sdk → ff-server) is correct; the bug was that the list was incomplete,
   not mis-ordered.

2. **In-workspace tests use crate-private access and miss external-consumer
   breakage.** v0.3.2 shipped `ScannerFilter` (PR #127) with
   `#[non_exhaustive]` + `pub` fields and zero public constructors. Every
   in-tree test passed because struct-literal construction is legal inside
   the defining crate. External crates (cairn) could only name
   `ScannerFilter::NOOP`. Similarly, `subscribe_completions_filtered` was
   reachable via the `CompletionBackend` trait (PR #124) but
   `FlowFabricWorker::backend()` returned `Arc<dyn EngineBackend>` — and
   `CompletionBackend` is a sibling trait, not a supertrait, so no upcast
   path existed on the public SDK surface. The headline filtered-completion-
   subscription API was GA-dead. CHANGELOG.md:8-29; `feedback_smoke_after_publish.md`.

3. **Placeholder-plumbed fields invite the ScannerFilter class of miss.**
   Public fields/options wired into the type system but unread by any code
   path give consumers the impression of functionality that doesn't exist.
   `ScannerFilter`'s initial commit (PR #127) before #134 fit the pattern
   (fields present, no way to set them from outside). `BackendTimeouts.keepalive`
   (dropped in commit 37a1ea0, PR #136) and the original `BackendRetry`
   shape (Worker II follow-on) were the same anti-pattern — placeholder
   knobs consumers could set that no downstream code honored.

## What fixed the class of bug

- **Publish-list drift CI (PR #132, commit ba7459d).** Added to
  `.github/workflows/security-and-quality.yml` as a required gate, wired
  into the `quality-gates-complete` sentinel. Diffs `cargo metadata
  --no-deps` publishable-crate names against the `cargo publish -p <name>`
  list grepped from `release.yml`. Catches root cause (1) at PR-review time
  for every new or removed workspace crate.

- **Smoke-after-publish discipline (post-v0.3.2).** Codified in
  `feedback_smoke_after_publish.md`: spawn a smoke agent in a scratch dir,
  `cargo add ff-sdk@X.Y.Z`, exercise every new or changed public type end-
  to-end. Catches root cause (2) at release-close time. Already proved its
  value — the 0.3.2 smoke (report at `rfcs/drafts/0.3.2-smoke-report.md`
  per CHANGELOG.md:17,28) produced Findings 1 and 2 that drove v0.3.3.

- **Remove-dead-placeholder-fields reflex (PR #136 and Worker II's
  `BackendRetry` reshape).** Documented in the backend-timeouts audit
  (`rfcs/drafts/backend-timeouts-retry-audit.md`, present in this worktree).
  Catches root cause (3) at code-review time by deleting fields that
  haven't been wired within one release window rather than retaining them
  "for future use."

## What would have caught it earlier

Honest retrospective — these were absent at v0.3.0 and could have prevented
one or more cycles.

- **Dry-run publish against a private registry before tag push.** The
  release workflow exercises `cargo package` via `--no-verify` publishes,
  but it does not pre-simulate resolution against an empty registry that
  would surface the ff-observability / ff-backend-valkey gaps. A private-
  registry dry-run would have caught root cause (1) before the real tag.
  Not implemented post-saga; publish-list drift CI (#132) is the cheaper
  substitute but only catches drift that mentions a new crate file, not
  arbitrary resolution issues.

- **`cargo package --workspace --allow-dirty` as an operator pre-tag
  ritual.** Called out in the release doc checklist but skipped on v0.3.0.
  `feedback_release_publish_list_drift.md` raises this to the "How to apply"
  list and PR #131 introduced it as a documented pre-tag step. Would have
  caught (1) with zero CI investment.

- **Manual external-constructibility audit for every new public type.**
  The PR review for #127 (ScannerFilter) had no bot comment on the
  `#[non_exhaustive]` + private-constructor combination because the rule
  is subtle — rustc only rejects at the external call site. The human
  reviewer (manager) missed it. Policy now: any new public type on a
  public surface requires an explicit note during review of how an
  external crate constructs it. The smoke agent catches this empirically;
  the review-time policy catches it before it ships.

## Lessons applied

Concrete policy for future releases, not generic:

1. **Every release tag runs `cargo package --workspace` as the final
   pre-tag check.** Operator-executed; any failure aborts the tag.
   Codified in PR #131's release-doc update and in
   `feedback_release_publish_list_drift.md`.

2. **Every release tag runs a published-artifact smoke in a scratch dir
   before the changelog is closed.** Target is every new or changed
   public type in the release. Failures that block external consumers
   trigger an immediate v0.(patch+1) hotfix rather than waiting for a
   consumer bug report. Codified in `feedback_smoke_after_publish.md`.

3. **Every new public type on a public surface needs a smoke-grade
   external-construction test, not just an in-crate struct-literal test.**
   `#[non_exhaustive]` + `pub` fields without a constructor is the exact
   trap ScannerFilter fell into; it must be audited at PR-review time and
   re-checked at smoke time.

4. **Placeholder fields that aren't wired within one release are
   removed, not retained.** `BackendTimeouts.keepalive` (PR #136) and
   `BackendRetry`'s original shape set the precedent. Retained
   placeholders ship as dead API surface that consumers will set and
   silently rely on.

## Cost

- **Release cycles spent:** 4 (v0.3.0, v0.3.1, v0.3.2, v0.3.3) to ship
  what was targeted for 1.
- **Yanks issued:** 9 crate-versions across 3 bumps (3 from v0.3.0,
  6 from v0.3.1, plus v0.3.2 superseded by v0.3.3 for the ScannerFilter
  hotfix — v0.3.2's 9 crates were not yanked because a second yank of an
  already-used version is structurally fine with consumers re-resolving).
- **Elapsed wall time:** v0.3.0 pre-16:35 → v0.3.3 at 19:53. Roughly
  3h30m from the first failed publish to the clean v0.3.3 publish, plus
  PR #136 follow-on at 21:16 for the placeholder cleanup. Call it
  ~4 hours of elapsed time.
- **Opportunity cost:** Worker X (smoke-after-publish discovery), the
  author of #130, #131, #132, and #134, plus the manager review time on
  each hotfix. Rough order-of-magnitude ~4 agent-hours of hotfix work
  that would not have been needed if the pre-tag ritual had run on
  v0.3.0.
- **Net outcome:** v0.3.3 is externally-usable; three pieces of standing
  infrastructure now guard the next release (publish-list drift CI,
  smoke-after-publish policy, placeholder-removal reflex). The saga was
  expensive but the class of bug is now closed at three distinct layers
  (PR-time, release-time, code-review-time).

## Links (PRs, commits, issues)

- PR #94 — observability /metrics endpoint (introduced ff-observability;
  root cause 1.a) — commit 03910f5.
- PR #114 — RFC-012 Stage 1a EngineBackend trait (introduced
  ff-backend-valkey; root cause 1.b) — commit 81e08dc.
- PR #124 — CompletionBackend trait — commit dd70982.
- PR #127 — ScannerFilter (root cause 2.a) — commit a8c8923.
- PR #130 — Hotfix v0.3.1: add ff-observability to publish set —
  commit 68c1b5b; `release.yml:182-193`.
- PR #131 — Hotfix v0.3.2: add ff-backend-valkey to publish set —
  commit da89fa9; `release.yml:194-206`.
- PR #132 — publish-list drift CI — commit ba7459d;
  `.github/workflows/security-and-quality.yml` (55-line addition).
- PR #134 — Hotfix v0.3.3: ScannerFilter builder + `FlowFabricWorker::completion_backend()` —
  commit 23d3ce3.
- PR #136 — drop unused `BackendTimeouts.keepalive` — commit 37a1ea0.
- Memory: `feedback_release_publish_list_drift.md`,
  `feedback_smoke_after_publish.md`.
- Smoke report referenced in CHANGELOG.md:17,28:
  `rfcs/drafts/0.3.2-smoke-report.md`.
- Related audit in this worktree:
  `rfcs/drafts/backend-timeouts-retry-audit.md`.
