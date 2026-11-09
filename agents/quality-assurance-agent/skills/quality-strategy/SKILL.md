---
name: quality-strategy
description: Define quality objectives, risk-based quality gates, metrics, ownership, and continuous improvement plan for a product or release
version: "1.0.0"
author: developer
triggers:
  - quality strategy
  - quality plan
  - QA strategy
  - 质量策略
  - 质量管理
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Quality Strategy Skill

## Core Rule

Quality strategy must be risk-based and evidence-driven. Do not define generic quality goals without measurable gates, owners, and evidence.

## Execution Steps

1. **Load context**
   - Use `mcp_memory__memory_recall` to retrieve prior quality gates, defect trends, release history, flaky tests, and accepted risks
   - Use `file_read` to inspect PRD, architecture docs, roadmap, release plan, support issues, test reports, and incident history
   - Identify product area, release target, users, stakeholders, compliance needs, and operational constraints

2. **Identify quality risks**
   - Assess risk by user impact, business criticality, complexity, change size, dependency exposure, defect history, security impact, and recovery difficulty
   - Separate product risks, technical risks, process risks, and operational risks
   - Mark risks requiring explicit stakeholder acceptance

3. **Define quality objectives**
   - Define measurable objectives for reliability, correctness, performance, security, accessibility, usability, compatibility, and operability as applicable
   - Tie objectives to user outcomes and business risk
   - Avoid vague goals such as "high quality" without metrics

4. **Define quality gates**
   - Define entry and exit criteria for requirements, design, implementation, test execution, release, and post-release monitoring
   - Specify required evidence: test results, coverage, defect status, performance data, security review, accessibility checks, release notes, rollback plan
   - Assign owners for each gate

5. **Define metrics and reporting cadence**
   - Choose metrics that drive decisions, not vanity metrics
   - Track defect leakage, escaped defects, severity mix, reopen rate, flaky tests, pass rate, automation stability, cycle time, and risk burndown where useful
   - Define reporting cadence and escalation thresholds

6. **Create improvement loop**
   - Define how quality retrospectives capture root causes and prevention actions
   - Use `mcp_memory__memory_store` to persist quality strategy, metrics, gates, and known risks
   - Use `file_write` to save the strategy when requested

## Output Format

```markdown
# Quality Strategy: [Product/Release]

## 1. Executive Summary
[Quality recommendation and top risks]

## 2. Scope
- **In scope**: [areas]
- **Out of scope**: [areas]
- **Release target**: [date/version]

## 3. Quality Risks
| Risk | Impact | Probability | Evidence | Mitigation | Owner |
|------|--------|-------------|----------|------------|-------|
| ... | ... | ... | ... | ... | ... |

## 4. Quality Objectives
| Objective | Metric | Target | Evidence |
|-----------|--------|--------|----------|
| Reliability | Error rate | < 0.1% | Monitoring |

## 5. Quality Gates
| Gate | Entry Criteria | Exit Criteria | Evidence | Owner |
|------|----------------|---------------|----------|-------|
| Requirements | PRD ready | Acceptance criteria approved | PRD review | PM/QA |

## 6. Test Strategy Summary
| Test Type | Purpose | Scope | Owner | Required For Release |
|-----------|---------|-------|-------|----------------------|
| Integration | Validate module contracts | Critical flows | QA/Eng | Yes |

## 7. Metrics and Cadence
| Metric | Cadence | Threshold | Escalation |
|--------|---------|-----------|------------|
| Blocker defects | Daily | > 0 | Release owner |

## 8. Continuous Improvement
- [Improvement action]
```

## Red Flags

Stop and revise if you see:

- Quality goals without measurable evidence
- Release gate without owner
- Risk not mapped to test or mitigation
- Metrics that do not inform decisions
- Known issue without acceptance owner
- No post-release monitoring plan
