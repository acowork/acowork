---
name: defect-management
description: Triage, classify, prioritize, assign, track, verify, and learn from defects with clear severity, priority, reproduction, and closure criteria
version: "1.0.0"
author: developer
triggers:
  - defect
  - bug triage
  - bug report
  - 缺陷管理
  - 缺陷分析
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Defect Management Skill

## Core Rule

Do not accept or close a defect without evidence. A defect needs reproduction, impact, severity, priority, owner, and verification criteria.

## Execution Steps

1. **Collect defect evidence**
   - Use `mcp_memory__memory_recall` to retrieve similar defects, known issues, release risks, and prior root causes
   - Use `file_read` to inspect bug reports, logs, screenshots, test results, requirements, and related code or docs when available
   - Capture environment, build/version, steps to reproduce, expected behavior, actual behavior, frequency, and impact
   - If reproduction is missing, request it before prioritization unless impact requires immediate escalation

2. **Classify severity and priority**
   - Severity: user/business/technical impact
   - Priority: urgency and sequencing decision
   - Distinguish blocker, critical, major, minor, trivial using project definitions
   - Consider workaround, frequency, affected users, data/security risk, and release proximity

3. **Triage and assign**
   - Determine owner role and next action
   - Link affected requirement, test case, release, or known issue
   - Decide: fix now, defer, duplicate, cannot reproduce, expected behavior, or needs more information
   - Define verification criteria before work starts

4. **Track resolution and verification**
   - Verify fixes against original reproduction and regression coverage
   - Reopen if fix does not address root cause or verification fails
   - Do not close based only on developer statement
   - Store recurring defect patterns for prevention

5. **Analyze trends**
   - Group defects by component, severity, root cause, phase introduced, phase detected, and escape reason
   - Identify prevention actions: requirement clarification, test coverage, automation, code review checklist, monitoring
   - Use `file_write` to save triage report when requested
   - Use `mcp_memory__memory_store` to persist defect trends and prevention actions

## Output Format

```markdown
# Defect Triage Report: [Defect ID/Title]

## Summary
- **Severity**: [Blocker/Critical/Major/Minor/Trivial]
- **Priority**: [P0/P1/P2/P3]
- **Status**: [New/Triaged/In Progress/Ready for QA/Verified/Closed/Reopened]
- **Owner**: [Role/name]

## Evidence
- **Environment**: [env/version]
- **Reproduction**:
  1. [Step]
  2. [Step]
- **Expected**: [expected behavior]
- **Actual**: [actual behavior]
- **Frequency**: [always/intermittent]
- **Attachments/Logs**: [links or notes]

## Impact Assessment
| Dimension | Assessment |
|-----------|------------|
| Users affected | ... |
| Business impact | ... |
| Workaround | ... |
| Release impact | ... |

## Triage Decision
[Fix now / Defer / Duplicate / Cannot reproduce / Needs info / Expected behavior]

## Verification Criteria
- [ ] Original reproduction no longer fails
- [ ] Regression test added or updated
- [ ] Related critical path still passes

## Prevention Notes
- [Root cause trend or process improvement]
```

## Red Flags

Stop and clarify if you see:

- No reproduction steps
- Severity based on emotion rather than impact
- Priority confused with severity
- Defect closed without QA verification
- Known issue deferred without owner acceptance
- Repeated defects without prevention action
