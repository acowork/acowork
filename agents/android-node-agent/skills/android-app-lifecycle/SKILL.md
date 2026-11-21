---
name: android-app-lifecycle
description: Manage apps at a distance - find and inspect installed packages, download an APK, drive the asynchronous install through the on-site system confirmation dialog, launch, and uninstall
version: "1.0.0"
author: developer
triggers:
  - install apk
  - uninstall app
  - what apps are installed
  - update the app
  - 安装应用
  - 卸载
  - 装个apk
tool_deps:
  - android_app_package
  - android_intent
  - http_request
  - ask_user_question
---

# Android App Lifecycle Skill

## Core Rule

**There is no silent install.** This app is not device owner: every `install` and `uninstall` ends in a system dialog that only a human next to the phone can tap. The tool call does not block for that — it returns `pending_confirmation`. Your job is to drive that state machine honestly: initiate, tell the user exactly what must happen at the phone, poll, and report the real terminal state.

## Execution Steps

### 1. Establish the current state

- `android_app_package` `op=list` with `filter` (`launchable` for "what can the user open", `user` for installed-by-user) or `query` for a name fragment.
- `op=info` + `package_name` before any change: version, enabled, signature digest. The info result is your **rollback reference** — you cannot downgrade past a signature mismatch, so know what is there before you overwrite it.
- If `data.visibility_limited` is true, `QUERY_ALL_PACKAGES` is missing: the list is the visible set only. Say so; a "not installed" conclusion from a filtered list is not a fact.

### 2. Get the APK (install only)

- From a URL the user provided: `http_request` download into the agent work dir. The install path **must** be inside work dir — a path outside it is `invalid_argument` by design.
- Verify what you downloaded before installing: size sanity, and when the user cares about provenance, the SHA-256 against what they gave you.
- No store/Play path exists in the public SDK. If the user expects "install from Play", open the listing instead: `android_intent` `view` with `market://details?id=<pkg>` (or the https play URL) — a human still finishes it.

### 3. Initiate

- `op=install`, `source_path=<work-dir apk>`. Optional `flags` only when the user's situation demands and understands them (`allow_downgrade` etc.).
- Expected immediate result: `ok=true`, `data.state="pending_confirmation"`, `data.session_id`. **This is not success yet.** The install has not happened.

### 4. The on-site confirmation dance

Tell the user, in chat, in one message:

1. Someone must go to the phone.
2. The system installer dialog is on screen (or will appear within seconds).
3. Exactly which button confirms ("Install" / "继续安装"), and which package it names — so they can see it is the right dialog.
4. That you will report the outcome when they are done.

Then poll `op=status` with the `session_id` — every ~15-30 s, not in a tight loop. Terminal states:

| Result | Meaning | Your move |
|---|---|---|
| `SUCCESS` | installed | `op=info` to confirm version, report, offer launch |
| `FAILED_ABORTED` | dialog dismissed / cancelled at the phone | Ask whether to retry; a new `install` call starts a new session |
| `confirmation_timeout` | nobody tapped within the window (~5 min) | Nothing happened; re-initiate only with user go-ahead |
| `FAILED_INSUFFICIENT_STORAGE` / `FAILED_NO_SPACE` | disk full | Report with the storage numbers from `android_device_status` `storage` |
| `FAILED_UPDATE_INCOMPATIBLE` | signature mismatch with the installed package | Explain: uninstall first (destructive — user confirms), or obtain the correctly signed APK |
| `FAILED_INVALID_APK` | corrupt or not an APK | Re-download or re-check the source |

### 5. Launch and verify

- `android_intent` `op=launch` `package_name=<pkg>`.
- Confirm it actually came up: `android_use` `screenshot` (or the launch result). Report the observed state, not the attempted one.
- `launch` failing with no resolvable activity on a fresh install usually means the APK has no LAUNCHER intent — it may be a service/library package. `op=info` `is_launchable` tells you; report the fact.

### 6. Uninstall

- Restate the exact `package_name` and the app label in chat and get explicit go-ahead — uninstall deletes the app's local data, including anything the user might still need from it.
- `op=uninstall` → `pending_confirmation` → same on-site dance, same polling via `op=status`.
- Verify with `op=info`: package gone.

## Pre-flight: the special-access gate

`install` before `REQUEST_INSTALL_PACKAGES` is granted returns `permission_required`. That toggle lives in system settings (`Settings → Apps → ACowork Host → Install unknown apps`) and needs a human at the phone **once**. Ask for it before starting a download, not after — discovering it mid-flow wastes the user's trip to the phone.

## Never

- Never claim "installed" on a `pending_confirmation` response.
- Never re-initiate an install while a session is still pending — check `op=status` first; parallel sessions for the same package abort each other.
- Never uninstall to "fix" a signature conflict without the user's explicit approval of the data loss.
