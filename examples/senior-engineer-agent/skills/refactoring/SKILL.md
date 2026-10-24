---
name: refactoring
description: Identify code smells and apply structured, behavior-preserving refactoring with characterization tests, small reversible steps, and regression safety
version: "1.1.0"
author: developer
triggers:
  - refactor
  - 重构
  - clean code
  - restructure
  - improve code
tool_deps:
  - file_read
  - file_write
  - shell
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

# Refactoring Skill

## Core Rule

Refactoring must preserve observable behavior. If behavior changes, it is redesign or bug fixing, not refactoring.

Do not refactor without a safety net. Add characterization tests first when coverage is weak.

## Execution Steps

1. **Establish baseline**
   - Use `mcp_memory__memory_recall` to retrieve project conventions and prior refactoring decisions
   - Use `file_read` to inspect target code, adjacent modules, tests, and public interfaces
   - Use `shell` to run relevant tests before changing code
   - Record current behavior, public contracts, and baseline failures if any
   - If tests are missing or weak, add characterization tests before refactoring

2. **Identify smells and risks**
   - Catalog concrete smells with locations:
     - Long Method
     - Large Class/Module
     - Duplicated Code
     - Feature Envy
     - Shotgun Surgery
     - Magic Numbers/Strings
     - Dead Code
     - Premature Abstraction
     - Hidden Shared State
     - Cross-Module Coupling
   - Prioritize by bug risk, readability, change frequency, and blast radius
   - Identify public API, schema, config, or behavior risks

3. **Design a small-step plan**
   - Split work into reversible steps that can each be tested independently
   - Keep one refactoring technique per step
   - Prefer compiler-verified refactors: rename, extract function, move code behind existing contracts, remove dead code
   - Avoid broad rewrites, unrelated cleanup, and speculative abstractions
   - If the change spans unrelated concerns, split it into separate refactoring plans

4. **Apply one refactor at a time**
   - Use `file_write` for one step only
   - Run focused tests after the step
   - If tests fail, revert or make the step smaller
   - Do not fix bugs discovered during refactoring in the same change; record them separately unless they block the refactor

5. **Validate broadly**
   - Run relevant test suites, lint, typecheck, and build commands
   - Check for performance regressions if hot paths changed
   - Check public API compatibility if interfaces moved or renamed
   - Confirm no observable behavior changed except explicitly approved mechanical effects

6. **Document and store rationale**
   - Update affected documentation only when public structure or usage changed
   - Use `mcp_memory__memory_store` to record refactoring rationale, safe patterns, and avoided pitfalls

## Output Format

```markdown
## Refactoring Report

### Goal
[Why this refactor was needed]

### Baseline
- Tests before change: [command/result]
- Characterization tests added: [yes/no/path]

### Code Smells Identified
| # | Smell | Location | Severity | Planned Technique |
|---|-------|----------|----------|-------------------|
| 1 | Long Method | src/module.rs:42 | High | Extract Function |

### Steps Applied
| Step | Change | Files | Verification |
|------|--------|-------|--------------|
| 1 | Extracted `validate_input` | `src/module.rs` | `cargo test validate_input`: PASS |

### Compatibility
- Public API changed: Yes/No
- Behavior changed: No
- Migration needed: Yes/No

### Verification
- [x] Characterization tests pass
- [x] Existing tests pass
- [x] Lint/typecheck pass or skipped with reason
- [x] No unrelated cleanup included

### Follow-up
- [Bug/design issue found but not fixed here]
```

## Red Flags

Stop and revise the plan if you see:

- Refactoring and bug fixing mixed together
- Multiple unrelated refactors in one step
- No tests around behavior being moved
- Large files split without understanding existing conventions
- New abstractions introduced before repeated stable use
- Public contracts changed without migration notes
- Test failure after a refactor that is explained away instead of fixed or reverted
