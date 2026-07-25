# Alloy RFC Implementation Playbook

| Field | Value |
| --- | --- |
| **Author** | arkadianet |
| **Date** | 2026-07-25 |
| **Applies to** | RFC-0001 … RFC-0016 (and follow-ons) |
| **Proven on** | RFC-0001 (PR #1–#2), RFC-0002 (PR #3–#4), RFC-0003 docs (PR #6) |

Use this as the default loop for every RFC. Do not redesign Architecture V2. Prefer vertical slices; defer bookkeeping (roadmap status, RFC-index “Implemented”) to a periodic cleanup pass unless a merge gate requires it.

---

## Authority order (always)

When documents disagree, resolve strictly:

1. **Current `main` source** (merged code)
2. **Merged implementation RFCs** (newest dependency first, e.g. 0002 before 0001 when both apply)
3. **Architecture V2** (intent / frozen product architecture)

Never change a public API only to match an older V2 sketch. V2 wins over *unmerged* RFC drafts; merged RFCs + `main` win over narrative docs (roadmap, reviews).

---

## Model roles (recommended)

| Role | When | Suggested model class | Job |
| --- | --- | --- | --- |
| **Spec writer** | Phase A | Strong reasoning (e.g. Claude Opus / high-thinking) *or* your frozen generate-prompt on a capable agent | Produce implementation-grade RFC only — no code |
| **Architecture reviewer** | Phase B (RFC docs PR) | **Independent** strong model (Opus-class / “principal systems”) — *not* the writer | PASS / NEEDS REVISION / FAIL against V2 + prior RFCs + `main` |
| **Implementer** | Phase C | Coding agent on latest `main` (Composer / Grok / Claude coding) | Code only in RFC scope; no redesign |
| **Compliance gate** | Phase D | **Independent** strong model (Opus-class preferred) | Compare impl ↔ V2 + this RFC; PASS/FAIL → fix → repeat |
| **Production gate** | Phase E | Same as compliance, or implementer with fixed checklist | Final checklist; **Approve only if production-ready** |
| **CodeRabbit** | Phase C–E (on open PR) | Bot | Automated findings; triage, don’t rubber-stamp |
| **Nitpick triage** | After CodeRabbit | Implementer | Verify each finding vs current code; fix still-valid; skip rest with reason |

**Rules of thumb**

- **Never** use the same agent transcript for “write RFC” and “architecture-approve RFC” without a fresh independent review.
- **Opus-class** (or equivalent top reasoning) for Phases B, D, E on critical-path RFCs (storage, session, scheduler, sandbox, MCP, router).
- **Faster coding models** are fine for Phase C and nitpick triage *after* the RFC is frozen.
- If budget-limited: skip Opus on tiny doc-only polish; **do not** skip Opus (or equivalent) on Phase D for critical-path RFCs.

---

## End-to-end phases

```text
A Spec → B RFC review → merge docs PR (“Ready for Implementation”)
      → C Implement on main → D Compliance (PASS/FAIL loop)
      → E Production checklist → F CodeRabbit triage → merge impl PR
      → G Hand off next RFC prompt → (later) cleanup pass
```

### Phase A — Write the RFC (docs only)

**Input:** Frozen generate-prompt (see 0002/0003 style): authority order, binding constraints, required sections, “output ONLY the RFC.”

**Do**

- Expand/replace `docs/rfcs/RFC-NNNN-….md` in place (never leave an outline).
- Update `docs/rfcs/README.md` only if status / deps / effort change (e.g. Draft → Ready for Implementation).
- Touch `example.env` only if new keys are required; **never** write `.env`.

**Do not**

- Write code, patches, or “illustrative” modules.
- Redesign V2 or prior RFCs.
- Invent deferred subsystems.

**Exit:** RFC text is implementable by another engineer against current `main`.

---

### Phase B — Architecture review of the RFC (docs PR)

**Who:** Independent Opus-class / principal systems review.

**Prompt shape**

```text
Review RFC-NNNN against Architecture V2 (frozen), RFC-0001…prior merged RFCs, and current main.
Verdict: APPROVE | NEEDS REVISION | REJECT.
Mandatory findings only for blockers. No redesign.
```

**Loop:** NEEDS REVISION → edit RFC → re-review until **APPROVE**.

**Merge:** Docs PR titled like `docs: RFC-NNNN Ready for Implementation (…)`.

**Artifact (optional but useful):** `docs/reviews/RFC-NNNN-ARCHITECTURE-REVIEW.md` with round history.

**Exit:** Status **Ready for Implementation**; architecture review APPROVE.

---

### Phase C — Implement against frozen RFC + `main`

**Who:** Coding agent. New session. Branch off latest `main` only.

**Constraints**

- Scope = this RFC’s MVP only.
- Extend existing traits/types; no parallel APIs.
- ≤5 crates; no sixth crate; single-binary MVP.
- Never overwrite `.env`.

**PR:** Draft implementation PR early; push often.

**Exit:** Feature complete enough for gates (tests exist; clippy/fmt aimed green).

---

### Phase D — Hard compliance gate (impl ↔ V2 + RFC)

**Who:** Independent Opus-class preferred.

**Prompt shape**

```text
Compare this implementation against Architecture V2 and RFC-NNNN.
Output: PASS or FAIL with reasons.
If FAIL: fix, then repeat.
```

**Loop until PASS.** No “almost.”

**Exit:** Documented PASS (comment or `docs/reviews/PR-N-RFC-NNNN-COMPLIANCE.md`).

---

### Phase E — Production-ready checklist (final gate)

**Who:** Opus-class or implementer with this exact checklist.

```text
Verify this RFC is complete. Approve only if production ready.

Checklist:
- Acceptance criteria satisfied
- Tests passing
- Documentation complete
- Public APIs stable
- Metrics emitted
- Logging implemented
- Error handling complete
- Examples compile (or benches/examples as applicable)
- No TODOs
- No dead code
```

Also run locally / in agent:

```bash
cargo test -p <touched-crate>   # and --workspace if boundaries touched
cargo clippy -p <touched-crate> --all-targets -- -D warnings
cargo fmt --check
```

**Exit:** **APPROVE** — or REJECT with blockers only.

Also satisfy series [Definition of Done](../rfcs/README.md#definition-of-done-merge-gate).

---

### Phase F — CodeRabbit / nitpick triage

**When:** After D/E, or interleaved once the PR is green enough for meaningful bot review.

**Prompt (implementer)**

```text
Verify each finding against current code. Fix only still-valid issues,
skip the rest with a brief reason, keep changes minimal, and validate.
```

**Rules**

- Fix correctness / stability / contract bugs.
- Skip “beyond MVP / changes public Fut contract / intentional race fix” with one-line reason in PR body.
- Re-run tests + clippy after the pass.
- Do not expand scope to satisfy low-value nits.

**Exit:** Actionable findings addressed or explicitly skipped; PR updated.

---

### Phase G — Merge implementation PR & hand off

1. Merge impl PR (squash/merge per repo habit).
2. Delete feature branch when done.
3. Craft the **next** RFC generate-prompt from lessons learned (authority order, extend-don’t-replace, MUST language, concrete `main` integration points).
4. Start a **new** session for the next RFC — do not overload the implementer transcript.

**Deferred by default (cleanup pass later)**

- Roadmap progress / checkbox flips (`docs/roadmap/`)
- RFC index status → Implemented / Merged
- RFC §16 markdown checkboxes inside the RFC body

Do these in a dedicated docs cleanup PR unless a milestone exit needs them now.

---

## Per-RFC checklist (copy/paste)

```text
RFC-NNNN: _______________

[ ] A  Spec written (implementation-grade); README index updated if needed
[ ] B  Architecture review APPROVE (independent model)
[ ]    Docs PR merged — Ready for Implementation
[ ] C  Impl branch from latest main; scope = this RFC only
[ ] D  Compliance PASS vs V2 + RFC (independent model; FAIL→fix→repeat)
[ ] E  Production checklist APPROVE; test/clippy/fmt green
[ ] F  CodeRabbit triage (fix still-valid / skip with reason)
[ ]    Impl PR merged
[ ] G  Next-RFC prompt prepared; new session
[ ]    (Later) cleanup: roadmap + RFC status
```

---

## Prompt templates (short)

### Generate RFC

Use your frozen 0003-style prompt: Context (merged RFCs + `main`) → Authority order → Integration points → Binding constraints → Required sections → Output ONLY the RFC.

### Review RFC (architecture)

```text
You are reviewing RFC-NNNN for Ready-for-Implementation.
Binding: Architecture V2 (frozen), merged RFCs, current main.
Verdict: APPROVE | NEEDS REVISION | REJECT.
Mandatory blockers only. No redesign. Cite sections.
```

### Implement

```text
Implement RFC-NNNN against current main.
RFC path: docs/rfcs/...
Do not redesign V2 or prior RFCs. Stay in MVP scope.
Author: arkadianet. Never overwrite .env (example.env only).
```

### Compliance

```text
Compare this implementation against Architecture V2 and RFC-NNNN.
PASS or FAIL with reasons. If FAIL: fix; repeat.
```

### Production gate

```text
Verify this RFC is complete. Approve only if production ready.
[full checklist from Phase E]
```

### CodeRabbit triage

```text
Verify each finding against current code. Fix only still-valid issues,
skip the rest with a brief reason, keep changes minimal, and validate.
```

---

## Anti-patterns

| Anti-pattern | Why it hurts |
| --- | --- |
| Implement before architecture APPROVE on the RFC | Rework when the contract moves |
| Same agent “approves” its own RFC without a fresh review | Rubber-stamp risk |
| Redesign V2 mid-impl because CodeRabbit suggested it | Scope explosion |
| Closing a milestone when only half its RFCs merged (e.g. M2 = 0002+0004) | Fake progress |
| Fixing every nitpick including out-of-MVP | Noise; delays merge |
| Updating roadmap mid-critical-path | Distraction; batch in cleanup |

---

## Related docs

- [Architecture V2](../architecture/alloy-architecture-v2.md) (frozen)
- [RFC index + Definition of Done](../rfcs/README.md)
- [Implementation roadmap](../roadmap/IMPLEMENTATION-ROADMAP.md) (sequencing; progress may lag)
- Reviews under [`docs/reviews/`](../reviews/)

— arkadianet
