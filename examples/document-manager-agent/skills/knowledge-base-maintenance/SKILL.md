---
name: knowledge-base-maintenance
description: Maintain knowledge bases by deduplicating, linking, updating, deprecating, tagging, indexing, and curating documents for discoverability and freshness
version: "1.0.0"
author: developer
triggers:
  - knowledge base
  - KB maintenance
  - 知识库
  - 文档维护
  - docs maintenance
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Knowledge Base Maintenance Skill

## Core Rule

A knowledge base is only useful if users can find current, trusted answers. Maintenance must reduce duplication, stale content, and unclear ownership.

## Execution Steps

1. **Audit current state**
   - Use `mcp_memory__memory_recall` to retrieve taxonomy, canonical docs, maintenance cadence, and known stale areas
   - Use `file_read` to inspect indexes, docs, tags, categories, search terms, and recent updates
   - Identify stale, duplicate, orphaned, conflicting, missing, and high-value documents

2. **Prioritize maintenance work**
   - Prioritize by reader impact, usage, risk, freshness, and business/operational importance
   - Identify quick fixes vs structural improvements
   - Define owner and review cadence for critical docs

3. **Curate and update**
   - Merge duplicates into canonical docs
   - Add links, tags, summaries, and related-doc sections
   - Mark deprecated or archived content clearly
   - Update indexes and navigation
   - Create missing stub pages only when owner and completion path are known

4. **Improve discoverability**
   - Add descriptive titles, summaries, keywords, tags, and cross-references
   - Ensure common reader questions have clear entry points
   - Use `file_write` to update docs or indexes when requested
   - Use `mcp_memory__memory_store` to persist taxonomy, canonical decisions, and maintenance rules

## Output Format

```markdown
# Knowledge Base Maintenance Report: [Scope]

## Audit Summary
| Category | Count | Notes |
|----------|-------|-------|
| Stale docs | ... | ... |
| Duplicates | ... | ... |
| Orphans | ... | ... |

## Maintenance Actions
| Document | Issue | Action | Owner | Priority |
|----------|-------|--------|-------|----------|
| ... | Stale | Update | ... | High |

## Canonical Source Updates
| Topic | Canonical Doc | Related Docs | Action |
|-------|---------------|--------------|--------|
| ... | ... | ... | ... |

## Discoverability Improvements
- [Index/tag/link/title update]

## Review Cadence
| Document/Area | Owner | Cadence | Next Review |
|---------------|-------|---------|-------------|
| ... | ... | ... | ... |
```

## Red Flags

Stop and revise if you see:

- Duplicate docs left without canonical decision
- Critical doc without owner
- Stale warning missing for outdated content
- Index updated but orphan links remain
- Stub pages created with no completion owner
- Navigation organized by internal team structure instead of reader tasks
