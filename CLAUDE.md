
# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 0. Working style

- Never ask whether to take a break, wrap up, sleep on it, or pause. Always describe the next open tasks and be ready to work.
- Keep messages concise with the exact content needed. No wrapping sentences, no personal opinions unless asked.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. Release Gate (every version tag)

**Never tag a release without running the full gate locally.** Crates.io + GitHub are public surfaces; a broken release harms every downstream consumer, and yanking is partial recovery at best.

Mandatory gate before `git tag vX.Y.Z`:

1. **Smoke** — `scripts/smoke-v0.7.sh` (or its successor) PASS on both Valkey + Postgres backends. No skipped scenarios. Capture output in the release PR body.
2. **All examples build clean** — every subdir of `examples/` compiles at the new version. `cargo build --bins` in each.
3. **Every existing example live-runs + the new release example live-runs.** Build-check is not sufficient — examples are consumer docs, and a build-clean example that panics on `.await` is worse than no example. Live-run every subdir of `examples/` that has a runnable bin, not just the one with no external credentials. For examples that need Valkey/Postgres, bring the fixture up (the smoke-gate fixtures from item #1 are reusable). For examples that need `ff-server`, run it. Paste each transcript (or at minimum a one-line PASS/FAIL summary per example plus full transcript for the new headline example) in the PR body. A single example that panics blocks the tag.

   `scripts/run-all-examples.sh` is the mechanical starting point: phase 3a covers build-clean across every example's own Cargo workspace (catches feature-propagation / dep-graph regressions that workspace-level `cargo check` misses). Later phases cover live-run orchestration for HITL and multi-bin scenarios. LLM-dependent examples (`coding-agent`, `llm-race`'s LLM path) stay pre-release-local — CI has no provider keys and mocked LLMs defeat the "real fidelity" point of the examples.
4. **New example for release headlines** — if the release adds consumer-facing surface (new trait methods, new crates, new public fields), land a new example that exercises them in ~150-200 lines. This IS the consumer docs; if we can't cleanly demonstrate the feature, the feature isn't ready. The new example must live-run per item #3 and its full transcript must appear in the PR body.
5. **README validation** — sweep root `README.md` + each `examples/*/README.md` + `docs/CONSUMER_MIGRATION_*.md` for staleness against current main (stale version refs, removed flags, renamed methods, missing new features). One PR, one sweep, before tag.
6. **CHANGELOG complete** — every `### Added` / `### Changed` / `### Fixed` entry since the prior tag is in the new version's section. No open `[Unreleased]` content at tag time.
7. **POSTGRES_PARITY_MATRIX.md current** — every new trait method has a matrix row, every deferral has a pointer to the follow-up issue.
8. **Pre-publish smoke job in release.yml** — must gate the tag workflow. Tag push triggers Verify → Smoke → Publish → GitHub Release; Smoke failure blocks Publish. Do not bypass with `--admin` on the release workflow.

Skipping any of these has cost us a partial-publish recovery before (v0.3.0, v0.6.0, v0.8.0). The gate exists because shortcutting it causes measurable harm — real yanks, real consumer breakage, real trust cost.

## 6. Autonomous-mode decision rule

When operating without real-time owner feedback ("autonomous mode" / "finish what you can" directives):

- **Decide** mechanical choices (wire compatibility, existing-pattern adherence, additive-only extensions, adjacency fixes).
- **Defer with a note** genuine architectural forks that would change scope, consumer contract, or require real tradeoff adjudication. Leave them documented and move on.
- **Never defer correctness-critical releases.** The release gate (§5 above) is not mechanical — it is the owner's insurance against shipping broken artifacts. Autonomous mode may cut a release; it must not cut one that fails any gate.

## 7. Archived design record

Accepted RFCs and historical design/migration docs live in a separate private repo at `avifenesh/flowfabric-archive`. Before writing a new RFC or re-designing a subsystem, check there for prior thinking. Do not duplicate design work.

Main repo keeps only:
- User-facing docs (README, CHANGELOG, MIGRATIONS, CONSUMER_MIGRATION_*, POSTGRES_PARITY_MATRIX, DEPLOYMENT, operator guides, RELEASING)
- Active in-progress RFCs in `rfcs/drafts/` (currently none on main; RFC-020 Wave 9 is tracked on PR #329)
- Code + tests + examples

Pending work lives in GitHub issues on `avifenesh/flowfabric` — not in planning docs.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.
