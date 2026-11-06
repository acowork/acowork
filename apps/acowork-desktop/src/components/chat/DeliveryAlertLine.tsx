/**
 * DeliveryAlertLine — the "this didn't go out" receipt, painted **live**
 * (ADR-105 D5, revision 2026-11-06).
 *
 * What it replaces: a persisted `system_notice` row with
 * `notice_type = "delivery_failed"`. That row was written by the Gateway and
 * never retracted, so a send that failed and was then retried successfully still
 * carried a yellow "The message wasn't delivered" line in the transcript weeks
 * later — on every reload, on every device, for every reader.
 *
 * The fact itself is still worth showing, and immediately: the human just sent
 * something and it did not land. So it is served as a decoration of the row the
 * send owed (`delivery.alert`), it expires on the service's own clock
 * (`expires_in`), and this component drops it locally on the same schedule — a
 * window that sits on one chat for ten minutes must not keep a dead receipt on
 * screen, and it must not need a re-read to take it away.
 *
 * Placement is the old receipt's, unchanged: under the human's own bubble, right
 * aligned, with the corner arrow that says "this refers back to what was just
 * sent". The wording is the surface's, not the server's: `error_code` is a
 * machine token and the sentence a human reads is localized copy.
 */

import { useEffect, useState } from "react";
import { CornerUpRight } from "lucide-react";
import { ChatEventText } from "./ChatEventText";
import { useTranslation } from "../../i18n/useTranslation";
import type { ChatDeliveryAlert } from "../../stores/chatSessionStore";

export interface DeliveryAlertLineProps {
  alert: ChatDeliveryAlert;
  /** Agent display name, when the surface can resolve one for `alert.target`. */
  name?: string;
  /** `end` on the human's own row (the only place this paints today). */
  align?: "start" | "end" | "center";
  className?: string;
}

export function DeliveryAlertLine({
  alert,
  name,
  align = "end",
  className,
}: DeliveryAlertLineProps) {
  const { t } = useTranslation();
  // The service already stopped counting when this page was read; the remaining
  // seconds are what this instance has left. Seeded once per (row, code, life):
  // a re-read that still reports the alert must not restart the clock, and the
  // key the parent passes (see below) is what makes a *new* failure re-arm it.
  const [expired, setExpired] = useState(alert.expires_in <= 0);

  useEffect(() => {
    setExpired(alert.expires_in <= 0);
    if (alert.expires_in <= 0) return;
    const timer = setTimeout(() => setExpired(true), alert.expires_in * 1000);
    return () => clearTimeout(timer);
  }, [alert.expires_in, alert.error_code]);

  if (expired) return null;

  // Which agent failed matters in a group and not in a 1:1 (there the @ is the
  // whole conversation). Same copy rule the persisted receipt used, so nothing a
  // reader sees changes except how long it stays.
  const text = name
    ? t("chatView.deliveryFailed", { name })
    : t("chatView.deliveryFailedNoTarget");

  return (
    <ChatEventText
      text={text}
      // `warn`, not `danger`: the receipt is retry-able and nothing in the run
      // broke. Red is reserved for a run that really failed.
      tone="warn"
      align={align}
      leading={<CornerUpRight className="h-3 w-3 shrink-0 opacity-70" />}
      className={className}
    />
  );
}
