---
name: regression-management
description: Manage regression scope, suites, prioritization, automation, flaky tests, change impact, and maintenance based on risk and defect history
version: "1.0.0"
author: developer
triggers:
  - regression
  - regression test
  - 回归测试
  - regression suite
  - flaky tests
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Regression Management Skill

## Core Rule

Regression suites must be risk-based and maintained. A large stale suite is not quality evidence.

Prioritize tests that protect critical user journeys, high-change areas, high-defect areas, and release-blocking behavior.

## Execution Steps

1. **Load regression context**
   - Use `mcp_memory__memory_recall` to retrieve defect history, flaky tests, release escapes, critical paths, and prior regression scope
   - Use `file_read` to inspect test plan, test reports, automation suite, changelog, PR list, release scope, and known issues
   - Identify changed areas and impacted user journeys

2. **Perform change impact analysis**
   - Map code/product changes to features, integrations, data flows, permissions, and non-functional risks
   - Identify must-run regression tests, sampled areas, and low-risk areas that can be skipped with rationale
   - Include prior escaped defects and customer-critical paths

3. **Prioritize regression suite**
   - Tier tests:
     - P0: release-blocking smoke and critical flows
     - P1: high-risk changed areas and recent defects
     - P2: broader stable coverage
     - P3: low-value or periodic coverage
   - Define when each tier runs: PR, nightly, pre-release, post-release

4. **Manage automation and flakiness**
   - Identify flaky tests and mark them separately from product failures
   - Do not treat flaky pass/fail as release confidence without investigation
   - Decide whether to fix, quarantine, rewrite, or retire flaky tests
   - Track automation value vs maintenance cost

5. **Define execution and reporting**
   - Specify owners, environments, data needs, commands/tools, and reporting format
   - Define pass/fail criteria and escalation for failures
   - Use `file_write` to save the regression plan when requested
   - Use `mcp_memory__memory_store` to persist suite changes, flaky tests, retired tests, and regression learnings

## Output Format

```markdown
# Regression Plan: [Release/Change]

## Change Impact Summary
| Change Area | Impacted Features | Risk | Regression Scope |
|-------------|-------------------|------|------------------|
| ... | ... | High | P0/P1 tests |

## Regression Tiers
| Tier | Purpose | Runs When | Owner | Exit Criteria |
|------|---------|-----------|-------|---------------|
| P0 | Critical smoke | PR + Pre-release | QA/Eng | 100% pass |
| P1 | High-risk areas | Pre-release | QA | No critical failures |

## Selected Tests
| Test/Scenario | Tier | Requirement/Risk | Automation | Environment | Owner |
|---------------|------|------------------|------------|-------------|-------|
| Login critical path | P0 | Auth availability | Yes | Staging | QA |

## Flaky Tests
| Test | Symptom | Decision | Owner | Due Date |
|------|---------|----------|-------|----------|
| ... | ... | Fix/Quarantine/Retire | ... | ... |

## Execution Report
| Tier | Total | Passed | Failed | Blocked | Notes |
|------|-------|--------|--------|---------|-------|
| P0 | ... | ... | ... | ... | ... |

## Skipped Scope
| Area | Reason | Risk Accepted By |
|------|--------|------------------|
| ... | Low impact/no change | QA |
```

## Red Flags

Stop and revise if you see:

- Regression scope unrelated to change impact
- Critical flow not in P0/P1
- Flaky failures ignored or counted as confidence
- Skipped scope without rationale
- Large suite with no prioritization
- No owner for failed or flaky tests
- Release decision based on partial regression without risk acceptance
