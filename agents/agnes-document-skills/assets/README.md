# ACowork Brand Logo Assets

Generated on 2026-06-24 with the `agnes-image-2.0-flash` model.
All assets are flat 2D vector-style raster PNGs, 1024×1024, white background, no 3D / shadows / gradients.

## Variants

| File | Variant | Color | Use case |
|---|---|---|---|
| `acowork-logo-v1.png` | Wordmark with embedded node network (A counter) | Google Blue `#1A73E8` | Default brand mark, "ACowork" hero use |
| `acowork-logo-v2-wordmark-blue.png` | Pure wordmark, no icons | Google Blue `#1A73E8` | App icon, favicon, clean wordmark, document header |
| `acowork-logo-v3-app-icon.png` | Square blue tile + 3-node network mark | Google Blue `#1A73E8` tile, white mark | App launcher icon, tab icon, splash screen |
| `acowork-logo-v4-lockup.png` | Mark + wordmark horizontal lockup | Google Blue `#1A73E8` | Website header, product splash, slide deck |
| `acowork-logo-v5-wordmark-black.png` | Pure black wordmark + node network | Pure black `#000000` | Dark backgrounds, single-color print, fax, engraving |

## Generation parameters (per variant)

All variants used:

- `model`: `agnes-image-2.0-flash`
- `size`: `1024x1024`
- `output mode`: `url` via `extra_body.response_format=url`
- `Authorization`: read from `D:\projects\tranxon\agent-cowork\document-manager\.env` → `AGNES_API_KEY`

Variant-specific prompts and the design intent for each are documented in the chat history that produced them.

## Recommended downstream sizes (not yet generated)

| Source | Targets | When |
|---|---|---|
| `acowork-logo-v2-wordmark-blue.png` | 16, 32, 64, 128, 256, 512 px | Favicon + tab icons |
| `acowork-logo-v3-app-icon.png` | 16, 32, 64, 128, 256, 512, 1024 px | App icon set for `apps/acowork-desktop` |
| `acowork-logo-v5-wordmark-black.png` | same as v2 | Dark theme assets |

Use any local image resizer (e.g. `magick`, `sips`, or the `sharp` Node lib already used by the desktop app) to downscale. The vector-clean source means scaling to small sizes stays crisp.

## Brand integration map (where these should land in the project)

| Location | Recommended asset | Note |
|---|---|---|
| `apps/acowork-desktop/src/components/layout/SplashScreen.tsx` | v3 app icon | Currently shows "ACowork" text only |
| `apps/acowork-desktop/src/components/layout/TitleBar.tsx` | v4 lockup or v2 wordmark | Currently shows text "ACowork" |
| `apps/acowork-desktop/src-tauri/icons/` (if present) | v3 resized set | Tauri build needs multi-size icon set |
| `apps/acowork-desktop/package.json` `description` | n/a | Could embed v2 as OG image |
| `assets/architecture.svg` | n/a | Could add a small v2 mark to the system agent box |
| README / docs header | v4 lockup | Top of every doc page |
| Favicon (`public/favicon.ico`) | v3 at 32px | Browser tab icon |

## Provenance

- Source of generation request: user chat on 2026-06-24
- Generation tool: `agnes-image` skill installed under `skills/agnes-image/SKILL.md`
- API: `https://apihub.agnes-ai.com/v1/images/generations`
- License: subject to the Agnes AI terms of service; verify commercial use rights before publishing
- No tracking pixel, no embedded metadata beyond standard PNG chunks
