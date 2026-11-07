---
name: documentation
description: Write clear, accurate technical documentation including API docs, guides, and architecture records
version: "1.0.0"
author: developer
triggers:
  - document
  - doc
  - 写文档
  - write docs
  - README
  - guide
tool_deps:
  - file_read
  - file_write
  - mcp_memory__memory_recall
---

# Documentation Skill

## Execution Steps

1. **Understand what needs documentation**
   - Use `file_read` to examine the code or system that needs documentation
   - Use `mcp_memory__memory_recall` to retrieve any prior documentation decisions, style guides, or templates
   - Determine the documentation type:
     - **API Reference**: Auto-generated from code annotations (rustdoc, JSDoc, docstrings)
     - **Architecture Decision Record (ADR)**: Why a particular technical choice was made
     - **How-to Guide**: Step-by-step instructions for a specific task
     - **Tutorial**: Learning-oriented walkthrough for newcomers
     - **README**: Project overview and quickstart
     - **Changelog**: Chronological record of notable changes
   - Identify the target audience: developers, operators, end users, or management

2. **Determine documentation scope and structure**
   - Define the scope: what is in scope and what is explicitly out of scope
   - Outline the document structure with section headings
   - Identify dependencies: what existing documents does this reference?
   - Determine the required depth: overview vs. exhaustive detail
   - Plan examples and code snippets — these are often more valuable than prose

3. **Write the content**
   - Use `file_write` to create or update the document
   - Follow the Diátaxis framework when applicable:
     - *Tutorial*: Learning-oriented, safely guides the reader
     - *How-to Guide*: Problem-oriented, helps the reader accomplish a goal
     - *Reference*: Information-oriented, describes the machinery
     - *Explanation*: Understanding-oriented, clarifies and illuminates
   - Write in clear, concise language — avoid jargon unless the audience expects it
   - Use active voice: "The function returns" not "It is returned by the function"

4. **Add examples and code snippets**
   - Every non-trivial concept should have a working code example
   - Examples should be self-contained and runnable (copy-paste friendly)
   - Include both the simplest case and a realistic case
   - Show expected output alongside code examples
   - Mark examples that require external setup (e.g., "Requires a running Redis server")

5. **Review and publish**
   - Proofread for clarity, accuracy, and consistency
   - Verify that all code examples actually work — run them if possible
   - Check that cross-references and links are valid
   - Ensure the document follows the project's documentation style
   - Use `file_write` to save the final version

## Output Format

Documentation types follow their own formats. Common elements:

```markdown
# [Title]

> Brief one-line description of what this document covers.

## Overview
[2-3 sentence summary]

## [Body sections per documentation type]

## Examples
[Working code examples with expected output]

## See Also
- [Links to related documentation]
```

For ADRs specifically:
```markdown
# ADR-[NNNN]: [Title]

## Status
[Proposed | Accepted | Deprecated | Superseded by ADR-XXXX]

## Context
[What is the issue that we're seeing that is motivating this decision?]

## Decision
[What is the change that we're proposing?]

## Consequences
[What becomes easier or harder to do because of this change?]
```

## Notes

- Documentation should answer three questions: What? Why? How?
- Code examples > prose explanations — show, don't tell
- When documenting APIs, include: parameters, return values, error conditions, and at least one example
- Keep documentation close to the code it describes — colocated docs stay up to date
- Use `mcp_memory__memory_store` to save documentation patterns and style decisions for consistency
