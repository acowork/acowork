---
name: architecture-design
description: Design system architecture through context discovery, requirements clarification, alternatives, approval gates, module decomposition, and implementation handoff
version: "1.1.0"
author: developer
triggers:
  - design
  - architecture
  - 系统设计
  - system design
  - 架构设计
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Architecture Design Skill

## Core Rule

Do not jump from an idea directly to implementation. First understand context, clarify requirements, present alternatives, and get the design direction approved.

Architecture is trade-off management. Always document the choice, rejected alternatives, rationale, risks, and rollback path.

## Execution Steps

1. **Explore project context**
   - Use `mcp_memory__memory_recall` to retrieve prior architectural decisions, constraints, conventions, and known risks
   - Use `file_read` to inspect existing docs, source structure, module boundaries, interfaces, tests, and deployment assumptions
   - Identify whether this is a new system, an extension, a migration, or a redesign
   - If the request spans multiple independent subsystems, propose decomposition before designing details

2. **Clarify requirements one question at a time**
   - Identify functional requirements, non-functional requirements, constraints, assumptions, and open questions
   - Ask focused questions for missing information that materially changes the architecture
   - Prefer multiple-choice questions when they help the user decide quickly
   - Do not overwhelm the user with a long questionnaire

3. **Present alternatives before choosing**
   - Propose 2-3 viable approaches
   - Compare trade-offs: simplicity, delivery speed, scalability, reliability, security, cost, operability, and reversibility
   - Lead with the recommended option and explain why
   - Ask for confirmation before proceeding when the choice changes scope or risk

4. **Design for boundaries and clarity**
   - Define bounded contexts, module responsibilities, ownership, and dependency direction
   - Each module should have one clear responsibility and a narrow public interface
   - Prefer contract-first dependencies: concrete modules depend on shared contracts, not each other’s internals
   - Avoid speculative extension points until there is a concrete current use case
   - Keep changes incremental and compatible with existing patterns

5. **Define interfaces and data flow**
   - Define public APIs, traits, commands, events, schemas, protocols, and error contracts
   - Specify synchronous vs asynchronous communication and request-response vs event-driven flows
   - Define state ownership, consistency model, persistence boundaries, and migration needs
   - Define configuration, initialization, permissions, and trust boundaries

6. **Plan validation, rollout, and rollback**
   - Define tests needed to validate the design
   - Define observability: logs, metrics, traces, health checks, and debugging signals
   - Define rollout phases and compatibility strategy
   - Define rollback path for risky changes and public contracts
   - If rollback is unclear, mark the design incomplete

7. **Write and self-review the design**
   - Use `file_write` to save the architecture document when requested
   - Use `mcp_memory__memory_store` to persist accepted decisions, rejected alternatives, risks, and assumptions
   - Self-review the design for placeholders, contradictions, ambiguous requirements, missing tests, and oversized scope
   - If the design has unresolved material questions, surface them before implementation planning

## Output Format

```markdown
# Architecture Design: [System Name]

## 1. Executive Summary
[Recommended approach and why]

## 2. Context
### Facts
- [Verified fact]
### Assumptions
- [Assumption]
### Constraints
- [Constraint]

## 3. Requirements
### Functional Requirements
- FR-1: [description]
### Non-Functional Requirements
- NFR-1: [description]
### Non-Goals
- [Explicitly out of scope]

## 4. Alternatives Considered
| Option | Summary | Pros | Cons | Risk | Reversibility |
|--------|---------|------|------|------|---------------|
| A | ... | ... | ... | ... | ... |

## 5. Recommended Architecture
[Mermaid diagram or concise description]

### Module: [Name]
- **Responsibility**: [what it owns]
- **Dependencies**: [what it depends on]
- **Public Interface**: [APIs/traits/events/schemas]
- **Failure Modes**: [timeouts, retries, partial failure]

## 6. Data Flow
[How data moves through the system]

## 7. Security and Operability
- Trust boundaries: [description]
- Permissions: [description]
- Observability: [description]
- Operational risks: [description]

## 8. Delivery Plan
| Phase | Goal | Validation | Rollback |
|-------|------|------------|----------|
| 1 | ... | ... | ... |

## 9. Key Decisions
| Decision | Choice | Rationale | Rejected Alternatives | Trade-offs |
|----------|--------|-----------|-----------------------|------------|
| ... | ... | ... | ... | ... |

## 10. Open Questions
- [Question]

## 11. Self-Review
- [x] Requirements covered
- [x] No placeholders
- [x] Interfaces explicit
- [x] Tests and rollout defined
- [x] Rollback path defined
```

## Red Flags

Stop and revise the design if you see:

- A large design with no incremental delivery path
- Components without clear ownership or public contracts
- Concrete modules depending on each other’s internals
- New abstractions with only one speculative caller
- Public API, schema, or config changes without compatibility and migration notes
- No test strategy for critical paths
- No rollback path for risky changes
- Ambiguous requirements hidden as implementation details
