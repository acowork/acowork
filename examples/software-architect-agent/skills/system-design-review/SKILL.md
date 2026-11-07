---
name: system-design-review
description: Review a system design for boundaries, coupling, scalability, reliability, security, operability, and reversibility
version: "1.0.0"
author: developer
triggers:
  - design review
  - architecture review
  - system review
  - review architecture
  - 架构评审
  - 设计评审
tool_deps:
  - file_read
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# System Design Review Skill

## Execution Steps

1. **Load context**
   - Use `mcp_memory__memory_recall` to retrieve prior decisions, conventions, and known constraints
   - Use `file_read` to inspect the proposed design, relevant source files, and existing architecture docs
   - Identify stated goals, non-goals, constraints, and affected subsystems

2. **Check requirements coverage**
   - Verify each stated requirement maps to a design element
   - Identify unstated non-functional requirements that may matter
   - Flag assumptions and missing decisions

3. **Review boundaries and coupling**
   - Check that each component has one clear responsibility
   - Check that dependencies point toward contracts rather than concrete internals
   - Identify cross-subsystem coupling, circular dependencies, and hidden shared state
   - Prefer extension through existing interfaces, traits, schemas, or protocols before broad rewrites

4. **Review contracts**
   - Verify APIs, events, schemas, traits, and error semantics are explicit
   - Check backward compatibility and migration needs for public contracts
   - Flag vague contracts such as "manager", "handler", or "utils" when responsibilities are unclear

5. **Review quality attributes**
   - Scalability: bottlenecks, hot paths, state growth, concurrency limits
   - Reliability: timeouts, retries, idempotency, partial failure, recovery
   - Security: trust boundaries, least privilege, secret handling, data exposure
   - Operability: logging, metrics, tracing, deployment, rollback, debugging
   - Cost: runtime cost, storage growth, operational overhead, dependency burden

6. **Classify findings**
   - Mark each finding as Blocker, Major, Minor, or Question
   - Include concrete evidence and affected files or design sections
   - Provide actionable alternatives, not just criticism

7. **Persist review insights**
   - Use `mcp_memory__memory_store` for recurring architectural risks, project conventions, and important decisions

## Output Format

```markdown
# System Design Review: [Design Name]

## Verdict
[Approve / Approve with changes / Needs redesign]

## Summary
[Short review summary]

## Findings

### Blocker
- **[Title]**
  - Evidence: [file/section/behavior]
  - Impact: [why this matters]
  - Recommendation: [specific change]

### Major
- **[Title]**
  - Evidence: [file/section/behavior]
  - Impact: [why this matters]
  - Recommendation: [specific change]

### Minor
- **[Title]**
  - Recommendation: [specific change]

### Questions
- [Question that must be answered]

## Boundary Review
| Component | Responsibility | Dependencies | Concerns |
|-----------|----------------|--------------|----------|
| ... | ... | ... | ... |

## Risk Matrix
| Risk | Probability | Impact | Mitigation | Rollback |
|------|-------------|--------|------------|----------|
| ... | ... | ... | ... | ... |

## Recommended Next Steps
1. [Action]
```

## Notes

- Do not approve a design that lacks failure-mode handling for critical paths
- Do not require broad refactors when a smaller contract-preserving change meets the goal
- If multiple attempted fixes expose new coupling in different places, recommend architectural redesign rather than another patch
