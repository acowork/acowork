# Agent as APP: Platform Design Overview

> Version: v3.4 | Last Updated: 2026-04-16

---

## 1. Background and Goals

Design a decentralized, high-security, scalable AI Agent runtime platform. The core idea is to treat each Agent as an independent "app package" (similar to Android apps), loaded and executed by a unified Agent Runtime process, running on the client (user's machine) and managed by a lightweight Gateway.

**Core Analogy — Android Model:**

| Android | ACowork | Purpose |
|---------|---------|----------|
| zygote / ART | Agent Runtime binary | Universal execution engine — only one |
| APK (DEX + resources) | .agent package (config + prompts + skills) | Declarative, no custom code |
| APK Signature | .agent Signing Block | Package signing, verifies integrity and origin |
| ActivityManagerService | Gateway | Lifecycle management |
| Binder IPC | MQTT pub/sub + HTTP reverse proxy | Inter-process communication (ADR-033) |
| ContentProvider | `acowork-user` user domain | User identity and preference management |
| PackageManagerService | Package Manager | Install/uninstall |
| AndroidManifest.xml | manifest.toml | Permission declarations |

> **v3.5 change**: the IPC channel moved from gRPC bidirectional streams to **MQTT pub/sub + HTTP reverse proxy** (ADR-033); the user domain was split out of the Gateway into the standalone `acowork-user` service (ADR-084), and memory is owned by `acowork-memory` (ADR-101). See [`06-communication.md`](./06-communication.md).

## 2. Core Features

- **Standardized Packaging**: Agents are distributed as compressed packages (.agent), containing configuration, Prompts, Skills, and tool declarations — **no executable code**. All packages must be signed; Gateway enforces signature verification during installation.
- **Unified Execution Engine**: Agent Runtime is the platform's single binary, responsible for loading .agent packages and executing Agent logic (LLM interaction, tool dispatch, memory read/write).
- **Process-Level Isolation**: Each Agent runs as an independent Agent Runtime process managed by Gateway, with its own workspace, private SQLite memory layer database, filesystem isolation, and optional resource limits (cgroups/containers).
- **Agent Autonomy**: Agent processes connect directly to LLM APIs, execute tools autonomously, and manage permission checks without relying on Gateway to proxy business logic.
- **Biomimetic Memory System**: Memory is owned by the standalone memory service `acowork-memory` (ADR-101), one SQLite database per Agent instance (`$HOME/.acowork/acowork-memory/instances/{instance_id}/memory.db`). The storage backend is SQLite (ADR-082): vectors live in a `vectors` table, lexical search runs on an FTS5 trigram index, and retrieval is a vector + lexical hybrid. The layers map onto four persisted labels (Episodic / Knowledge / Procedural / Autobiographical); the transient layer is the LLM context window and is never persisted. It includes forgetting mechanisms (three-factor decay), privacy levels (PrivacyLevel), and the memory lifecycle (Retrieve/Inject/Record/Consolidate/Decay/Compact).
  > Associative diffusion retrieval (§6) is a design goal, not implemented — ADR-082 D4 removed inter-layer graph traversal and creates no `edges` table.
- **Permission Declaration and Authorization**: Agents declare required permissions (network, filesystem, cross-Agent calls, etc.) in their manifest; Gateway configures sandboxes at startup; Agents perform runtime self-checks.
- **Cross-Platform Support**: .agent package format and the MQTT + HTTP protocol contract are unified across platforms; per-platform runtime mechanisms (process model, transport layer, sandbox) adapt to platform characteristics.

## 3. Overall Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     Gateway (Persistent)                      │
│                                                              │
│  ┌────────────┐ ┌────────────┐ ┌───────────┐ ┌───────────┐ │
│  │ Package    │ │ Lifecycle  │ │ Intent    │ │ Rate      │ │
│  │ Manager    │ │ Manager    │ │ Router    │ │ Limiter   │ │
│  └────────────┘ └────────────┘ └───────────┘ └───────────┘ │
│                                                              │
│  ┌────────────┐ ┌────────────┐             ┌───────────┐   │
│  │ Budget    │ │ Key       │             │ Config    │   │
│  │ Tracker   │ │ Vault     │             │ Manager   │   │
│  └────────────┘ └───────────┘             └───────────┘   │
│                                                              │
└──────────────────────────┬───────────────────────────────────┘
                           │ MQTT pub/sub + HTTP proxy
                           │ (ADR-033)
                           │ (Transport: Unix Socket / Named Pipe / Local TCP)
       ┌───────────────────┼───────────────────┐
       │                   │                   │
       ▼                   ▼                   ▼
┌─────────────────┐ ┌─────────────────┐ ┌─────────────────┐
│ Agent Runtime   │ │ Agent Runtime   │ │ Agent Runtime   │
│ (Unified Binary)│ │ (Unified Binary)│ │ (Unified Binary)│
│                 │ │                 │ │                 │
│ ┌─────────────┐│ │ ┌─────────────┐│ │ ┌─────────────┐│
│ │ System Agent ││ │ │ Weather Agent││ │ │ Calendar Agent││
│ │ (com.roll-   ││ │ │ (config +   ││ │ │ (config +   ││
│ │  ball.sys-   ││ │ │  prompt +   ││ │ │  prompt +   ││
│ │  tem)        ││ │ │  skills)    ││ │ │  skills)    ││
│ └─────────────┘│ │ └─────────────┘│ │ └─────────────┘│
│                 │ │                 │ │                 │
│ ✅ Private SQLite memory layer│ │ ✅ Private SQLite memory layer│ │ ✅ Private SQLite memory layer│
│ ✅ Direct LLM   │ │ ✅ Direct LLM   │ │ ✅ Direct LLM   │
│ ✅ Tool Execute │ │ ✅ Tool Execute │ │ ✅ Tool Execute │
│ ✅ Local Budget │ │ ✅ Local Budget │ │ ✅ Local Budget │
│ ⭐ System Privs │ │                 │ │                 │
│                 │ │                 │ │                 │
│ ↗ Usage Report  │ │ ↗ Usage Report  │ │ ↗ Usage Report  │
│ ↗ Rate Request  │ │ ↗ Rate Request  │ │ ↗ Rate Request  │
│ ↗ Intent Send/Rx│ │ ↗ Intent Send/Rx│ │ ↗ Intent Send/Rx│
│ ↗ Identity Sync │ │ ↗ Identity Qry  │ │ ↗ Identity Qry  │
└─────────────────┘ └─────────────────┘ └─────────────────┘

                    ┌─────────────────────────┐
                    │  Memory Sync Service     │
                    │  (Cloud Sync / Cross-Dev) │
                    │  - All Zone plaintext sync │
                    │  - PrivacyLevel controls │
                    └─────────────────────────┘
```

## 4. Responsibility Division Principles

**Agents are as autonomous as possible; Gateway only manages what must be centralized.**

| Responsibility | Execution Location | Reason |
|----------------|-------------------|--------|
| LLM calls | Agent process | Direct connection, no RPC overhead, streaming natural, Agent autonomy |
| Tool execution | Agent process | Autonomous permission checks, low latency |
| Private memory read/write | Memory service `acowork-memory` | Tool call (ADR-101), no business logic in the Runtime |
| API Key storage | Gateway Vault | Centralized secure management |
| API Key distribution | One-time at startup to Agent | Agent needs direct LLM access |
| Budget tracking | Gateway (receives reports) | Cross-Agent statistics |
| Budget enforcement | Agent (local pre-check) | Low latency, autonomous |
| Budget hard limit | Gateway (over-limit signal) | Safety net |
| Rate limiting | Gateway (token allocation) | Shared resource coordination |
| User identity and preferences | `acowork-user` user domain | Centralized identity management (ADR-084) |
| Intent routing | Gateway | Cross-process scheduling |
| Sandbox configuration | Gateway (at startup) | System-level permissions |

## 4.1 LLM-First Principle

**Trust LLM over rules — unless rules can solve problems LLM cannot.**

In the long run, LLM capabilities will continue to improve (lower hallucination rates, stronger reasoning), while rule-based solutions lack generalization and are not a sustainable approach. Within the trade-off of capability boundaries, ACowork follows these guidelines:

- **Semantic judgments go to LLM**: Classification, scoring, quality checks — tasks involving semantic understanding — are done by LLM, not simulated with rules
- **Mechanical constraints go to rules**: Length validation, rate limiting, safety filtering — self-constraints LLM cannot enforce — are handled by Runtime rules
- **Rules as supplement, not replacement**: When rules cannot bring significant improvement over LLM, do not replace LLM with rules — that is a capability regression
- **Offline consolidation is the quality safeguard**: Trust LLM judgments in real-time; use LLM (not rules) to review and calibrate with full context during offline phases

## 5. Future Extensions

- **Agent Store**: Public repository where users can install with one click, with ratings and reviews.
- **Paid Agents**: License verification support (integrated into Gateway).
- **Federated Memory**: Memory sharing among multiple users (with authorization).
- **Agent Composition**: Multiple Agents orchestrated as workflows (DAG scheduling).
- **Multi-modal Agents**: Agents supporting image, audio, and video input/output.
- **Mobile Deep Integration**: Android multi-process Service architecture, iOS App Extension integration, mobile UI interaction optimization.

---

> Detailed design in sub-documents:
> - [02-agent-package.md](./02-agent-package.md) — Agent packaging format, signing mechanism, manifest.toml
> - [03-agent-runtime.md](./03-agent-runtime.md) — Agent Runtime internal structure and main loop
> - [04-gateway.md](./04-gateway.md) — Gateway component detailed design
> - [05-memory.md](./05-memory.md) — Memory layered architecture
> - [06-communication.md](./06-communication.md) — Communication protocols (MQTT pub/sub + HTTP reverse proxy + Intent mechanism)
> - [08-security.md](./08-security.md) — Security design
> - [10-debug-protocol.md](./10-debug-protocol.md) — Debug protocol (DevMode, breakpoints, record/replay)
> - [12-tool-system.md](./12-tool-system.md) — Tool system (Built-in / WASM / Gateway)
> - [13-skill-system.md](./13-skill-system.md) — Skill system (SKILL.md + SQLite memory layer experience layer)
> - [14-desktop-app.md](./14-desktop-app.md) — Desktop app (Tauri, layout, system tray)

---

## Language Rules

- **Docs in Chinese**: All design documents (`.md` files under `docs/`) are written in Chinese
- **Code comments in English**: All Rust source code comments (`//`, `//!`, `///`) must be in English
- **Multi-language docs**: After the project is fully developed, translate other language versions from the Chinese source documents
