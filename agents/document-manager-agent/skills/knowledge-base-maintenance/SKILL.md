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
  - doc_read
  - doc_write
  - doc_list
  - doc_search
  - doc_stat
  - doc_move
  - doc_remove
  - doc_list_trash
  - doc_restore
  - doc_list_requests
  - pm_add_activity
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Knowledge Base Maintenance Skill

## Core Rule

A knowledge base is only useful if users can find current, trusted answers. Maintenance must reduce duplication, stale content, and unclear ownership.

## System of Record

Maintenance is a sweep of the library plus tracked follow-up.

- Find candidates with `doc_search` / `doc_list` (and `doc_stat` for `updated_at`,
  `version`, creator) — stale and duplicated pages are visible from metadata.
- Merging or superseding: `doc_move` the superseded page to an archive directory and
  `doc_write` the pointer at the old path, or `doc_remove` it into the trash if it is
  genuinely dead (recoverable via `doc_list_trash` / `doc_restore`).
- Every recurring maintenance job needs a `start_at` — a schedule that lives only in your head does not exist. Filing it is the PM's call: report it with `pm_add_activity` and let the PM raise the card.
- `doc_list_requests` before editing: your own pending changes may already cover it.

## Execution Steps

1. **Audit current state**
   - Use `mcp_memory__memory_recall` to retrieve taxonomy, canonical docs, maintenance cadence, and known stale areas
   - Use `doc_read` to inspect indexes, docs, tags, categories, search terms, and recent updates
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
   - Use `doc_write` to update docs or indexes when requested
   - Use `mcp_memory__memory_store` to persist taxonomy, canonical decisions, and maintenance rules

## Output

One file, published once into `{doc_path}/the canonical page for the artifact (one page per task)` — this is the task's deliverable, not a record of
doing it. Outline it: headings and bullets, ≤ ~80 lines unless the material is long-form.
See `rules.md` → *Document output*.

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
