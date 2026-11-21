---
name: android-sensor-log
description: Log phone sensors and location - enumerate sensors, take windowed samples or run a bounded stream, record a GPS track, land the data as a file in the work dir, then read it back and summarize with real statistics
version: "1.0.0"
author: developer
triggers:
  - read sensors
  - log acceleration
  - record a track
  - gps trace
  - 传感器数据
  - 记录轨迹
  - 测一下
tool_deps:
  - android_sensor
  - android_location
  - android_device_status
  - file_read
---

# Android Sensor Log Skill

## Core Rule

Sensor data is a **file deliverable, never a context resident**. A 100 Hz window is thousands of events; the stream op writes it under `device-captures/` and you analyze the file. Your chat answer is the statistics — min/max/mean, trend, outliers — with the path quoted for the raw record.

## Execution Steps

1. **Know the hardware first.** `android_sensor` `op=list` before choosing anything: which `string_type`s exist, their `range`, `resolution`, `min_delay`/`max_delay` (the sampling floor you must not under-estimate), `power` (drain honesty), and `fifo_max_event_count` (batching ceiling). Phones vary wildly — a device without a barometer is not an error to retry, it is a fact to report.

2. **Pick the mode by question shape.**
   - "What is it right now" → `op=read` `window_ms=1000..3000`. One call, inline answer.
   - "How does it change over N minutes" → `stream_start` with `sampling` + `max_report_latency_ms`, bounded by a stop you will actually honor.
   - "Where did it go" → `android_location` `track` (see step 4).
   - Never leave a stream running because it "might be useful later" — each stream is an exclusive sensor claim and a battery line item. Max 2 streams per agent; the same sensor across agents is `busy`.

3. **Sample rates are physics, not preference.** `fastest` on an accelerometer at 200 Hz for 10 minutes is ~120k events and a large file; ask what the analysis needs. `ui` (60 ms) covers most human-scale motion; `game` (20 ms) for gesture-level; `fastest` only for vibration/acoustics. Batch with `max_report_latency_ms` when trend matters more than per-event timing — it is dramatically cheaper on battery.

4. **Location tracks.** `android_location` `op=track`: `provider=gps` outdoors (first fix can take 30 s cold — set expectations), `network` indoors where GPS is dead. `min_interval_ms` 5000 and `min_distance_m` 10-20 for a movement track (smaller values log your standing still). Duration cap 600 s per call — for longer, chain `track` calls and concatenate the files; say so up front. `status` first: if location services are off system-wide, that is a human-at-the-phone fix (or a quick-settings toggle the user can ask for), not a retry.
   - Outdoors, pull `GnssStatus` with the track (satellites used in fix, cn0) — a 30 m-accuracy fix is noise for "did I walk to the station" and you should report accuracy, not just coordinates.

5. **Read back and analyze.** `file_read` the artifact. For any window over a few thousand events, compute rather than eyeball: per-sensor min/max/mean/std, timestamp span, actual achieved rate (events ÷ span — compare with requested; the gap is the platform telling you `min_delay` was optimistic), and gaps in the timeline (a 30 s hole in a GPS track means tunnel or provider dropout — mark it, do not interpolate silently).

6. **Deliver.** Chat answer: the finding first ("walked 2.1 km, 24 min, two stops"), then the statistics that support it, then the artifact path. The user can ask for the raw file later; do not paste it.

## Battery and Presence Honesty

The phone is remote and unattended; these calls spend a resource nobody can refill:

- Before any stream/track over ~2 minutes, check `android_device_status` `battery`. Below ~20%, say what you are about to cost (`power` mA figures from `list`) and get a nod.
- Screen-off batching: without a wakelock the Doze cycle will stall your stream. If the data must be continuous, keep the screen on via `android_system_control` `screen_state` `keep_screen_on=true` with its timeout — and turn it off when the stream stops. A phone that dozes mid-track produces a plausible-looking, wrong line.
- `ACTIVITY_RECOGNITION` permission gates step counter/activity sensors on API 29+ — `permission_required` there is the one-time on-site setup, report it as such.

## Gotchas

- One-shot and on-change sensors (`TYPE_ONE_SHOT` flag, light/proximity style) have no "latest" — `read` without a window is meaningless; always `window_ms`.
- `TYPE_ACCELEROMETER` includes gravity (1 g on some axis at rest); "motion" usually wants `TYPE_LINEAR_ACCELERATION` if the device has it, else subtract gravity yourself and say which you did.
- Sensor fusion types (`STEP_DETECTOR` etc.) are events with 1.0/0.0 values — counting them is the analysis; do not average.
- Mock locations: the track result marks `is_from_mock_provider` — if a "GPS" track arrives with mock flags, report it; the user may be testing with a mock app and deserves to know which source answered.
