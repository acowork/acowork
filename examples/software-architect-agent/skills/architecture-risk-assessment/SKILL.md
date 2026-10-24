---
name: architecture-risk-assessment
description: Assess architectural risk, blast radius, reversibility, operational failure modes, migration safety, and rollback readiness
version: "1.0.0"
author: developer
triggers:
  - architecture risk
  - risk assessment
  - blast radius
  - rollback plan
  - 风险评估
  - 回滚计划
tool_deps:
  - file_read
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Architecture Risk Assessment Skill

## Execution Steps

1. **Establish scope**
   - Use `mcp_memory__memory_recall` to retrieve prior incidents, architecture decisions, and project constraints
   - Use `file_read` to inspect the proposed change, affected files, interfaces, and deployment path
   - Identify touched subsystems, public contracts, data stores, security boundaries, and operational dependencies

2. **Classify risk tier**
   - Low: isolated docs, tests, or internal implementation with no behavior change
   - Medium: behavior change inside one bounded subsystem
   - High: security, runtime, gateway, persistence, public API, schema, data migration, deployment, or multi-subsystem boundary changes
   - When uncertain, classify higher

3. **Analyze failure modes**
   - What can fail during rollout?
   - What can fail after partial deployment?
   - What data can be corrupted, leaked, lost, duplicated, or made inconsistent?
   - What user-visible behavior can regress?
   - What operational signals would reveal the failure?

4. **Assess reversibility**
   - Can the change be reverted safely?
   - Are migrations backward-compatible?
   - Are new config keys, schemas, protocols, or APIs public contracts?
   - Are there one-way doors that require explicit approval?

5. **Define mitigations**
   - Reduce blast radius through phased rollout, compatibility layers, kill switches, or narrow scope
   - Add tests, assertions, metrics, logs, and runbooks for high-risk paths
   - Define rollback and recovery steps before implementation or deployment

6. **Persist learnings**
   - Use `mcp_memory__memory_store` for recurring risks, mitigation patterns, and project-specific rollback practices

## Output Format

```markdown
# Architecture Risk Assessment: [Change Name]

## Risk Tier
[Low / Medium / High]

## Scope and Blast Radius
| Area | Impact | Notes |
|------|--------|-------|
| ... | ... | ... |

## Key Risks
| Risk | Probability | Impact | Detection | Mitigation | Rollback |
|------|-------------|--------|-----------|------------|----------|
| ... | ... | ... | ... | ... | ... |

## One-Way Doors
- [Decision or migration that is hard to reverse]

## Required Validation
- [Test/check/metric/runbook]

## Rollout Recommendation
[Recommended rollout sequence]

## Rollback Plan
1. [Step]
2. [Step]

## Open Questions
- [Question]
```

## Notes

- Do not accept "we can just revert" for data migrations, public contracts, or distributed deployments without proving it
- Silent fallback can increase risk by hiding failures; prefer explicit errors for unsafe states
- If rollback is unclear, treat the design as incomplete
