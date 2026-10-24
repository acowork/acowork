---
name: risk-assessment
description: Identify, score, mitigate, monitor, and escalate project risks with owners, triggers, residual risk, contingency plans, and review cadence
version: "1.1.0"
author: developer
triggers:
  - risk
  - 风险评估
  - assess risk
  - risk analysis
  - risk register
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Risk Assessment Skill

## Core Rule

A risk is an uncertain future event. An issue is already happening. Do not mix them.

Every High or Medium risk needs an owner, trigger, mitigation, contingency, residual risk, and review cadence.

## Execution Steps

1. **Load context and history**
   - Use `mcp_memory__memory_recall` to retrieve historical risks, incidents, estimate misses, dependency failures, and mitigation patterns
   - Use `file_read` to inspect PRD, sprint plan, task breakdown, architecture/design docs, dependency lists, and status reports
   - Identify assumptions that, if false, would threaten scope, schedule, quality, cost, or trust

2. **Identify risks by category**
   - Technical: uncertainty, integration complexity, performance, security, data migration, reliability
   - Schedule: optimistic estimates, dependency delays, review bottlenecks, scope creep
   - Resource: key-person dependency, skill gaps, availability, onboarding, burnout
   - Product: ambiguous requirements, weak validation, stakeholder disagreement, adoption risk
   - External: vendor changes, compliance, market, customer commitments, platform changes
   - Operational: deployment, monitoring, support load, rollback, incident response

3. **Convert vague risks into actionable risks**
   - Write risks as: "If [event], then [impact], because [cause]"
   - Remove generic risks like "things may take longer" unless tied to a cause and trigger
   - Separate active issues into an issue log with owner and resolution plan

4. **Score and classify**
   - Rate Impact and Probability from 1-5
   - Calculate Score = Impact × Probability
   - Classify:
     - High: 15-25, active mitigation and weekly review
     - Medium: 6-14, mitigation or contingency and bi-weekly review
     - Low: 1-5, accept and monitor monthly
   - Estimate residual risk after mitigation

5. **Plan response and monitoring**
   - Choose response: Avoid, Mitigate, Transfer, Accept
   - Define preventive actions, contingency actions, triggers, escalation criteria, owner, and deadline
   - Assign a single risk owner responsible for monitoring and escalation
   - Define when the risk can be closed or downgraded

6. **Escalate decision risks**
   - If risk response requires scope, timeline, budget, staffing, or quality trade-off, prepare decision options
   - Present options with recommendation, impact, deadline, and consequence of no decision

7. **Save and persist**
   - Use `file_write` to save the risk register when requested
   - Use `mcp_memory__memory_store` to persist risk patterns, realized risks, mitigation outcomes, and estimation learnings

## Output Format

```markdown
# Risk Register: [Project Name]

## Risk Summary
- **Overall status**: [Low / Medium / High]
- **Top concern**: [Most important risk]
- **Decision needed**: [Yes/No and by when]

## Risk Matrix
| ID | Risk | Category | Impact | Probability | Score | Response | Owner | Trigger | Mitigation | Contingency | Residual Risk | Review Cadence | Status |
|----|------|----------|--------|-------------|-------|----------|-------|---------|------------|-------------|---------------|----------------|--------|
| RSK-001 | If [event], then [impact], because [cause] | Technical | 4 | 4 | 16 | Mitigate | [Role] | [Signal] | [Action] | [Fallback] | Medium | Weekly | Active |

## Top Risks
1. **RSK-001** — [why it matters, next action]
2. **RSK-002** — [why it matters, next action]
3. **RSK-003** — [why it matters, next action]

## Issues Separated from Risks
| Issue | Owner | Resolution Plan | Due Date |
|-------|-------|-----------------|----------|
| ... | ... | ... | ... |

## Escalations
| Decision | Options | Recommendation | Deadline | Consequence of No Decision |
|----------|---------|----------------|----------|----------------------------|
| ... | ... | ... | ... | ... |

## Review Plan
- High risks: weekly
- Medium risks: bi-weekly
- Low risks: monthly
```

## Red Flags

Stop and revise if you see:

- Risk without owner
- Risk without observable trigger
- High risk with no mitigation or contingency
- Risk described as a vague feeling instead of event and impact
- Active issue incorrectly listed as a risk
- Mitigation that has no deadline or responsible owner
- No escalation path for risks requiring trade-off decisions
