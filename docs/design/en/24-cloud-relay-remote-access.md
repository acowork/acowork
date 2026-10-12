# 24-cloud-relay-remote-access — Thin Cloud Relay for Desktop and Mobile Access to a Private Gateway

> **Version:** v0.3.0 (protocol frozen at `proto = 1`)  
> **Status:** In progress; self-hosted BYO relay tier.  
> **Canonical source:** [Chinese design document](../zh/24-cloud-relay-remote-access.md). This English counterpart records the architecture and MQTT bridge contract relevant to the mobile app; consult the canonical document for the complete design, rollout plan, and operational details.

## 1. Summary

A Gateway establishes one outbound multiplexed tunnel (WSS + yamux) to a self-hosted relay. The relay is an SNI-routed byte pipe: it does not interpret HTTP or MQTT, and the MQTT broker remains embedded in the Gateway. HTTP and MQTT-over-WebSocket traffic terminate at dedicated loopback listeners on the Gateway. User authentication is performed by the Gateway's account system; the relay has no user accounts and does not validate user tokens.

The relay is an optional, self-hosted BYO access mode, not the only remote-access solution. Its protocol is frozen at `proto = 1`; only security fixes are in scope.

## 2. Gateway listeners and MQTT bridge

| Listener | Address | Purpose |
|---|---|---|
| Main HTTP | `127.0.0.1:19876` | Local/LAN API and the LAN `/mqtt` WebSocket bridge when the remote MQTT listener is enabled. |
| Main MQTT | `127.0.0.1:19875` | Embedded broker listener for local Gateway traffic. |
| Remote HTTP | `127.0.0.1:19877` | Router with the RemoteOrigin guard; hosts the relay `/mqtt` WebSocket bridge. |
| Remote MQTT | `127.0.0.1:19874` | Embedded broker's strict-auth listener; reachable through the `/mqtt` WebSocket bridge. |

The `/mqtt` endpoint is a WebSocket-to-TCP byte bridge and does not authenticate MQTT itself. Relay clients reach it through the remote HTTP listener on `:19877`; LAN clients reach the same bridge through the main HTTP listener on `:19876`. Both bridges forward to the strict MQTT listener on `:19874`, which validates CONNECT credentials against the Gateway's user service. If account authentication is unavailable, CONNECT is refused (fail closed).

This LAN bridge allows mobile clients to use the same MQTT-over-WS channel on both LAN and relay topologies. The URL is derived from the saved Gateway base URL: `ws://{host}:{port}/mqtt` for HTTP/LAN and `wss://{relay-device-domain}/mqtt` for HTTPS/relay. The WebSocket handshake uses the `mqtt` subprotocol.

## 3. Trust and authorization boundary

The relay authenticates the Gateway tunnel using the device identity protocol and applies connection-level limits. It does not inspect HTTP/MQTT payloads or authorize user topics. HTTP requests and MQTT CONNECT authentication are handled at the Gateway.

The strict MQTT listener authenticates the user but currently does not enforce per-user topic-level authorization. In particular, broad subscriptions such as `acowork/agents/+/status` can expose lifecycle status for every agent on that Gateway to any valid user token. See [Mobile App §8.7](./25-mobile-app.md#87-known-gaps-non-blocking-for-v1-but-review-before-external-launch) for the mobile-specific implications and tracked authorization gap.

## 4. Mobile presence channel

Mobile maintains a separate MQTT connection for global agent presence, independent of the session-scoped realtime connection. It subscribes to `acowork/agents/+/status` at QoS 1 and consumes `DataEnvelope.agent_status` (field 9). Retained status plus heartbeat and Last Will replay the latest verdict after reconnect. The client maps the wire lifecycle verdict to the same `offline` / `error` / `idle` vocabulary as the HTTP directory. Presence starts when authentication reaches `ready` and stops on logout or session expiry; the directory poll remains the fallback source when the bridge is unavailable.

The presence client uses a distinct MQTT client ID from the session client: MQTT 3.1.1 replaces an existing connection when a duplicate client ID connects. The `-presence` suffix is part of the device segment because the strict listener accepts only the established three-colon-segment client ID shape.

## 5. References

- [Mobile App design, §8 Realtime Event Stream](./25-mobile-app.md#8-realtime-event-stream-v1-dual-channel)
- [Chinese canonical relay design](../zh/24-cloud-relay-remote-access.md)
- [ADR-076: Multi-user account system](../../adr/en/ADR-076-multi-user-account-system.md)
