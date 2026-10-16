# Architecture Decision Records (English)

> The English parallel set of the ACowork ADRs. The **Chinese version is the source of truth**;
> these files are translations maintained alongside it. When the two disagree, the Chinese
> document governs.

**Coverage**: 86 / 86 documents translated.

Numbering notes: `037` is unused; `061` and `062` each have two files (a decision and its
companion report), so the count exceeds the number of unique ADR numbers.

## Reading order

ADRs are not a sequence to be read front to back. Suggested entry points:

| If you want to understand... | Read |
|---|---|
| What the platform is and where the boundaries are | [ADR-009](./ADR-009-gateway-workspace-isolation.md), [ADR-055](./ADR-055-remote-runtime-node-topology.md), [ADR-033](./ADR-033-mqtt-replace-grpc-websocket.md) |
| How the control plane and the data plane are split | [ADR-034](./ADR-034-mqtt-http-boundary.md) |
| How context is compressed and summarised | [ADR-010](./ADR-010-context-compression-simplification.md) → [ADR-011](./ADR-011-compaction-as-distillation.md) → [ADR-032](./ADR-032-context-recall.md) |
| How sessions are stored, filtered and rendered | [ADR-024](./ADR-024-merge-metadata-into-index.md), [ADR-021](./ADR-021-unified-session-data-loading.md), [ADR-050](./ADR-050-chat-list-data-driven-refactor.md) |
| Who may see what and do what | [ADR-042](./ADR-042-mqtt-user-identity-delivery.md), [ADR-073](./ADR-073-agent-instance-identity-decomposition.md), [ADR-076](./ADR-076-multi-user-account-system.md), [ADR-084](./ADR-084-user-standalone-process.md) |
| The agent lifecycle contract | [ADR-085](./ADR-085-agent-lifecycle-state-machine.md) |

## Index

| # | ADR | Title | Status | 中文 |
|---|---|---|---|---|
| 1 | [ADR-009](./ADR-009-gateway-workspace-isolation.md) | Gateway Workspace Isolation | Accepted | [zh](../zh/ADR-009-gateway-workspace-isolation.md) |
| 2 | [ADR-010](./ADR-010-context-compression-simplification.md) | Major Simplification of the Context Compaction Strategy | Accepted (Phase 1 and 2 complete: programmatic folding removed, LLM c… | [zh](../zh/ADR-010-context-compression-simplification.md) |
| 3 | [ADR-011](./ADR-011-compaction-as-distillation.md) | Unified Strategy for Context Summarization and Distillation | Proposed | [zh](../zh/ADR-011-compaction-as-distillation.md) |
| 4 | [ADR-012](./ADR-012-per-session-model-isolation.md) | Per-Session Model/Provider Isolation | Proposed | [zh](../zh/ADR-012-per-session-model-isolation.md) |
| 5 | [ADR-013](./ADR-013-debug-observer-pipeline.md) | Refactoring the Debug Module Boundary — The Observer Pipeline Pattern | Proposed | [zh](../zh/ADR-013-debug-observer-pipeline.md) |
| 6 | [ADR-014](./ADR-014-loop-module-decomposition.md) | Decomposing the AgentLoop Main Loop — From God Object to Responsibility Modules | Implemented (8/8 phases complete) | [zh](../zh/ADR-014-loop-module-decomposition.md) |
| 7 | [ADR-015](./ADR-015-agent-startup-sequencing.md) | Agent Startup Sequencing Refactor — From Async Race to Phased Readiness | Draft (pending implementation) | [zh](../zh/ADR-015-agent-startup-sequencing.md) |
| 8 | [ADR-016](./ADR-016-centralized-exception-handling.md) | Centralized Exception Handling — Classification to Core, Orchestration to Reliable, Presentation to the Frontend | Draft (pending implementation) | [zh](../zh/ADR-016-centralized-exception-handling.md) |
| 9 | [ADR-017](./ADR-017-agent-avatar-runtime-config.md) | Agent Avatar Runtime Configuration — manifest as Install Default, agent_config.json as Mutable Runtime Config | Draft (pending implementation) | [zh](../zh/ADR-017-agent-avatar-runtime-config.md) |
| 10 | [ADR-018](./ADR-018-gateway-disconnection-self-exit.md) | Runtime and Embed Self-Exit After a Gateway Disconnection Timeout | Proposed | [zh](../zh/ADR-018-gateway-disconnection-self-exit.md) |
| 11 | [ADR-019](./ADR-019-lsp-relay-standalone-process.md) | Decoupling LSP Relay from the Gateway into a Standalone Process | Draft (pending decision) | [zh](../zh/ADR-019-lsp-relay-standalone-process.md) |
| 12 | [ADR-020](./ADR-020-data-flow-tiering.md) | End-to-End Data Flow Tiering — Solving LLM Streaming Blocking File I/O and Other Control Channels | P0 implemented | [zh](../zh/ADR-020-data-flow-tiering.md) |
| 13 | [ADR-021](./ADR-021-unified-session-data-loading.md) | Unified Session Data Loading — Dropping Streaming Transport in Favour of HTTP Pull + Notifications | Draft | [zh](../zh/ADR-021-unified-session-data-loading.md) |
| 14 | [ADR-022](./ADR-022-streaming-flush-on-role-change.md) | Flush on Streaming Role Change — Making JSONL a Faithful Real-Time Record | Adopted / awaiting implementation confirmation | [zh](../zh/ADR-022-streaming-flush-on-role-change.md) |
| 15 | [ADR-023](./ADR-023-centralized-timeout-config.md) | Unified Timeout Configuration — A Single Source of Truth Across Crates | Draft (pending decision) | [zh](../zh/ADR-023-centralized-timeout-config.md) |
| 16 | [ADR-024](./ADR-024-merge-metadata-into-index.md) | Merging Session Metadata into the Index, Removing the Conversation File Header | Draft | [zh](../zh/ADR-024-merge-metadata-into-index.md) |
| 17 | [ADR-025](./ADR-025-temperature-resolution-chain.md) | Layered Temperature Resolution Chain and Observability | Proposed | [zh](../zh/ADR-025-temperature-resolution-chain.md) |
| 18 | [ADR-026](./ADR-026-context-window-resolution-chain.md) | Context Window Resolution Chain (per-agent context window cap) | Accepted (revised by ADR-074 on 2026-09-15; where the two conflict, A… | [zh](../zh/ADR-026-context-window-resolution-chain.md) |
| 19 | [ADR-027](./ADR-027-conversation-meta-token-usage.md) | Cumulative Token Consumption Statistics in Conversation Meta | Draft | [zh](../zh/ADR-027-conversation-meta-token-usage.md) |
| 20 | [ADR-028](./ADR-028-agent-core-token-usage-cache.md) | Process-Level Accumulated Token Usage Cache in AgentCore | In progress | [zh](../zh/ADR-028-agent-core-token-usage-cache.md) |
| 21 | [ADR-029](./ADR-029-agent-tools-persistence-and-toggle.md) | Builtin Tools Persistence and Enable Control — agent_tools.json | Draft (pending decision) | [zh](../zh/ADR-029-agent-tools-persistence-and-toggle.md) |
| 22 | [ADR-030](./ADR-030-sidecar-endpoint-dynamic-push.md) | Dynamic Push of Sidecar Endpoints — Gateway → Runtime | Completed (C1 ✅ C2 ✅ C3 ✅ C4 ✅) | [zh](../zh/ADR-030-sidecar-endpoint-dynamic-push.md) |
| 23 | [ADR-031](./ADR-031-drop-legacy-ipc-consolidate-on-grpc.md) | Dropping the Legacy IPC Channel Remnants — Full Consolidation onto gRPC | Implemented | [zh](../zh/ADR-031-drop-legacy-ipc-consolidate-on-grpc.md) |
| 24 | [ADR-032](./ADR-032-context-recall.md) | Context ID-Based Compaction (Placeholder + On-Demand Recall) | Under revision (a recall → compress → recall infinite loop was fixed… | [zh](../zh/ADR-032-context-recall.md) |
| 25 | [ADR-033](./ADR-033-mqtt-replace-grpc-websocket.md) | Replace gRPC + WebSocket with MQTT — Unifying the Gateway Protocol Stack | Proposed | [zh](../zh/ADR-033-mqtt-replace-grpc-websocket.md) |
| 26 | [ADR-034](./ADR-034-mqtt-http-boundary.md) | Control Plane / Data Plane Layering — MQTT / HTTP Responsibility Boundary Specification | Draft v10.0 (Phase 2 ~ Phase 10 all complete, 2026-07-14) | [zh](../zh/ADR-034-mqtt-http-boundary.md) |
| 27 | [ADR-035](./ADR-035-mqtt-streaming-push-refactor.md) | Streaming Transport Refactor — MQTT Direct Data Push + Frontend per-Session Line Buffering, Deprecating HTTP Incrementa… | Draft | [zh](../zh/ADR-035-mqtt-streaming-push-refactor.md) |
| 28 | [ADR-036](./ADR-036-mqtt-status-push.md) | MQTT Connection State Pushed by the Backend, Frontend Only Consumes | Draft | [zh](../zh/ADR-036-mqtt-status-push.md) |
| 29 | [ADR-038](./ADR-038-session-lifecycle-explicit-model.md) | Explicit Session Lifecycle Model | Draft — implemented (Phases 1–3 complete) | [zh](../zh/ADR-038-session-lifecycle-explicit-model.md) |
| 30 | [ADR-039](./ADR-039-mqtt-client-lifecycle.md) | MQTT Client Lifecycle Framework | Implemented (Phase 1 ✅, Phase 2 ✅) | [zh](../zh/ADR-039-mqtt-client-lifecycle.md) |
| 31 | [ADR-040](./ADR-040-runtime-adapter-use-case-layer.md) | Runtime Adapter Consolidation — Introducing a UseCase Trait Layer and Clearing gRPC Dead Code | Draft (awaiting scope confirmation) | [zh](../zh/ADR-040-runtime-adapter-use-case-layer.md) |
| 32 | [ADR-041](./ADR-041-chat-list-adapter.md) | The Chat List Adapter Abstraction Layer — The Single Bridge from Data to Rendering | Draft | [zh](../zh/ADR-041-chat-list-adapter.md) |
| 33 | [ADR-042](./ADR-042-mqtt-user-identity-delivery.md) | User Identity Delivered over an MQTT Global Resource Topic | Draft | [zh](../zh/ADR-042-mqtt-user-identity-delivery.md) |
| 34 | [ADR-043](./ADR-043-session-config-state-split.md) | Splitting Session State into Config / State Themes | Draft | [zh](../zh/ADR-043-session-config-state-split.md) |
| 35 | [ADR-044](./ADR-044-cancellation-token.md) | Stop Signal Path Analysis and Cancellation Token Unification | Draft | [zh](../zh/ADR-044-cancellation-token.md) |
| 36 | [ADR-045](./ADR-045-tool-progress-and-cancel.md) | Tool Execution Progress Heartbeats and Single-Tool Cancellation | Implemented | [zh](../zh/ADR-045-tool-progress-and-cancel.md) |
| 37 | [ADR-046](./ADR-046-unified-attachment-entries.md) | Unified Attachment Entries (File Upload / Image Upload / Add to Chat) | Draft | [zh](../zh/ADR-046-unified-attachment-entries.md) |
| 38 | [ADR-047](./ADR-047-session-config-decouple-from-inference.md) | Decoupling Session Config Persistence from the LLM Inference Loop | Draft | [zh](../zh/ADR-047-session-config-decouple-from-inference.md) |
| 39 | [ADR-048](./ADR-048-debug-protocol-mqtt-http.md) | Migrating the Debug Protocol from WebSocket to MQTT Events + HTTP RPC | Proposal | [zh](../zh/ADR-048-debug-protocol-mqtt-http.md) |
| 40 | [ADR-049](./ADR-049-session-status-substates.md) | Session Status Refinement — from the Coarse-Grained Streaming to a 6-Variant Business State Machine | Proposed | [zh](../zh/ADR-049-session-status-substates.md) |
| 41 | [ADR-050](./ADR-050-chat-list-data-driven-refactor.md) | Data-Driven Chat List Refactor - Complete Decoupling of UI and Data | Draft | [zh](../zh/ADR-050-chat-list-data-driven-refactor.md) |
| 42 | [ADR-051](./ADR-051-runtime-memory-provider-decoupling.md) | Runtime Memory Provider Decoupling — the Runtime Only Cares About the Provider and Does Not Access Grafeo Directly | Settled | [zh](../zh/ADR-051-runtime-memory-provider-decoupling.md) |
| 43 | [ADR-052](./ADR-052-tool-compression-llm-autonomous.md) | Autonomous LLM Tool Compression — context_retrieve + context_abandon Replace Hardcoded Triggers | Decided, partially superseded | [zh](../zh/ADR-052-tool-compression-llm-autonomous.md) |
| 44 | [ADR-053](./ADR-053-agent-specific-compaction-prompt.md) | Agent-Level Compaction Prompt — prompts/summary.md Replaces the Unified COMPACTION_SYSTEM_PROMPT | Decided | [zh](../zh/ADR-053-agent-specific-compaction-prompt.md) |
| 45 | [ADR-054](./ADR-054-debug-context-snapshot-coverage.md) | Debug Context Snapshot Coverage Extension — Section Listing + Bringing messages / todo / request_params into the Snapsh… | Implemented (draft 2026-09-12 → all 4 steps completed the same day; i… | [zh](../zh/ADR-054-debug-context-snapshot-coverage.md) |
| 46 | [ADR-055](./ADR-055-remote-runtime-node-topology.md) | Remote Runtime Deployment - The Node Agent Topology | Accepted (Phase 1-5a implementation complete; Phase 5b outstanding) | [zh](../zh/ADR-055-remote-runtime-node-topology.md) |
| 47 | [ADR-056](./ADR-056-global-default-compact-model.md) | Global Default Compact Model (Cross-Provider Alternative + Three-Tier Fallback) | Settled | [zh](../zh/ADR-056-global-default-compact-model.md) |
| 48 | [ADR-057](./ADR-057-compaction-distillation-into-graph.md) | The Memory Module's Full Gap Landscape and the P0 Distillation Pipeline Design (Revised) | the P0 triples path is revoked (the P0 residue A2 has not started) \|… | [zh](../zh/ADR-057-compaction-distillation-into-graph.md) |
| 49 | [ADR-058](./ADR-058-workspace-fs-watcher-mqtt-event.md) | Workspace Filesystem Changes Pushed to Desktop over MQTT for Automatic Refresh | Proposed (revised per the architecture review, see the review report) | [zh](../zh/ADR-058-workspace-fs-watcher-mqtt-event.md) |
| 50 | [ADR-059](./ADR-059-parallel-onboarding-handshake.md) | First-Run Onboarding Uses a Parallelized Protocol Based on a Capability-Readiness Snapshot and Confirmation Handshakes | proposal | [zh](../zh/ADR-059-parallel-onboarding-handshake.md) |
| 51 | [ADR-060](./ADR-060-prompt-cache-friendly-context-block-reorg.md) | Prompt-Cache-Friendly Context Block Reorganization — Stable Prefix + Append at the Tail | Proposed | [zh](../zh/ADR-060-prompt-cache-friendly-context-block-reorg.md) |
| 52 | [ADR-061](./ADR-061-context-compression-byte-budget.md) | Context Compression Rework — a 5-Level Decreasing Strategy Replacing Round-Count Retention | v3 revision (2026-09-05; see §20 three-atom refactor + §6 the 5-level… | [zh](../zh/ADR-061-context-compression-byte-budget.md) |
| 53 | [ADR-061](./ADR-061-pm-storage-tree.md) | acowork-pm Storage Selection — Directory Tree + Physical Nesting as Authoritative + Zero Redundant Fields | Settled | [zh](../zh/ADR-061-pm-storage-tree.md) |
| 54 | [ADR-062](./ADR-062-memory-quality-benchmark-report.md) | M4: Retrieval Quality Before/After Benchmark Report | — | [zh](../zh/ADR-062-memory-quality-benchmark-report.md) |
| 55 | [ADR-062](./ADR-062-memory-quality-config-and-retrieval-gate.md) | Centralizing Memory Quality Parameters and the Retrieval Quality Gate (MemoryQualityConfig) | 已实施（2026-09） | [zh](../zh/ADR-062-memory-quality-config-and-retrieval-gate.md) |
| 56 | [ADR-063](./ADR-063-package-level-prompt-override.md) | Package-Level LLM Prompt Override Mechanism — Extending the prompts/ Special-Filename Convention | Decided | [zh](../zh/ADR-063-package-level-prompt-override.md) |
| 57 | [ADR-064](./ADR-064-pm-standalone-process.md) | Decoupling PM from the Gateway into a Standalone Process | Decided (2026-09-02, settled by the architecture review) | [zh](../zh/ADR-064-pm-standalone-process.md) |
| 58 | [ADR-065](./ADR-065-unify-mqtt-client-lifecycle.md) | Unifying the MQTT Client Lifecycle Across All Four Ends | Decided (2026-09-03), implemented | [zh](../zh/ADR-065-unify-mqtt-client-lifecycle.md) |
| 59 | [ADR-066](./ADR-066-llm-provider-cache-tokens.md) | Pass-Through and Cumulative Accounting of LLM Provider Cache Tokens | In progress | [zh](../zh/ADR-066-llm-provider-cache-tokens.md) |
| 60 | [ADR-067](./ADR-067-decouple-context-usage-from-devmode.md) | Decouple Context Usage Section Sizes from DevMode | Implemented | [zh](../zh/ADR-067-decouple-context-usage-from-devmode.md) |
| 61 | [ADR-068](./ADR-068-memory-layer-promotion-two-axis-orthogonal.md) | Orthogonalizing the Two Memory Axes and Refactoring the Offline Distiller (Episodic-as-Source-of-Truth) | Implemented (fixed and completed after the review #32 in 2026-09); in… | [zh](../zh/ADR-068-memory-layer-promotion-two-axis-orthogonal.md) |
| 62 | [ADR-069](./ADR-069-mcp-tool-level-optin.md) | Per-Tool Opt-In for MCP Tools — agent_mcp_tools.json | Accepted | [zh](../zh/ADR-069-mcp-tool-level-optin.md) |
| 63 | [ADR-070](./ADR-070-doc-standalone-process-and-tree-storage.md) | acowork-doc as a Standalone Process + Tree Storage Selection | Decided (2026-09, settled when D0–D4 implementation completed) | [zh](../zh/ADR-070-doc-standalone-process-and-tree-storage.md) |
| 64 | [ADR-071](./ADR-071-distiller-runtime-config-and-trigger.md) | Memory Distiller Runtime Config and Trigger Wiring (Making EpisodicDistiller Operable) | Implemented (2026-09, W1–W5 landed; see the work table for commits; W… | [zh](../zh/ADR-071-distiller-runtime-config-and-trigger.md) |
| 65 | [ADR-072](./ADR-072-mcp-install-package-spec.md) | Generic MCP Install Detection Module — Declarative PackageSpec | Accepted | [zh](../zh/ADR-072-mcp-install-package-spec.md) |
| 66 | [ADR-073](./ADR-073-agent-instance-identity-decomposition.md) | Layering the Agent Identity Model — Fully Decoupling agent_id / agent_instance_id / node_id | Accepted | [zh](../zh/ADR-073-agent-instance-identity-decomposition.md) |
| 67 | [ADR-074](./ADR-074-per-session-context-window-override.md) | Per-Session Context Window Override — context_window Extended from Per-Agent to Per-Session | Accepted (finalized at review on 2026-09-15) | [zh](../zh/ADR-074-per-session-context-window-override.md) |
| 68 | [ADR-075](./ADR-075-node-identity-uuid-and-node-name.md) | Restoring the Node Identity Model — node_id Upgraded to a Stable UUID, node_name Taking Over the Display Role | Draft | [zh](../zh/ADR-075-node-identity-uuid-and-node-name.md) |
| 69 | [ADR-076](./ADR-076-multi-user-account-system.md) | Multi-User Account System | Draft (the business semantics are valid; the implementation shape has… | [zh](../zh/ADR-076-multi-user-account-system.md) |
| 70 | [ADR-077](./ADR-077-system-agent-demotion-to-default-agent.md) | Demoting the System Agent to a Preinstalled Default Agent — Removing the Gateway's Privileged Builtin Path | Decided (finalized 2026-11-12) | [zh](../zh/ADR-077-system-agent-demotion-to-default-agent.md) |
| 71 | [ADR-078](./ADR-078-git-status-bar.md) | Workspace Git Version Control Bar (Desktop Git Status Bar) | Draft (pending review) | [zh](../zh/ADR-078-git-status-bar.md) |
| 72 | [ADR-079](./ADR-079-doc-realtime-collab-tiptap-yjs.md) | Real-Time Collaborative Document Editing (Tiptap + Yjs) Technical Plan | Proposed (P1 starts after ADR-076 lands) | [zh](../zh/ADR-079-doc-realtime-collab-tiptap-yjs.md) |
| 73 | [ADR-080](./ADR-080-gateway-advertise-host-ip-change-watchdog.md) | Gateway advertise-host Drift Self-Healing (if-watch drives pm / doc / embed) | Accepted | [zh](../zh/ADR-080-gateway-advertise-host-ip-change-watchdog.md) |
| 74 | [ADR-081](./ADR-081-global-search.md) | Global Search (Ctrl+Shift+F Six-Source Aggregated Retrieval) | Draft (pending review) | [zh](../zh/ADR-081-global-search.md) |
| 75 | [ADR-082](./ADR-082-memory-storage-sqlite-vector-fts.md) | Migrating the Memory Storage Backend to SQLite (vectors + FTS, no graph) | Implemented | [zh](../zh/ADR-082-memory-storage-sqlite-vector-fts.md) |
| 76 | [ADR-083](./ADR-083-context-compaction-cancellation-and-deadline.md) | Context Compaction Cancellation + Distillation Deadline Guard | Proposed (pending decision) | [zh](../zh/ADR-083-context-compaction-cancellation-and-deadline.md) |
| 77 | [ADR-084](./ADR-084-user-standalone-process.md) | Splitting Accounts / User Chat Out of Gateway into a Standalone Process acowork-user | Decided (2026-10-20, finalized at architecture review) | [zh](../zh/ADR-084-user-standalone-process.md) |
| 78 | [ADR-085](./ADR-085-agent-lifecycle-state-machine.md) | The Agent Lifecycle State Machine (ready: bool → the AgentStatus.state enum) | Accepted (v2 revision, implemented on 2026-10-01 and passed review —… | [zh](../zh/ADR-085-agent-lifecycle-state-machine.md) |
| 79 | [ADR-086](./ADR-086-mobile-app-im-ia-and-multi-session.md) | Mobile App Information Architecture (IM Form) and Multi-Session Carrier | Accepted (2026-10-02; the UI/interaction design passed two-layer auto… | [zh](../zh/ADR-086-mobile-app-im-ia-and-multi-session.md) |
| 80 | [ADR-087](./ADR-087-node-agent-owner-permissions.md) | The Node and Agent Owner Permission Model — Plugging the Hole Where "the Whole Machine Is Writable by Every User by Def… | Draft (v2 revision; Q1/Q2/Q4 and the "single owner + multiple guests"… | [zh](../zh/ADR-087-node-agent-owner-permissions.md) |
| 81 | [ADR-088](./ADR-088-cloud-hub-application-layer-remote-access.md) | Adding the Cloud Hub Application-Layer Remote-Access Mode, Coexisting Permanently with the Thin Relay | Decided (not yet implemented; Phase 0 not started) | [zh](../zh/ADR-088-cloud-hub-application-layer-remote-access.md) |
| 82 | [ADR-089](./ADR-089-lsp-relay-reachability-vs-exposure.md) | LSP Relay Reachability vs. Exposure — Desktop Accesses the Relay Through a Gateway Reverse Proxy | Proposed (pending architecture review) | [zh](../zh/ADR-089-lsp-relay-reachability-vs-exposure.md) |
| 83 | [ADR-090](./ADR-090-mcp-lsp-install-ownership-node.md) | MCP Install Moves to the Agent's First Enable — Gateway Keeps the Global Registry and Key Vault | Accepted (implemented 2026-10-10; the LSP half was split out into ADR-091) | [zh](../zh/ADR-090-mcp-lsp-install-ownership-node.md) |
| 84 | [ADR-091](./ADR-091-lsp-server-registry-node.md) | LSP Server Registry Moves Down to the Node — preset ∪ custom Merge, Persistent Install State | Accepted (implemented 2026-10-10, P0–P7) | [zh](../zh/ADR-091-lsp-server-registry-node.md) |
| 85 | [ADR-092](./ADR-092-user-message-identity-and-bounded-reads.md) | Stable Identity and Bounded Reads for User Messages — Restructuring the user chat Storage and Refresh Model | Accepted | [zh](../zh/ADR-092-user-message-identity-and-bounded-reads.md) |
| 86 | [ADR-093](./ADR-093-global-search-llm-summary-citations.md) | LLM Synthesis and Clickable Citations for Global Search | Draft (pending review) | [zh](../zh/ADR-093-global-search-llm-summary-citations.md) |
| 87 | [ADR-094](./ADR-094-progressive-skill-auto-load-and-load-skill-tool.md) | Per-Agent Skills Auto-Load (Progressive Default) + `load_skill` Built-in Tool | Accepted (2026-10 revision) | [zh](../zh/ADR-094-progressive-skill-auto-load-and-load-skill-tool.md) |
| 88 | [ADR-095](./ADR-095-global-search-conversation-memory-retrieval-and-summary-candidates.md) | Global Search: Conversation and Memory Retrieval, and Summary Candidate Selection | Draft (pending review) | [zh](../zh/ADR-095-global-search-conversation-memory-retrieval-and-summary-candidates.md) |

---

## Supporting files

| File | Purpose |
|---|---|
| [GLOSSARY.md](./GLOSSARY.md) | Chinese ↔ English term mapping. Consult it before translating or reviewing a new ADR. |
| [_TEMPLATE.md](./_TEMPLATE.md) | Skeleton for a new ADR, matching the section structure used across the set. |

## Conventions

- **Section numbering is stable across languages.** The Chinese `§决策 4` is the English `§ Decision 4`; the Chinese `§5.5` is the English `§5.5`.
- **Identifiers are never translated**: type names, field names, header names, endpoint paths, file paths, environment variables and CLI flags appear verbatim in both versions.
- **Decision numbers are preserved**: a decision referenced as `决策 4` in Chinese is `Decision 4` in English, including after a later ADR revises or supersedes it.
- **Supersession notices are preserved verbatim.** Where a later ADR reverses an earlier decision, the English text says so in the same place the Chinese text does — an English reader must never conclude that a superseded path is still current.
- **Proper nouns are kept as-is**: the decider name (大鱼), package names, crate names and product names are not translated.
- **Code samples stay in their original form**; only the surrounding prose is translated. A Rust or TOML snippet therefore reads identically in both files, which is what makes the two diffable.

## Known gaps

- The English protocol set ([`docs/protocols/en/`](../../protocols/en/README.md)) lags the Chinese one. Several ADRs therefore link to their Chinese protocol counterpart, which is the complete version: the link points at the authoritative document rather than at a stub.
- A few ADRs reference paths that have since moved (notably `core/acowork-grafeo`, since split into `core/acowork-memory`, and the user domain, since moved from `acowork-gateway` to `acowork-user`). Where one current file covers the old target, the link points there; where the target is genuinely gone, the link was repointed to the nearest live equivalent while the surrounding prose still names the original path.
- Planned-but-unwritten paths (such as `apps/cli/`) are written as inline code rather than as hyperlinks, so a reader does not click through to a directory that does not exist yet.

## Maintenance

When a Chinese ADR changes, update its English counterpart in the same commit. A stale English
ADR is worse than a missing one: it reads as authoritative and is not.
