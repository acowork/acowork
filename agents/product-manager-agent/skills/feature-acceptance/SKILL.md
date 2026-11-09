---
name: feature-acceptance
description: Validate implemented features against PRD, Figma/design, acceptance criteria, analytics, edge cases, and release readiness
version: "1.0.0"
author: developer
triggers:
  - feature acceptance
  - acceptance
  - UAT
  - 验收
  - 产品验收
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Feature Acceptance Skill

## Core Rule

Do not accept a feature by impression. Acceptance requires evidence against PRD, design, user flow, edge cases, and success metrics.

## Execution Steps

1. **Load acceptance basis**
   - Use `mcp_memory__memory_recall` to retrieve PRD decisions, Figma review findings, known risks, and accepted trade-offs
   - Use `file_read` to inspect PRD, acceptance criteria, design specs, test reports, release notes, and known issues
   - Identify in-scope requirements and non-goals

2. **Validate requirement coverage**
   - Check each Must requirement against implemented behavior or test evidence
   - Check important Should requirements and document gaps
   - Verify design alignment with Figma or approved design reference
   - Check copy, states, permissions, responsive behavior, and analytics events

3. **Classify gaps**
   - Blocker: prevents user value, breaks core flow, violates Must requirement, or creates unacceptable risk
   - Important: should fix before release unless explicitly accepted
   - Minor: can follow up without blocking release
   - Question: needs product decision

4. **Make acceptance recommendation**
   - Accept, Accept with follow-ups, Conditional Accept, or Reject
   - For conditional acceptance, define exact conditions, owner, and deadline
   - Use `file_write` to save acceptance report when requested
   - Use `mcp_memory__memory_store` to persist acceptance decision, gaps, follow-ups, and rationale

## Output Format

```markdown
# Feature Acceptance Report: [Feature]

## Verdict
[Accept / Accept with follow-ups / Conditional Accept / Reject]

## Basis Reviewed
- PRD: [reference]
- Design/Figma: [reference]
- Test evidence: [reference]

## Requirement Coverage
| Requirement | Expected | Observed/Evidence | Status | Notes |
|-------------|----------|-------------------|--------|-------|
| REQ-001 | ... | ... | Pass/Fail/Partial | ... |

## Findings
### Blockers
- [Finding]
### Important
- [Finding]
### Minor
- [Finding]
### Questions
- [Question]

## Analytics and Launch Readiness
- [ ] Required events present
- [ ] Success metrics measurable
- [ ] Known issues documented
- [ ] Support/docs ready or not needed

## Follow-ups
| Item | Owner | Due Date | Blocking |
|------|-------|----------|----------|
| ... | ... | ... | Yes/No |
```

## Red Flags

Stop and reject or conditionally accept if you see:

- Must requirement not met
- Core Figma flow missing or changed without decision
- No evidence for important behavior
- Analytics missing for success metric
- Known issue without owner or acceptance
- Edge states not implemented for critical flow
