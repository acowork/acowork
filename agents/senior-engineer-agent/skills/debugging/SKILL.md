---
name: debugging
description: Systematically debug and troubleshoot issues with root-cause analysis, evidence gathering, failing regression tests, and verified fixes
version: "1.1.0"
author: developer
triggers:
  - debug
  - fix bug
  - 排查问题
  - troubleshoot
  - investigate
tool_deps:
  - file_read
  - file_write
  - shell
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

# Debugging Skill

## Core Rule

No fix before root-cause investigation. A symptom patch is not a fix.

If you cannot explain why the bug happens, do not change production code except to add diagnostic evidence.

## Execution Steps

1. **Reproduce and capture facts**
   - Use `mcp_memory__memory_recall` to check whether a similar issue, convention, or known failure mode exists
   - Use `file_read` to inspect relevant source, tests, configuration, and docs
   - Use `shell` to run the exact failing command or scenario
   - Record environment, inputs, expected behavior, actual behavior, logs, stack traces, and frequency
   - If the issue is not reproducible, gather more evidence instead of guessing

2. **Read errors completely**
   - Read full error messages, stack traces, file paths, line numbers, and error codes
   - Identify the first meaningful failure, not only the final symptom
   - Check recent changes, dependencies, configuration, and environment differences
   - Compare failing behavior with a nearby working example in the same codebase

3. **Trace data and component boundaries**
   - Trace the bad value or state backward from failure point to origin
   - For multi-component paths, verify what enters and exits each boundary
   - Check config propagation, state transitions, permissions, serialization, retries, and timeouts
   - Add minimal temporary diagnostics only when needed to locate the failing boundary
   - Never log secrets, tokens, credentials, or sensitive payloads

4. **Form and test one hypothesis**
   - State one specific hypothesis: "I think X is the root cause because Y"
   - Test the hypothesis with the smallest possible experiment
   - Change one variable at a time
   - If the hypothesis is wrong, discard it and form a new one from the evidence
   - Do not stack fixes or make broad refactors while investigating

5. **Stop after repeated failed fixes**
   - If one fix attempt fails, return to evidence gathering
   - If two fix attempts fail, reassess assumptions and compare against working patterns
   - If three fix attempts fail, stop and question the architecture before attempting another fix
   - Treat repeated failures across different locations as a likely boundary, coupling, or design problem

6. **Create a regression test before fixing**
   - Add the smallest failing test that reproduces the bug
   - Verify the test fails for the expected reason
   - If no test framework exists, create the smallest repeatable reproduction command or script
   - Do not proceed until the failure proves the bug is captured

7. **Implement the minimal fix**
   - Use `file_write` to change only the root cause
   - Avoid unrelated cleanup, refactoring, renaming, or formatting
   - Keep the fix narrow and reversible
   - If the fix changes behavior, update tests and relevant docs

8. **Verify and record**
   - Use `shell` to run the regression test and confirm it passes
   - Run relevant existing tests, lint, and type checks
   - Re-run the original reproduction steps
   - Use `mcp_memory__memory_store` to persist non-obvious root causes, project-specific failure modes, and debugging insights

## Output Format

```markdown
## Debug Report

### Issue
[Brief description]

### Facts Observed
- [Error/output/path/line]

### Root Cause
[Precise explanation of why the bug occurs]

### Hypotheses Tested
| Hypothesis | Evidence | Result |
|------------|----------|--------|
| ... | ... | Confirmed/Rejected |

### Regression Test
- Test: [name/path]
- Verified failing before fix: Yes/No

### Fix
[Description with file and line references]

### Verification
- [x] Original reproduction no longer fails
- [x] Regression test passes
- [x] Relevant existing tests pass
- [x] Lint/typecheck pass or skipped with reason

### Follow-up
- [Architecture concern or remaining risk, if any]
```

## Red Flags

Stop and return to investigation if you catch yourself doing any of these:

- Proposing a fix before explaining the root cause
- Trying changes to "see if it works"
- Changing multiple things at once
- Skipping the regression test
- Treating a symptom as the root cause
- Adding broad refactoring to a bug fix
- Ignoring a failed hypothesis and adding another patch on top
- Attempting a fourth fix after three failures without architecture discussion
