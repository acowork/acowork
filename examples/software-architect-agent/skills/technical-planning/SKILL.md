---
name: technical-planning
description: Convert an approved architecture or spec into a concrete implementation plan with files, boundaries, tests, rollout, and rollback steps
version: "1.0.0"
author: developer
triggers:
  - implementation plan
  - technical plan
  - execution plan
  - plan implementation
  - 技术计划
  - 实施计划
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Technical Planning Skill

## Execution Steps

1. **Read the approved design**
   - Use `mcp_memory__memory_recall` to retrieve relevant decisions and conventions
   - Use `file_read` to inspect the design/spec, existing file structure, tests, and implementation patterns
   - Confirm scope, non-goals, constraints, and acceptance criteria

2. **Map file responsibilities**
   - List files to create, modify, or test
   - Assign one clear responsibility to each file or module
   - Keep files focused and colocate code that changes together
   - Follow existing codebase patterns unless a targeted improvement is required by the design

3. **Define implementation slices**
   - Break work into small tasks that each produce a working, testable increment
   - For each task, specify exact files, expected interfaces, and validation commands
   - Avoid mixed feature/refactor/infrastructure tasks unless technically inseparable

4. **Plan tests first**
   - For behavior changes, specify failing tests before implementation
   - Prefer real behavior tests over mocks unless isolation requires a mock
   - Include edge cases, failure modes, compatibility behavior, and regression tests

5. **Plan rollout and rollback**
   - Define migration steps, feature exposure, compatibility windows, and deployment ordering
   - Define observability signals that confirm success or reveal failure
   - Define rollback steps for each risky phase

6. **Self-review**
   - Check that every requirement maps to at least one task
   - Remove placeholders such as TBD, TODO, or "handle edge cases"
   - Check type, interface, and file-name consistency across tasks
   - Use `file_write` to save the plan when requested
   - Use `mcp_memory__memory_store` to persist key decisions and implementation constraints

## Output Format

```markdown
# [Feature/System] Technical Implementation Plan

## Goal
[One sentence]

## Architecture Summary
[2-3 sentences explaining the approach]

## Scope
### In Scope
- [Item]
### Non-Goals
- [Item]

## File Responsibility Map
| File | Action | Responsibility |
|------|--------|----------------|
| `path/to/file` | Create/Modify/Test | ... |

## Tasks

### Task 1: [Name]
**Files:**
- Create: `path/to/file`
- Modify: `path/to/file`
- Test: `path/to/test`

**Steps:**
1. Write failing test: [specific behavior]
2. Run: `[exact command]`
3. Implement: [specific interface or behavior]
4. Run: `[exact command]`
5. Verify: [expected result]

## Rollout Plan
| Phase | Change | Success Signal | Rollback |
|-------|--------|----------------|----------|
| ... | ... | ... | ... |

## Validation Checklist
- [ ] Requirement coverage checked
- [ ] Boundary and dependency direction checked
- [ ] Tests defined before behavior implementation
- [ ] Migration and compatibility checked
- [ ] Rollback path defined
```

## Notes

- Plans must be concrete enough for another engineer to execute without architectural context
- Prefer small reversible changes over large coordinated rewrites
- If a task cannot be tested independently, split the design further
