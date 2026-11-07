---
name: release-acceptance
description: Assess release readiness using quality gates, test evidence, defect status, known issues, operational readiness, rollback, and go/no-go decision criteria
version: "1.0.0"
author: developer
triggers:
  - release acceptance
  - release readiness
  - go no-go
  - 发布验收
  - 发布质量
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Release Acceptance Skill

## Core Rule

No release recommendation without evidence. A release decision must state Go, No-Go, or Conditional Go with risks, known issues, owners, and acceptance criteria.

## Execution Steps

1. **Load release context**
   - Use `mcp_memory__memory_recall` to retrieve release history, quality gates, accepted risks, defect trends, and prior rollback issues
   - Use `file_read` to inspect release plan, test reports, defect list, risk register, monitoring plan, rollback plan, and release notes
   - Confirm release scope, target date, stakeholders, and decision-maker

2. **Evaluate quality gates**
   - Check requirements sign-off, test execution, defect status, regression results, non-functional checks, documentation, operational readiness, and rollback readiness
   - Mark each gate as Pass, Fail, Waived, or Not Applicable
   - Waived gates require explicit owner and risk acceptance

3. **Assess defects and known issues**
   - Identify open blocker/critical defects
   - Review major/minor known issues, workarounds, customer impact, and support readiness
   - Confirm fixes are verified, not merely implemented
   - Confirm regression coverage for high-risk fixes

4. **Assess operational readiness**
   - Check deployment plan, monitoring, alerting, support runbook, feature flags, rollback, data migration, and communication plan
   - Identify one-way doors and recovery constraints

5. **Make recommendation**
   - Choose Go, No-Go, or Conditional Go
   - Provide evidence, risks, conditions, decision deadline, and owner acceptance
   - Use `file_write` to save the release acceptance report when requested
   - Use `mcp_memory__memory_store` to persist release decision, risk acceptance, known issues, and post-release learnings

## Output Format

```markdown
# Release Acceptance Report: [Release/Version]

## Recommendation
**Decision**: [Go / No-Go / Conditional Go]

## Executive Summary
[Brief evidence-based recommendation]

## Release Scope
- [Feature/fix]

## Quality Gates
| Gate | Status | Evidence | Owner | Notes |
|------|--------|----------|-------|-------|
| Regression tests | Pass | [report] | QA | ... |
| Rollback plan | Pass | [plan] | Release owner | ... |

## Defect Status
| Severity | Open | Accepted | Fixed Verified | Notes |
|----------|------|----------|----------------|-------|
| Blocker | 0 | 0 | 2 | ... |

## Known Issues
| Issue | Severity | Impact | Workaround | Accepted By | Expiry/Follow-up |
|-------|----------|--------|------------|-------------|------------------|
| ... | ... | ... | ... | ... | ... |

## Operational Readiness
- [ ] Deployment plan ready
- [ ] Monitoring/alerts ready
- [ ] Rollback plan verified
- [ ] Support communication ready
- [ ] Release notes ready

## Conditions for Release
- [Condition if Conditional Go]

## Post-Release Monitoring
| Signal | Threshold | Owner | Action |
|--------|-----------|-------|--------|
| Error rate | > X% | Ops | Rollback review |
```

## Red Flags

Stop and recommend No-Go or Conditional Go if you see:

- Open blocker defect
- Critical test gate failed without accepted waiver
- Known issue without owner acceptance
- Rollback plan missing for risky change
- Test evidence unavailable
- Release scope changed after testing without retest assessment
- Operational monitoring not ready
