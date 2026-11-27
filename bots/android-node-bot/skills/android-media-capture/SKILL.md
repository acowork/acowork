---
name: android-media-capture
description: Capture photos, video, audio, and screen recordings on the phone, land the artifacts in the work dir, and bring the result back to the user through chat with the right preview and path
version: "1.0.0"
author: developer
triggers:
  - take a photo
  - record video
  - record audio
  - screen recording
  - 拍张照片
  - 录个屏
  - 录一段音
tool_deps:
  - android_photo
  - android_video_record
  - android_audio_record
  - android_use
  - android_intent
  - image_read
  - file_read
---

# Android Media Capture Skill

## Core Rule

A capture is not done until the user has **seen or received it in chat**. The file sitting in the work dir is the middle state, not the deliverable. Photos ship with a preview; video and audio ship with path + facts (duration, size) — never into context, always as an artifact.

## Route Table

| Ask | Tool | Why |
|---|---|---|
| "take a photo" | `android_photo` | One JPEG, Camera2, preview attaches automatically |
| "show me the screen" | `android_use` `screenshot` | The phone's display, not the camera |
| "record the screen" | `android_use` `record_start` / `record_stop` | Motion over time on the display — animations, loading, video playback |
| "record video (camera)" | `android_video_record` | Camera + mic to a file |
| "record audio" | `android_audio_record` | Mic to a file |
| "play that back on the phone" | `android_audio_play` | Legitimate instrument use (testing a speaker path, a voice app) — not a user alert |

Do not confuse the camera with the screen and vice versa: "what do you see" is a screenshot; "what's in front of the phone" is a photo. Ask once if genuinely ambiguous.

## Execution Steps

1. **Pre-flight.** `android_device_status` `sections=["camera","audio_devices","permissions","battery"]`: which lens exists, which mic, which permission is missing, and — for anything over ~1 minute of recording — whether the battery can survive it. A dead phone mid-capture loses the file and the connection.

2. **Capture.** Defaults are sane; override only what the request implies:
   - Photo: `lens_facing` (back default), `warmup_frames` when it is the first shot after a lens switch (AE/AF convergence — the first frame of a cold camera is often dark or soft).
   - Video: `duration_ms` for one-call records; for open-ended, `start`/`stop` with `max_duration_ms` as a safety net (default 10 min).
   - Audio: container by use — `aac_m4a` for general, `wav`/`raw_pcm` when it will be analyzed, not listened to. `effects` (aec/ns/agc) only when the source justifies it.
   - Screen: `record_start` then `record_stop`; `max_duration_ms` guards a forgotten session. The screen lock is shared with screenshots — while recording, `screenshot` returns `busy`; use `record_status` to watch it.

3. **Deliver in chat.**
   - Photo: the tool result already carries the downscaled preview; quote the `path` alongside it so the user can ask for the full-res file later.
   - Video/audio/screen: report `path`, `duration_ms`, `size_bytes`, `mime`. Read nothing back into context.
   - Verify before claiming: for photo, glance at the preview (is the subject actually in frame? is it black?); for files, the result fields. "I recorded it" without the duration is an unverified claim.

4. **Escalate on request.** "Send this to the messaging app" → `android_intent` `share` with the `path` (FileProvider handles the URI — a raw `file://` path would be refused by the receiving app since Android 7). "Put it in the gallery" → it is deliberately **not** in the gallery (media never enters the system library from these tools); say so and offer the share route instead.

## Locks and Conflicts

- One camera, one mic, one screen — each a single lock across all agents. `busy` names the holder: report it, do not queue.
- Recording and playback may run together (the bridge allows up to 4 playback streams); a second recorder may not.
- An audio record colliding with `stt` (both mic): whichever called second gets `busy`. Release explicitly (`stop`) before switching modes rather than hoping the window expired.
- API 34+ revokes a MediaProjection grant after one use: a screen-record or screenshot failing with a projection error after previously working means the consent expired — a human re-taps the projection dialog once.

## Known Limits — state them, don't fight them

- `FLAG_SECURE` screens record black (DRM video, banking). The recording "succeeds" and is useless — check the first frames of anything sensitive and say so.
- Screen recording does not capture system audio reliably below API 29 and by default records silent video (`audio=false`); if the user needs "the sound from the phone", the honest answer is: mic recording alongside, or not possible remotely.
- Camera flash, zoom, and manual exposure are `capture_request` keys (`CONTROL_AE_MODE`, `FLASH_MODE`, `SCALER_ZOOM_RATIO`...), not separate tools — reach for them only when defaults miss the shot.
