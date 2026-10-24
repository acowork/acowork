---
name: document-collection
description: Collect source documents, references, meeting notes, specs, links, and artifacts with provenance, scope, freshness, and usage constraints
version: "1.0.0"
author: developer
triggers:
  - collect docs
  - document collection
  - 搜集文档
  - 收集资料
  - gather documents
tool_deps:
  - file_read
  - file_write
  - network
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Document Collection Skill

## Core Rule

Do not mix source material with interpretation. Every collected item needs provenance, scope, owner if known, and confidence level.

## Execution Steps

1. **Define collection scope**
   - Use `mcp_memory__memory_recall` to retrieve existing document inventories, canonical sources, taxonomy, and archive rules
   - Clarify purpose, audience, topic boundary, output format, source types, time range, and freshness needs
   - Ask one focused question when scope is ambiguous

2. **Collect source materials**
   - Use `file_read` to inspect local documents, notes, specs, plans, meeting records, exports, and indexes
   - Use `network` only for provided or approved links and references
   - Capture files, links, titles, authors, dates, versions, owners, and related topics
   - Do not collect secrets, credentials, or private personal data into docs

3. **Classify and tag**
   - Classify each item by document type, audience, lifecycle status, topic, owner, and freshness
   - Mark items as source, draft, decision, reference, meeting note, artifact, derived summary, or archive candidate
   - Identify duplicates and possible canonical sources

4. **Create inventory**
   - Use `file_write` to save inventory when requested
   - Include provenance and open questions
   - Use `mcp_memory__memory_store` to persist inventory location, canonical-source candidates, and collection rules

## Output Format

```markdown
# Document Collection Inventory: [Topic]

## Scope
- **Purpose**: [why collecting]
- **Audience**: [who will use it]
- **Included**: [scope]
- **Excluded**: [non-scope]

## Inventory
| ID | Title | Source/Path | Type | Owner | Date/Version | Status | Tags | Confidence | Notes |
|----|-------|-------------|------|-------|--------------|--------|------|------------|-------|
| DOC-001 | [title] | [path/link] | PRD | [owner] | [date] | Current | [tags] | High | ... |

## Canonical Source Candidates
| Topic | Candidate | Reason | Verification Needed |
|-------|-----------|--------|---------------------|
| ... | ... | ... | ... |

## Duplicates / Conflicts
| Item A | Item B | Conflict | Recommended Action |
|--------|--------|----------|--------------------|
| ... | ... | ... | ... |

## Open Questions
- [Question]
```

## Red Flags

Stop and clarify if you see:

- Source without provenance
- Conflicting docs with no canonical owner
- Private or sensitive data mixed into general docs
- Collection scope too broad to be useful
- Document freshness unknown for critical content
