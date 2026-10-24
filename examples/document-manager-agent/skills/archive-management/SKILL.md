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
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Archive Management Skill

## Core Rule

Archiving should preserve retrievability and context. Do not simply move files without metadata, reason, status, and replacement path when applicable.

## Execution Steps

1. **Identify archive candidates**
   - Use `mcp_memory__memory_recall` to retrieve archive rules, document lifecycle policies, and canonical sources
   - Use `file_read` to inspect document inventory, indexes, stale docs, superseded docs, and duplicates
   - Determine why each document should be archived: obsolete, superseded, historical snapshot, duplicate, completed project, or compliance retention

2. **Assess archive impact**
   - Identify inbound links, references, owners, active users, and replacement docs
   - Decide whether to archive, deprecate, merge, delete, or keep current
   - Do not archive active canonical docs without explicit replacement

3. **Prepare archive metadata**
   - Record title, source path, archive path, owner, date, version, status, reason, replacement, retention period, and retrieval keywords
   - Add deprecation or archive notice when document remains discoverable

4. **Update indexes and references**
   - Use `file_write` to create archive manifest, notices, or updated indexes when requested
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
