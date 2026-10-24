---
name: document-quality-review
description: Review documents for accuracy, completeness, clarity, consistency, structure, links, freshness, ownership, metadata, and publication readiness
version: "1.0.0"
author: developer
triggers:
  - review docs
  - document review
  - 文档审查
  - docs quality
  - proofreading
tool_deps:
  - file_read
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Document Quality Review Skill

## Core Rule

A document is not ready if readers cannot trust it, find what they need, or know whether it is current.

Review against purpose, audience, source accuracy, structure, and maintainability.

## Execution Steps

1. **Load review context**
   - Use `mcp_memory__memory_recall` to retrieve style guide, templates, taxonomy, canonical sources, and prior review findings
   - Use `file_read` to inspect target document and related canonical references
   - Identify audience, purpose, owner, lifecycle status, and publication target

2. **Review accuracy and completeness**
   - Check claims against source material where available
   - Identify missing prerequisites, steps, examples, edge cases, owner, status, and review date
   - Flag assumptions presented as facts

3. **Review clarity and structure**
   - Check title, summary, heading hierarchy, logical flow, scannability, examples, tables, and checklists
   - Check whether the document answers the reader's likely task
   - Identify jargon, ambiguity, duplicate sections, and overly long prose

4. **Review consistency and maintainability**
   - Check naming, terminology, formatting, links, anchors, code blocks, metadata, and related-doc sections
   - Check freshness, deprecation status, canonical-source alignment, and ownership
   - Identify stale or conflicting content

5. **Report findings**
   - Classify findings as Blocker, Important, Minor, or Question
   - Use `mcp_memory__memory_store` for recurring documentation quality issues and style decisions

## Output Format

```markdown
# Document Quality Review: [Document]

## Verdict
[Ready / Ready with changes / Needs revision]

## Summary
[Short assessment]

## Findings
### Blocker
- **[Title]**
  - Evidence: [section/link]
  - Impact: [why it matters]
  - Recommendation: [fix]

### Important
- **[Title]**
  - Recommendation: [fix]

### Minor
- [Suggestion]

### Questions
- [Question]

## Checklist
- [ ] Audience clear
- [ ] Purpose clear
- [ ] Source/canonical references checked
- [ ] Metadata present
- [ ] Links valid
- [ ] Owner and review cadence present
- [ ] No stale/conflicting content
```

## Red Flags

Do not mark ready if you see:

- No owner for long-lived doc
- No clear audience or purpose
- Broken critical links
- Conflicts with canonical source
- Unverified claims presented as facts
- Missing prerequisites for procedural docs
- Stale content with no warning
