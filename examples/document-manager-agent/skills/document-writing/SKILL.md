---
name: document-writing
description: Write clear technical, product, process, operational, onboarding, and knowledge-base documentation with audience, purpose, examples, and review gates
version: "1.0.0"
author: developer
triggers:
  - write docs
  - documentation
  - 文档编写
  - README
  - guide
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Document Writing Skill

## Core Rule

Write for a specific reader and task. Do not create generic documentation without audience, purpose, owner, and freshness expectations.

## Execution Steps

1. **Define audience and purpose**
   - Use `mcp_memory__memory_recall` to retrieve writing conventions, templates, taxonomy, and style decisions
   - Use `file_read` to inspect source material, related docs, examples, and canonical references
   - Identify audience, task, document type, depth, owner, and lifecycle status
   - Choose structure: tutorial, how-to, reference, explanation, runbook, ADR, FAQ, onboarding, release note, or knowledge-base article

2. **Plan structure**
   - Start with reader outcome and key context
   - Define headings, examples, prerequisites, steps, references, and expected outputs
   - Decide what belongs in this document and what should be linked elsewhere
   - Avoid duplicating canonical docs unless intentional

3. **Write content**
   - Use clear headings, short sections, tables, checklists, and examples
   - Explain why important decisions exist, not only what to do
   - Include commands, code snippets, or workflows only when verified or clearly marked as examples
   - Mark assumptions and open questions

4. **Review and save**
   - Check accuracy, completeness, clarity, consistency, links, and freshness
   - Use `file_write` to save or update document when requested
   - Use `mcp_memory__memory_store` to persist document location, template, style decisions, and owner/freshness rules

## Output Format

```markdown
# [Document Title]

> [One-line summary of what this document helps the reader do]

## Audience
[Who this is for]

## Purpose
[Reader task or outcome]

## Prerequisites
- [Prerequisite]

## Overview
[Short context]

## Steps / Content
1. [Step]
2. [Step]

## Examples
[Example]

## Troubleshooting / Notes
- [Issue and fix]

## Related Documents
- [Link]

## Metadata
- **Owner**: [owner]
- **Status**: [Draft/Current/Deprecated/Archived]
- **Last reviewed**: [date]
- **Review cadence**: [cadence]
```

## Red Flags

Stop and clarify if you see:

- No defined audience
- No owner for long-lived doc
- Unverified instructions presented as fact
- Large copied content instead of linking canonical source
- No examples for complex workflow
- Document contradicts existing canonical docs
