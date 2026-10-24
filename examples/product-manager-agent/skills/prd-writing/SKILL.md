---
name: prd-writing
description: Write product requirements with problem framing, goals, non-goals, user stories, Figma/design references, acceptance criteria, metrics, and launch considerations
version: "1.0.0"
author: developer
triggers:
  - PRD
  - product requirements
  - write requirements
  - 产品需求
  - 需求文档
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# PRD Writing Skill

## Core Rule

A PRD is a decision document. It must clarify the problem, scope, success metrics, acceptance criteria, design references, and open decisions.

Do not write vague requirements that cannot be accepted or rejected.

## Execution Steps

1. **Load context**
   - Use `mcp_memory__memory_recall` to retrieve product decisions, research insights, roadmap constraints, Figma review findings, and stakeholder preferences
   - Use `file_read` to inspect strategy, research, roadmap, existing PRDs, Figma exports/specs, support feedback, and technical constraints
   - Separate facts, assumptions, decisions, and open questions

2. **Clarify problem and goals**
   - Define user problem, target segment, business goal, success metric, constraints, and urgency
   - Ask one focused question when ambiguity affects scope or priority
   - Define non-goals explicitly

3. **Define requirements**
   - Write user stories and functional requirements with stable IDs
   - Add acceptance criteria for success, failure, permissions, empty/loading/error states, and edge cases
   - Add non-functional requirements when relevant: performance, accessibility, security, compatibility, reliability, analytics
   - Link design references, especially Figma frames or prototype links when available

4. **Define analytics and launch readiness**
   - Define events, funnels, guardrail metrics, and post-launch evaluation
   - Identify rollout, support, documentation, and risk considerations
   - Capture dependencies and open decisions

5. **Self-review and persist**
   - Check for placeholders, contradictions, vague words, missing acceptance criteria, missing metrics, and missing non-goals
   - Use `file_write` to save the PRD when requested
   - Use `mcp_memory__memory_store` to persist PRD location, decisions, requirements, and open questions

## Output Format

```markdown
# PRD: [Feature Name]

## 1. Summary
[What we are building, for whom, and why]

## 2. Problem and Context
### User Problem
[Problem]
### Evidence
- [Research/data/support evidence]
### Assumptions
- [Assumption]

## 3. Goals and Non-Goals
### Goals
- [Goal]
### Non-Goals
- [Non-goal]

## 4. Users and Use Cases
| User Segment | Use Case | Success Outcome |
|--------------|----------|-----------------|
| ... | ... | ... |

## 5. Requirements
| ID | Requirement | Priority | Acceptance Criteria | Design Reference |
|----|-------------|----------|---------------------|------------------|
| REQ-001 | [Requirement] | Must | [Testable criteria] | [Figma frame/link] |

## 6. States and Edge Cases
- Empty: [behavior]
- Loading: [behavior]
- Error: [behavior]
- Permission denied: [behavior]
- Mobile/responsive: [behavior]

## 7. Metrics and Analytics
| Metric/Event | Purpose | Target/Definition |
|--------------|---------|-------------------|
| ... | ... | ... |

## 8. Risks, Dependencies, and Open Questions
| Item | Type | Owner | Needed By |
|------|------|-------|-----------|
| ... | ... | ... | ... |

## 9. Launch Notes
- Rollout: [plan]
- Support/docs: [needs]
- Experimentation: [if any]
```

## Red Flags

Stop and clarify if you see:

- Requirement without acceptance criteria
- No problem statement or evidence
- No non-goals
- Figma/design referenced but missing states
- Metrics missing for important feature
- Open decision hidden as requirement
