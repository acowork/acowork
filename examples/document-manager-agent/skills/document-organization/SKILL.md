---
name: document-organization
description: Organize documents into clear information architecture, categories, indexes, tags, navigation, canonical sources, and cross-reference maps
version: "1.0.0"
author: developer
triggers:
  - organize docs
  - document organization
  - 信息架构
  - 整理文档
  - docs IA
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Document Organization Skill

## Core Rule

Organize documents around how readers find and use information, not around how files happened to be created.

## Execution Steps

1. **Load inventory and conventions**
   - Use `mcp_memory__memory_recall` to retrieve taxonomy, naming conventions, canonical sources, and archive rules
   - Use `file_read` to inspect existing docs tree, README files, indexes, summaries, and document inventories
   - Identify current navigation, duplicate areas, stale docs, orphan docs, and missing entry points

2. **Define information architecture**
   - Group documents by audience, task, lifecycle, domain, and retrieval pattern
   - Define categories, tags, ownership, and document status labels
   - Identify canonical source for each major topic
   - Define what should be indexed, archived, merged, split, renamed, or linked

3. **Create navigation and indexes**
   - Build table of contents, category indexes, topic maps, and cross-reference links
   - Prefer links to canonical docs over duplicated content
   - Make document status and freshness visible

4. **Plan reorganization safely**
   - Preserve existing links or document redirects/compatibility notes when paths change
   - Avoid large unexplained moves without mapping old path to new path
   - Use `file_write` to save organization plan or indexes when requested
   - Use `mcp_memory__memory_store` to persist taxonomy and canonical-source decisions

## Output Format

```markdown
# Document Organization Plan: [Scope]

## Current State
- **Entry points**: [list]
- **Issues**: [duplicates/stale/orphans/navigation gaps]

## Proposed Information Architecture
| Category | Purpose | Audience | Canonical Entry | Owner |
|----------|---------|----------|-----------------|-------|
| ... | ... | ... | ... | ... |

## Document Actions
| Document | Current Location | Action | Target Location | Reason |
|----------|------------------|--------|-----------------|--------|
| ... | ... | Keep/Merge/Split/Archive/Rename | ... | ... |

## Canonical Source Map
| Topic | Canonical Doc | Related Docs | Notes |
|-------|---------------|--------------|-------|
| ... | ... | ... | ... |

## Navigation Updates
- [Index/table of contents changes]

## Link Preservation
| Old Path | New Path | Compatibility Action |
|----------|----------|----------------------|
| ... | ... | ... |
```

## Red Flags

Stop and revise if you see:

- New taxonomy not aligned to reader tasks
- Multiple canonical docs for same topic
- Renames or moves without link preservation plan
- Important docs without owner or status
- Index that lists everything but helps nobody find anything
