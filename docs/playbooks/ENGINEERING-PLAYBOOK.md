# Alloy Engineering Playbook

> **Purpose**
>
> This document defines the canonical engineering workflow for Alloy.
>
> Every RFC, implementation, review, and pull request should follow this
> process unless explicitly overridden by the project owner.
>
> The goal is predictable engineering, not maximum code generation.

| Field | Value |
| --- | --- |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **Status** | Canonical operating guide |

---

# Core Philosophy

The project optimizes for:

* Correctness over speed.
* Simplicity over cleverness.
* Integration over abstraction.
* Vertical slices over horizontal infrastructure.
* Stable public interfaces.
* Production-ready increments.

Every merged change should leave the repository in a releasable state.

Large speculative frameworks are discouraged.

If something is not required by the current work item (RFC or scoped change),
it should normally not exist.

---

# Engineering Principles

## 1. Architecture is intentional

Architecture V2 defines the product architecture.

RFCs define implementation.

Current `main` defines reality.

Architecture evolves through RFCs—not through opportunistic implementation.

Never redesign architecture during implementation.

---

## 2. Extend, don't replace

Always prefer extending existing:

* traits
* types
* modules
* APIs
* event schemas
* error enums
* configuration surfaces

Avoid parallel implementations.

Avoid v2/v3 APIs.

Avoid temporary compatibility layers unless explicitly required by the RFC.

---

## 3. Integration before abstraction

Before introducing a new abstraction ask:

> Can this integrate into an existing component?

Prefer:

```text
extend Runtime
```

instead of:

```text
create Runtime2
```

Prefer:

```text
extend Scheduler
```

instead of:

```text
introduce GenericTaskFramework
```

Generic infrastructure must have multiple proven consumers before becoming shared.

---

## 4. MVP first

Every RFC should implement the minimum production-capable feature.

Not:

* future plugin APIs
* hypothetical extension points
* speculative configuration
* “for later” abstractions

Future RFCs can always extend.

Removing abstractions is expensive.

---

## 5. Vertical slices

Each RFC should deliver one complete capability end-to-end within its scope.

A thin complete path is preferred over partial infrastructure with no caller.

Example shape (illustrative):

```text
Request → control API → runtime host → storage → observable result
```

Do not ship half a stack “for the next RFC” unless the owning RFC marks it **Stub**.

---

## 6. Public API stability

Once merged, public interfaces are considered stable.

Changing them requires:

* a new RFC (or an explicit amendment review)
* migration rationale
* compatibility analysis

Do not casually rename, reshape, or dual-track APIs.

---

# Authority Order

When documentation disagrees, resolve strictly top-down:

1. **Current `main`** (merged source)
2. **Merged implementation RFCs**
3. **Architecture V2** (frozen product architecture)
4. **Draft RFCs**
5. **Roadmaps**
6. **Review documents / bot comments**

Implementation always follows the highest authority that applies.

Never modify an existing public API solely to match an older Architecture V2 sketch when `main` and a merged RFC already define the contract.

---

# Decision Hierarchy

When multiple reviews disagree:

```text
Architecture V2
      ↓
Current main
      ↓
Merged RFC
      ↓
Compliance review (impl ↔ RFC + V2)
      ↓
Production review
      ↓
Human code review
      ↓
Automated review (e.g. CodeRabbit)
      ↓
Lint / style suggestions
```

CodeRabbit never overrides architecture.

Lints never override design.

AI suggestions never override judgment that conflicts with higher authority.

---

# AI Usage Principles

AI is an engineering assistant.

It is not an architect.

Never accept changes simply because:

* AI suggested them
* CodeRabbit found them
* another model prefers them
* the suggestion looks “more idiomatic” or “more complete”

Every change must satisfy:

* architecture
* current RFC (or scoped work item)
* current codebase
* this playbook’s principles

### When NOT to accept AI suggestions

Reject or defer suggestions that:

* introduce abstractions without a second proven consumer
* expand scope into a future RFC
* redesign Architecture V2 mid-implementation
* add parallel traits/types beside existing ones
* change public APIs for taste
* “improve” performance with unmeasured complexity
* add configuration, metrics, or extension points not required by the RFC
* contradict higher authority in the decision hierarchy

Accept suggestions that:

* fix correctness, data loss, crashes, or contract violations
* close RFC acceptance criteria gaps
* improve error taxonomy / observability required by the RFC
* reduce real complexity without new abstractions
* are essentially free (true nits) and do not obscure the review

---

# RFC Lifecycle

```text
Draft
  ↓
Architecture Review
  ↓
Ready for Implementation   ← RFC frozen for substantive change
  ↓
Implementation
  ↓
Compliance Review
  ↓
Production Review
  ↓
Code review / bot triage
  ↓
Merge
  ↓
Cleanup (status/roadmap; often batched)
```

Only one primary phase should be active at a time for a given RFC.

Prefer a **fresh AI session** between phases to avoid anchoring on the previous phase’s reasoning.

---

# RFC Stability Rules

**Before Architecture Approval**

RFC text may change freely (still docs-only; no code required).

**After Architecture Approval (“Ready for Implementation”)**

Only:

* typo fixes
* clarifications that do not change contracts
* non-normative implementation notes

Substantive contract changes require another architecture review round (and usually a docs PR) before implementation continues.

**After the implementation PR merges**

Treat the RFC + `main` as the contract. Further change needs a new RFC or an explicit amendment with review.

---

# Model Responsibilities

| Phase | Goal | Independence |
| --- | --- | --- |
| Spec | Produce implementation-grade RFC | Writer session |
| Architecture Review | Validate design vs V2 + `main` + prior RFCs | **Must not be the RFC author transcript** |
| Implementation | Write code in RFC scope | Fresh session on latest `main` |
| Compliance | Compare implementation to RFC + V2 | **Independent** strong review |
| Production | Validate production readiness | Independent preferred |
| Automated code review | Find implementation defects | Bot (e.g. CodeRabbit) |
| Nitpick triage | Fix still-valid findings only | Implementer |

### Suggested model class

| Phase | Suggested class | Notes |
| --- | --- | --- |
| Spec | Strong reasoning | Frozen generate-prompt; output RFC only |
| Architecture Review | **Opus-class / principal systems** | Mandatory for critical-path RFCs |
| Implementation | Capable coding agent | Composer / Grok / Claude coding, etc. |
| Compliance | **Opus-class independent** | PASS/FAIL → fix → repeat |
| Production | Opus-class or implementer + checklist | Approve only if ready |
| Bot triage | Coding agent | Verify vs code; skip with reason |

If budget-limited: do not skip independent architecture + compliance review on critical-path work (runtime, storage, session, scheduler, sandbox, MCP, router).

---

# Session Management

Prefer separate AI conversations for:

* RFC generation
* RFC architecture review
* implementation
* compliance gate
* production gate

Avoid carrying long reasoning chains across phases.

Each phase should evaluate the repository independently against authority order and this playbook.

---

# Phase workflow (operational)

## Phase A — Write the RFC (docs only)

**Input:** Implementation-grade generate prompt with binding constraints and required sections.

**Do**

* Expand/replace `docs/rfcs/RFC-NNNN-….md` in place (not an outline).
* Update `docs/rfcs/README.md` only if status / dependencies / effort change.
* Update `example.env` only if new keys are required; **never** write `.env`.

**Do not**

* Write product code or patches.
* Redesign Architecture V2 or prior merged RFCs.
* Invent deferred subsystems.

**Exit:** Another engineer can implement from current `main` using only the RFC.

---

## Phase B — Architecture review (docs PR)

**Who:** Independent Opus-class / principal systems review.

**Verdict:** `APPROVE` | `NEEDS REVISION` | `REJECT`.

**Loop:** revise RFC → re-review until **APPROVE**.

**Merge:** docs PR (e.g. `docs: RFC-NNNN Ready for Implementation`).

**Optional artifact:** `docs/reviews/RFC-NNNN-ARCHITECTURE-REVIEW.md`.

**Exit:** Status Ready for Implementation; RFC substantively frozen.

---

## Phase C — Implement against frozen RFC + `main`

**Who:** Coding agent. New session. Branch from latest `main`.

**Constraints**

* Scope = this RFC’s MVP only.
* Extend existing surfaces; no parallel APIs.
* Respect crate boundaries (≤5 crates MVP; no sixth crate for convenience).
* Never overwrite `.env`.

**PR expectations:** see below.

**Exit:** Feature complete enough for compliance and production gates.

---

## Phase D — Compliance gate (impl ↔ V2 + RFC)

**Who:** Independent strong review.

**Prompt shape**

```text
Compare this implementation against Architecture V2 and RFC-NNNN.
Output: PASS or FAIL with reasons.
If FAIL: fix, then repeat.
```

**Loop until PASS.**

**Exit:** Documented PASS (PR comment or `docs/reviews/…`).

---

## Phase E — Production-ready checklist

**Approve only if production-ready.**

Checklist:

* Acceptance criteria satisfied
* Tests passing
* Documentation complete (for this RFC’s scope)
* Public APIs stable and matching the RFC
* Metrics emitted where the RFC requires them
* Logging implemented where the RFC requires it
* Error handling complete
* Examples / benches compile as applicable
* No TODOs / placeholders in scope (explicit Stub only)
* No dead code / clippy clean / fmt clean
* No known architecture violations

Also run:

```bash
cargo test -p <touched-crate>    # and --workspace if boundaries touched
cargo clippy -p <touched-crate> --all-targets -- -D warnings
cargo fmt --check
```

Satisfy the series [Definition of Done](../rfcs/README.md#definition-of-done-merge-gate).

---

## Phase F — Automated review triage (CodeRabbit, etc.)

**Prompt**

```text
Verify each finding against current code. Fix only still-valid issues,
skip the rest with a brief reason, keep changes minimal, and validate.
```

Classify findings with [Review Severity](#review-severity). Fix Critical/Major before merge; defer Minor; ignore Nit unless free.

**Exit:** Actionables addressed or explicitly skipped in the PR body.

---

## Phase G — Merge and handoff

1. Merge the implementation PR.
2. Prepare the next RFC generate-prompt from lessons learned.
3. Start a **new** session for the next RFC.
4. Batch roadmap / index status updates into a cleanup PR (see Cleanup Policy).

---

# Pull Request Expectations

Implementation PRs SHOULD:

* stay inside RFC (or ticket) scope
* avoid unrelated cleanup and drive-by refactors
* compile and stay reviewable throughout development
* keep commits logically grouped
* explain intentional deviations from the RFC (rare; needs rationale)
* document skipped bot findings with one-line reasons

Implementation PRs SHOULD NOT:

* redesign architecture
* implement the next RFC “while we’re here”
* mix large formatting/refactors with behavior changes
* add speculative abstractions or config

Docs PRs (RFC Ready for Implementation) SHOULD:

* contain only specification / index / example.env documentation
* not include product code

---

# Review Severity

Every finding SHOULD be classified.

### Critical — must fix before merge

* correctness bugs
* data loss / corruption risk
* crashes / deadlocks in normal paths
* public API contract violations
* architecture violations
* security issues in scope

### Major — should fix before merge

* missing tests for acceptance criteria
* incomplete required metrics / logging / error mapping
* durable lifecycle gaps (open/migrate/close/resume) when in scope
* meaningful race or shutdown hazards

### Minor — can defer

* documentation polish
* naming improvements
* readability refactors without behavior change

### Nit — ignore unless essentially free

* pure formatting already covered by `fmt`
* wording preference
* micro-style without clarity gain

Automated tools do not assign severity for you—humans (or the triage agent following this taxonomy) do.

---

# Testing Philosophy

Every RFC SHOULD add tests appropriate to its scope.

Prefer, in order:

1. Unit tests for contracts and edge cases
2. Integration tests for persistence, lifecycle, and cross-module behavior
3. Concurrency / recovery / failure-injection when the RFC’s domain requires them

Defer until justified:

* microbenchmarks as merge gates
* property tests without a clear invariant
* tests that lock incidental implementation details

Tests verify **behavior and contracts**, not private structure.

---

# Observability Requirements

New functionality SHOULD expose, as required by its RFC:

* structured logging for important lifecycle and failure paths
* typed errors with a clear taxonomy and mapping at boundaries
* in-process metrics where the RFC specifies counters/gauges

Debugging production issues SHOULD NOT require a special build for basic “what happened?” questions.

Do not invent OTLP or exporter stacks unless the owning RFC says so.

---

# Definition of Done

A change (typically an RFC implementation) is complete when:

* Acceptance criteria satisfied
* Tests passing
* Clippy clean (touched scope / workspace policy)
* Formatting clean
* Documentation updated for the change
* Public APIs stable and intentional
* Metrics implemented when required
* Logging implemented when required
* Errors handled
* No in-scope TODOs / placeholders (explicit Stub only)
* No dead code
* No known architecture violations
* Code review approved (human and/or completed bot triage)

---

# Common Failure Modes

Avoid these recurring mistakes.

### Premature abstraction

Creating infrastructure before consumers exist.

### Parallel implementations

Introducing replacement systems instead of extending existing ones.

### Scope creep

Implementing future RFCs early “because it’s related.”

### Architecture drift

Changing design during implementation without amending the RFC.

### Review ping-pong

Alternating reviewers without a single authority hierarchy.

### AI over-optimization

Accepting every suggestion despite increased complexity or scope.

### Large unrelated refactors

Making review impossible and hiding behavior changes.

### Rubber-stamp self-review

Using the same transcript to both author and architecture-approve a design.

### Fake milestone completion

Treating a milestone done when only a subset of its RFCs merged.

### Silent public API churn

Renaming or reshaping merged surfaces without an RFC.

---

# Cleanup Policy

Roadmaps, RFC indexes, and status checkboxes SHOULD normally be updated in dedicated cleanup PRs.

Implementation PRs SHOULD focus on implementation.

Do not block a correct merge solely for roadmap bookkeeping—unless a milestone exit explicitly requires status sync.

---

# Prompt templates (short)

### Generate RFC

Use a frozen implementation-RFC prompt: Context (`main` + merged RFCs) → Authority order → Integration points → Binding constraints → Required sections → Output **only** the RFC document.

### Architecture review

```text
Review RFC-NNNN against Architecture V2 (frozen), merged RFCs, and current main.
Verdict: APPROVE | NEEDS REVISION | REJECT.
Mandatory blockers only. No redesign. Cite sections.
```

### Implement

```text
Implement RFC-NNNN against current main.
Follow docs/playbooks/ENGINEERING-PLAYBOOK.md.
Do not redesign V2 or prior RFCs. Stay in MVP scope.
Extend existing APIs; do not invent parallel ones.
Never overwrite .env (example.env only).
```

### Compliance

```text
Compare this implementation against Architecture V2 and RFC-NNNN.
PASS or FAIL with reasons. If FAIL: fix; repeat.
```

### Production gate

```text
Verify this work is complete. Approve only if production ready.
Use the Definition of Done / production checklist in the Engineering Playbook.
```

### Bot triage

```text
Verify each finding against current code. Fix only still-valid issues,
skip the rest with a brief reason and severity, keep changes minimal, and validate.
```

---

# Per-work-item checklist

```text
Work item / RFC-NNNN: _______________

[ ] A  Spec written (implementation-grade)
[ ] B  Architecture review APPROVE (independent)
[ ]    Docs PR merged — Ready for Implementation (RFC frozen)
[ ] C  Impl branch from latest main; scope respected
[ ] D  Compliance PASS (independent; FAIL→fix→repeat)
[ ] E  Production checklist APPROVE; test/clippy/fmt green
[ ] F  Bot triage (Critical/Major fixed; skips documented)
[ ]    Impl PR merged — repo releasable
[ ] G  Next work-item prompt prepared; new session
[ ]    (Later) cleanup: roadmap / indexes / status
```

---

# Golden Rules

1. Current `main` is the source of truth for reality.
2. Architecture changes through RFCs, never through drive-by implementation.
3. Extend existing systems before creating new ones.
4. Integrate before abstracting.
5. Deliver complete vertical slices within scope.
6. Production-ready beats feature-complete theater.
7. Independent review is mandatory for architecture and compliance on critical path.
8. AI assists engineering; it does not replace engineering judgment.
9. If the RFC does not require it, don’t build it.
10. Leave the repository in a releasable state after every merge.

---

# Related docs

* [Architecture V2](../architecture/alloy-architecture-v2.md) (frozen)
* [RFC index + Definition of Done](../rfcs/README.md)
* [Implementation roadmap](../roadmap/IMPLEMENTATION-ROADMAP.md) (sequencing; progress may lag)
* Reviews under [`docs/reviews/`](../reviews/)

— arkadianet
