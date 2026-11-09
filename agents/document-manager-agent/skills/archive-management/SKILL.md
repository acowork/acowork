---
name: archive-management
description: Archive documents with metadata, lifecycle status, retention rules, versioning, provenance, retrieval paths, and deprecation notes
version: "1.0.0"
author: developer
triggers:
  - archive docs
  - document archive
  - 归档
  - 文档归档
  - retention
tool_deps:
  - doc_read
  - doc_write
  - doc_move
  - doc_remove
  - doc_list_trash
  - doc_restore
  - doc_rmdir
  - pm_create_task
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Archive Management Skill

## Core Rule

Archiving should preserve retrievability and context. Do not simply move files without metadata, reason, status, and replacement path when applicable.

## System of Record

Archiving is a move with a rule, not a delete with a hope.

- `doc_move` the retired document into the archive directory; it keeps its id,
  version and history. Say in the body (or in the index page) *why* it was retired and
  what replaced it.
- Only genuinely valueless content goes to `doc_remove` (trash). `doc_list_trash`
  and `doc_restore` are the undo; there is no purge, so do not promise permanence.
- Directory pruning after an archive sweep: `doc_rmdir` empties bottom-up, refuses a
  non-empty directory, and never cascades.
- Retention follow-up is a `pm_create_task` with a due date, not a note.

## Execution Steps

1. **Identify archive candidates**
   - Use `mcp_memory__memory_recall` to retrieve archive rules, document lifecycle policies, and canonical sources
   - Use `doc_read` to inspect document inventory, indexes, stale docs, superseded docs, and duplicates
   - Determine why each document should be archived: obsolete, superseded, historical snapshot, duplicate, completed project, or compliance retention

2. **Assess archive impact**
   - Identify inbound links, references, owners, active users, and replacement docs
   - Decide whether to archive, deprecate, merge, delete, or keep current
   - Do not archive active canonical docs without explicit replacement

3. **Prepare archive metadata**
   - Record title, source path, archive path, owner, date, version, status, reason, replacement, retention period, and retrieval keywords
   - Add deprecation or archive notice when document remains discoverable

4. **Update indexes and references**
   - Use `doc_write` to create archive manifest, notices, or updated indexes when requested
   - Preserve retrieval path and cross-links
   - Use `mcp_memory__memory_store` to persist archive decisions and locations

## Output Format

```markdown
# Archive Plan: [Scope]

## Archive Candidates
| Document | Current Path | Action | Reason | Replacement | Owner | Retention |
|----------|--------------|--------|--------|-------------|-------|-----------|
| ... | ... | Archive/Deprecate/Merge/Delete | ... | ... | ... | ... |

## Link Impact
| Document | Inbound References | Action |
|----------|--------------------|--------|
| ... | ... | ... |

## Archive Manifest
| Archived Doc | Archive Path | Status | Archived Date | Retrieval Keywords |
|--------------|--------------|--------|---------------|--------------------|
| ... | ... | Archived | YYYY-MM-DD | ... |

## Required Updates
- [Index or link update]
```

## Red Flags

Stop and clarify if you see:

- Archiving canonical doc without replacement
- No archive reason
- No retrieval path
- Broken inbound links
- Unknown retention requirement
- Deleting instead of archiving when history may matter
