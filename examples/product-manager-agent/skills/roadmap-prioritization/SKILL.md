---
name: roadmap-prioritization
description: Prioritize roadmap items using evidence, impact, effort, confidence, risk, dependencies, strategic alignment, and opportunity cost
version: "1.0.0"
author: developer
triggers:
  - roadmap
  - prioritization
  - prioritize
  - 路线图
  - 优先级
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Roadmap Prioritization Skill

## Core Rule

Prioritization is a trade-off decision. Every item selected means something else is delayed or rejected.

Do not prioritize by stakeholder volume alone; use evidence, impact, effort, confidence, risk, and strategic fit.

## Execution Steps

1. **Load candidate context**
   - Use `mcp_memory__memory_recall` to retrieve strategy, roadmap decisions, metrics, user insights, stakeholder preferences, and prior trade-offs
   - Use `file_read` to inspect backlog, PRDs, research, analytics, support feedback, risk register, and dependency plans
   - Normalize candidate items so each has user, problem, expected outcome, and effort estimate

2. **Select prioritization model**
   - Use RICE, ICE, MoSCoW, Cost of Delay, Kano, or custom weighted scoring
   - Explain why the model fits the decision context
   - Define scoring scale before scoring

3. **Score and compare**
   - Score reach, impact, confidence, effort, strategic alignment, urgency, risk, dependency, and reversibility as applicable
   - Identify items with low confidence that require discovery before roadmap commitment
   - Identify dependencies and sequencing constraints

4. **Recommend roadmap**
   - Present recommended Now / Next / Later grouping
   - Explain trade-offs and opportunity cost
   - Surface items explicitly rejected or deferred
   - Define validation milestones and decision gates

5. **Persist decisions**
   - Use `file_write` to save roadmap recommendation when requested
   - Use `mcp_memory__memory_store` to persist prioritization rationale, decisions, rejected alternatives, and review date

## Output Format

```markdown
# Roadmap Prioritization: [Time Horizon]

## Prioritization Model
[Model and scoring rules]

## Candidate Scores
| Item | User/Problem | Reach | Impact | Confidence | Effort | Risk | Score | Notes |
|------|--------------|-------|--------|------------|--------|------|-------|-------|
| ... | ... | ... | ... | ... | ... | ... | ... | ... |

## Recommended Roadmap
### Now
- [Item and rationale]
### Next
- [Item and rationale]
### Later
- [Item and rationale]

## Deferred / Rejected
| Item | Reason | Revisit Trigger |
|------|--------|-----------------|
| ... | ... | ... |

## Dependencies and Decision Gates
| Item | Dependency/Gate | Owner | Date |
|------|-----------------|-------|------|
| ... | ... | ... | ... |
```

## Red Flags

Stop and revise if you see:

- No scoring criteria
- Roadmap driven only by loudest stakeholder
- High-effort item with low confidence committed without discovery
- No explicit deferred items
- Dependencies ignored in ordering
- No revisit trigger for deferred decisions
