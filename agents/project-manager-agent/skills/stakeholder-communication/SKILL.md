---
name: stakeholder-communication
description: Prepare targeted stakeholder communications with audience analysis, decision framing, trade-offs, deadlines, and follow-up accountability
version: "1.1.0"
author: developer
triggers:
  - communicate
  - 汇报
  - stakeholder update
  - executive summary
  - stakeholder communication
tool_deps:
  - doc_read
  - doc_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
  - pm_create_task
  - pm_get_task
  - pm_list_tasks
  - pm_add_activity
---

# Stakeholder Communication Skill

## Core Rule

Communication must be audience-specific and action-oriented. If the audience needs to decide, lead with the decision and deadline.

Do not hide bad news. State the situation, impact, options, recommendation, and next action.

## System of Record

Report from the service, and record what you reported.

- Pull every number from `pm_list_tasks` / `pm_get_task` before it goes into a report.
  Quote counts and dates with the filter that produced them.
- Escalations are tasks, not paragraphs: `pm_create_task` with `type="checkpoint"`
  and the decision needed in the title, so a human can approve or reject it.
- Publish the report with `doc_write` (this is what "shared" means); keep the
  recipient list in the document, not in your memory.
- `pm_add_activity` on the tracked item when a report changes a commitment.

## Execution Steps

1. **Identify audience and purpose**
   - Use `mcp_memory__memory_recall` to retrieve stakeholder profiles, communication preferences, prior feedback, and decision patterns
   - Use `doc_read` to inspect relevant PRD, plan, status report, risk register, or meeting notes
   - Identify audience type: executive, product, engineering, team, customer, partner, compliance, operations
   - Define communication purpose: inform, decide, escalate, align, unblock, or confirm
   - Identify the decision-maker and affected stakeholders

2. **Determine the message frame**
   - Lead with the recommendation or status
   - Separate facts, estimates, assumptions, and opinions
   - Quantify impact whenever possible
   - State implications with "so what"
   - Anticipate objections and prepare evidence-based answers

3. **Present options when decisions are needed**
   - Provide 2-3 realistic options
   - Include trade-offs: scope, timeline, cost, quality, risk, customer impact, reversibility
   - Recommend one option and explain why
   - Include deadline and consequence of no decision

4. **Choose format and detail level**
   - Executive: concise status, impact, decisions, risks
   - Product: scope, priority, customer value, timeline
   - Engineering: dependencies, blockers, technical constraints, ownership
   - Team: actions, owners, deadlines, context
   - External partner/customer: commitments, dates, dependencies, support needs

5. **Create artifact and follow up**
   - Use `doc_write` to create the communication artifact when requested
   - Use `mcp_memory__memory_store` to record what was communicated, to whom, decisions, commitments, feedback, and follow-up date
   - Track pending responses and escalate missed decision deadlines

## Output Format

```markdown
# [Project Name] — Stakeholder Communication

## Audience
- **Primary**: [Audience]
- **Decision-maker**: [Role/name]
- **Purpose**: [Inform / Decide / Escalate / Align / Unblock]

## Executive Summary
[3 sentences max: status, impact, recommended action]

## Key Messages
1. [Most important message]
2. [Second message]
3. [Third message]

## Decision Needed
| Decision | Options | Recommendation | Deadline | Consequence of No Decision |
|----------|---------|----------------|----------|----------------------------|
| ... | A/B/C | ... | ... | ... |

## Impact
- **Scope**: [impact]
- **Timeline**: [impact]
- **Cost/Capacity**: [impact]
- **Risk**: [impact]

## Next Actions
| Action | Owner | Due Date |
|--------|-------|----------|
| ... | ... | ... |
```

## Red Flags

Stop and revise if you see:

- Same message sent to every audience
- Bad news softened so much that action is unclear
- Decision request without deadline
- Options without recommendation
- Recommendation without evidence
- No owner for follow-up actions
- Technical detail overwhelming non-technical stakeholders
