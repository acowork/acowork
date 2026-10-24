---
name: architecture-strategy
description: Create architecture strategies with requirement analysis, alternatives, trade-offs, decisions, migration path, and rollback plan
version: "1.0.0"
author: developer
triggers:
  - architecture strategy
  - architecture design
  - system design
  - technical strategy
  - 架构策略
  - 架构设计
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Architecture Strategy Skill

## Execution Steps

1. **Explore context**
   - Use `mcp_memory__memory_recall` to retrieve prior architecture decisions and project constraints
   - Use `file_read` to inspect existing structure, docs, interfaces, and conventions
   - Identify current system shape, integration points, operational constraints, and known pain points
   - Separate facts from assumptions

2. **Clarify requirements**
   - List functional requirements
   - List non-functional requirements: performance, scale, latency, reliability, security, compliance, operability, cost
   - List constraints: existing stack, team familiarity, timeline, migration limits, compatibility requirements
   - List open questions that materially affect the design

3. **Generate alternatives**
   - Propose 2-3 viable approaches
   - Compare each approach by complexity, risk, time-to-deliver, scalability, security, operability, and reversibility
   - Lead with the recommended option and explain why

4. **Define target architecture**
   - Define subsystems, responsibilities, dependencies, and ownership
   - Define public contracts: APIs, events, schemas, traits, protocols, and error semantics
   - Define data flow, control flow, state ownership, and consistency model
   - Define trust boundaries, permission model, and sensitive data handling

5. **Plan delivery**
   - Break delivery into small, independently testable phases
   - Define validation for each phase: tests, metrics, observability, rollout checks
   - Define migration and compatibility strategy
   - Define rollback path and blast radius for risky changes

6. **Persist decisions**
   - Use `file_write` to save the architecture strategy when requested
   - Use `mcp_memory__memory_store` to persist decisions, rejected alternatives, assumptions, and follow-up questions

## Output Format

```markdown
# Architecture Strategy: [System Name]

## 1. Executive Summary
[Recommendation and why]

## 2. Context
### Facts
- [Verified fact]
### Assumptions
- [Assumption requiring validation]
### Constraints
- [Constraint]

## 3. Requirements
### Functional Requirements
- FR-1: [description]
### Non-Functional Requirements
- NFR-1: [description]

## 4. Alternatives
| Option | Summary | Pros | Cons | Risk | Reversibility |
|--------|---------|------|------|------|---------------|
| A | ... | ... | ... | ... | ... |

## 5. Recommended Architecture
[Architecture diagram or description]

### Subsystem: [Name]
- **Responsibility**: [what it owns]
- **Dependencies**: [what it depends on]
- **Public Contracts**: [APIs/events/schemas/traits]
- **Failure Modes**: [timeouts, retries, partial failure]

## 6. Data Flow
[How data moves through the system]

## 7. Security and Trust Boundaries
[Permissions, secrets, isolation, data exposure]

## 8. Delivery Plan
| Phase | Goal | Validation | Rollback |
|-------|------|------------|----------|
| 1 | ... | ... | ... |

## 9. Architecture Decisions
| Decision | Choice | Rationale | Rejected Alternatives | Trade-offs |
|----------|--------|-----------|-----------------------|------------|
| ... | ... | ... | ... | ... |

## 10. Open Questions
- [Question]
```

## Notes

- Architecture is trade-off management; always document what is chosen and what is rejected
- Prefer narrow, explicit contracts over broad generic abstractions
- If a design requires many coordinated changes before anything works, decompose it further
- Avoid speculative extension points until a concrete current use case exists
