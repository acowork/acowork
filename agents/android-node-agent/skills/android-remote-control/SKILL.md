---
name: android-remote-control
description: "Drive an unattended phone's UI from chat: observe screen or accessibility tree, act via intent-first then node/coordinate, verify the change, and fall back to clipboard paste for stubborn text fields"
version: "1.0.0"
author: developer
triggers:
  - open an app
  - tap on
  - fill the form
  - control the phone
  - 打开应用
  - 点一下
  - 操作手机
  - 帮我在手机上
tool_deps:
  - android_use
  - android_clipboard
  - android_intent
  - android_device_status
  - ask_user_question
---

# Android Remote Control Skill

## Core Rule

The user is not looking at the phone. Every action you take must be justified by an observation you made **this session**, and every claim of success must be backed by a re-observation after it. A tap that "should have worked" is not a result.

## Strategy Order — cheapest and most robust first

1. **Intent beats UI.** Wanting to open a screen? `android_intent` `launch` (app), `view` (URL/file/deep link), `share`, `dial`. One call, survives app redesigns, no screen needed at all.
2. **Node beats coordinate.** On-screen target found in the `snapshot` tree? `android_use` `node` with the `view_id`/`text`/`selector` — rotation and font scale cannot break it.
3. **Coordinate is the last resort.** `tap`/`swipe`/`gesture` with x/y only when the tree is empty (WebView, canvas, game, custom view) or `FLAG_SECURE` hid the nodes but the surface is still touchable.

## Execution Steps

1. **Check the rails once per session**
   - `android_use` `op=status`: accessibility connected? projection granted? display size/rotation.
   - Accessibility off → stop. Tell the user the exact path (`Settings → Accessibility → ACowork Host`) and that it needs a human at the phone. Do not proceed with coordinate-only guesses on an unrotated assumption.

2. **Observe**
   - `snapshot` first (text tree: class, view_id, text, contentDescription, bounds, clickable). It is cheaper than an image and usually sufficient.
   - `screenshot` when the tree is thin, when visual state matters (color, position, media), and always before a coordinate action.
   - Screen off / locked? `global` `home` may not wake a secured lockscreen — say so and ask the user to unlock remotely is impossible; a human must unlock. Report and stop.

3. **Act**
   - One meaningful action per step. Batched blind taps are how you end up in the wrong app.
   - Text input: `type` (focused editable or `selector`). If it fails or the field ignores `ACTION_SET_TEXT` (rich editors, WebView inputs), use the **clipboard fallback** below.

4. **Verify**
   - The action's follow-up screenshot (`include_screenshot`, default on) is your verify step — actually compare it against the expected state.
   - No change? Re-`snapshot`; the target may have been a non-clickable label next to the real button. Change strategy, do not repeat strategy.

5. **Recover**
   - Wrong screen / app stuck: `global` `back` (bounded retries, max ~3), then `global` `home`, then relaunch via intent. Never `back` blindly in a loop — each press changes state you cannot see.

## Clipboard Fallback for Text Fields

Some fields reject `ACTION_SET_TEXT` (IME-less WebViews, editors with custom input handling). The paste route:

1. `android_clipboard` `write` the exact text.
2. Focus the field: `node` action=focus, or `tap` its bounds.
3. Long-press the field (`long_press`, ~700 ms) → the context menu appears in the tree → `node` tap the item whose text is Paste/粘贴.
4. Verify the text landed with `snapshot`; re-`snapshot` can lag ~250 ms after a paste — the default `wait_ms` covers it, raise it for WebViews.

Note the platform limit: on Android 10+ you can **write** the clipboard but reading another app's clip is restricted. The fallback only depends on writing, so it always works.

## Known Blind Spots — name them, do not fight them

| Surface | Symptom | Correct move |
|---|---|---|
| `FLAG_SECURE` (passwords, banking, DRM) | black screenshot, empty tree | Report; the content is unreadable by design |
| System permission / install dialogs | visible in tree, taps ignored | Only a human can tap: describe the dialog, the button to press, and that someone must be at the phone |
| Lockscreen with PIN/pattern | no wake-through API | Report; needs a human |
| Notification shade via drag | opens, but its list is another service's tree | Use `android_notification` `list` to read, `dismiss` to clear — do not UI-scroll the shade |
| IME private key views | keyboard area is a black box | Tap visible keys by coordinate, or rely on `type`/IME action `ime_enter` |

## Concurrency

The screen is one lock shared by all agents on this phone. `busy` from any `android_use` action means another agent is operating the screen — check `android_device_status` `bridge` section for the holder, report it, and retry later. Do not poll in a tight loop.
