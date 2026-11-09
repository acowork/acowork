---
name: agnes-image-quickref
description: Quick-reference appendix for the agnes-image skill (env vars, cURL snippets, prompt templates, size cheat sheet, error handling). Load this when the user wants a fast lookup instead of the full skill.
version: "1.0.0"
author: document-manager
parent: agnes-image
triggers:
  - agnes image cheatsheet
  - agnes-image quickref
  - agnes image 速查
tool_deps:
  - http_request
  - file_write
  - mcp_memory__memory_recall
---

# agnes-image Quick Reference

One-page lookup for the `agnes-image-2.0-flash` model. Use this when you already know the workflow and just need the parameters, sizes, or a copy-pasteable command.

## Environment

| Source | Variable | Purpose | Required |
|---|---|---|---|
| `<workspace>/.env` file (recommended) | `AGNES_API_KEY` | Bearer token for `apihub.agnes-ai.com` | yes |
| System env var | `AGNES_API_KEY` | Fallback if `.env` is absent | yes |

Resolution order: `.env` file → system env var → user-provided inline key → ask user.

Set up the `.env` file:

```bash
cp .env.example .env
# then edit .env and replace AGNES_API_KEY with your real key
```

Or set the env var directly:

```bash
# Windows PowerShell
$env:AGNES_API_KEY = "sk-your-key"
# bash / Git Bash
export AGNES_API_KEY="sk-your-key"
```

> **Security**: never commit `.env` to git (it is already in `.gitignore`), never paste the key into `SKILL.md` or chat logs, and rotate the key in the Agnes AI console if it ever leaks.

## Endpoint

```
POST https://apihub.agnes-ai.com/v1/images/generations
```

Headers:

```
Authorization: Bearer ${AGNES_API_KEY}
Content-Type: application/json
```

## Parameters at a glance

| Param | Type | Required | T2I | I2I | Notes |
|---|---|---|---|---|---|
| `model` | string | yes | ✓ | ✓ | Always `agnes-image-2.0-flash` |
| `prompt` | string | yes | ✓ | ✓ | Required for every call |
| `size` | string | yes | ✓ | ✓ | See size cheat sheet below |
| `image` | string[] | yes | ✗ | ✓ | Public URL or Data URI Base64 |
| `return_base64` | boolean | no | ✓ | ✗ | Shortcut for T2I Base64 output |
| `extra_body.response_format` | string | no | ✓ | ✓ | `url` (default) or `b64_json` |

## Size cheat sheet

Documented examples:

- `1024x1024` — square (1:1)
- `1024x768` — landscape (4:3)
- `768x1024` — portrait (3:4)

Other size values are not in the canonical doc — verify with the user before sending.

## Copy-paste cURL snippets

### Text-to-image → URL

```bash
curl -s https://apihub.agnes-ai.com/v1/images/generations \
  -H "Authorization: Bearer $AGNES_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "agnes-image-2.0-flash",
    "prompt": "A clean product photo of a glass cube on a white studio background, soft shadows, high detail",
    "size": "1024x768",
    "extra_body": { "response_format": "url" }
  }'
```

### Text-to-image → Base64

```bash
curl -s https://apihub.agnes-ai.com/v1/images/generations \
  -H "Authorization: Bearer $AGNES_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "agnes-image-2.0-flash",
    "prompt": "A clean product photo of a glass cube on a white studio background, soft shadows, high detail",
    "size": "1024x768",
    "return_base64": true
  }'
```

### Image-to-image → URL (multi-image and editing use the same shape)

```bash
curl -s https://apihub.agnes-ai.com/v1/images/generations \
  -H "Authorization: Bearer $AGNES_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "agnes-image-2.0-flash",
    "prompt": "Restyle this product photo in a watercolor illustration style, keep composition",
    "size": "1024x768",
    "image": ["https://example.com/source.png"]
  }'
```

## Prompt template

Structure: `[Main subject] + [Scene / background] + [Style] + [Lighting] + [Composition] + [Quality requirements]`

Example slots:

- Main subject: `a vintage bicycle leaning against a brick wall`
- Scene / background: `in a quiet alley at golden hour`
- Style: `cinematic photography, 35mm film grain`
- Lighting: `warm side light, soft rim light`
- Composition: `centered subject, shallow depth of field`
- Quality requirements: `high detail, 8k, sharp focus`

Combined prompt: `A vintage bicycle leaning against a brick wall, in a quiet alley at golden hour, cinematic photography with 35mm film grain, warm side light with soft rim light, centered subject with shallow depth of field, high detail, 8k, sharp focus.`

## Response cheat sheet

URL mode:

```json
{
  "created": 1780000000,
  "data": [
    { "url": "https://storage.googleapis.com/agnes-aigc/xxx.png", "b64_json": null, "revised_prompt": null }
  ]
}
```

Base64 mode:

```json
{
  "created": 1780000000,
  "data": [
    { "url": null, "b64_json": "iVBORw0KGgo...", "revised_prompt": null }
  ]
}
```

Read `data[0].url` for URL mode and `data[0].b64_json` for Base64 mode. `data[0].revised_prompt` is non-null only when the model rewrote the prompt.

## Error handling cheat sheet

| Status | Action |
|---|---|
| 401 / 403 | Stop, ask the user to verify or rotate `AGNES_API_KEY` |
| 429 | Wait with backoff, retry up to 2 times, surface quota concerns |
| 5xx | Retry up to 2 times, then surface the error and stop |
| Schema error (e.g. user requests `seed` or `negative_prompt`) | Flag that the parameter is undocumented before sending |

## See also

- Full skill: `agnes-image/SKILL.md`
- Canonical doc: `https://agnes-ai.com/doc/agnes-image-20-flash`
