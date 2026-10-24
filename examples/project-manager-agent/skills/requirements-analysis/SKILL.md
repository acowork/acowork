---
name: requirements-analysis
description: Collect, clarify, prioritize, validate, and control requirements into an approval-ready PRD with testable acceptance criteria and change gates
version: "1.1.0"
author: developer
triggers:
  - analyze requirements
  - 需求分析
  - PRD
  - product requirements
  - gather requirements
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Requirements Analysis Skill

## Core Rule

No plan without validated requirements. A requirement is not ready until it has an owner, priority, acceptance criteria, dependencies, and explicit non-goals.

If a requirement is ambiguous, ask one focused question before turning it into scope.

## Execution Steps

1. **Collect context and sources**
   - Use `mcp_memory__memory_recall` to retrieve project context, stakeholder preferences, prior requirements, decisions, and constraints
   - Use `file_read` to inspect docs, meeting notes, backlog items, roadmap, support issues, and customer feedback
   - Identify sources: users, business stakeholders, engineering, operations, compliance, support, market, and dependencies
   - Separate facts, assumptions, opinions, and open questions

2. **Clarify intent one question at a time**
   - Identify the problem, target users, success criteria, constraints, and urgency
   - Ask one focused question when missing information changes scope, priority, or feasibility
   - Prefer multiple-choice questions when they reduce ambiguity
   - Do not hide unresolved ambiguity inside the PRD

3. **Define scope boundaries**
   - Write goals and non-goals explicitly
   - Separate stated requirements from inferred requirements
   - Identify what is in scope for this release and what is deferred
   - Flag scope spanning independent subsystems and recommend decomposition when needed

4. **Make requirements testable**
   - Assign stable IDs: REQ-001, NFR-001, etc.
   - Define acceptance criteria that can be tested or demonstrated
   - Include negative and edge-case acceptance criteria for high-risk behavior
   - Define metrics for non-functional requirements: latency, reliability, scale, security, cost, accessibility, operability

5. **Prioritize and assess feasibility**
   - Apply MoSCoW prioritization: Must, Should, Could, Won't
   - Validate each Must Have against launch success criteria
   - Identify dependencies, conflicts, uncertainty, external blockers, and approval gates
   - For high-uncertainty items, recommend spike, prototype, or stakeholder decision before commitment

6. **Create the PRD and review gate**
   - Use `file_write` to save the PRD when requested
   - Include owners, decision-makers, sign-off status, and unresolved questions
   - Present the PRD for stakeholder review before task decomposition or sprint commitment
   - Use `mcp_memory__memory_store` to persist requirement decisions, changes, rationale, and PRD location

7. **Self-review before handoff**
   - Check that every Must Have has acceptance criteria
   - Check that non-goals prevent scope creep
   - Check for placeholders, contradictions, duplicate requirements, vague words, and missing owners
   - Check that open questions are explicit and not hidden as assumptions

## Output Format

```markdown
# PRD: [Product/Feature Name]

## 1. Executive Summary
[Problem, target users, recommended scope, and launch definition]

## 2. Context
### Facts
- [Verified fact]
### Assumptions
- [Assumption requiring validation]
### Constraints
- [Constraint]

## 3. Goals and Non-Goals
### Goals
- [Goal]
### Non-Goals
- [Explicitly out of scope]

## 4. Requirements
### Functional Requirements
| ID | Requirement | Priority | Owner | Acceptance Criteria | Dependencies | Status |
|----|-------------|----------|-------|---------------------|--------------|--------|
| REQ-001 | [Description] | Must | [Role] | [Testable criteria] | — | Proposed |

### Non-Functional Requirements
| ID | Requirement | Metric | Target | Validation |
|----|-------------|--------|--------|------------|
| NFR-001 | Performance | P95 latency | < 200ms | Load test |

## 5. User Stories
- As a [role], I want [action], so that [benefit]

## 6. Dependencies and Conflicts
| Item | Type | Impact | Resolution |
|------|------|--------|------------|
| ... | ... | ... | ... |

## 7. Risks and Open Questions
### Risks
- [Risk and mitigation]
### Open Questions
- [ ] [Question] — Owner: [role] — Needed by: [date]

## 8. Approval Gate
| Role | Name/Owner | Decision | Date | Conditions |
|------|------------|----------|------|------------|
| PM | | Pending | | |
| Tech Lead | | Pending | | |

## 9. Change Log
| Date | Change | Reason | Approved By |
|------|--------|--------|-------------|
| ... | ... | ... | ... |
```

## Red Flags

Stop and clarify if you see:

- Must Have without acceptance criteria
- Requirements that describe implementation before user value
- "ASAP", "easy", "simple", "fast", or "robust" without measurable definition
- No explicit non-goals
- Conflicting stakeholder priorities with no decision owner
- Scope spanning multiple independent systems without decomposition
- PRD treated as final while open questions still affect scope or estimates
