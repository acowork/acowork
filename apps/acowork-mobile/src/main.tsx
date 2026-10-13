import { newId } from './lib/id'
import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { setChatTransport } from './stores/chatStore'
import { setDirectorySource, useAgentStore } from './stores/agentStore'
import { httpChatTransport, fetchAgents, fetchNodes, fetchDirectory, tokenClaims } from './lib/api'
import { setRealtimeDeps } from './lib/realtime'
import { setPresenceDeps, startPresence, stopPresence } from './lib/presence'
import { startSafeAreaSync } from './lib/safeArea'
import type { ConnectOpts } from './lib/mqtt-wire'
import { useAuthStore } from './stores/authStore'
import { useSettingsStore } from './stores/settingsStore'
import type { AgentSummary, NodeRow, UserSummary } from './lib/types'
import './styles/tokens.css'
import './styles/app.css'
import './styles/mobile.css'

// Wire the transport before the first render: a screen that mounts and
// immediately calls openSession would otherwise find a null transport and
// silently no-op. `httpChatTransport` is the complete implementation —
// spreading overrides over it silently dropped its 404 handling, so bind
// the object itself and nothing else.
setChatTransport(httpChatTransport)

/* Realtime channel selection (v1.2 §8.0): both shapes now expose the
 * MQTT-over-WS bridge at `/mqtt` — the relay one rides the public https entry
 * (`wss://<gw-id>.relay/mqtt`), the LAN one is the Gateway's main listener.
 * A Gateway without the account system fails the CONNECT closed, and both
 * engines then fall back to polling on their own. */
function mqttWsUrl(): string | null {
  const u = useAuthStore.getState().baseUrl
  if (u.startsWith('https://')) return `wss://${u.slice('https://'.length)}/mqtt`
  if (u.startsWith('http://')) return `ws://${u.slice('http://'.length)}/mqtt`
  return null
}

/** MQTT 3.1.1 cannot re-authenticate, so every (re)connect must carry a live
 *  token — ask for a rotation before it expires (Desktop refresher semantics,
 *  §8.1). `clientId` is a callback because the two channels must not share
 *  one: a duplicate CONNECT takes the other socket's session over. */
async function mqttConnectOpts(clientId: (sub: string, device: string) => string): Promise<ConnectOpts | null> {
  const auth = useAuthStore.getState()
  if (!auth.accessToken) return null
  const claims = tokenClaims(auth.accessToken)
  const nearExpiry = claims.exp !== undefined && claims.exp * 1000 - Date.now() < 60_000
  const token = nearExpiry ? await auth._refresh() : auth.accessToken
  if (!token) return null
  const sub = tokenClaims(token).sub ?? 'mobile'
  return { clientId: clientId(sub, deviceId()), username: sub, password: token }
}

const sessionClientId = (sub: string, device: string): string => `user:${sub}:mobile:${device}`

setRealtimeDeps({
  wsUrl: mqttWsUrl,
  connectOpts: () => mqttConnectOpts(sessionClientId),
})

/* Presence rides its own connection (§8.1): the session channel dies with the
 * chat screen while the list needs verdicts all the time. The `-presence`
 * suffix stays inside the device segment — the Gateway's
 * `remote_client_shape` rejects a fourth colon in a client id. */
setPresenceDeps({
  wsUrl: mqttWsUrl,
  connectOpts: () => mqttConnectOpts((sub, device) => `${sessionClientId(sub, device)}-presence`),
  onUpdate: (agentId, update) => useAgentStore.getState().applyPresence(agentId, update.status),
})

function deviceId(): string {
  const KEY = 'acowor...'
  try {
    let id = localStorage.getItem(KEY)
    if (!id) {
      id = newId()
      localStorage.setItem(KEY, id)
    }
    return id
  } catch {
    return newId()
  }
}

// Directory source: wire shapes → store shapes. The mapping lives at the
// transport boundary so no store ever sees a Gateway field name.
setDirectorySource({
  loadAgents: async (): Promise<AgentSummary[]> => {
    const rows = await fetchAgents()
    return rows.map((r) => ({
      id: r.instance_id,
      name: r.display_name ?? r.name,
      node_id: r.node_id,
      role: r.role ?? null,
      icon: r.builtin_avatar ?? null,
      avatar: r.avatar ?? null,
      status: !r.alive ? 'offline' : r.lifecycle === 'failed' ? 'error' : 'idle',
    }))
  },
  loadUsers: async (): Promise<UserSummary[]> => {
    const rows = await fetchDirectory()
    return rows.map((u) => ({ id: u.user_id, display_name: u.display_name, avatar: u.avatar ?? null, builtin_avatar: u.builtin_avatar ?? null }))
  },
  loadNodes: async (): Promise<NodeRow[]> => {
    const rows = await fetchNodes()
    // Server already filtered by can_view; keep the wire shape (identity +
    // liveness is all the inbox needs). Pass through node_id/online/node_name.
    return rows.map((n) => ({ node_id: n.node_id, online: n.online, node_name: n.node_name ?? null }))
  },
})

// Appearance is applied before the shell renders: a light flash on a
// dark-theme phone reads as a broken app, and the CSS cannot resolve
// 跟随系统 + an explicit override by itself (see lib/theme.ts).
const stopThemeWatch = useSettingsStore.getState().start()
// Same reason, same lifecycle: the status-bar clearance has to be on `:root`
// before the first navbar paints, or the back button lands under the clock
// on notch-less Android (see lib/safeArea.ts).
const stopSafeAreaWatch = startSafeAreaSync()
if (import.meta.hot) {
  import.meta.hot.dispose(() => stopThemeWatch())
  import.meta.hot.dispose(() => stopSafeAreaWatch())
}

// Cold start: decide the boot phase from persisted state (§7.1).
void useAuthStore.getState().boot()
// DIAG: log every phase change to WebView console (visible via remote debug / logcat)
useAuthStore.subscribe((s, prev) => { if (s.phase !== prev.phase) console.log('[AUTH] PHASE', prev.phase, '→', s.phase) })
// Directory + previews refresh whenever we reach `ready`; presence lives with
// the session too — its credentials are the access token, so a logout or an
// expired session must close the socket instead of being refused forever.
useAuthStore.subscribe((s, prev) => {
  if (s.phase === prev.phase) return
  if (s.phase === "ready") {
    void useAgentStore.getState().refreshInbox()
    startPresence()
  } else if (prev.phase === "ready") {
    stopPresence()
  }
})

const el = document.getElementById('root')
if (!el) throw new Error('#root not found')
createRoot(el).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
