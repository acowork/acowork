---
name: sprint-planning
description: Plan sprint scope using readiness gates, team capacity, historical velocity, explicit sprint goals, risk buffers, and commitment checks
version: "1.1.0"
author: developer
triggers:
  - sprint
  - 迭代规划
  - sprint planning
  - plan sprint
  - iteration planning
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Sprint Planning Skill

## Core Rule

Do not commit unready work. Sprint commitment requires clear acceptance criteria, dependencies understood, capacity available, and a measurable sprint goal.

A sprint goal is the commitment; individual stories are the forecast.

## Execution Steps

1. **Review previous sprint evidence**
   - Use `mcp_memory__memory_recall` to retrieve velocity, completion rate, carry-over, interruptions, and retrospective actions
   - Review planned vs completed work and why items slipped
   - Carry over unfinished work only after re-estimating remaining effort
   - Apply retrospective actions as explicit sprint planning constraints

2. **Inspect and refine backlog**
   - Use `file_read` to examine backlog, PRD, task breakdown, risk register, and dependencies
   - Check readiness for each candidate item:
     - Clear acceptance criteria
     - Estimate available
     - Dependencies known
     - Owner role clear
     - Test/review/demo path defined
     - No blocking open questions
   - Move unready items to refinement instead of sprint commitment

3. **Calculate realistic capacity**
   - Calculate person-days or story points from team availability
   - Subtract holidays, PTO, on-call, meetings, support load, production work, and known interruptions
   - Apply utilization factor, typically 0.7-0.8
   - Reserve 10-20% buffer for unplanned work, depending on risk and incident load
   - Use the lower of calculated capacity and historical reliable velocity when uncertain

4. **Select sprint scope**
   - Define one measurable sprint goal first
   - Select the smallest set of items needed to achieve the goal
   - Prefer finishing over starting
   - Check individual load, critical path, dependencies, and review bandwidth
   - Include testing, documentation, release, stakeholder review, and carry-over work explicitly

5. **Run commitment gate**
   - Confirm total committed effort fits capacity after buffer
   - Confirm Must items align with sprint goal
   - Confirm no committed item has unresolved external dependency without mitigation
   - Confirm stakeholders understand trade-offs and non-committed items
   - If capacity is insufficient, present options: reduce scope, extend timeline, add capacity, or accept risk

6. **Save and persist**
   - Use `file_write` to save the sprint plan when requested
   - Use `mcp_memory__memory_store` to persist sprint goal, commitment, capacity assumptions, selected items, risks, and retrospective actions

## Output Format

````markdown
# Sprint Plan: Sprint [N]

## Sprint Goal
[One measurable sentence]

## Planning Inputs
- **Source backlog**: [path/reference]
- **Previous velocity**: [last 3 sprint average]
- **Carry-over**: [items and reason]
- **Retrospective actions**: [actions applied]

## Capacity
| Category | Amount | Notes |
|----------|--------|-------|
| Gross capacity | [X] | [team × days] |
| Planned absence/meetings/support | -[Y] | [details] |
| Risk buffer | -[Z] | [percentage] |
| Net commitment capacity | [N] | [points/person-days] |

## Committed Items
| ID | Item | Owner Role | Estimate | Priority | DoD | Dependency | Risk |
|----|------|------------|----------|----------|-----|------------|------|
| PB-001 | [Description] | [Role] | 5 pts | Must | [Done criteria] | — | Low |

**Total committed**: [X] / [capacity]

## Not Committed
| Item | Reason | Next Action |
|------|--------|-------------|
| ... | Not ready / capacity / dependency | ... |

## Risks and Dependencies
| Risk/Dependency | Impact | Mitigation | Owner |
|-----------------|--------|------------|-------|
| ... | ... | ... | ... |

## Commitment Gate
- [x] Sprint goal measurable
- [x] All committed items ready
- [x] Capacity includes buffer
- [x] Carry-over re-estimated
- [x] Dependencies and risks visible

## Review Cadence
- Daily progress check: [time]
- Mid-sprint scope review: [date]
- Demo/review: [date]
````

## Red Flags

Stop and re-plan if you see:

- More committed work than capacity after buffer
- Items without acceptance criteria
- Sprint goal that is just a list of tasks
- Carry-over accepted without root-cause analysis
- External dependency with no mitigation
- Everyone at 100% utilization
- No testing, review, or release capacity
