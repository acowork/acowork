# RAG 标准查询协议 — 企业接入指南

> 版本：1.0 | 协议版本：1.0 | 更新日期：2026-04-27
> 模块：acowork-runtime (Phase 4 S4)

---

## 1. 概述

ACowork 定义了一套标准 HTTP 查询协议，企业 RAG 服务适配此协议后即可作为 Bot 的扩展检索通道。ACowork **不实现 RAG 引擎**，不为各家 RAG 实现 adapter，而是要求企业侧确保其服务兼容本协议。

### 核心原则

- **纯对接，不托管**：ACowork 是 HTTP 客户端，不托管 RAG 数据
- **配置驱动 Opt-In**：仅当 Bot manifest 声明 RAG 时才启用
- **优雅降级**：RAG 不可达时返回空结果，不阻塞 Bot 执行
- **安全优先**：endpoint 必须为 HTTPS，认证走 Vault 管理

---

## 2. 请求协议

### 2.1 HTTP 请求

```
POST <endpoint>
Content-Type: application/json
Authorization: Bearer <token>       # Bearer 认证（可选）
X-API-Key: <key>                    # API Key 认证（可选）
```

### 2.2 请求体 JSON Schema

```json
{
  "$schema": "http://json-schema.org/draft-07/schema#",
  "type": "object",
  "required": ["protocol_version", "query", "top_k"],
  "properties": {
    "protocol_version": {
      "type": "string",
      "const": "1.0",
      "description": "协议版本，当前固定为 1.0"
    },
    "query": {
      "type": "string",
      "description": "查询文本"
    },
    "collection": {
      "type": "string",
      "description": "集合/索引名称（可选，来自 manifest 配置）"
    },
    "top_k": {
      "type": "integer",
      "minimum": 1,
      "maximum": 100,
      "description": "返回结果最大数量"
    },
    "score_threshold": {
      "type": "number",
      "minimum": 0.0,
      "maximum": 1.0,
      "description": "最低相关性阈值（可选）"
    },
    "filters": {
      "type": "object",
      "description": "企业自定义过滤条件（可选）",
      "additionalProperties": true
    },
    "extensions": {
      "type": "object",
      "description": "协议扩展字段（保留，Phase 6 使用）",
      "additionalProperties": true
    }
  }
}
```

### 2.3 请求示例

**自动检索（MemoryManager Retrieve 阶段）**：

```json
{
  "protocol_version": "1.0",
  "query": "Q3 产品路线图",
  "collection": "product_docs",
  "top_k": 3,
  "score_threshold": 0.7
}
```

**显式工具调用（LLM 触发 rag_query）**：

```json
{
  "protocol_version": "1.0",
  "query": "VPN 远程访问策略",
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

## 3. 响应协议

### 3.1 成功响应

HTTP 200 + JSON body：

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
            "description": "结果文本内容"
          },
          "source_url": {
            "type": "string",
            "description": "来源文档 URL（可选）"
          },
          "chunk_id": {
            "type": "string",
            "description": "文档内片段 ID（可选）"
          },
          "score": {
            "type": "number",
            "minimum": 0.0,
            "maximum": 1.0,
            "description": "相关性分数"
          }
        }
      }
    },
    "extensions": {
      "type": "object",
      "description": "协议扩展字段（保留）",
      "additionalProperties": true
    }
  }
}
```

### 3.2 响应示例

```json
{
  "protocol_version": "1.0",
  "results": [
    {
      "content": "Q3 产品路线图包含 AI 助手功能，预计 7 月发布",
      "source_url": "https://wiki.corp.example.com/roadmap-q3",
      "chunk_id": "roadmap-3",
      "score": 0.92
    },
    {
      "content": "工程团队 Q3 交付计划：基础架构升级 + 新功能开发",
      "source_url": "https://wiki.corp.example.com/eng-plan",
      "chunk_id": "eng-7",
      "score": 0.85
    }
  ]
}
```

### 3.3 空结果

```json
{
  "protocol_version": "1.0",
  "results": []
}
```

### 3.4 错误响应

RAG 服务返回非 2xx 状态码时，ACowork 视为查询失败，触发优雅降级（返回空结果）。建议 RAG 服务在错误时返回 JSON：

```json
{
  "error": "internal_server_error",
  "message": "Index temporarily unavailable"
}
```

---

## 4. 认证配置

### 4.1 认证方式

| 方式         | 请求头                          | manifest 配置           |
| ------------ | ------------------------------- | ----------------------- |
| Bearer Token | `Authorization: Bearer <token>` | `auth_type = "bearer"`  |
| API Key      | `X-API-Key: <key>`              | `auth_type = "api_key"` |
| 无认证       | 无                              | 不设置 `auth_ref`       |

> OAuth 2.0 支持留 Phase 6 实现。

### 4.2 认证信息管理

认证信息通过 Vault 统一管理，**不暴露在 manifest 或进程环境中**：

```toml
# manifest.toml
[[tools]]
type = "rag"
name = "enterprise_knowledge"

[tools.rag]
endpoint = "https://rag.corp.example.com/v1/query"
auth_ref = "vault:rag_enterprise_key"    # Vault 引用
auth_type = "bearer"                      # 认证方式
```

Vault 引用格式：`vault:<provider_name>`

- Runtime 启动时通过 IPC 从 Gateway Vault 获取实际 Key 值
- Key 使用 `secrecy::SecretString` 保护，不进入日志或堆栈

---

## 5. Manifest 配置参考

### 5.1 完整配置

```toml
[[tools]]
type = "rag"
name = "enterprise_knowledge"           # 工具显示名（LLM 看到的名字）

[tools.rag]
endpoint = "https://rag.corp.example.com/v1/query"  # 必须 HTTPS
collection = "product_docs"             # 可选：指定集合
auth_ref = "vault:rag_enterprise_key"   # 可选：Vault 认证引用
auth_type = "bearer"                    # 认证方式：bearer / api_key
max_results = 5                         # 默认返回结果数
score_threshold = 0.7                   # 默认最低分数
timeout_secs = 10                       # 查询超时（秒）
```

### 5.2 必需权限声明

使用 RAG 工具的 Bot 必须声明以下权限：

```toml
[[permissions]]
type = "RagQuery"                       # RAG 查询权限

[[permissions]]
type = "Network"                        # 网络白名单（宽泛）
# 或精确指定 endpoint
# type = "Network"
# value = "https://rag.corp.example.com/v1/query"
```

> RAG endpoint 必须使用 HTTPS。HTTP endpoint 会被权限校验拒绝。

### 5.3 最小配置（无认证）

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

## 6. 企业 RAG 自适配示例

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

## 7. 双触发模型

RAG 有两种触发方式，均由 manifest 配置使能：

### 触发 1：自动检索（MemoryManager Retrieve 阶段）

每轮迭代自动触发，用当前用户消息作 query，轻量查询（top_k=3）：

```
步骤② MemoryManager.retrieve()
  ├─ SQLite 记忆通道: hybrid_search + graph_expand  ← 始终执行
  └─ RAG 通道: RagClient.query(用户消息, top_k=3)  ← 仅 manifest 声明 RAG 时
     ├─ 成功 → 结果按来源标注 [Memory] / [RAG:enterprise_knowledge]
     ├─ 超时(5s) → 跳过 RAG 通道，仅用 SQLite 记忆结果
     └─ 不可达 → 同上，不阻塞 Agent
```

### 触发 2：显式工具调用（Tool Dispatch 阶段）

LLM 主动调用 RAG 工具，用于针对性深入查询：

```
步骤⑤ Tool Dispatch
  └─ LLM 输出 tool_call: enterprise_knowledge(query="Q3产品路线图", top_k=10)
     ├─ Permission Check: rag:query + network:<endpoint_url>
     ├─ 从 Vault 获取认证凭据
     ├─ RagClient.query(query, top_k=10, filters=...)
     └─ 返回带 source_url / chunk_id 的结果
```

### 去重策略

自动通道结果作为"背景上下文"注入 system prompt，显式工具结果作为"工具返回值"追加到对话历史。两者在上下文中位置不同，语义不重叠。

---

## 8. 安全约束

| 约束       | 说明                                                  |
| ---------- | ----------------------------------------------------- |
| HTTPS 强制 | RAG endpoint 必须使用 HTTPS，HTTP 被权限校验拒绝      |
| 双权限校验 | 必须同时持有 `rag:query` 和 `network:<endpoint>` 权限 |
| Vault 认证 | 认证信息不在 manifest 或环境中明文暴露                |
| 超时上限   | 默认 10s，可配置，超时不阻塞 Bot                    |
| 网络白名单 | RAG endpoint 必须在 manifest 声明的 network 白名单内  |

---

## 9. Phase 6 协议演进路线

协议包含 `protocol_version` 和 `extensions` 字段，为 Phase 6 预留扩展能力：

### 9.1 Phase 6 预期演进

| 演进项                        | 说明                                                               |
| ----------------------------- | ------------------------------------------------------------------ |
| RagClient → RemoteMemoryStore | 实现 MemoryStore trait，支持 hybrid_search + graph_expand 完整 API |
| OAuth 2.0                     | 支持授权码模式、客户端凭证模式                                     |
| 多租户隔离                    | namespace/collection/index 约束（RAG-06）                          |
| 流式响应                      | 大结果集分页/流式返回                                              |

### 9.2 协议版本策略

- `protocol_version: "1.0"` — Phase 4 当前版本
- `protocol_version: "2.0"` — Phase 6 引入 MemoryStore 兼容协议
- 版本号变更时，ACowork 客户端根据 `protocol_version` 选择不同的解析逻辑
- 现有 1.0 响应格式在 2.0 中保持向后兼容

### 9.3 extensions 字段

请求和响应均预留 `extensions` 字段，用于协议版本内的增量扩展：

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

ACowork 透传 `extensions`，不做解释。企业 RAG 可利用此字段传递自定义参数。
