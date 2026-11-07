---
name: format-conversion
description: Convert documents between Markdown, plain text, structured templates, tables, summaries, HTML-like content, and archival formats while preserving meaning and metadata
version: "1.0.0"
author: developer
triggers:
  - convert docs
  - format conversion
  - 格式转换
  - markdown
  - HTML
tool_deps:
  - doc_read
  - doc_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Format Conversion Skill

## Core Rule

Format conversion must preserve meaning, structure, links, metadata, code blocks, tables, and provenance. If information cannot be preserved, flag the loss explicitly.

## System of Record

A conversion produces a new document, not an overwrite.

- `doc_read` the source, convert, then `doc_write` (`mode="create"`) to the target
  path; give the derived page a line stating it is derived, from which version, and
  by what transformation.
- Never convert in place: the source is evidence, the derivative is a copy, and a
  version conflict on the source should be impossible.
- If the target exists, `doc_stat` it first — replacing a converted page silently is
  how two formats drift apart.

## Execution Steps

1. **Confirm source and target**
   - Use `mcp_memory__memory_recall` to retrieve format preferences, templates, and publishing rules
   - Use `doc_read` to inspect source document and related assets
   - Confirm target format, audience, destination, required metadata, and fidelity expectations
   - Identify unsupported elements before conversion

2. **Map structure**
   - Map headings, lists, tables, links, images/assets, code blocks, diagrams, footnotes, metadata, and callouts
   - Decide how to represent unsupported elements
   - Preserve stable IDs, anchors, and cross-references when possible

3. **Convert content**
   - Use `doc_write` to create converted document when requested
   - Keep content semantically equivalent
   - Do not silently summarize unless asked
   - Add conversion notes when fidelity trade-offs exist

4. **Validate conversion**
   - Check heading hierarchy, links, tables, code fences, images/assets, metadata, and formatting
   - Compare source and target for missing sections
   - Use `mcp_memory__memory_store` to persist reusable conversion mapping or template decisions

## Output Format

```markdown
# Format Conversion Report: [Document]

## Conversion
- **Source**: [path/type]
- **Target**: [path/type]
- **Purpose**: [why converting]

## Structure Mapping
| Source Element | Target Representation | Notes |
|----------------|-----------------------|-------|
| H1/H2 | Markdown headings | Preserved |
| Table | Markdown table | Preserved |

## Fidelity Notes
- [Any formatting or content limitations]

## Validation Checklist
- [x] Headings preserved
- [x] Links checked
- [x] Tables readable
- [x] Code blocks preserved
- [x] Metadata preserved
- [x] No unsupported content silently dropped
```

## Red Flags

Stop and clarify if you see:

- Target format cannot represent critical source content
- Conversion would drop links, tables, or code blocks
- User asked for exact conversion but source requires interpretation
- Images/assets referenced but unavailable
- Metadata requirements unknown
