// ADR-123 D5/D7: the disk-watermark alarm dialog.
//
// The Gateway publishes a **retained** `DiskWatermark` on
// `acowork/desktop/storage` whenever the volume holding `data_dir` crosses
// `[cache].disk_alarm_percent`; the Rust bridge forwards it as the
// `disk-watermark` Tauri event. This component is the only consumer of that
// event and the only place a dialog appears.
//
// Two things it deliberately does NOT do:
// - It does not clean anything. The buttons are "later" and "go clean"; the
//   actual sweep is a user click on the cache-management card, because the
//   Gateway never auto-cleans (ADR-123 §1.4).
// - It does not decide when to nag. `alarm_since` identifies the alarmed
//   episode: the Gateway republishes hourly while the disk stays full, so a
//   dismissed dialog is remembered by that number and does not re-open. A new
//   episode (disk crossed again after recovering) carries a new `alarm_since`
//   and does open again.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";

import { ConfirmDialog } from "./ConfirmDialog";
import { formatSizeBytes } from "../../lib/agentHub";
import { log } from "../../lib/logger";

/** Mirror of the Gateway's `StorageWatermarkEvent` (ADR-123 §3.4). */
interface StorageWatermarkEvent {
  total_bytes: number;
  available_bytes: number;
  used_percent: number;
  threshold_percent: number;
  alarm: boolean;
  /** Unix seconds the alarm turned on; absent while it is off. */
  alarm_since?: number | null;
  checked_at: number;
}

/** Remembering the dismissal across a Desktop restart is the point: a user who
 *  closed the dialog and kept working must not be greeted by it on every boot.
 *  Machine-level, not account-level — the disk is shared, so this is a plain
 *  localStorage key rather than a `prefsScope` entry. */
const DISMISSED_KEY = "acowork.diskAlarm.dismissedSince";

function readDismissed(): number | null {
  const raw = localStorage.getItem(DISMISSED_KEY);
  const value = raw ? Number(raw) : Number.NaN;
  return Number.isFinite(value) ? value : null;
}

export function DiskAlarmDialog({ onOpenStorage }: { onOpenStorage: () => void }) {
  const { t } = useTranslation();
  const [event, setEvent] = useState<StorageWatermarkEvent | null>(null);
  const [dismissed, setDismissed] = useState<number | null>(readDismissed);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    let cancelled = false;
    void listen<StorageWatermarkEvent>("disk-watermark", (e) => {
      setEvent(e.payload);
    })
      .then((un) => {
        if (cancelled) un();
        else dispose = un;
      })
      .catch((err) => log.warn("[DiskAlarmDialog] listen failed:", err));
    return () => {
      cancelled = true;
      dispose?.();
    };
  }, []);

  // One dialog per alarmed episode. An "all clear" payload (alarm: false,
  // no alarm_since) is a state update, not a prompt — it just clears the view.
  const alarmSince = event?.alarm ? (event.alarm_since ?? null) : null;
  const open = alarmSince !== null && alarmSince !== dismissed;

  const dismiss = useCallback(() => {
    if (alarmSince === null) return;
    localStorage.setItem(DISMISSED_KEY, String(alarmSince));
    setDismissed(alarmSince);
  }, [alarmSince]);

  const goClean = useCallback(() => {
    dismiss();
    onOpenStorage();
  }, [dismiss, onOpenStorage]);

  if (!event) return null;

  return (
    <ConfirmDialog
      open={open}
      title={t("settings.cache.alarmTitle")}
      message={t("settings.cache.alarmBody", {
        percent: Math.round(event.used_percent),
        free: formatSizeBytes(event.available_bytes),
        total: formatSizeBytes(event.total_bytes),
      })}
      confirmLabel={t("settings.cache.alarmAction")}
      onConfirm={goClean}
      onCancel={dismiss}
    />
  );
}