---
name: android-ble-connect
description: Work a BLE peripheral end to end - scan with filters, connect, discover services, negotiate MTU, subscribe to notifications, read and write characteristics, collect out-of-window notifications, and recover from disconnects
version: "1.0.0"
author: developer
triggers:
  - connect ble
  - bluetooth device
  - ble sensor
  - gatt
  - 蓝牙设备
  - 连接蓝牙
  - 蓝牙传感器
tool_deps:
  - android_ble_scan
  - android_ble_gatt
  - android_device_status
---

# Android BLE Connect Skill

## Core Rule

GATT is a state machine, not a function call. Every operation's meaning depends on the connection's current state, and the phone forgets your service cache on every new connection. Never issue a read/write/subscribe against a target you have not discovered **on this connection generation**.

## The Canonical Flow

```
status → scan → connect → discover_services → request_mtu → subscribe → (read/write) → disconnect → close
```

1. **Radio check.** `android_device_status` `sections=["bluetooth"]`: adapter on? LE supported? Pairing/bond state of the target if known. Adapter off is a user problem — report it, do not toggle (no public API).

2. **Scan.** `android_ble_scan` with `filters` narrowed to what the user told you (name, service UUID, manufacturer id). Multiple filters are OR, matching the platform — narrow by using ONE filter with several fields. `include_raw=true` when you need advertising data to pick the right device among duplicates. Default window 10 s; raise `duration_ms` for a slow advertiser, but the scan holds an exclusive lock — never park it "just in case".
   - Not found: rescan once with looser filters (name substring dropped, RSSI floor removed) before declaring absence. Peripherals sleep.

3. **Connect.** `android_ble_gatt` `op=connect` `address=<from scan>`. Choose `auto_connect=false` for interactive work (direct, faster, dies with the link); `true` only for bonded devices you want the stack to chase. `transport=le` for BLE-only peripherals. The result carries `connection_state` and the initial `mtu` (usually 23).

4. **Discover.** `op=discover_services` immediately after connect — mandatory. Then `op=list` for the full tree: services, instance ids, characteristic UUIDs + properties + permissions, descriptors. **Record the tree**; every later call names targets from it.

5. **Negotiate MTU.** `op=request_mtu` `mtu=247` (or 512 on API 30+ stacks that take it). The result carries the negotiated value — writes longer than `mtu-3` fail, so know your number before chunking.

6. **Subscribe before reading live data.** For a characteristic with notify/indicate: `op=subscribe` `notification=notify|indicate` (the tool writes the CCCD for you). Set `subscribe_wait_ms` for the in-call collection window. **Notifications outside the window are buffered in the bridge — collect them later with `op=status`.** A common bug pattern is "no data": the characteristic was written by the peripheral between windows and the data is sitting in `status`.

7. **Read / write.**
   - `op=read` / `op=write` `value_hex`. Write type: `default` (write-with-response, returns `gatt_status`), `no_response` for streams (fire-and-forget, no status — pace yourself, the stack drops silently).
   - Descriptors: `read_descriptor` / `write_descriptor` (e.g. the CCCD itself, manufacturer config blocks).
   - Every result carries `gatt_status` — `GATT_SUCCESS` (0) is the only pass. Non-zero: see the error table.

8. **Tear down honestly.** `op=disconnect`, then `op=close`. Skipping `close` leaks the GATT handle — the stack caps ~30-35 concurrent clients per app and then **every** connect fails with status 133 until process death. If you have ever connected and not closed, you are leaking.

## Error Branches

| Symptom | Meaning | Move |
|---|---|---|
| `gatt_status=133` on connect or mid-session | generic GATT failure; classically: leaked handle, device asleep, or a second client on the peripheral | `close`, wait ~1 s, reconnect once. Repeated 133: report — the peripheral or radio is the problem |
| `gatt_status=8` | connection timeout / supervision timeout (device out of range or sleeping) | Rescan to confirm presence before reconnecting |
| `gatt_status=22` | preparation queue needed (long write exceeds MTU) | Chunk to `mtu-3`, or use long-write prep if the stack surfaces it |
| `gatt_status=10` | status too low for a link parameter update | Ignore if `request_connection_priority` partially applied; re-check with `read_phy`/`status` |
| `busy` on connect | another agent holds this address | Name the holder (from `android_device_status` `bridge`), report, stop |
| Write rejected, length error | MTU not what you assumed | `op=status` for current `mtu`, re-chunk |

## Link Quality Knobs (only when the flow demands)

- `request_connection_priority` `high` for interactive latency, `low_power` for a battery logger.
- `set_preferred_phy` / `read_phy` — Coded PHY for range, 2M for throughput; the device must support it (`status` reports what took).
- `create_bond` only when the user wants pairing persisted; it raises a system dialog — a human at the phone taps it (same rule as install confirmations).

## Session Discipline

- One logical session: connect → work → disconnect+close. Do not leave connections parked "for later" — another agent or the user's own app may need the peripheral, and parked handles are how 133s propagate.
- `op=status` is your ground truth for: connected? which address? mtu? rssi? buffered notifications? Check it before concluding "the device sends nothing".
