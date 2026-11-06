# ADR-108: Consolidating the Right Rail — the Tools Panel Retires, Its Cards Move into Setup

- **Number**: ADR-108
- **Status**: Implemented (2026-11-06)
- **Date**: 2026-11-06
- **Decided by**: Nicholas (plan confirmed)
- **Related**: ADR-029 (builtin-tool persistence and enablement), ADR-034 §7.6.5 (the merged tools endpoint), ADR-048 (DevMode debugging), ADR-063 §3.7 (editing the package's `prompts/` / `skills/`), ADR-069 (MCP tool-level opt-in), ADR-090 (MCP install ownership and install-status UI), ADR-094 (the `load_skill` builtin tool), ADR-101 (memory service and the memory-panel gate), ADR-104 (recommended-MCP declaration and the recommendation group)
- **Chinese source of truth**: [docs/adr/zh/ADR-108-right-panel-consolidation.md](../zh/ADR-108-right-panel-consolidation.md) (maintained in the same commit)

---

## Summary

**In one sentence**: the right rail shrinks from six tabs to **five** (Workspace / Status / Memory / Debug / Setup). The Tools tab is retired: its two cards — builtin tools, MCP servers (including the recommendation group) — move into the Setup tab, and the Debug tab's Prompts & Skills card moves there as well. The Debug tab keeps only the debug switch (with the iteration / phase / token readout), the context snapshots and the compression history. Setup therefore grows from four cards to seven, so **only the first three start expanded** and the rest start collapsed.

**In one sentence (why)**: a tab only earns its place if it matches the **ownership of the data** behind it. The two cards in the Tools tab write **this agent's own configuration** (`agent_tools.json` / `agent_mcp.json`) — the same object, the same permission gate and the same write path as the cards in Setup that write `agent_config.json`. Splitting them across two tabs forces the user to remember "which switch lives in which tab", and the visual grouping that memory cost buys does not exist — there were only two cards in that tab. The same holds for Prompts & Skills (ADR-063 §3.7): it edits files inside the package, has nothing to do with debugging, and sat at the top of the Debug tab only because that tab used to be empty.

---

## Background (code evidence)

1. **The Tools tab was nothing but two cards.** `ToolsTab` rendered the "Builtin Tools" collapsible card, the "MCP Servers" collapsible card, and the recommendation group between them (ADR-104: installing a recommendation moves a row *into* the switch list below, so cause and effect must share the screen). No third block, no tab-level toolbar.
2. **The Setup tab already spoke the same card grammar.** Both tool cards and Setup's four cards are `ListBox` + `ExpandableRow` level-1 collapsible cards — same `min-h-[36px]` header, same hairline separation. ADR-094's description of where `load_skill` lives ("Tools panel → Builtin Tools list") names a card from the same family as the ones in Setup.
3. **One configuration had already been split once.** ADR-094 removed the Setup panel's `skillsAutoLoad` switch and made the `load_skill` row in the Tools panel the only control for skill mode. "This agent's configuration" has been divided across two tabs ever since, and comments had to keep explaining where the other half lives — this ADR erases that class of cross-panel pointer.
4. **The Prompts & Skills card never depended on debugging.** ADR-063 §3.7 requires it to be browsable and pre-loadable regardless of DevMode and regardless of whether the agent is running; placing it at the top of the Debug tab was a matter of convenience, and it pushed the tab's own content (DevMode branches, snapshots, compression history) down.
5. **"Everything expanded" was a property of four cards.** Four cards open is a one-screen overview; seven open is a scroll wall where everything past the first card is off-screen.

---

## Decisions

### D1 — The rail has five tabs, and `tools` disappears at the type level

`PanelTab` narrows to `workspace | status | memory | debug | setup`. Consequently:

- the panel registry (icon, order, visibility) loses its tools entry, and `RightNavBar` no longer needs its generic "lucide icon slot" — tools was its only user; every other tab paints an Outline/Filled masked icon pair;
- `AppLayout`'s agent-stop fall-back no longer lists `tools`;
- the `rightPanel.tools` locale key is deleted (en / zh-CN).

No hidden Tools tab is kept: a value that can still become `activePanelTab` without an entry point would only let "the rail and the panel disagree" grow back.

### D2 — Cards belong to whoever owns the data they write; order comes from the registry

Builtin tools, MCP servers (with the recommendation group) and Prompts & Skills join **Setup**, alongside Agent Info / Session Params / Model Params / Chat Router.

Card order is the order of `AGENT_SETUP_CARDS` ([agentSetupCards.ts](../../../apps/acowork-desktop/src/components/right-panel/agentSetupCards.ts)), and **that table is the single source of render order**:

`agentInfo → sessionParams → modelParams → chatRouter → promptsSkills → builtinTools → mcpServers`

The three migrated cards sit after the original four so existing muscle memory is not disturbed.

### D3 — Only the first three start expanded; the rule is defined once and not persisted

`DEFAULT_EXPANDED_CARD_COUNT = 3`, and `defaultOpenForCard(id)` derives the initial value from a card's **position** in the registry. The panel drives every `ExpandableRow` from one `Record<cardId, boolean>`; the migrated cards receive their initial value through a `defaultOpen` prop.

Two deliberate trade-offs:

- **The rule lives in the registry, not in the cards.** Seven scattered `useState(true)` calls look equivalent, but inserting a card then silently changes what "the first three" means, and nothing fails.
- **No persistence.** Expand state stays component-local, exactly as before the merge: switching tabs unmounts the panel and the rule re-seeds it. Adding a storage layer for seven booleans would demote the one rule we have ("the first three expand") into "whatever the user touched last".

### D4 — The host owns scrolling; the cards own none

The migrated cards no longer carry their own scroll root, tab padding or "no agent selected" empty state — the Setup panel provides all three (a single `OverlayScrollbar` column plus its own empty state). Hence:

- **The builtin-tools list loses its nine-row height cap.** That `maxHeight` was an artefact of the card owning a whole tab (it had to bound itself or the snapshots / compression history would be pushed off-screen). Inside Setup it is one block of a single scrolling column, and a nested scroller only hides rows behind a **second** scroll position — one extra scroll action, one fewer everything-at-a-glance.
- Inter-card dividers reuse the host's `-mx-3 my-2 border-t` hairline so the three migrated cards read as one stretch of that column rather than as foreign containers.

### D5 — The Debug tab narrows to debugging; it no longer hosts configuration editing

The Debug tab keeps: the debug switch block (Enter/Exit Debug + session controls + the iteration / phase / token / session-state readout), the context snapshots card, and the compression history card.

**ADR-063 §3.7's "top of the Debug tab" placement no longer holds** — the card's content, editing interaction and reload semantics are unchanged; only its host moved. The Debug tab exists to *observe one run*; it should not simultaneously be the place where you edit the prompts the next run will use, whose write target is a package file and has nothing to do with runtime state.

### D6 — Invariants

- **Endpoints / stores / permissions / write paths are untouched**: `GET/PUT /tools`, `/mcp-tools`, `/prompts/reload`, the `agent_mcp.json` enablement path and ADR-087's read-only verdict all stay as they were. Only the render host and the entry point changed.
- **ADR-104's causal adjacency is unchanged**: the recommendation group still sits directly above the MCP switch list, inside the same card cell.
- **ADR-029 / ADR-094 switch semantics are unchanged**: a builtin row's switch is still `PUT /agents/{id}/builtin-tools`; `load_skill` is still the only skill-mode control.
- **ADR-101's memory gate is unchanged**: the Memory tab still needs "agent running + memory service ready".

---

## Consequences

- **Users**: one fewer tab to search (six → five); Setup's first screen shows three expanded cards with the rest visible by title, one click away.
- **Code**: `ToolsTab.tsx` → `ToolsCards.tsx` (no panel chrome, no empty state); `PromptsSkillsCard`'s initial expand state is injected by its host (`defaultOpen`); `RightNavBar`'s generic icon slot is gone.
- **Tests**: `agentSetupCards` (the rule itself), `AgentSetupTab.cards` (seven cards in order, first three expanded, builtin list without an inner scroll host).
- **Docs**: the PRD §4.4 tab table is updated and a new §4.4.3 documents Setup's seven cards; the "…in the Tools panel" / "…at the top of the Debug tab" location statements in ADR-029 / ADR-034 §7.6.5 / ADR-063 §3.7 / ADR-069 / ADR-090 / ADR-094 / ADR-104 carry a pointer to this ADR without rewriting their historical conclusions.

---

## Not doing

- **No user-defined card order and no persistence** (a corollary of D3).
- **No sub-tabs inside Setup**: that would swap "six panels" for "one panel with six sub-panels" — the same burden plus a level.
- **No capability is removed**: the state readout and the compression history stay in the Debug tab; nothing was cut on the grounds that the panel should be shorter.
- **No change to the recommendation group's position**: inserting it between `promptsSkills` and `builtinTools` would break ADR-104's "install it, then watch it join the list below".

---

## Verification

- `apps/acowork-desktop/src/components/right-panel/agentSetupCards.test.ts` — registry order, first three expanded, the rest collapsed, state map consistent with the rule.
- `apps/acowork-desktop/src/components/right-panel/AgentSetupTab.cards.test.tsx` — really renders seven cards (order + `aria-expanded` true for the first three), exactly one Prompts & Skills card, and all twelve builtin tools rendered with the rows resolving to the **same** panel scroll viewport as the card header (re-wrapping the list in an `OverlayScrollbar` makes that case fail).
- `npx tsc --noEmit`, `npm run check:i18n`, and the relevant `apps/acowork-desktop` vitest suites are green.
