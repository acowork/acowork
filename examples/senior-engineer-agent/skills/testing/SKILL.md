---
name: testing
description: Design and write behavior-focused tests using TDD, red-green-refactor, regression coverage, and deterministic validation
version: "1.1.0"
author: developer
triggers:
  - test
  - write tests
  - 编写测试
  - unit test
  - integration test
  - TDD
tool_deps:
  - file_read
  - file_write
  - shell
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

# Testing Skill

## Core Rule

For new behavior, bug fixes, refactoring, or behavior changes: write the test first, watch it fail, then implement the minimal code to pass.

If you did not see the test fail for the expected reason, you have not proven the test protects the behavior.

## Execution Steps

1. **Analyze the target and conventions**
   - Use `mcp_memory__memory_recall` to retrieve project-specific testing conventions and known edge cases
   - Use `file_read` to inspect the code under test, existing tests, fixtures, and test commands
   - Identify the public contract: inputs, outputs, side effects, error conditions, and invariants
   - Find similar tests in the codebase and follow their style
   - Avoid adding new test libraries unless the project already uses them or the user approves

2. **Design behavior-focused cases**
   - Cover one behavior per test
   - Prefer tests that exercise real code over tests that only verify mocks
   - Include happy paths, boundary values, error paths, regression cases, and critical edge cases
   - Use descriptive names that state behavior and expected outcome
   - Split tests whose names need "and" because they usually cover multiple behaviors

3. **RED: write the failing test first**
   - Use `file_write` to add the smallest test for the next behavior
   - Run only the focused test first with `shell`
   - Confirm it fails, not errors
   - Confirm the failure message proves the intended missing behavior
   - If the test passes immediately, fix the test because it is not testing new behavior

4. **GREEN: implement the minimal code**
   - Write only enough production code to make the focused test pass
   - Do not add speculative options, abstractions, configuration, or unrelated cleanup
   - Do not change the test to match an incorrect implementation
   - Run the focused test again and confirm it passes

5. **REFACTOR: clean while green**
   - Improve names, reduce duplication, or extract helpers only after tests pass
   - Keep behavior unchanged
   - Re-run the focused test after each refactor
   - If a refactor breaks tests, revert or make the step smaller

6. **Repeat and broaden validation**
   - Add the next failing test for the next behavior
   - After focused tests pass, run relevant test suites
   - Run lint/typecheck/build commands when available
   - Check output for warnings, flakes, panics, and nondeterminism

7. **Record useful findings**
   - Use `mcp_memory__memory_store` for recurring edge cases, testing patterns, flaky-test causes, and project-specific test commands

## Output Format

```markdown
## Test Report

### Test Target
[Description of contract tested]

### Test Cases
| # | Category | Test Name | Behavior | Red Verified |
|---|----------|-----------|----------|--------------|
| 1 | Regression | test_rejects_empty_email | Empty email returns validation error | Yes |

### Files Changed
- `path/to/test`
- `path/to/source`

### Commands Run
| Command | Result |
|---------|--------|
| `cargo test test_name` | PASS |

### Verification Checklist
- [x] Each new behavior had a failing test first
- [x] Failure reason was expected
- [x] Minimal implementation made test pass
- [x] Relevant existing tests pass
- [x] Lint/typecheck pass or skipped with reason

### Known Gaps
- [Gap or none]
```

## Good Test Criteria

- Minimal: one behavior per test
- Clear: name describes expected behavior
- Contract-focused: tests observable behavior, not implementation details
- Deterministic: no uncontrolled time, randomness, network, or order dependence
- Useful failure: failure message points to the broken contract

## Red Flags

Stop and correct the approach if any of these occur:

- Production code before test for new behavior
- Test passes immediately for behavior that should not exist yet
- Test only proves mock behavior
- Large setup makes the API hard to use
- Test requires hidden global state or ordering
- Fixing implementation by weakening the test
- Marking work complete without running the relevant commands
