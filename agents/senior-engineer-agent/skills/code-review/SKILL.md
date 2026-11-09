---
name: code-review
description: Perform a thorough code review with severity-ranked findings covering correctness, security, tests, architecture boundaries, performance, maintainability, and rollout risk
version: "1.1.0"
author: developer
triggers:
  - review
  - code review
  - 审查代码
  - PR review
tool_deps:
  - file_read
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

# Code Review Skill

## Core Rule

Review the work product, not intentions. Findings must cite evidence and explain impact.

Critical and Important issues must be resolved before proceeding unless the user explicitly accepts the risk.

## Execution Steps

1. **Understand scope and intent**
   - Use `mcp_memory__memory_recall` to retrieve project-specific coding standards, architectural boundaries, and prior review feedback
   - Use `file_read` to inspect changed files, surrounding context, tests, docs, and relevant interfaces
   - Identify change type: bug fix, feature, refactor, config, dependency, docs, or generated artifact
   - Identify claimed behavior and acceptance criteria from the request, commit message, or PR description
   - If scope is too large or mixed, recommend splitting before deep review

2. **Check correctness**
   - Verify the code does what the change claims
   - Trace data flow through changed paths
   - Check edge cases: empty, null, malformed, boundary values, missing optional fields, concurrency, retries, and partial failure
   - Check error handling: no swallowed errors, ambiguous fallbacks, panics in runtime paths, or incorrect recovery
   - Confirm behavior changes are intentional and documented

3. **Check tests and verification**
   - Verify new behavior has tests
   - For bug fixes, verify there is a regression test that would fail before the fix
   - For refactors, verify characterization or existing behavior tests cover moved logic
   - Check that tests exercise real behavior rather than only mocks
   - Check that relevant lint, typecheck, build, or test commands were run or should be requested

4. **Check security and privacy**
   - Validate input boundaries, path handling, command execution, deserialization, and injection risks
   - Check permissions, trust boundaries, least privilege, and access control placement
   - Ensure secrets, tokens, credentials, sensitive payloads, and personal data are not logged or committed
   - Check that fallback behavior does not silently broaden access or hide unsafe states

5. **Check architecture and maintainability**
   - Verify dependency direction respects public contracts and module boundaries
   - Identify cross-subsystem coupling, hidden shared state, god modules, and speculative abstractions
   - Check names, responsibilities, cohesion, and readability
   - Prefer small targeted changes over broad rewrites
   - Apply rule-of-three before approving new shared abstractions

6. **Check performance and operability**
   - Look for avoidable O(n²) behavior, unbounded memory growth, blocking calls in async paths, needless allocations, and missing cleanup
   - Check observability for important failure paths: logs, metrics, error messages, and diagnostics
   - Check rollout, compatibility, migration, and rollback risk for public APIs, schemas, config, persistence, and distributed behavior

7. **Classify findings and recommend action**
   - Use severity levels:
     - **Critical**: correctness, security, data loss, privacy, or production-breaking issue; must fix
     - **Important**: likely bug, missing test, maintainability risk, migration gap; should fix before proceeding
     - **Minor**: readability, naming, small cleanup; can follow up
     - **Question**: ambiguity requiring clarification
   - Provide concrete recommendations with file and line references when possible
   - Acknowledge good decisions worth preserving
   - Use `mcp_memory__memory_store` for recurring project review findings and conventions

## Output Format

```markdown
## Code Review Report

### Verdict
[Approve / Approve with minor comments / Request changes / Block]

### Summary
[Brief overall assessment]

### Critical Issues
- **`path/to/file.ext:line` — [Title]**
  - Evidence: [what the code does]
  - Impact: [why it matters]
  - Recommendation: [specific fix]

### Important Issues
- **`path/to/file.ext:line` — [Title]**
  - Evidence: [what the code does]
  - Impact: [why it matters]
  - Recommendation: [specific fix]

### Minor Suggestions
- **`path/to/file.ext:line` — [Title]**
  - Recommendation: [specific improvement]

### Questions
- [Question]

### Tests and Verification
- Tests present: Yes/No
- Regression coverage: Yes/No/Not applicable
- Commands recommended: `[command]`

### Positive Observations
- [Good pattern or decision]
```

## Red Flags

Do not approve without comment if you see:

- No test for new behavior or bug fix
- Security-sensitive path without negative tests
- Public contract change without compatibility or migration plan
- Silent fallback hiding unsafe or costly behavior
- Cross-module dependency bypassing public contracts
- Mixed feature, refactor, and formatting changes
- Large generated or unrelated changes hiding functional changes
- Secrets, credentials, personal data, or raw sensitive payloads in files or logs
