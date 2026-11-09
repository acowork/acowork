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
  - doc_read
  - doc_write
  - doc_mkdir
  - doc_move
  - doc_rmdir
  - doc_stat
  - doc_list
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Document Organization Skill

## Core Rule

Organize documents around how readers find and use information, not around how files happened to be created.

## System of Record

The library structure is the file system of the doc service — restructure it there.

- `doc_mkdir` creates a directory (one level at a time; the parent must exist).
- `doc_move` moves and renames **documents** (paths must end in `.md`) and can publish
  atomically by writing to a temp name first. It does not move directories: to
  restructure one, move its documents out, `doc_rmdir` it, and `doc_mkdir` the
  replacement.
- `doc_rmdir` removes an **empty** directory only and never cascades — if it refuses,
  the directory still holds documents or sub-directories; empty it deliberately.
- Do not "reorganize" by rewriting bodies: a moved document keeps its id and history,
  a rewritten one loses both.
- Publish the resulting map with `doc_write` so humans can see the target structure.

## Execution Steps

1. **Load inventory and conventions**
   - Use `mcp_memory__memory_recall` to retrieve taxonomy, naming conventions, canonical sources, and archive rules
   - Use `doc_read` to inspect existing docs tree, README files, indexes, summaries, and document inventories
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
   - Use `doc_write` to save organization plan or indexes when requested
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
