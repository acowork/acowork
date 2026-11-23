# ADR-119: Chunked Resumable Upload — One Binary-Chunk Channel for user→agent, Hosted-App Save, and agent→agent Intent Attachments

**Status**: Accepted (2026-11-23; user ruled "not only hosted-app save, but also user→agent and agent→agent file transfer; supplement the intent mechanism where it carries no attachments". Per-file default cap 1 GiB, configurable. v1.1 same-day review revisions: Q1–Q4 + Q2-bis ruled and settled, see §7.)
**Date**: 2026-11-23
**Deciders**: User
**Revisions**:
- **v1.1 (2026-11-23, post-review revisions)** — ① D12 transfer-store ownership pinned to the **user service** (removing the "Gateway/user service" ambiguity); reference validation gains **creator binding** (`creator_instance_id`); ② §3 gains contract details: cap semantics, input validation, single-writer & complete races, re-PUT recovery after a failed verification, atomic swap, declared-values-are-not-facts, client-side streaming hash, sweeper ownership; ③ §4.3: response-direction `attachments` MUST be ignored in v1; ④ §6 gains the double-hop transfer ceiling (accepted by user ruling; optimization is data-driven); ⑤ disk governance sets **no per-store quota** — it moves to [ADR-120](./ADR-120-gateway-unified-cache-management.md) unified Gateway cache management (global watermark alarm + Desktop dialog + cache-management card).
- v1.0 (2026-11-23) — initial ruling.

**Related**:
- [ADR-046](./ADR-046-unified-attachment-entries.md) / [ADR-040](./ADR-040-runtime-adapter-use-case-layer.md) (attachment entry union and the Runtime use-case layer)
- [ADR-055](./ADR-055-remote-runtime-node-topology.md) (Gateway → Node → Runtime three-hop reverse proxy; a per-request body cap at every hop)
- [ADR-009](./ADR-009-gateway-workspace-isolation.md) (workspace files go through Runtime only; no cross-agent reads of private workspaces)
- [ADR-118](./ADR-118-hosted-app-dual-canvas.md) (hosted-app save `PUT /workspaces/file`)
- [Design 27](../../design/en/27-intent-messaging-and-group-chat.md) (intent envelope / params split; the group-chat `chat_attachment_ids` reference + node-auth pull pattern, generalized here to agent→agent)
- [ADR-112](./ADR-112-workspace-preview-capability-ticket.md) (capability-ticket precedent: references carry authorized coordinates, not bare paths)
- [ADR-120](./ADR-120-gateway-unified-cache-management.md) (unified Gateway cache management: disk-watermark alarm and per-service self-reported cleanup; takes over this ADR's disk governance from v1.1)

> The Chinese document is the source of truth; this English counterpart is maintained in the same commit.

---

## 1. Decision summary

### 1.1 One line

**Every large file rides the same fixed-size raw-binary chunk channel; intents and message envelopes carry references only, and the receiver pulls bytes over node-auth HTTP as a stream.** Per-request body caps no longer bound file size; progress and resume are by-products of chunking.

### 1.2 Decision table

| # | Decision | Conclusion |
|---|---|---|
| D1 | Wire encoding | Raw binary chunks (`application/octet-stream`). **MUST NOT** base64: 4/3 inflation means a 64 MiB body holds only 48 MiB of file |
| D2 | Chunk size | Fixed **2 MiB**, MUST stay below the smallest per-hop body cap (all hops 64 MiB today). No configurable chunk size |
| D3 | Endpoint semantics | tus's minimal four verbs: `POST` create (length+sha256) → `PUT …/{offset}` append (idempotent) → `HEAD` query offset (resume) → `POST …/complete` stream-merge + verify. **No tus library / tusd** |
| D4 | Progress | acknowledged bytes / total; frontend `File.slice` loop + callback; **no Uppy / tus-js-client** |
| D5 | Chunk landing & merge | Chunks in `.uploads/{uploadId}/{offset}`; complete stream-appends in ascending offset, **MUST NOT** read the whole file into memory; verify length+sha256 before swapping the target |
| D6 | Cap decoupling | Per-hop body caps stay 64 MiB; per-file cap decoupled, configurable, default **1 GiB** (supersedes `MAX_UPLOAD_BYTES`, see §3 cap semantics); disk is the final ceiling — governed by [ADR-120](./ADR-120-gateway-unified-cache-management.md) unified cache management + a global watermark alarm (v1.1: no per-store quota) |
| D7 | Scope | **All four producers**: user→agent (1:1 composer and group mention upload), hosted-app save, agent→agent intent attachments. One chunk contract, different storage landings (§4) |
| D8 | Resume | Keep `HEAD` offset; cross-session resume |
| D9 | Small files skip chunking | <2 MiB keeps existing single-request routes; **MUST NOT** add three round-trips to small attachments |
| D10 | Cleanup | Delete chunks on successful complete / explicit abort / **24 h** timeout; a failed merge keeps chunks until timeout |
| D11 | Intent attachments = references, not bytes | `IntentRequest` gains `attachments`: `{id, coordinates, size, sha256}` of bytes already landed in the transfer store. **MUST NOT** put bytes in `params` or the MQTT envelope (`GATEWAY_MQTT_MAX_PACKET_SIZE = 10 MiB`, `core/acowork-core/src/defaults.rs`) |
| D12 | Transfer store ownership | agent→agent bytes land in a **user-service-owned shared transfer area** (pinned in v1.1, removing the "Gateway/user service" ambiguity: mirrors the group-chat `files/` semantics and the "single trusted write entry" pattern; the Gateway only reverse-proxies and never stores, holding the Gateway boundary red line), never the sender's private workspace and never a cross-agent private-workspace read (ADR-009). The transfer record MUST carry `creator_instance_id`; reference validation = creator match. Receiver pulls over node-auth HTTP |
| D13 | Pull = streaming GET | Receiver pulls via a streaming response GET (a route shaped like `download_chat_attachment`, `core/acowork-gateway/src/http/intent_api.rs`). Response streams are not bound by request-body caps; **download is not chunked in v1**; upgrade path = Range |
| D14 | Implementation order | ① unified chunk service + user→agent; ② hosted-app save onto the same contract; ③ intent `attachments` + transfer store + pull route (depends on ①) |

## 2. Context and problem

Single-request upload puts the whole file in one HTTP body, so file size is pinned by the smallest body cap on the path: the Node proxy `proxy_agent` buffers the body as `Bytes` and once lacked `DefaultBodyLimit`, inheriting axum's 2 MiB default (now raised to 64 MiB, `core/acowork-node/src/proxy/mod.rs`); Gateway / Runtime global `GLOBAL_BODY_LIMIT = 64 MiB`; attachment user cap `MAX_UPLOAD_BYTES = 50 MiB` (`core/acowork-runtime/src/usecases/attachment.rs`); hosted-app save inflates another 4/3 via base64.

Today's four producers:

- **user→agent 1:1**: composer multipart `POST /sessions/{sid}/files`, single request, subject to the caps above.
- **user→agent group mention**: already reference-based — `attachment_ids` point at files uploaded to the group's `files/` dir; Gateway pins them on the shared `user_message` row; Runtime pulls bytes over node-auth HTTP (Design 27 §4.2.1/§5.3). The upload side is still single-request.
- **hosted-app save**: app → bridge → host → `PUT /workspaces/file` (base64 JSON), single request.
- **agent→agent intent**: `IntentRequest` (`core/acowork-gateway/src/http/intent_api.rs`) has **no attachment field** — only `params` / `repo` / `session_config` etc. The group-chat reference pattern was never generalized to intent. This is the gap this ADR closes.

Raising caps only postpones the problem; single requests also mean whole-file buffering per hop, whole-file re-send on failure, and no resume. Bytes in intent params would additionally hit the MQTT 10 MiB packet cap. Chunking + reference-pull solves all of it at once.

## 3. Unified chunk contract

Four verbs; the owner differs by landing (Runtime owns attachment/workspace landings, the user service owns the transfer landing); Gateway / Node pass through:

| Verb | Path shape | Body / headers | Answer |
|---|---|---|---|
| POST | `{store}/uploads` | JSON `{name, size, sha256}` | `{uploadId}` |
| PUT | `{store}/uploads/{uploadId}/{offset}` | binary chunk ≤2 MiB | `{received}`; 409 on non-contiguous offset (sequential in v1) |
| HEAD | `{store}/uploads/{uploadId}` | — | `Upload-Offset` |
| POST | `{store}/uploads/{uploadId}/complete` | — | stream-merge + verify → land target, return that landing's metadata |
| DELETE | `{store}/uploads/{uploadId}` | — | abort, delete chunks |

`{store}` is: `/sessions/{sid}` (1:1 attachment), the group session's files area, `/workspaces` (hosted save; complete swaps the target file), `/transfers` (agent→agent shared area, **owned by the user service**, D12). Chunk 2 MiB; re-sending an offset idempotently overwrites; before complete the target does not exist / is not swapped.

**v1.1 contract details** (settled at review; MUST-level):

- **Cap semantics**: `MAX_UPLOAD_BYTES` (50 MiB) is superseded by a per-store configurable per-file cap (default 1 GiB, D6), enforced at complete against both the declared total length and the bytes actually received; the old constant is retired and MUST NOT coexist with the new cap.
- **Input validation**: `uploadId` MUST be server-generated (never client-chosen — path-traversal guard into `.uploads/`); `{offset}` MUST be a non-negative integer multiple of the chunk size (2 MiB), else 400.
- **Single writer & races**: one `uploadId` accepts one writer at a time — under cross-session resume (D8) a second writer's PUT MUST 409; a PUT arriving while complete is running MUST be rejected; a repeated complete after success MUST idempotently return the same metadata.
- **Failed-verification recovery**: when complete's length/sha256 verification fails, chunks are kept (D10) and the `uploadId` returns to a PUT-able state — the client MAY re-PUT any offset and complete again; no forced full re-upload. Per-chunk digests (pinpointing the corrupt chunk) stay an upgrade item, batched with out-of-order concurrent chunks.
- **Atomic swap**: complete MUST land the target via temp-file + rename, never in-place overwrite.
- **Declared values are not facts**: until complete's verification passes, the client-declared `size` / `sha256` MUST NOT be consumed as fact by anyone (intent receiver, router, UI) — the same discipline as ADR-103 v0.8 ("the platform must not hand down an assertion about an endpoint as a configuration fact").
- **Client streaming hash**: declaring sha256 at create time means the client reads the file once up front; implementations MUST hash streaming (`File.slice` loop feeding the hash), MUST NOT read the whole file into memory.
- **Sweeper ownership**: Runtime sweeps its `.uploads` (startup + periodic); the user service sweeps the transfer area; both enforce D10's 24 h TTL.

## 4. The four producers

### 4.1 user→agent (1:1 + group)
Composer / mention uploads >2 MiB switch to §3; <2 MiB keeps multipart. Group chat keeps its `attachment_ids` reference semantics; only the upload side changes channel. The Runtime pull path is unchanged.

### 4.2 Hosted-app save
The bridge slices save bytes into chunks over `/workspaces/uploads`; complete stream-swaps the target workspace file. The host `AppFrame` no longer base64-PUTs the whole file at once. The ArrayBuffer→Uint8Array normalization stays (chunks are binary anyway).

### 4.3 agent→agent intent attachments (the gap)
- The sender Runtime first lands bytes via §3 into `/transfers/uploads` (the **user service's** shared area, pinned in v1.1; the Gateway only reverse-proxies); complete returns a transfer id. The transfer record MUST carry `creator_instance_id`.
- `intent_send` / `IntentRequest` gain `attachments: [{id, size, sha256, name}]` (references, D11). Gateway validates the id exists and the **sender = creator** (`creator_instance_id` match, v1.1), pins it on the intent row (the row is the receiver's authorization, isomorphic to Design 27 §4.2.1), forwards it into the receiver's execution turn.
- The receiver Runtime pulls bytes over node-auth streaming GET (D13, a route shaped like `download_chat_attachment`, authorized by transfer id + receiver coordinates), lands them in its own workspace, then handles them as ordinary attachments.
- An old Gateway / Runtime lacking the `attachments` key degrades to "no attachments this turn" (same shape as Design 27's `chat_attachments` degradation), not a whole-message failure.
- **Response direction carries no attachments in v1** (Q4 ruling): an `attachments` key on a `kind = "response"` intent MUST be ignored without error; opening it later is purely additive (the field is already on the envelope).

### 4.4 Small files
All four producers: <2 MiB keeps each one's existing single-request route, no chunk round-trips (D9).

## 5. Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Just raise the 64 MiB cap | Does not fix whole-file buffering / re-send / no resume; video and image-heavy files still hit the ceiling |
| base64 chunks | 4/3 inflation + JSON buffering waste |
| Bytes in intent params | Hits the MQTT 10 MiB packet cap; JSON inflation; the envelope should carry references only (Design 27 split) |
| Cross-agent direct read of the sender workspace | Violates ADR-009's private boundary; needs cross-agent ACL, heavier than a shared transfer area |
| Full tus / S3 multipart + client libs | Dependency surface and semantics overkill; only four verbs' semantics are needed |
| Configurable chunk size | Extra config surface and validation branch; 2 MiB is already far below every hop cap |
| Chunked download in v1 | Response streams are not bound by request-body caps, so unnecessary; Range kept as upgrade |

## 6. Consequences and ceilings

- Upside: file size decoupled from per-request caps across all four producers; Node buffers only 2 MiB per hop; failure re-sends one chunk; cross-session resume; intent can carry attachments for the first time without hitting the MQTT cap.
- Downside: more round-trips for large files (1 GiB ≈ 512 chunks); server temp dirs and cleanup duty; the transfer area adds a shared store and an authorization surface; complete is one streaming IO pass.
- Known ceilings (deliberate v1 simplifications, marked `ponytail:` at the implementation): sequential single-connection upload (RTT-bound throughput; upgrade = out-of-order concurrent chunks); unchunked download (upgrade = Range); no instant-upload (upgrade = sha256 index).
- **Double-hop transfer ceiling (v1.1, accepted by user ruling)**: agent→agent bytes transit the transfer store, so the full journey is two hops (sender → store → receiver). This is the inherent price of store-and-forward, rooted in three established constraints: intent's async wake-up semantics (the receiver may not be spawned at send time, so bytes must land and wait), the ADR-055 hub topology (cross-machine traffic only flows through the Gateway; no direct Runtime channel), and the ADR-009 private boundary (only the receiver writes its own workspace). On a single-machine deployment both hops are loopback — the overhead is ≈ 2× local disk IO with no network amplification; only "cross-machine + large file + high frequency" together produce 2× WAN amplification. **Ruling: ship the current design first; optimize on measured data** — instrument the transfer store (cross-machine bytes, P95 pull latency); upgrade path in order: sha256 instant-upload index → out-of-order concurrent chunks + Range → (only if the data proves it necessary) direct node-to-node transfer.
- Rollback: the chunk routes and intent `attachments` are additive; deleting them reverts to single-request upload / attachment-less intent. The old multipart and group-reference paths are untouched, so small files and old clients are unaffected.
## 7. Open questions (all settled in v1.1)

Q1–Q4 and Q2-bis raised at review were ruled by the user; this section is the ruling record (format mirrors ADR-103 §10):

| # | Question | Ruling |
|---|---|---|
| Q1 | Which service owns the transfer store | **The user service**. Three independent lines of evidence: ① the group-chat `files/` precedent — Design 27 §4.2.1 "the trusted service that is the single write entry"; ② the pull path already runs through the user service today — `download_chat_attachment` forwards to the `user_process` port after auth (`core/acowork-gateway/src/http/intent_api.rs`), and D13 reuses that shape; ③ the Gateway boundary red line (communication + resource management + reverse proxy; holds no business data) rules out Gateway storage, and Node-local storage breaks the cross-machine pull path (ADR-055) |
| Q2 | In-flight quota policy | **No per-store / per-owner quota** (user ruling: "a dedicated limit here means little" — many services deployed on the Gateway consume disk; governance must be unified). Disk governance upgrades to Gateway-level unified cache management, see [ADR-120](./ADR-120-gateway-unified-cache-management.md): per-service self-reported usage, a settings/gateway cache-management card, user-driven cleanup; the transfer store keeps only D10's TTL auto-recycling |
| Q2-bis | Global disk watermark | **Yes, as an alarm** (user ruling): when the watermark (e.g. 90%) trips, the Gateway only reports to the frontend; Desktop pops a dialog; clicking it navigates to the settings/gateway cache-management page where the user decides which services' caches to clean; **no automatic write rejection**. The residual risk (disk keeps growing between alarm and user response) is explicitly accepted, with D10's TTL as the automatic backstop. Details: ADR-120 D5 |
| Q3 | Recovery granularity after a failed complete verification | **Keep chunks + re-PUT any offset, then re-complete** (§3 contract details; D10 already keeps the chunks — the contract just needed the sentence); no forced full re-upload. Per-chunk digests stay an upgrade item, batched with out-of-order concurrent chunks |
| Q4 | May response intents carry attachments | **Request direction only in v1**; a response's `attachments` MUST be ignored without error (§4.3); opening it later is purely additive |
