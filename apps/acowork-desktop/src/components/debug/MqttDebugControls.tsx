// ============================================================================
// MqttDebugControls — manual disconnect/reconnect test for the MQTT broker.
//
// Purpose
// -------
// The Gateway broker runs in-process (rumqttd). To exercise the
// reconnection paths in:
//   - Desktop's MqttClient (force_reconnect + soft-restart)
//   - Runtime's retained `session_state` recovery
// we need a way to cleanly stop the broker without killing the whole
// Gateway. The Gateway exposes two debug HTTP endpoints
// (`POST /api/debug/mqtt/{shutdown,start}`) for this.
//
// This component surfaces those endpoints as two icon buttons in the
// status bar. Clicking "Stop" shuts the broker down; the Desktop's
// status flips to Reconnecting; click "Start" to bring it back and
// observe the retained-message recovery.
//
// ADR-XXX: debug-only. Lives in the status bar so QA / developers can
// exercise the reconnect path without DevTools. Hidden by default;
// only visible when `localStorage.acowork.mqttDebug === "1"`.
// ============================================================================

import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Power, PowerOff } from "lucide-react";
import { useStatusBarStore } from "../../stores/statusBarStore";
import { cn } from "../../lib/utils";

const VISIBILITY_KEY = "acowork.mqttDebug";

function isVisible(): boolean {
  try {
    return localStorage.getItem(VISIBILITY_KEY) === "1";
  } catch {
    return false;
  }
}

export function MqttDebugControls() {
  const [busy, setBusy] = useState<"shutdown" | "start" | null>(null);
  const setStatus = useStatusBarStore((s) => s.setStatus);
  const clearStatus = useStatusBarStore((s) => s.clearStatus);

  const callEndpoint = useCallback(
    async (action: "shutdown" | "start") => {
      setBusy(action);
      try {
        const result = await invoke<{ ok: boolean; message: string }>(
          action === "shutdown"
            ? "debug_mqtt_shutdown"
            : "debug_mqtt_start"
        );
        setStatus(
          result.message,
          result.ok ? "info" : "error",
          "debug"
        );
        // Auto-clear info messages after a few seconds; keep errors visible.
        if (result.ok) {
          setTimeout(() => clearStatus("debug"), 4000);
        }
      } catch (e) {
        setStatus(
          `MQTT ${action} failed: ${e}`,
          "error",
          "debug"
        );
      } finally {
        setBusy(null);
      }
    },
    [setStatus, clearStatus]
  );

  if (!isVisible()) return null;

  return (
    <span
      className="flex items-center gap-1 pl-2 ml-auto"
      title="MQTT broker debug controls (set acowork.mqttDebug=1 in localStorage to show)"
      data-testid="mqtt-debug-controls"
    >
      <button
        type="button"
        onClick={() => callEndpoint("shutdown")}
        disabled={busy !== null}
        className={cn(
          "flex items-center gap-1 px-1.5 py-px rounded-md",
          "bg-danger-surface/80 hover:bg-danger-surface-hover",
          "border border-danger-border/50",
          "text-danger-text",
          "disabled:opacity-50 disabled:cursor-not-allowed",
          "transition-colors"
        )}
        title="Stop the MQTT broker (Desktop will reconnect)"
      >
        <PowerOff className="h-3 w-3" aria-hidden="true" />
        <span className="text-11 font-medium">Stop</span>
      </button>
      <button
        type="button"
        onClick={() => callEndpoint("start")}
        disabled={busy !== null}
        className={cn(
          "flex items-center gap-1 px-1.5 py-px rounded-md",
          "bg-ok-surface/80 hover:bg-ok-surface",
          "border border-ok/50",
          "text-ok",
          "disabled:opacity-50 disabled:cursor-not-allowed",
          "transition-colors"
        )}
        title="Start the MQTT broker"
      >
        <Power className="h-3 w-3" aria-hidden="true" />
        <span className="text-11 font-medium">Start</span>
      </button>
    </span>
  );
}

/**
 * Helper to enable the controls from the browser console:
 *   localStorage.setItem("acowork.mqttDebug", "1"); location.reload();
 */
export const __MqttDebugControlsVisibilityKey = VISIBILITY_KEY;