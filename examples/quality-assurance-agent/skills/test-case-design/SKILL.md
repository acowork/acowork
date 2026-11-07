---
name: test-case-design
description: Design behavior-focused test cases with acceptance criteria traceability, boundary values, negative paths, non-functional checks, and automation suitability
version: "1.0.0"
author: developer
triggers:
  - test cases
  - design tests
  - test scenario
  - 测试用例
  - 用例设计
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Test Case Design Skill

## Core Rule

Test cases must prove behavior and risk coverage, not implementation details. Every important requirement should map to at least one positive and one meaningful negative or edge case when applicable.

## Execution Steps

1. **Load requirements and context**
   - Use `mcp_memory__memory_recall` to retrieve known edge cases, defect patterns, test conventions, and flaky areas
   - Use `file_read` to inspect PRD, acceptance criteria, design docs, test plan, existing tests, and defect history
   - Identify target behavior, user roles, inputs, outputs, state changes, and error conditions

2. **Design scenarios**
   - Cover happy path, boundary values, invalid input, missing data, permissions, concurrency, retries, recovery, compatibility, and regression cases as relevant
   - Use equivalence partitioning, boundary value analysis, decision tables, and state transitions where helpful
   - Prioritize scenarios by user impact and defect likelihood

3. **Define expected results**
   - Each case must have precise preconditions, steps, expected result, and pass/fail criteria
   - Expected result should be observable by user, API, data state, logs, metrics, or system output
   - Avoid vague expected results like "works correctly"

4. **Assess automation suitability**
   - Mark tests as automated, manual, exploratory, or not worth automating
   - Automate repeatable high-value regression checks
   - Keep exploratory testing for unknown risk, usability, and complex behavior discovery
   - Identify flaky risk and required stabilization work

5. **Save and persist**
   - Use `file_write` to save test cases when requested
   - Use `mcp_memory__memory_store` to persist reusable edge cases, scenario patterns, and automation decisions

## Output Format

```markdown
# Test Cases: [Feature/Area]

## Traceability Summary
| Requirement/Risk | Covered By | Gap |
|------------------|------------|-----|
| REQ-001 | TC-001, TC-002 | None |

## Test Cases
| ID | Title | Priority | Type | Preconditions | Steps | Expected Result | Automation | Requirement/Risk |
|----|-------|----------|------|---------------|-------|-----------------|------------|------------------|
| TC-001 | [Behavior] | High | Functional | [State] | 1. ... | [Observable result] | Yes/No | REQ-001 |

## Exploratory Charters
| Charter | Risk | Timebox | Notes |
|---------|------|---------|-------|
| Explore invalid session recovery | Reliability | 60 min | Focus on retry/logout behavior |

## Data and Environment Needs
- [Data/environment]

## Open Gaps
- [Gap]
```

## Red Flags

Stop and revise if you see:

- Test case without expected result
- Expected result not observable
- Requirement with no coverage
- Only happy-path tests for high-risk feature
- Automation chosen without considering stability and maintenance cost
- Tests depending on uncontrolled order, time, or external state
