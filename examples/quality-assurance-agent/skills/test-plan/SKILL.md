---
name: test-plan
description: Create a risk-based test plan with scope, test levels, environments, data, responsibilities, schedule, entry and exit criteria
version: "1.0.0"
author: developer
triggers:
  - test plan
  - testing plan
  - QA plan
  - 测试计划
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Test Plan Skill

## Core Rule

A test plan must map risks and requirements to concrete test coverage, environments, owners, and exit criteria.

Do not create a plan that only lists test types without explaining what risk each test addresses.

## Execution Steps

1. **Confirm scope and inputs**
   - Use `mcp_memory__memory_recall` to retrieve prior test plans, defect history, flaky tests, environment constraints, and release gates
   - Use `file_read` to inspect PRD, design docs, task breakdown, architecture, risk register, and release plan
   - Confirm in-scope features, out-of-scope areas, target platforms, environments, data needs, and release timeline
   - Identify unresolved questions that block planning

2. **Assess test risk**
   - Identify high-risk areas by change size, complexity, user impact, integration exposure, defect history, security, performance, and rollback difficulty
   - Prioritize test effort around high-risk and high-impact paths
   - Define what can be safely sampled and what must be exhaustive

3. **Define coverage by test level**
   - Unit: local behavior and edge cases
   - Integration: module contracts and data flow
   - Contract/API: request/response compatibility and error semantics
   - System/E2E: critical user journeys
   - Exploratory: unknown risks and usability issues
   - Non-functional: performance, security, accessibility, compatibility, reliability, recovery as applicable

4. **Plan execution logistics**
   - Define environments, test data, tools, owners, dependencies, schedule, and reporting cadence
   - Define entry criteria before testing starts
   - Define exit criteria before release recommendation
   - Identify test blockers and escalation path

5. **Define traceability and reporting**
   - Map requirements and risks to test coverage
   - Define status reporting format and defect triage cadence
   - Use `file_write` to save the test plan when requested
   - Use `mcp_memory__memory_store` to persist plan, risks, environments, and coverage decisions

## Output Format

```markdown
# Test Plan: [Feature/Release]

## 1. Scope
### In Scope
- [Feature/area]
### Out of Scope
- [Feature/area]

## 2. Test Objectives
- [Objective]

## 3. Risk-Based Coverage
| Risk/Requirement | Priority | Test Type | Coverage Approach | Owner |
|------------------|----------|-----------|-------------------|-------|
| REQ-001 | High | E2E + Integration | Critical path + failure cases | QA |

## 4. Test Levels
| Level | Scope | Entry Criteria | Exit Criteria | Tools |
|-------|-------|----------------|---------------|-------|
| Integration | Module contracts | Build ready | Critical contracts pass | [tool] |

## 5. Environment and Data
| Environment | Purpose | Data Needs | Owner | Status |
|-------------|---------|------------|-------|--------|
| Staging | Release validation | Production-like seeded data | QA | Ready |

## 6. Schedule and Responsibilities
| Activity | Owner | Start | End | Dependency |
|----------|-------|-------|-----|------------|
| Test execution | QA | ... | ... | Build ready |

## 7. Defect Triage
- Cadence: [daily/weekly]
- Severity definitions: [link or summary]
- Escalation path: [owner]

## 8. Exit Criteria
- [ ] No open blocker defects
- [ ] Critical user journeys pass
- [ ] Known issues accepted by owner
- [ ] Release notes and rollback plan reviewed

## 9. Open Questions
- [Question]
```

## Red Flags

Stop and clarify if you see:

- Test plan without requirements or risks
- No environment owner
- Exit criteria missing or subjective
- Critical path not covered
- No defect triage cadence
- Test data requirements undefined
