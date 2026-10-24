---
name: meeting-notes
description: Capture structured meeting notes with purpose, decisions, dissent, action items, owners, due dates, confirmations, and follow-up tracking
version: "1.1.0"
author: developer
triggers:
  - meeting
  - 会议纪要
  - meeting notes
  - meeting minutes
  - take notes
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_store
  - mcp_memory__memory_recall
---

# Meeting Notes Skill

## Core Rule

The value of meeting notes is decisions and follow-through, not transcription.

Every decision needs rationale. Every action item needs one owner, due date, and success condition.

## Execution Steps

1. **Prepare context**
   - Use `mcp_memory__memory_recall` to retrieve project context, prior decisions, open action items, stakeholder preferences, and unresolved questions
   - Use `file_read` to inspect agenda, prior notes, status report, risk register, or relevant project docs
   - Document meeting purpose, desired outcomes, attendees, decision-makers, and absent stakeholders
   - Bring forward unresolved action items from prior meetings

2. **Capture structured discussion**
   - Organize notes by agenda item
   - Separate facts, opinions, decisions, risks, questions, and action items
   - Capture disagreement or dissent when it affects decisions or risk
   - Record enough context for absent stakeholders to understand outcomes
   - Avoid full transcript unless explicitly requested

3. **Confirm decisions in the meeting**
   - For each decision, record what was decided, why, who decided, alternatives considered, and conditions
   - Mark provisional decisions that require absent stakeholder approval
   - Mark deferred decisions with owner and due date
   - Confirm important decisions before the meeting ends

4. **Create actionable follow-up**
   - For each action item, define:
     - What must be done
     - One accountable owner
     - Due date
     - Success condition
     - Dependencies or context
   - Do not assign action items to "the team"
   - Confirm owners and due dates before meeting ends

5. **Distribute and persist**
   - Use `file_write` to save formatted notes when requested
   - Put decisions and action items near the top
   - Distribute notes within 24 hours
   - Use `mcp_memory__memory_store` to persist decisions, action items, owner commitments, and follow-up dates

## Output Format

```markdown
# Meeting Notes: [Meeting Title]

**Date**: [YYYY-MM-DD]
**Time**: [Start] — [End]
**Purpose**: [Why this meeting happened]
**Desired Outcome**: [Decision/alignment/output expected]
**Attendees**: [List with roles]
**Absent Stakeholders**: [List and impact]

## Summary
[3-5 bullets with outcomes]

## Decisions
| ID | Decision | Rationale | Decider(s) | Alternatives Considered | Conditions | Status |
|----|----------|-----------|------------|-------------------------|------------|--------|
| D-001 | [Decision] | [Why] | [Who] | [A/B/C] | [Condition] | Confirmed/Provisional |

## Action Items
| ID | Action | Owner | Due Date | Success Condition | Dependencies | Status |
|----|--------|-------|----------|-------------------|--------------|--------|
| AI-001 | [Task] | [One owner] | [Date] | [Done means] | [Dependency] | Open |

## Discussion Notes
### [Agenda Item]
- **Facts**: [facts]
- **Opinions/Concerns**: [opinions]
- **Open Questions**: [questions]

## Deferred Items
| Item | Owner | Needed By | Next Step |
|------|-------|-----------|-----------|
| ... | ... | ... | ... |

## Next Meeting / Follow-up
- [Date or trigger]
```

## Red Flags

Stop and clarify if you see:

- Decision without rationale
- Action item without one owner
- Due date written as "soon" or "ASAP"
- Important disagreement omitted
- Absent decision-maker needed but decision marked final
- Notes that are a transcript but do not show outcomes
- Action item success condition unclear
