# RAG Standard Query Protocol — Enterprise Integration Guide

> Version: 1.0 | Protocol Version: 1.0 | Updated: 2026-04-27  
> Module: acowork-runtime (Phase 4 S4)

---

## 1. Overview

ACowork defines a standard HTTP query protocol. After an enterprise RAG service adapts to this protocol, it can serve as an extended retrieval channel for Bots. **ACowork does not implement a RAG engine** nor provide adapters for individual RAG implementations; instead, it requires that the enterprise side ensures its service is compatible with this protocol.

### Core Principles

- **Pure Integration, No Hosting**: ACowork acts as an HTTP client and does not host RAG data.
- **Configuration-Driven Opt-In**: RAG is enabled only when the Bot manifest declares it.
- **Graceful Degradation**: When RAG is unreachable, return empty results without blocking Bot execution.
- **Security First**: Endpoint must be HTTPS; authentication is managed via Vault.

---

## 2. Request Protocol

### 2.1 HTTP Request

```
POST <endpoint>
Content-Type: application/json
Authorization: Bearer <token>       # Bearer authentication (optional)
X-API-Key: <key>                    # API Key authentication (optional)
```

### 2.2 Request Body JSON Schema

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["protocol_version", "query", "top_k"],
  "properties": {
    "protocol_version": {
      "type": "string",
      "const": "1.0",
      "description": "Protocol version, currently fixed at 1.0"
    },
    "query": {
      "type": "string",
      "description": "Query text"
    },
    "collection": {
      "type": "string",
      "description": "Collection/index name (optional, from manifest configuration)"
    },
    "top_k": {
      "type": "integer",
      "minimum": 1,
      "maximum": 100,
      "description": "Maximum number of results to return"
    },
    "score_threshold": {
      "type": "number",
      "minimum": 0.0,
      "maximum": 1.0,
      "description": "Minimum relevance threshold (optional)"
    },
    "filters": {
      "type": "object",
      "description": "Enterprise custom filter conditions (optional)",
      "additionalProperties": true
    },
    "extensions": {
      "type": "object",
      "description": "Protocol extension fields (reserved, for Phase 6 use)",
      "additionalProperties": true
    }
  }
}
```

### 2.3 Request Examples

**Automatic Retrieval (MemoryManager Retrieve Phase)**:

```json
{
  "protocol_version": "1.0",
  "query": "Q3 product roadmap",
  "collection": "product_docs",
  "top_k": 3,
  "score_threshold": 0.7
}
```

**Explicit Tool Call (LLM-triggered rag_query)**:

```json
{
  "protocol_version": "1.0",
  "query": "VPN remote access policy",
  "collection": "company_policies",
  "top_k": 10,
  "score_threshold": 0.5,
  "filters": {
    "department": "IT",
    "year": 2026
  }
}
```

---

## 3. Response Protocol

### 3.1 Successful Response

HTTP 200 + JSON body:

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["protocol_version", "results"],
  "properties": {
    "protocol_version": {
      "type": "string",
      "const": "1.0"
    },
    "results": {
      "type": "array",
      "items": {
        "type": "object",
        "required": ["content", "score"],
        "properties": {
          "content": {
            "type": "string",
            "description": "Result text content"
          },
          "source_url": {
            "type": "string",
            "description": "Source document URL (optional)"
          },
          "chunk_id": {
            "type": "string",
            "description": "Fragment ID within the document (optional)"
          },
          "score": {
            "type": "number",
            "minimum": 0.0,
            "maximum": 1.0,
            "description": "Relevance score"
          }
        }
      }
    },
    "extensions": {
      "type": "object",
      "description": "Protocol extension fields (reserved)",
      "additionalProperties": true
    }
  }
}
```

### 3.2 Response Example

```json
{
  "protocol_version": "1.0",
  "results": [
    {
      "content": "The Q3 product roadmap includes AI assistant features, expected release in July",
      "source_url": "https://wiki.corp.example.com/roadmap-q3",
      "chunk_id": "roadmap-3",
      "score": 0.92
    },
    {
      "content": "Engineering team Q3 delivery plan: infrastructure upgrades + new feature development",
      "source_url": "https://wiki.corp.example.com/eng-plan",
      "chunk_id": "eng-7",
      "score": 0.85
    }
  ]
}
```

### 3.3 Empty Result

```json
{
  "protocol_version": "1.0",
  "results": []
}
```

### 3.4 Error Response

When the RAG service returns a non-2xx status code, ACowork treats it as a query failure and triggers graceful degradation (returns empty result). It is recommended that the RAG service returns JSON on error:

```json
{
  "error": "internal_server_error",
  "message": "Index temporarily unavailable"
}
```

---

## 4. Authentication Configuration

### 4.1 Authentication Methods

| Method       | Header                          | manifest Configuration  |
| ------------ | ------------------------------- | ----------------------- |
| Bearer Token | `Authorization: Bearer <token>` | `auth_type = "bearer"`  |
| API Key      | `X-API-Key: <key>`              | `auth_type = "api_key"` |
| No Auth      | None                            | Omit `auth_ref`         |

> OAuth 2.0 support is reserved for Phase 6.

### 4.2 Credential Management

Credentials are managed centrally via Vault and **are never exposed in manifest or process environment**:

```toml
# manifest.toml
[[tools]]
type = "rag"
name = "enterprise_knowledge"

[tools.rag]
endpoint = "https://rag.corp.example.com/v1/query"
auth_ref = "vault:rag_enterprise_key"    # Vault reference
auth_type = "bearer"                      # Authentication method
```

Vault reference format: `vault:<provider_name>`

- At runtime startup, the actual key value is fetched via IPC from Gateway Vault.
- Keys are protected using `secrecy::SecretString` and are not logged or exposed in stack traces.

---

## 5. Manifest Configuration Reference

### 5.1 Full Configuration

```toml
[[tools]]
type = "rag"
name = "enterprise_knowledge"           # Tool display name (seen by LLM)

[tools.rag]
endpoint = "https://rag.corp.example.com/v1/query"  # Must be HTTPS
collection = "product_docs"             # Optional: specify collection
auth_ref = "vault:rag_enterprise_key"   # Optional: Vault auth reference
auth_type = "bearer"                    # Authentication method: bearer / api_key
max_results = 5                         # Default number of results
score_threshold = 0.7                   # Default minimum score
timeout_secs = 10                       # Query timeout (seconds)
```

### 5.2 Required Permission Declarations

A Bot using a RAG tool must declare the following permissions:

```toml
[[permissions]]
type = "RagQuery"                       # RAG query permission

[[permissions]]
type = "Network"                        # Network whitelist (broad)
# Or specify exact endpoint
# type = "Network"
# value = "https://rag.corp.example.com/v1/query"
```

> RAG endpoint must use HTTPS. HTTP endpoints are rejected by permission checks.

### 5.3 Minimal Configuration (No Auth)

```toml
[[permissions]]
type = "RagQuery"

[[permissions]]
type = "Network"

[[tools]]
type = "rag"
name = "knowledge_base"

[tools.rag]
endpoint = "https://rag.example.com/v1/query"
max_results = 5
score_threshold = 0.7
```

---

## 6. Enterprise RAG Self-Adaptation Examples

### 6.1 Qdrant

```python
from fastapi import FastAPI, Request
from qdrant_client import QdrantClient

app = FastAPI()
client = QdrantClient(host="localhost", port=6333)

@app.post("/v1/query")
async def query(request: Request):
    body = await request.json()
    results = client.search(
        collection_name=body.get("collection", "default"),
        query_vector=get_embedding(body["query"]),
        limit=body["top_k"],
        score_threshold=body.get("score_threshold"),
    )
    return {
        "protocol_version": "1.0",
        "results": [{
            "content": hit.payload.get("content", ""),
            "source_url": hit.payload.get("source_url"),
            "chunk_id": hit.payload.get("chunk_id"),
            "score": hit.score,
        } for hit in results]
    }
```

### 6.2 Milvus

```python
from fastapi import FastAPI, Request
from pymilvus import Collection

app = FastAPI()

@app.post("/v1/query")
async def query(request: Request):
    body = await request.json()
    collection = Collection(body.get("collection", "default"))
    results = collection.search(
        data=[get_embedding(body["query"])],
        anns_field="embedding",
        param={"metric_type": "COSINE", "params": {"nprobe": 10}},
        limit=body["top_k"],
        expr=build_filter_expr(body.get("filters")),
    )
    return {
        "protocol_version": "1.0",
        "results": [{
            "content": hit.entity.get("content", ""),
            "source_url": hit.entity.get("source_url"),
            "chunk_id": hit.entity.get("chunk_id"),
            "score": hit.score,
        } for hit in results[0]]
    }
```

### 6.3 Elasticsearch

```python
from fastapi import FastAPI, Request
from elasticsearch import Elasticsearch

app = FastAPI()
es = Elasticsearch("http://localhost:9200")

@app.post("/v1/query")
async def query(request: Request):
    body = await request.json()
    resp = es.search(
        index=body.get("collection", "default"),
        body={
            "query": {
                "bool": {
                    "must": [{"match": {"content": body["query"]}}],
                    **build_filters(body.get("filters")),
                }
            },
            "size": body["top_k"],
            "min_score": body.get("score_threshold", 0),
        }
    )
    return {
        "protocol_version": "1.0",
        "results": [{
            "content": hit["_source"].get("content", ""),
            "source_url": hit["_source"].get("source_url"),
            "chunk_id": hit["_source"].get("chunk_id"),
            "score": hit["_score"],
        } for hit in resp["hits"]["hits"]]
    }
```

---

## 7. Dual-Trigger Model

RAG can be triggered in two ways, both enabled by manifest configuration:

### Trigger 1: Automatic Retrieval (MemoryManager Retrieve Phase)

Automatically triggered every iteration, using the current user message as query, lightweight (top_k=3):

```
Step ② MemoryManager.retrieve()
  ├─ SQLite 记忆层 channel: hybrid_search + graph_expand  ← always executed
  └─ RAG channel: RagClient.query(user_message, top_k=3)  ← only if manifest declares RAG
     ├─ Success → results annotated by source [Memory] / [RAG:enterprise_knowledge]
     ├─ Timeout (5s) → skip RAG channel, use only SQLite 记忆层 results
     └─ Unreachable → same, does not block Agent
```

### Trigger 2: Explicit Tool Call (Tool Dispatch Phase)

LLM actively calls the RAG tool for targeted in-depth queries:

```
Step ⑤ Tool Dispatch
  └─ LLM outputs tool_call: enterprise_knowledge(query="Q3 product roadmap", top_k=10)
     ├─ Permission Check: rag:query + network:<endpoint_url>
     ├─ Fetch credentials from Vault
     ├─ RagClient.query(query, top_k=10, filters=...)
     └─ Return results with source_url / chunk_id
```

### Deduplication Strategy

Results from the automatic channel are injected as "background context" into the system prompt; results from explicit tool calls are appended to conversation history as "tool return values". They occupy different positions in the context and do not semantically overlap.

---

## 8. Security Constraints

| Constraint           | Description                                                                 |
| -------------------- | --------------------------------------------------------------------------- |
| HTTPS Mandatory      | RAG endpoint must use HTTPS; HTTP is rejected by permission checks          |
| Dual Permission Check | Must hold both `rag:query` and `network:<endpoint>` permissions            |
| Vault Authentication | Credentials are not exposed in plaintext in manifest or environment         |
| Timeout Limit        | Default 10s, configurable; timeout does not block Bot                    |
| Network Whitelist    | RAG endpoint must be within the network whitelist declared in manifest      |

---

## 9. Phase 6 Protocol Evolution Roadmap

The protocol includes `protocol_version` and `extensions` fields to reserve extensibility for Phase 6:

### 9.1 Expected Phase 6 Evolutions

| Evolution Item                 | Description                                                                 |
| ------------------------------ | --------------------------------------------------------------------------- |
| RagClient → RemoteMemoryStore  | Implement MemoryStore trait, support full hybrid_search + graph_expand API  |
| OAuth 2.0                      | Support authorization code grant and client credentials grant               |
| Multi-tenancy isolation        | Namespace/collection/index constraints (RAG-06)                            |
| Streaming responses            | Pagination/streaming for large result sets                                 |

### 9.2 Protocol Version Strategy

- `protocol_version: "1.0"` — Current Phase 4 version
- `protocol_version: "2.0"` — Phase 6 introduces MemoryStore-compatible protocol
- When version number changes, ACowork client chooses different parsing logic based on `protocol_version`
- Existing 1.0 response format remains backward-compatible in 2.0

### 9.3 Extensions Field

Both request and response reserve an `extensions` field for incremental extensions within a protocol version:

```json
{
  "protocol_version": "1.0",
  "query": "...",
  "top_k": 5,
  "extensions": {
    "x-custom-reranker": true,
    "x-tenant-id": "engineering"
  }
}
```

ACowork passes `extensions` through without interpretation. Enterprise RAG services may use this field to pass custom parameters.