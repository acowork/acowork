---
name: figma-design-review
description: Review Figma designs, screenshots, prototypes, or design specs for product intent, UX flow, states, accessibility, analytics, and engineering handoff readiness
version: "1.0.0"
author: developer
triggers:
  - Figma
  - design review
  - figma review
  - 设计评审
  - 原型评审
tool_deps:
  - file_read
  - file_write
  - network
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Figma Design Review Skill

## Core Rule

A design is not ready for engineering until it covers the core flow, important states, edge cases, content, accessibility, analytics needs, and handoff details.

If direct Figma access is unavailable, ask for screenshots, exported frames, prototype link, frame names, or design specs. Do not invent unseen details.

## Execution Steps

1. **Load product and design context**
   - Use `mcp_memory__memory_recall` to retrieve PRD decisions, user segments, Figma review findings, design standards, and launch constraints
   - Use `file_read` to inspect PRD, acceptance criteria, user flow, prior design notes, screenshots, or exported Figma specs
   - Use `network` only when a Figma or design reference is available and access is allowed
   - Identify design source: Figma file, prototype, screenshot, exported frames, or written spec

2. **Map design to product intent**
   - Verify design supports target user, problem, goals, and success criteria
   - Map frames/screens to requirements and user journey steps
   - Identify missing flows or mismatched assumptions

3. **Review UX completeness**
   - Check default, empty, loading, error, success, disabled, permission, offline, long-content, and responsive states
   - Check navigation, hierarchy, labeling, copy clarity, affordance, feedback, and recovery paths
   - Check accessibility basics: contrast, focus order, keyboard path, touch targets, readable text, error messages

4. **Review handoff readiness**
   - Confirm components, spacing, variants, interactions, content rules, breakpoints, assets, and design tokens are specified or linkable
   - Identify engineering ambiguities and decisions needed before build
   - Define analytics events and instrumentation points for key user actions

5. **Provide findings and recommendation**
   - Classify findings as Blocker, Important, Minor, or Question
   - Use `file_write` to save review when requested
   - Use `mcp_memory__memory_store` to persist design decisions, gaps, and accepted trade-offs

## Output Format

```markdown
# Figma Design Review: [Feature/Flow]

## Design Source
- Figma/prototype: [link or unavailable]
- Frames reviewed: [frames]
- PRD/requirements: [reference]

## Verdict
[Ready / Ready with changes / Not ready]

## Requirement Traceability
| Requirement | Design Coverage | Gap |
|-------------|-----------------|-----|
| REQ-001 | Frame A, Frame B | None |

## Findings
### Blocker
- **[Title]**
  - Evidence: [frame/state]
  - Impact: [user/product impact]
  - Recommendation: [change]

### Important
- **[Title]**
  - Evidence: [frame/state]
  - Recommendation: [change]

### Minor
- [Suggestion]

### Questions
- [Question]

## State Coverage
| State | Covered | Notes |
|-------|---------|-------|
| Empty | Yes/No | ... |
| Loading | Yes/No | ... |
| Error | Yes/No | ... |
| Mobile/responsive | Yes/No | ... |

## Handoff Checklist
- [ ] Components/variants clear
- [ ] Interactions/prototype clear
- [ ] Copy final or owner assigned
- [ ] Accessibility concerns addressed
- [ ] Analytics events defined
- [ ] Open decisions owned
```

## Red Flags

Stop and request clarification if you see:

- No visible design artifact or frame reference
- Core user flow missing
- No error/loading/empty states for important flows
- PRD and design disagree
- Engineering-critical interaction unspecified
- Accessibility issues likely to block launch
- Analytics points missing for success metrics
