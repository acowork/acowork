# ADR-120 Splitting the Agent System Prompt: `system.md` (Role Identity) and `rules.md` (Business-Scenario Rules)

- **Status**: Accepted (Runtime + the six official agent packages implemented)
- **Date**: 2026-11-23
- **Decision maker**: 大鱼
- **Prerequisites**: [ADR-063](./ADR-063-package-level-prompt-override.md) (special `prompts/` filenames + prompt editing/hot reload), [ADR-094](./ADR-094-progressive-skill-auto-load-and-load-skill-tool.md) (dialog base kept separate from the skills summary), [ADR-104](./ADR-104-agent-manifest-declaration-surface.md) (the `.agent` package is a build artifact)

---

## 1. Background

### 1.1 Fact: one `system.md` carried two different kinds of content

Before the split, every agent package had exactly one `prompts/system.md`, and it held two kinds of material at once:

| Kind | Example (senior-engineer) | Must it change per business scenario? |
|---|---|---|
| Role attributes | identity, domain expertise, engineering principles, code-review checklist, debugging method, communication and output format | No — changing it changes the role |
| Business-scenario rules | project participation loop, system of record, deliverable area `{doc_path}/技术方案/`, approval gates, state machine, non-goals | Yes — different scenarios have different deliverable conventions, tool surfaces and approval chains |

That is the problem: **landing the same role in a different business scenario could only be done by rewriting `system.md` wholesale**. As a result

- one edit mixed "the role changed" with "the scenario changed", and a reviewer could not tell them apart;
- two deployments of the same role could not maintain their own rule sets — they copied the whole prompt and drifted;
- facts (which service is the system of record, where deliverables land) sat buried in identity prose, so readers took them for preferences rather than hard constraints.

### 1.2 Fact: how the Runtime assembles `prompts/`

`build_system_prompt_with_mode` (`core/acowork-runtime/src/package/prompt_builder.rs`):

1. `collect_markdown_files` gathers every `.md` / `.txt` under `prompts/`;
2. it **sorts them by filename**;
3. it skips the task-instruction files listed in `OVERRIDABLE_PROMPTS` (`summary.md`, `title.md`, `compact-template.md`, `abstention.md`, `chat.md`);
4. each file becomes `## {stem}\n\n{body}`, joined with `\n\n---\n\n`.

So a newly added `prompts/rules.md` **is picked up automatically** — but the filename sort puts it **before** `system.md`, i.e. the contract would precede the identity it constrains.

### 1.3 Fact: the per-instance customization path already exists

- an installed agent instance owns a full copy of `prompts/` and `skills/`;
- the Setup panel's Prompts & Skills card reads and writes those two subtrees through the `__agent_package__` workspace (ADR-063 §3.7);
- saving calls `POST /api/agents/{id}/prompts/reload`, which rebuilds the main system prompt and pushes it to every live session.

Conclusion: **customizing an agent for a business scenario needed no new mechanism.** What was missing was cutting the customizable part out of the role identity.

## 2. Decisions

### Decision 1: The boundary between the two files is "does replacing it replace the role?"

- `prompts/system.md` — **role identity**: who it is, what it is expert in, which principles it holds, how it speaks. Scenario-agnostic; replacing it means a different agent (a new package, not a new config).
- `prompts/rules.md` — **business-scenario contract**: what is the system of record, which tools are allowed, where deliverables land, which approval gates exist, which workflow to follow, what not to do. Replacing it keeps the role and changes how the work is done.

The criterion is operational: **swap the file — if the team would have to re-learn *who this agent is*, it belongs in `system.md`; if it would only have to learn *how it must work here*, it belongs in `rules.md`.**

Two boundaries that are easy to get wrong, decided by the same criterion:

- **Role methodology stays in `system.md`** (the senior engineer's five-step debugging method and code-review checklist; the architect's design-review checklist and anti-patterns) — it is how this professional works, and it does not change with the scenario.
- **Platform/project workflow goes to `rules.md`** (`Architecture Workflow`, `Default Workflow`, `Quality Workflow`, `Your Loop`) — it is determined by how the project runs, and it changes with the scenario.

### Decision 2: Fixed section order `system.md` → `rules.md` → the rest by filename

`PROMPT_SECTION_ORDER = [SYSTEM_PROMPT_FILE, RULES_PROMPT_FILE]` plus `prompt_section_rank()`: listed files first in list order, everything else after, by filename.

- Rationale: identity precedes the contract that constrains it, and both precede incidental sections.
- Matching is **case-sensitive and whole-filename**, in lockstep with `is_overridable_prompt` — otherwise on a case-insensitive filesystem `Rules.md` would silently take `rules.md`'s position while being treated as an ordinary section elsewhere.

### Decision 3: `rules.md` is an ordinary dialog section, not an instruction override

It does **not** join `OVERRIDABLE_PROMPTS`:

- it must appear in the main system prompt (otherwise nobody reads it);
- it does not replace a hardcoded constant, so the `load_optional_prompt` override chain means nothing for it;
- it must be directly editable in the Setup panel's prompt tree — being an ordinary section already gives it that.

### Decision 4: Single-file packages stay backward compatible

A package with only `system.md` behaves exactly as before the split (pinned by `test_build_system_prompt_without_rules_is_unchanged`). No data migration, no package-format version bump.

### Decision 5: Customization lands in the instance's `prompts/rules.md`

The packaged `rules.md` is a **default**, not the only definition. To land the same role elsewhere:

1. edit `prompts/rules.md` for that instance in the Setup panel's Prompts & Skills card;
2. save — the reload applies it to the next turn, with no restart, no package change, no new ADR;
3. scenario changes stop touching role identity, so a reviewer sees "this is a scenario change" at a glance.

### Decision 6: The official packages migrated in this batch; the repack rides the merge

`project-manager`, `document-manager`, `product-manager`, `quality-assurance`, `senior-engineer`, `software-architect` were split per Decision 1, each got a patch version bump. Repacking `agents/agent-packages/*.agent` is **deliberately not part of this batch**: another branch still holds unmerged agents, and packaging + publishing will be done once after the merge, so the repository never carries a half-repacked set (some packages signed with new content, others still old).

The cost is explicit: until that joint repack, `core/acowork-sign/tests/example_packages.rs` (which requires each package to match its source tree byte for byte) stays red. That is the **expected** state, not a defect; it turns green with the repack.

The third-party `github.ponytail` package is not migrated: its prompt *is* one engineering persona with no platform-scenario contract to cut out — splitting it would only manufacture an empty file.

## 3. Consequences

### Positive

- "The role changed" and "the scenario changed" become two visible, separately reviewable edits.
- Multiple deployments of one role share a single `system.md`; their rule sets evolve independently instead of being copied wholesale.
- Scenario rules (system of record, deliverable areas, approval gates) live in one place, so a newcomer reads one file to learn how work must be done here.
- The section order is fixed by code, not by a coincidence of filename sorting.

### Negative / trade-offs

- A package upgrade overwrites the instance's scenario customization with the packaged `rules.md` — **instance-level customizations do not survive an upgrade today**. Rules that must persist belong in the package source, or need a separate override layer (out of scope for this ADR).
- Two files force an authorship judgement; a borderline section (e.g. a role's methodology) needs an explicit decision. The criterion in Decision 1 is the only rule.
- The assembled character count is essentially unchanged, so this costs no extra context.

### Verification

- `cargo test -p acowork-runtime --lib`: 2117 passed (including the 5 new cases).
- `cargo test -p acowork-core --test agent_participation_contract`: 3 passed.
- `./dev/ci.sh redlines`: all green.
- The packaging check (`cargo test -p acowork-sign --test example_packages`) runs **after the joint repack** — see Decision 6; it is expected to stay red until then.

## 4. Related files

- [core/acowork-runtime/src/package/prompt_builder.rs](../../../core/acowork-runtime/src/package/prompt_builder.rs) (`SYSTEM_PROMPT_FILE`, `RULES_PROMPT_FILE`, `PROMPT_SECTION_ORDER`, `prompt_section_rank`)
- [core/acowork-runtime/src/http/prompts.rs](../../../core/acowork-runtime/src/http/prompts.rs) (save → reload chain, unchanged)
- [agents/senior-engineer-agent/prompts/system.md](../../../agents/senior-engineer-agent/prompts/system.md) / [rules.md](../../../agents/senior-engineer-agent/prompts/rules.md)
- [docs/design/en/02-agent-package.md](../design/en/02-agent-package.md) (package structure)
- [docs/design/en/03-agent-runtime.md](../design/en/03-agent-runtime.md) §3.1 (context assembly, layer 1)
