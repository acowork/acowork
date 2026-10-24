---
name: progress-tracking
description: Track project progress against plan using evidence, variance thresholds, blocker ownership, recovery options, and stakeholder-ready status reports
version: "1.1.0"
author: developer
triggers:
  - track progress
  - 进度跟踪
  - status update
  - weekly report
  - project status
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Progress Tracking Skill

## Core Rule

Status must be evidence-based. Do not report green if scope, schedule, quality, or risk evidence says otherwise.

Bad news should be surfaced early with options and a recommended recovery path.

## Execution Steps

1. **Collect evidence from sources**
   - Use `mcp_memory__memory_recall` to retrieve plan, sprint commitment, previous status, velocity, risks, blockers, and decisions
   - Use `file_read` to inspect tracking files, task lists, changelogs, PRs, release notes, test results, or meeting notes
   - Collect completed, in-progress, blocked, at-risk, and newly discovered work
   - Separate verified facts from estimates and self-reported status

2. **Compare actuals to plan**
   - Compare planned vs actual delivery, quality, scope, and risk movement
   - Calculate variance where possible: tasks, points, dates, defects, incidents, or milestone progress
   - Identify trend: improving, stable, or degrading
   - Do not average away critical blockers or high-risk outliers

3. **Classify project status**
   - Green: on track, risks controlled, no decision needed
   - Yellow: at risk, mitigation active, decision may be needed soon
   - Red: off track, commitment likely missed, decision needed now
   - Define thresholds explicitly for the project context

4. **Analyze deviations and blockers**
   - Identify root causes: scope change, dependency delay, estimate miss, quality issue, resource gap, unclear requirement
   - Assign every blocker a single owner, next action, and due date
   - Escalate blockers when the owner cannot resolve them within the needed timeframe
   - Distinguish risks from active issues

5. **Prepare adjustment options**
   - For minor deviation, adjust within the sprint or plan
   - For moderate deviation, present scope, timeline, staffing, or quality trade-offs
   - For major deviation, escalate with options and recommendation
   - Always include impact of doing nothing

6. **Publish and persist**
   - Use `file_write` to save the status report when requested
   - Use `mcp_memory__memory_store` to persist status, metrics, decisions, velocity, estimation accuracy, and blockers

## Output Format

```markdown
# Project Status Report — [Date]

## Executive Summary
- **Status**: [Green / Yellow / Red]
- **Headline**: [One sentence]
- **Decision needed**: [Yes/No — what and by when]

## Progress Overview
| Metric | Plan | Actual | Variance | Trend |
|--------|------|--------|----------|-------|
| Sprint completion | 80% | 65% | -15% | Degrading |

## Completed This Period
- [x] [Verified deliverable]

## In Progress
| Item | Owner | Expected Completion | Confidence | Notes |
|------|-------|---------------------|------------|-------|
| ... | ... | ... | High/Med/Low | ... |

## Blocked / At Risk
| Item | Blocker/Risk | Owner | Next Action | Due Date | Escalation Needed |
|------|--------------|-------|-------------|----------|-------------------|
| ... | ... | ... | ... | ... | Yes/No |

## Variance Analysis
- **Schedule**: [status and cause]
- **Scope**: [status and cause]
- **Quality**: [status and cause]
- **Resources**: [status and cause]

## Recovery Options
| Option | Impact | Trade-off | Recommendation |
|--------|--------|-----------|----------------|
| A | ... | ... | Recommended |

## Next Period Focus
1. [Priority]
2. [Priority]
3. [Priority]
```

## Red Flags

Stop and correct the report if you see:

- Status color unsupported by evidence
- Blocker without owner and next action
- Bad news buried below low-priority details
- No recovery options for Yellow or Red status
- New scope included without change log
- Active issue reported only as a risk
- Metrics reported without explaining implications
