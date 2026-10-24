---
name: product-strategy
description: Define product vision, target users, positioning, strategic bets, success metrics, risks, and decision principles
version: "1.0.0"
author: developer
triggers:
  - product strategy
  - product vision
  - 产品策略
  - 产品定位
  - strategy
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Product Strategy Skill

## Core Rule

Product strategy must connect user value, business outcome, differentiation, and measurable success. Do not define strategy as a feature list.

## Execution Steps

1. **Load context**
   - Use `mcp_memory__memory_recall` to retrieve prior product decisions, user segments, metrics, roadmap constraints, and stakeholder preferences
   - Use `file_read` to inspect product docs, research, analytics exports, roadmap, PRDs, support feedback, and competitive notes
   - Separate facts, assumptions, open questions, and decisions already made

2. **Clarify strategic frame**
   - Define target users, core problem, desired outcome, market/context, constraints, and time horizon
   - Ask one focused question at a time when missing information changes strategy
   - Identify non-goals and strategic trade-offs

3. **Generate strategic options**
   - Present 2-3 strategic paths
   - Compare user impact, business impact, effort, risk, confidence, differentiation, and reversibility
   - Lead with a recommendation and rationale

4. **Define strategy and metrics**
   - Define product vision, positioning, target segment, value proposition, strategic bets, and success metrics
   - Define leading and lagging indicators
   - Define risks and assumptions to validate

5. **Persist and hand off**
   - Use `file_write` to save strategy when requested
   - Use `mcp_memory__memory_store` to persist strategic choices, rejected alternatives, metrics, and assumptions

## Output Format

```markdown
# Product Strategy: [Product/Area]

## Executive Summary
[Recommended strategy and why]

## Target Users
| Segment | Problem | Current Workaround | Value Proposition |
|---------|---------|--------------------|-------------------|
| ... | ... | ... | ... |

## Strategic Options
| Option | Summary | User Impact | Business Impact | Effort | Risk | Confidence |
|--------|---------|-------------|-----------------|--------|------|------------|
| A | ... | ... | ... | ... | ... | ... |

## Recommended Strategy
- **Positioning**: [positioning]
- **Strategic Bets**: [bets]
- **Non-Goals**: [non-goals]

## Success Metrics
| Metric | Type | Baseline | Target | Instrumentation |
|--------|------|----------|--------|-----------------|
| ... | Leading/Lagging | ... | ... | ... |

## Risks and Assumptions
| Assumption/Risk | Validation Plan | Owner | Due Date |
|-----------------|-----------------|-------|----------|
| ... | ... | ... | ... |
```

## Red Flags

Stop and clarify if you see:

- Strategy described only as features
- No target user segment
- No measurable success metric
- No differentiation or trade-off
- No validation plan for major assumptions
