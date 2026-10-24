# Skill System

> Version: v3.2 | Last Updated: 2026-04-14

---

Skill is the mechanism for extending Agent behavior patterns. Through Skill Instructions, LLM acquires domain-specific knowledge and operation procedures, gaining specialized behaviors beyond its base capabilities.

The Skill system uses a **two-layer model**: SKILL.md as the static definition layer (publishing state), SQLite memory layer as the dynamic experience layer (runtime state). Skills in debugging phase iterate within SQLite; once mature, they are committed to SKILL.md.

## 1. Architecture Overview

```
Skill System
├── Definition Layer (static)    ← SKILL.md files, distributed with .agent package
│   ├── YAML frontmatter        Metadata (name, triggers, tool deps, model compatibility snapshot)
│   └── Markdown body           Instruction body (execution steps, notes, output format)
│
├── Experience Layer (dynamic)   ← SQLite memory nodes, Agent-private
│   ├── SkillDraft              Draft Skill (debugging phase)
│   ├── SkillIteration          Iteration version (snapshot per modification)
│   ├── SkillExecution          Execution record (result of each trial run)
│   └── SkillExperience         Runtime experience of published Skills
│
└── Runtime Integration
    ├── Skill Loader            Load SKILL.md + query SQLite memory layer experience
    ├── Prompt Builder          Merge static definition + dynamic experience + model adaptation
    └── Debug Controller        Debug mode (create/trial run/iterate/publish)
```

**Two-Layer Model Design Principles:**

| Layer | Storage Location | Lifecycle | Distributable | Auditable | Version-controllable |
|-------|-----------------|-----------|---------------|-----------|----------------------|
| Definition Layer | SKILL.md | Versioned with .agent package | Yes | Yes (open and read) | Yes |
| Experience Layer | SQLite memory layer | Persisted in Agent workspace | No (private data) | No (graph DB) | No |

Analogy: SKILL.md is the **textbook** (public, standard, shareable), SQLite memory layer experience layer is the **personal notes** (private, practice-based, individual). Only their combination forms complete Skill behavior.

## 2. SKILL.md Format (Static Definition Layer)

SKILL.md uses YAML frontmatter + Markdown body, compatible with the Agent Skills open standard (agentskills.io).

### 2.1 File Location

```
<agent_id>.agent
└── skills/
    └── <skill_name>/
        ├── SKILL.md           # Required, Skill definition
        └── references/        # Optional, supplementary docs, template data
            ├── template.md
            └── examples.json
```

### 2.2 Complete Format

```yaml
---
# === Metadata ===
name: weekly-report
description: Summarize this week's work into a structured weekly report
version: "1.0.0"
author: agent                    # "agent" = Agent self-created, "developer" = developer-authored
source_draft: draft-abc123       # Associated draft ID when Agent self-creates (traceable)

# === Triggers ===
triggers:
  - weekly report
  - summarize this week
  - week recap

# === Tool dependencies ===
tool_deps:
  # Memory tools are no longer built-ins — they are MCP tools of the Gateway-hosted
  # memory service (ADR-100). Registered names carry the `mcp_<server>__` prefix and
  # the per-tool switch lives in `agent_mcp_tools.json` (ADR-069).
  - mcp_memory__memory_recall
  - file_write

# === Platform compatibility (optional) ===
platforms:
  desktop: required              # Desktop required
  mobile: optional               # Mobile optional (behavior may degrade)

# === Model compatibility (publishing snapshot, runtime uses SQLite memory layer as authoritative) ===
tested_models:
  - provider: openai
    model: gpt-4o
    rating: excellent
  - provider: ollama
    model: qwen3:8b
    rating: good
    note: "Needs flattened instruction adaptation"
---

# Weekly Report Skill

## Execution Steps

1. Use `mcp_memory__memory_recall` to retrieve this week's conversation and work records
2. Organize completed items by project
3. Generate a structured weekly report:
   - Items completed this week (with progress notes)
   - Items in progress (with blockers)
   - Plans for next week
4. Use `file_write` to save to user-specified path

## Output Format

Use Markdown format; one section per project; keep within 500 words.

## Notes

- If there are no work records this week, reply "No work records this week"
- Prefer `mcp_memory__memory_recall` to get data rather than asking user to repeat
- Default style is concise, unless user explicitly requests detailed version
```

### 2.3 Field Descriptions

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Skill name, unique within Agent |
| `description` | Yes | Feature description, used for Skill search and display |
| `version` | No | Skill version number (semver) |
| `author` | No | Creator: `agent` (Agent self-learned) or `developer` (developer-written) |
| `source_draft` | No | Draft ID when Agent self-created, for tracing debugging history |
| `triggers` | Yes | Trigger word list, LLM matches based on user input |
| `tool_deps` | No | Built-in / WASM tools depended on; Runtime uses for permission checks |
| `platforms` | No | Platform support declaration, see §2.4 |
| `tested_models` | No | Model compatibility snapshot, see §5 |

### 2.4 Platform Compatibility Declaration

Skills can declare platform support levels, consistent with Tools' platform mechanism (see `12-tool-system.md` §2.1):

```yaml
platforms:
  desktop: required     # Desktop required, mobile install rejected
  mobile: optional      # Mobile optional, behavior may degrade
  # desktop: true       # Short form, equivalent to required
  # mobile: false       # Short form, equivalent to unsupported
```

| Value | Meaning | Mobile Install |
|-------|---------|----------------|
| `required` | This Skill core depends on desktop capabilities | Reject install |
| `optional` | Can run degraded on mobile | Allow install, behavior limited |
| Not declared | Default all-platform | Allow install |

**Degradation Scenario Example**: A "DevOps Deploy" Skill depending on the `shell` tool declares `desktop: required` because `shell` is unavailable on mobile. A "News Digest" Skill depending on `web_fetch` is all-platform by default because `web_fetch` is all-platform.

## 3. SQLite memory layer Experience Layer (Dynamic Runtime State)

The experience data the Agent accumulates while using Skills is stored in SQLite memory layer as an enhancement layer over the SKILL.md static definition.

### 3.1 Node Type Overview

```
SQLite Memory Layer (`nodes` table)
│
├─ SkillDraft            Draft Skill (debugging phase, unpublished)
├─ SkillIteration        Iteration version (snapshot per draft modification)
├─ SkillExecution        Execution record (result of each trial run)
└─ SkillExperience       Runtime experience of published Skills (success patterns, failure lessons, user preferences, model compatibility)
```

### 3.2 SkillDraft (Draft Skill)

During debugging, Skills self-created by Agent or guided by user are stored as SkillDraft nodes.

```rust
struct SkillDraft {
    draft_id: String,              // Unique ID (auto-generated, e.g. "draft-abc123")
    skill_name: String,            // Skill name
    description: String,           // Description
    instructions: String,          // Instruction body (Markdown)
    triggers: Vec<String>,         // Trigger words
    tool_deps: Vec<String>,        // Depended tools
    created_at: DateTime,
    updated_at: DateTime,
    status: DraftStatus,           // Draft status
}

enum DraftStatus {
    Draft,       // Newly created, not yet tested
    Testing,     // Currently debugging
    Ready,       // Debug complete, awaiting user confirmation to publish
    Published,   // Published to SKILL.md
}
```

### 3.3 SkillIteration (Iteration Version)

Each time a draft is modified, save a complete snapshot of the current version to form a complete iteration history.

```rust
struct SkillIteration {
    iteration_id: String,
    draft_id: String,              // Associated draft
    version: u32,                  // Iteration round (starting from 1)
    instructions: String,          // This round's instruction content (complete snapshot)
    triggers: Vec<String>,
    tool_deps: Vec<String>,
    change_summary: String,        // Modification note ("Added error handling steps")
    trigger_reason: String,        // What triggered this modification ("Run failure: missing data collection")
    created_at: DateTime,
}
```

### 3.4 SkillExecution (Execution Record)

Complete record of each trial run, associated with draft and iteration version.

```rust
struct SkillExecution {
    execution_id: String,
    draft_id: String,              // Associated draft
    iteration_id: String,          // Which iteration version was executed
    outcome: ExecutionOutcome,     // Execution result
    user_feedback: Option<String>, // User feedback ("Output too long")
    error_detail: Option<String>,  // Failure detail
    duration_ms: u64,              // Execution duration

    // Model info
    model_provider: String,        // "openai" / "claude" / "ollama"
    model_name: String,            // "gpt-4o" / "claude-sonnet-4-20250514" / "qwen3:8b"
    model_params: Option<ModelParams>,
    created_at: DateTime,
}

struct ModelParams {
    temperature: f32,
    max_tokens: u32,
}

enum ExecutionOutcome {
    Success,      // Fully successful
    Partial,      // Partial success (completed but with flaws)
    Failure,      // Execution failure
    Skipped,      // Skipped (e.g. insufficient permissions)
}
```

### 3.5 SkillExperience (Runtime Experience of Published Skills)

After a Skill is published, runtime continues to accumulate experience. This experience is injected as supplement during subsequent executions.

```rust
struct SkillExperience {
    skill_id: String,              // Corresponding SKILL.md name
    usage_count: u64,              // Total usage count
    success_count: u64,            // Success count
    last_used: DateTime,

    // Patterns learned from practice
    learned_patterns: Vec<LearnedPattern>,
    failure_cases: Vec<FailureCase>,
    user_preferences: HashMap<String, String>,

    // Model compatibility
    // Linkage: when success rate for some task class on some model falls below 60%, triggers automatic update of AutobiographicalNode Limitation node
    // See 05-memory.md Phase 2 "Self-evaluation Driven AutobiographicalNode Update"
    model_compatibility: HashMap<ModelKey, ModelCompatibility>,
}

struct LearnedPattern {
    pattern: String,               // "When user only says 'weather', default to current city"
    context: String,               // Scenario in which it was learned
    confirmed_count: u32,          // How many times verified
}

struct FailureCase {
    case: String,                  // "api.weather.com occasionally returns 503"
    workaround: Option<String>,    // Workaround ("Retry usually succeeds")
    occurrence_count: u32,         // Occurrence count
}

struct ModelKey {
    provider: String,
    model: String,
}

struct ModelCompatibility {
    tested: bool,
    test_count: u32,
    success_count: u32,
    last_tested: DateTime,
    rating: CompatibilityRating,
    adaptations: Vec<ModelAdaptation>,
    known_issues: Vec<String>,
}

enum CompatibilityRating {
    Excellent,   // Success rate > 90%
    Good,        // Success rate > 70%, with a few known issues
    Limited,     // Success rate > 50%, requires model-specific adaptation
    Untested,    // Not tested
}

struct ModelAdaptation {
    adaptation: String,            // "Use shorter instructions, avoid complex nesting"
    reason: String,                // "qwen3:8b has low compliance rate for long instructions"
    created_at: DateTime,
}
```

### 3.6 Node Relationship Graph

```
SkillDraft (current draft)
  │
  ├─ [HAS_ITERATION] → SkillIteration #1 (initial version)
  │                       │
  │                       ├─ [EXECUTED_AS] → SkillExecution #1 (Failure)
  │                       │                     model: openai/gpt-4o
  │                       │                     feedback: "Missing data collection step"
  │                       │
  │                       ├─ [EXECUTED_AS] → SkillExecution #2 (Success)
  │                       │                     model: openai/gpt-4o
  │                       │
  │                       └─ [NEXT_ITERATION] → SkillIteration #2
  │                                               │
  │                                               ├─ [EXECUTED_AS] → SkillExecution #3 (Partial)
  │                                               │                     model: ollama/qwen3:8b
  │                                               │                     feedback: "Output format chaotic"
  │                                               │
  │                                               └─ [NEXT_ITERATION] → SkillIteration #3 (final)
  │
  └─ [PUBLISHED_AS] → SKILL.md (skills/weekly-report/SKILL.md)
                          │
                          └─ [HAS_EXPERIENCE] → SkillExperience
                                                  │
                                                  ├─ learned_patterns: [...]
                                                  ├─ model_compatibility:
                                                  │   "openai/gpt-4o": Excellent
                                                  │   "ollama/qwen3:8b": Good
                                                  │       └─ adaptations: ["Flattened instructions"]
                                                  └─ user_preferences:
                                                      output_style: "concise"
```

## 4. Skill Lifecycle

The complete Skill lifecycle is divided into three phases: creation and debugging, publication, and runtime evolution.

### 4.1 Phase 1: Creation and Debugging (Pure SQLite memory layer)

```
User: learn how to help me summarize weekly reports
       │
       ▼
① Agent creates SkillDraft in SQLite memory layer
   status = Draft
       │
       ▼
② Enter Debug mode, trial run
       │
       ├─ Run 1 → SkillExecution (Failure)
       │   → Agent modifies draft → SkillIteration #2
       │   → SQLite memory records failure_case + change_summary
       │
       ├─ Run 2 → SkillExecution (Partial)
       │   → User feedback: "Output too long"
       │   → Agent modifies draft → SkillIteration #3
       │
       └─ Run 3 → SkillExecution (Success) ✓
       │
       ▼
③ Agent marks draft as Ready
   status = Ready
       │
       ▼
④ User review (optional)
   ├─ "View debug history" → display all Iteration and Execution records
   ├─ "Roll back to version 2" → restore SkillIteration #2's instructions
   ├─ "Save draft, continue next time" → status remains Ready
   └─ "Publish" → enter Phase 2
```

**Key Debug Mode Capabilities:**

| Capability | Description |
|------------|-------------|
| Trial run | Execute Skill in Debug mode; results not written to production memory |
| Iterative modification | Agent automatically modifies draft instructions based on execution results |
| History tracing | View any iteration version and its execution records at that time |
| User feedback | User can give feedback on each execution result |
| Draft persistence | Save on interruption, continue next time; draft state fully preserved |
| Model switching | Trial run on different models, verify cross-model compatibility |

### 4.2 Phase 2: Publication (SQLite memory → SKILL.md)

After user confirms Skill debugging is complete, Runtime performs the publication operation:

```
① Read SkillDraft final state from SQLite memory
   (latest SkillIteration's instructions + metadata)
       │
       ▼
② Generate YAML frontmatter
   name / description / triggers / tool_deps
   Extract from SkillIteration
       │
       ▼
③ Generate tested_models snapshot
   Extract tested models and their ratings from model_compatibility
       │
       ▼
④ Write to skills/<skill_name>/SKILL.md
       │
       ▼
⑤ Update SQLite memory
   ├─ SkillDraft.status = Published
   ├─ Create SkillExperience node
   │   (migrate learned_patterns / model_compatibility from debugging period)
   └─ Associate SkillDraft → SKILL.md
       │
       ▼
⑥ Notify user publication complete
```

**Published SKILL.md Example (auto-generated):**

```yaml
---
name: weekly-report
description: Summarize this week's work into a structured weekly report
version: "1.0.0"
author: agent
source_draft: draft-abc123
triggers:
  - weekly report
  - summarize this week
  - week recap
tool_deps:
  - mcp_memory__memory_recall
  - file_write
tested_models:
  - provider: openai
    model: gpt-4o
    rating: excellent
  - provider: ollama
    model: qwen3:8b
    rating: good
    note: "Needs flattened instruction adaptation"
---

# Weekly Report Skill

(Markdown body from final iteration version's instructions)
```

### 4.3 Phase 3: Runtime and Evolution (SKILL.md + SQLite memory layer Experience)

When a published Skill executes each time, Runtime assembles the complete context:

```
Skill Loader loads SKILL.md (static definition)
       │
       ▼
SQLite queries SkillExperience node (dynamic experience)
       │
       ├─ No experience node (first use after publication)
       │   → Use SKILL.md original instructions directly
       │
       └─ Has experience node → merge into enhanced Skill instructions:
            │
            ├─ Base instructions from SKILL.md
            ├─ Append learned_patterns as supplementary hints
            ├─ Append user_preferences as constraints
            ├─ Append failure_cases as notes
            └─ Inject current model's adaptations (if any)
       │
       ▼
Execution result written to SQLite memory layer
       │
       ├─ Success → episodic memory + update SkillExperience.success_count
       ├─ Failure → update SkillExperience.failure_cases
       └─ User feedback → update SkillExperience.user_preferences
```

**Self-Learning Closed Loop:**

```
Execute Skill → Record result → Accumulate experience → Enhance next execution
     ↑                                        │
     └────────────────────────────────────────┘
```

When experience accumulates to a certain level (e.g. learned_patterns exceeds 5, or model_compatibility newly adds a low-rated model), Runtime can prompt the user:

> "weekly-report Skill has accumulated 12 new experiences (3 success patterns, 1 new model adaptation). Suggest entering debug mode to update Skill definition."

At the same time, when failure_cases accumulates more than 3 similar failures, it triggers the Skill ↔ ProceduralNode linkage extraction (see `05-memory.md` Phase 2), generating cross-Skill general behavior patterns.

After user confirmation, merge the experience layer back into SKILL.md, forming a new publication round.

## 5. Model Compatibility

Skill execution effectiveness is strongly related to LLM. The same Skill may perform very differently on different models.

### 5.1 Model Compatibility Records

Each Skill's model compatibility is recorded in two layers:

| Layer | Storage | Content | Purpose |
|-------|---------|---------|---------|
| Debugging phase | SkillExecution nodes | Model + result per trial run | Precise tracing per test |
| Experience phase | SkillExperience.model_compatibility | Per-model aggregated compatibility data | Runtime decision basis |
| Publishing state | SKILL.md tested_models | Compatibility snapshot | Distribution reference |

### 5.2 Runtime Model Check

When assembling Skill context, Runtime checks current model compatibility:

```
Check current model (model_provider / model_name)
       │
       ├─ Excellent / Good
       │   → Execute normally
       │
       ├─ Limited
       │   → Inject model adaptations as supplementary hints
       │   → Example: "Note: Current model requires simple direct instruction format."
       │
       └─ Untested (first execution on this model)
           ├─ Do not block execution
           ├─ Auto-record execution result in model_compatibility
           └─ If 3 consecutive failures, notify user:
              "This Skill does not perform well on current model;
               suggest switching to verified model or entering debug mode for adaptation"
```

### 5.3 Cross-Model Debugging

Users can switch models for trial run in Debug mode to verify Skill's cross-model compatibility:

```
Debug complete on GPT-4o → Publish
       │
       ▼
Switch to qwen3:8b → Find instruction compliance rate low
       │
       ▼
Debug adaptation on qwen3:8b → Simplify instruction format
       │
       ▼
Publish update → SKILL.md tested_models adds two records
```

## 6. Runtime Integration

### 6.1 Skill Loader

Skill Loader is responsible for loading SKILL.md and querying SQLite memory layer experience:

```rust
struct SkillLoader {
    memory_store: Arc<SqliteStore>,
}

struct LoadedSkill {
    name: String,
    definition: SkillDefinition,      // From SKILL.md
    experience: Option<SkillExperience>, // From SQLite memory layer (may be empty)
    model_adaptations: Vec<String>,   // Current model's adaptation instructions
}

impl SkillLoader {
    /// Load published Skill
    fn load_published(&self, skill_name: &str, current_model: &ModelKey)
        -> Result<LoadedSkill>
    {
        // 1. Read skills/<skill_name>/SKILL.md
        let definition = self.parse_skill_md(skill_name)?;

        // 2. Query SQLite's SkillExperience
        let experience = self.memory_store.get_skill_experience(skill_name)?;

        // 3. Extract current model's adaptation instructions
        let model_adaptations = experience
            .as_ref()
            .and_then(|exp| exp.model_compatibility.get(current_model))
            .map(|mc| mc.adaptations.iter().map(|a| a.adaptation.clone()).collect())
            .unwrap_or_default();

        Ok(LoadedSkill { name: skill_name.to_string(), definition, experience, model_adaptations })
    }

    /// Load draft Skill (debug mode)
    fn load_draft(&self, draft_id: &str) -> Result<SkillDraft> { ... }
}
```

### 6.2 Skill Injection in Prompt Builder

Prompt Builder merges static definition and dynamic experience when injecting Skills into System Prompt:

```
System Prompt assembly (Step 5: Skill Instructions)
       │
       ▼
For each loaded Skill:
       │
       ├─ Base instructions
       │   From SKILL.md's Markdown body
       │
       ├─ Experience supplement (if SkillExperience)
       │   ## Learned from experience
       │   - {learned_pattern_1}
       │   - {learned_pattern_2}
       │
       ├─ User preferences (if any)
       │   ## User Preferences
       │   - Output style: concise
       │   - Skip clothing suggestions
       │
       └─ Model adaptation (if model_adaptations)
       ## Current Model Adaptation
       - Use simple direct instruction format, avoid nested lists
```

### 6.3 Context Trimming Priority

When Token budget is insufficient, Skill content trimming order (see `03-agent-runtime.md`):

| Priority | Content | Trimming Order |
|----------|---------|----------------|
| Highest | SKILL.md base instructions | Trim last |
| Medium | Model adaptation instructions | Trim first (loss of quality but not fatal) |
| Medium | Experience supplement (learned_patterns) | Trim second |
| Low | User preferences | Trim first |
| Lowest | Failure lessons (failure_cases) | Trim earliest |

## 7. Design Decision Records

| Decision | Choice | Reason |
|----------|--------|--------|
| Two-layer model | SKILL.md (static) + SQLite memory (dynamic) | SKILL.md ensures distributability, auditability, version control; SQLite memory layer supports self-learning and experience accumulation |
| Debug in SQLite memory layer | Don't directly modify SKILL.md | Debugging is an exploratory process needing iteration history, rollback, A/B testing; graph database naturally supports this |
| One-way commit | SQLite memory → SKILL.md | Like git workflow: iterate in workspace → commit to repo |
| Model compatibility recording | SkillExecution + SkillExperience | Skill effectiveness is strongly related to LLM; different models need different adaptations; must record |
| SKILL.md format | YAML frontmatter + Markdown | Compatible with Agent Skills open standard (agentskills.io), the de facto standard across six major platforms |
| Experience injection not replacement | Merge at runtime, don't modify SKILL.md | Ensure SKILL.md as stable baseline; experience as dynamic enhancement layer overlay |
| Context trimming | Experience layer trimmed first | Base instructions are Skill's core logic; experience is icing on the cake |
| Drafts not in package | SkillDraft only in SQLite memory layer | Unpublished drafts should not be distributed as part of package |

## 8. Future Extensions

### 8.1 Skill Cascading Degradation (Phase 4+)

When a Skill's depended tools are unavailable on the current platform (e.g. mobile `shell` unavailable), Skill should automatically degrade instead of entirely failing.

**Vision:**

- SKILL.md's `tool_deps` mark each dependency as `required` or `optional` (default optional)
- When `required` tool is unavailable, the entire Skill is skipped and not injected into System Prompt
- When `optional` tool is unavailable, the Skill is still injected, but instructions note "this step is unavailable on current platform, please skip"
- When Runtime assembles Skill context, it compares `tool_deps` with current platform's available tool list and generates degraded version instructions

**Design Issues to Resolve:**

- Is "skip a step" clear enough for LLM? Should alternative steps be generated?
- Does degraded Skill execution need to be recorded in SkillExperience? How to distinguish full execution from degraded execution?
- Does SKILL.md need to explicitly declare degradation strategy (like `on_tool_unavailable: skip_step | skip_skill | fallback`)?

These questions need to be answered after the Skill system is stably running, accumulating answers from real scenarios, before detailed design can be done.