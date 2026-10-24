---
name: user-research
description: Plan and synthesize user research, interviews, surveys, personas, journeys, pain points, opportunities, and product recommendations
version: "1.0.0"
author: developer
triggers:
  - user research
  - customer interview
  - 用户调研
  - persona
  - journey map
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# User Research Skill

## Core Rule

Research output must distinguish evidence from interpretation. Do not treat one anecdote as a validated pattern.

## Execution Steps

1. **Define research objective**
   - Use `mcp_memory__memory_recall` to retrieve prior research, personas, segments, product decisions, and open assumptions
   - Use `file_read` to inspect PRDs, analytics, support tickets, feedback, interview notes, and roadmap questions
   - Define research question, target segment, decision to inform, and confidence needed

2. **Plan research method**
   - Choose interviews, survey, usability test, prototype review, analytics review, support-ticket synthesis, or competitive study
   - Define participant criteria, sample size, script, tasks, and ethical/privacy constraints
   - Identify bias risks and limitations

3. **Synthesize evidence**
   - Group observations into themes
   - Separate verbatim evidence, behavioral observations, interpretation, and recommendations
   - Identify frequency and confidence level
   - Capture contradictions and outliers

4. **Translate to product opportunities**
   - Define user problems, jobs-to-be-done, journey pain points, and opportunity areas
   - Recommend product actions with impact, confidence, and effort
   - Define follow-up validation where confidence is low

5. **Persist learnings**
   - Use `file_write` to save research summary when requested
   - Use `mcp_memory__memory_store` to persist insights, personas, journey pain points, and product recommendations

## Output Format

```markdown
# User Research Summary: [Topic]

## Research Objective
[Decision this research informs]

## Method
- **Method**: [interviews/survey/usability/etc.]
- **Participants/Data**: [sample]
- **Limitations**: [bias/coverage limits]

## Key Findings
| Finding | Evidence | Confidence | Product Implication |
|---------|----------|------------|---------------------|
| ... | Quote/data | High/Med/Low | ... |

## User Journey / Pain Points
| Stage | User Goal | Pain Point | Opportunity |
|-------|-----------|------------|-------------|
| ... | ... | ... | ... |

## Recommendations
| Recommendation | Impact | Effort | Confidence | Next Step |
|----------------|--------|--------|------------|-----------|
| ... | ... | ... | ... | ... |

## Open Questions
- [Question]
```

## Red Flags

Stop and revise if you see:

- Recommendation unsupported by evidence
- Quotes without participant context
- No limitations section
- Research not tied to a product decision
- One user story generalized to all users
