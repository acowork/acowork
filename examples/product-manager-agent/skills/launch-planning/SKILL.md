---
name: launch-planning
description: Plan product launch with rollout strategy, positioning, communication, analytics, support readiness, risks, experiments, and post-launch learning
version: "1.0.0"
author: developer
triggers:
  - launch
  - go to market
  - release plan
  - 发布计划
  - 上线计划
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Launch Planning Skill

## Core Rule

A launch is not complete when code ships. It is complete when target users can discover, understand, use, and be supported on the feature, and success can be measured.

## Execution Steps

1. **Load launch context**
   - Use `mcp_memory__memory_recall` to retrieve product strategy, prior launches, stakeholder preferences, known risks, and metric baselines
   - Use `file_read` to inspect PRD, roadmap, acceptance report, release readiness, support docs, analytics plan, and communication drafts
   - Confirm target users, launch scope, release date, channels, and decision owner

2. **Define launch goals and audience**
   - Define launch objective, target segments, positioning, messaging, and expected user behavior
   - Define success metrics, guardrail metrics, and post-launch review date
   - Define non-goals and excluded audiences if rollout is limited

3. **Plan rollout and risk controls**
   - Choose rollout type: internal dogfood, beta, staged rollout, feature flag, general availability, or experiment
   - Define eligibility, kill switch, rollback criteria, monitoring, and support readiness
   - Identify known issues and accepted risks

4. **Plan communications**
   - Define user-facing announcement, internal enablement, support/FAQ, release notes, and stakeholder updates
   - Assign owners and deadlines
   - Ensure messaging matches actual feature scope and limitations

5. **Define learning loop**
   - Define analytics events, dashboards, feedback channels, experiment design, and decision thresholds
   - Define post-launch actions: iterate, scale, pause, rollback, or retire
   - Use `file_write` to save launch plan when requested
   - Use `mcp_memory__memory_store` to persist launch plan, decisions, metric baselines, and post-launch learnings

## Output Format

```markdown
# Launch Plan: [Feature/Product]

## Launch Summary
- **Launch type**: [dogfood/beta/staged/GA/experiment]
- **Target audience**: [segment]
- **Launch date**: [date]
- **Decision owner**: [owner]

## Goals and Metrics
| Goal | Metric | Baseline | Target | Review Date |
|------|--------|----------|--------|-------------|
| ... | ... | ... | ... | ... |

## Rollout Plan
| Phase | Audience | Criteria to Enter | Criteria to Exit | Owner |
|-------|----------|-------------------|------------------|-------|
| Dogfood | Internal | Build ready | No blockers | PM/QA |

## Messaging
| Audience | Message | Channel | Owner | Date |
|----------|---------|---------|-------|------|
| Users | ... | In-app/email | PM | ... |

## Readiness Checklist
- [ ] Feature accepted
- [ ] Analytics ready
- [ ] Support/FAQ ready
- [ ] Release notes ready
- [ ] Known issues accepted
- [ ] Rollback/kill switch defined
- [ ] Monitoring dashboard ready

## Risks and Mitigations
| Risk | Trigger | Mitigation | Owner |
|------|---------|------------|-------|
| ... | ... | ... | ... |

## Post-Launch Review
- Date: [date]
- Decisions to make: [iterate/scale/pause/rollback]
```

## Red Flags

Stop and revise if you see:

- Launch plan without success metrics
- No support or communication owner
- No rollback or kill-switch thinking for risky release
- Messaging promises more than feature delivers
- No post-launch review date
- Analytics missing for launch goals
