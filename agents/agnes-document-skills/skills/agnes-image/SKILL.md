---
name: agnes-image
description: Generate or edit images via Agnes AI agnes-image-2.0-flash model (text-to-image, image-to-image, multi-image composition, inpainting) through the OpenAI-compatible /v1/images/generations endpoint
version: "1.0.0"
author: document-manager
triggers:
  - 文生图
  - 图生图
  - agnes image
  - agnes-image
  - 生成图片
  - generate image
  - image generation
  - text to image
  - image to image
  - image editing
  - 多图合成
  - 图像编辑
tool_deps:
  - http_request
  - file_write
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

# agnes-image Skill

Generate, edit, and compose images with the Agnes AI `agnes-image-2.0-flash` model through a single OpenAI-compatible endpoint. The skill covers text-to-image (T2I), image-to-image (I2I), multi-image composition, and image editing workflows.

## When to use this skill

Activate this skill when the user wants to:

- Generate an image from a text prompt (文生图 / text-to-image)
- Transform or restyle an existing image from a text prompt (图生图 / image-to-image)
- Compose or blend multiple reference images into a new one (多图合成)
- Edit part of an image via a text prompt (图像编辑)
- Get an image as a hosted URL or as a Base64 string

Do **not** activate this skill for:

- Pure image understanding / captioning (use a vision LLM instead)
- Pure text generation, code generation, or chat (use an LLM skill)
- Video generation, audio generation, or 3D model generation (different models)

## Core Rule

Always include the user's intent in `prompt`, keep `model` fixed at `agnes-image-2.0-flash`, and explicitly choose an output mode (URL or Base64) before calling the API. Never invent parameters, headers, or endpoints that are not documented below.

## Authentication

The API uses a Bearer token. Resolve the API key in this priority order:

1. Environment variable `AGNES_API_KEY`
2. User-provided inline key in the current turn
3. If neither exists, **stop and ask the user** for the key — do not call the API

Never hardcode the API key in saved files, logs, or memory entries.

## Workflow

### Step 1: Clarify the task

- Confirm the mode: T2I (text only) / I2I (one or more input images) / editing
- Confirm the output format: hosted URL (default) or Base64
- Confirm the size bucket the user wants (e.g. `1024x1024`, `1024x768`, `768x1024`)
- For I2I, multi-image, or editing, collect every input image as either a public URL or a `data:image/...;base64,...` Data URI
- Use `mcp_memory__memory_recall` to check whether the user has stored preferred defaults (size, output format, style tags) before asking

### Step 2: Build the prompt

Recommended structure: `[Main subject] + [Scene / background] + [Style] + [Lighting] + [Composition] + [Quality requirements]`.

Example: `A clean product photo of a glass cube on a white studio background, soft shadows, high detail`

If the user supplies a vague prompt, expand it using the structure above and surface the expanded prompt back to the user before calling the API.

### Step 3: Build the request body

| Parameter | Type | Required | Notes |
|---|---|---|---|
| `model` | string | yes | Fixed: `agnes-image-2.0-flash` |
| `prompt` | string | yes | T2I or editing instruction |
| `size` | string | yes | e.g. `1024x768`, `1024x1024`, `768x1024` |
| `image` | string[] | I2I only | Public URL or `data:image/...;base64,...` |
| `return_base64` | boolean | no | T2I only: `true` returns Base64 instead of URL |
| `extra_body.response_format` | string | no | `url` (default) or `b64_json` |

Rules:

- T2I: include `model`, `prompt`, `size`; set `extra_body.response_format` to `url` or `b64_json`, **or** set `return_base64: true` as a shortcut for Base64
- I2I / multi-image / editing: include `model`, `prompt`, `size`, and `image` array. Do **not** use `return_base64` for I2I; use `extra_body.response_format` if Base64 is needed
- Only the parameters above are documented. Do not add `n`, `seed`, `guidance_scale`, `negative_prompt`, `style`, etc. unless the user explicitly asks and you have flagged that the parameter is not in the canonical doc

### Step 4: Call the API

```
POST https://apihub.agnes-ai.com/v1/images/generations
Headers:
  Authorization: Bearer ${AGNES_API_KEY}
  Content-Type: application/json
Body: <JSON from Step 3>
```

Use the `http_request` tool with `method=POST`, `url=https://apihub.agnes-ai.com/v1/images/generations`, `content_type=json`, and the JSON body assembled in Step 3. The Bearer header goes in the `headers` map. If the API returns 401, stop and ask the user to verify `AGNES_API_KEY`.

### Step 5: Handle the response

The response shape:

```json
{
  "created": 1780000000,
  "data": [
    {
      "url": "https://storage.googleapis.com/agnes-aigc/xxx.png",
      "b64_json": null,
      "revised_prompt": null
    }
  ]
}
```

Field reference:

| Field | Type | Meaning |
|---|---|---|
| `created` | integer | Request creation timestamp (epoch seconds) |
| `data` | array | Generated image results, one entry per image |
| `data[].url` | string / null | Hosted image URL, usually `null` when Base64 is requested |
| `data[].b64_json` | string / null | Base64 image data, usually `null` when URL is requested |
| `data[].revised_prompt` | string / null | Model-revised prompt, `null` if not revised |

Choose what to return based on the original task:

- T2I URL mode → return `data[0].url` as a markdown image link
- T2I Base64 mode → write the decoded PNG/JPEG to disk via `file_write` and return the file path
- I2I / multi-image / editing → same handling, treat the first result as the primary output

Always show the resolved prompt, the request size, the output mode, and the resulting image/link in the final reply. If `revised_prompt` is non-null, surface it as a hint that the model interpreted the prompt differently than expected.

### Step 6: Persist learning (optional)

Use `mcp_memory__memory_store` to record only durable user preferences, such as:

- Default size (e.g. `1024x1024`)
- Default output format (URL vs Base64)
- Recurring style tags or brand vocabulary

Never store API keys, raw prompts containing private data, or generated image Base64 payloads in memory.

## API Reference

### Endpoint

```
POST https://apihub.agnes-ai.com/v1/images/generations
```

### Headers

```
Authorization: Bearer YOUR_API_KEY
Content-Type: application/json
```

### Request body (full schema)

| Field | Type | Required | Description |
|---|---|---|---|
| `model` | string | yes | Model name, fixed at `agnes-image-2.0-flash` |
| `prompt` | string | yes | Text prompt describing the target image or the edit |
| `size` | string | yes | Output size, e.g. `1024x768`, `1024x1024`, `768x1024` |
| `image` | string[] | I2I required | Input image array, public URL or Data URI Base64 |
| `return_base64` | boolean | no | T2I only: return Base64 instead of URL |
| `extra_body.response_format` | string | no | Output format, common values `url` or `b64_json` |

### Response body

```json
{
  "created": 1780000000,
  "data": [
    {
      "url": "https://storage.googleapis.com/agnes-aigc/xxx.png",
      "b64_json": null,
      "revised_prompt": null
    }
  ]
}
```

## Examples

### 1. Text-to-image, URL output

```bash
curl https://apihub.agnes-ai.com/v1/images/generations \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "agnes-image-2.0-flash",
    "prompt": "A clean product photo of a glass cube on a white studio background, soft shadows, high detail",
    "size": "1024x768",
    "extra_body": { "response_format": "url" }
  }'
```

Result lives at `data[0].url`.

### 2. Text-to-image, Base64 output

```bash
curl https://apihub.agnes-ai.com/v1/images/generations \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "agnes-image-2.0-flash",
    "prompt": "A clean product photo of a glass cube on a white studio background, soft shadows, high detail",
    "size": "1024x768",
    "return_base64": true
  }'
```

Result lives at `data[0].b64_json`.

### 3. Image-to-image (inferred pattern)

```json
{
  "model": "agnes-image-2.0-flash",
  "prompt": "Restyle this product photo in a watercolor illustration style, keep composition",
  "size": "1024x768",
  "image": [
    "https://example.com/source.png"
  ]
}
```

> Note: I2I, multi-image composition, and image editing are documented as supported by the model. Always confirm with the user which reference images to use and what edit to apply.

## Red Flags

Stop and clarify if you see:

- No API key available and the user did not provide one inline
- A size value not in the documented set (verify before sending)
- A request that mixes `return_base64: true` with I2I (the doc marks `return_base64` as T2I-only)
- The user wants parameters that are not in the documented schema (e.g. `n`, `seed`, `negative_prompt`, `style`, `guidance_scale`); surface that the parameter is undocumented before sending
- The response is `401` / `403` → ask the user to verify or rotate `AGNES_API_KEY`
- The response is `429` → wait and retry with backoff; surface quota concerns to the user
- The response is `5xx` → retry up to 2 times; if it still fails, surface the error and stop

## Related Documents

- Canonical doc: `https://agnes-ai.com/doc/agnes-image-20-flash` (served as a client-rendered React page; body fetched manually and transcribed into this skill)
- Skill location: `C:\Users\nicholas\.acowork\acowork-gateway\config\packages\com.acowork.document-manager\skills\agnes-image\SKILL.md`
- Credential file: `<workspace>/.env` (see Authentication section; `.env.example` provides the template)

## Metadata

- **Owner**: document-manager
- **Status**: Current
- **Last reviewed**: 2026-06-24
- **Review cadence**: when Agnes AI publishes an update to `agnes-image-2.0-flash` parameters or endpoint
- **Source of truth**: `https://agnes-ai.com/doc/agnes-image-20-flash`
