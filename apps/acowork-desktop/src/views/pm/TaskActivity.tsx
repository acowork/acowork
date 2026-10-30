/**
 * TaskActivity — 任务面上的提交历史时间线：Agent 交了什么、人类回了什么。
 *
 * 为什么需要它：`pm_submit_task` 的正文过去只落在 `task.result`，那是覆盖式
 * 的"最后一次"，而人类审核时填的 `_comment`（拒绝理由）服务端收到就丢。结果
 * 就是任务提交之后，任务编辑界面上一个字都看不到 —— 验收一个人交上来的东西，
 * 得去群聊里翻他到底说了什么。
 *
 * 数据来源是 `GET /tasks/:tid/activity`（服务端 `{task_dir}/activity.json`，
 * 只追加，见 core/acowork-pm/src/store/tree.rs）；回复写回同一路径的 POST。
 * 这里**不**从 `task.result` 兜底：补种是服务端的事（旧任务也一样能读到），
 * 前端再拼一次就是第二个真源。
 *
 * 回复不是状态变更：批准/拒绝走看板与卡片上的审核按钮（`TaskStatusMove` /
 * `boardStore.reviewTask`），这里只留一条"说话"的入口，两者不互相冒充。
 */
import { useCallback, useState } from "react";
import { ChatMarkdown } from "../../components/chat/ChatMarkdown";
import { OverlayScrollbar } from "../../components/common/OverlayScrollbar";
import { StyledTextarea } from "../../components/common/StyledInput";
import { attachmentUrl } from "../../lib/pm-api";
import { useTranslation } from "../../i18n/useTranslation";
import { useActorLabel } from "./pmMembers";
import { formatBytes, formatDateTime } from "./taskFormatters";
import type { PmActivityKind, PmAttachmentMeta, PmTaskActivity } from "../../lib/pm-types";

const KIND_I18N: Record<PmActivityKind, string> = {
  submitted: "pm.activity.submitted",
  approved: "pm.activity.approved",
  rejected: "pm.activity.rejected",
  comment: "pm.activity.comment",
};

/** 结论用颜色区分，但颜色不是唯一通道（正文标签本身也在行上）。 */
const KIND_TONE: Record<PmActivityKind, string> = {
  submitted: "text-[var(--color-accent)]",
  approved: "text-emerald-600 dark:text-emerald-400",
  rejected: "text-red-600 dark:text-red-400",
  comment: "text-text-secondary",
};

interface TaskActivitySectionProps {
  activity: PmTaskActivity[];
  /** 任务附件列表：提交引用的 `attachment_ids` 靠它解析成文件名/缩略图。 */
  attachments: PmAttachmentMeta[];
  replying: boolean;
  onReply: (text: string) => Promise<boolean>;
}

export function TaskActivitySection({
  activity,
  attachments,
  replying,
  onReply,
}: TaskActivitySectionProps) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const busy = sending || replying;

  const send = useCallback(async () => {
    const text = draft.trim();
    if (!text || busy) return;
    setSending(true);
    const ok = await onReply(text);
    setSending(false);
    // 失败时留着草稿：那是用户刚打出来的字，不该因为一次网络错误消失。
    if (ok) setDraft("");
  }, [draft, busy, onReply]);

  return (
    <div className="space-y-2">
      {activity.length === 0 ? (
        <p className="text-11 text-text-tertiary">{t("pm.activity.empty")}</p>
      ) : (
        <OverlayScrollbar
          maxHeight="18rem"
          containerClassName="rounded-md border border-border-outer"
          className="divide-y divide-border-divider"
        >
          {activity.map((entry) => (
            <ActivityRow key={entry.id} entry={entry} attachments={attachments} />
          ))}
        </OverlayScrollbar>
      )}

      <div className="space-y-1.5">
        <StyledTextarea
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          rows={2}
          placeholder={t("pm.activity.replyPlaceholder")}
          aria-label={t("pm.activity.reply")}
        />
        <div className="flex justify-end">
          <button
            type="button"
            disabled={busy || !draft.trim()}
            onClick={() => void send()}
            className="rounded-md btn-accent px-3 py-1.5 text-xs font-medium disabled:opacity-50"
          >
            {busy ? t("pm.activity.sending") : t("pm.activity.reply")}
          </button>
        </div>
      </div>
    </div>
  );
}

function ActivityRow({
  entry,
  attachments,
}: {
  entry: PmTaskActivity;
  attachments: PmAttachmentMeta[];
}) {
  const { t } = useTranslation();
  // 显示名走的是任务面上同一套解析（agent 目录 / 登录账号），解析不到就留空
  // —— 这行是给人读的，不该出现裸 uuid。
  const actorName = useActorLabel(entry.actor);

  return (
    <div className="px-2.5 py-2">
      <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-11">
        <span className={`font-medium ${KIND_TONE[entry.kind]}`}>
          {t(KIND_I18N[entry.kind])}
        </span>
        <span className="font-medium text-text-secondary">
          {actorName ?? t("pm.activity.unknownActor")}
        </span>
        <span aria-hidden className="text-text-tertiary">
          ·
        </span>
        <span className="text-text-tertiary">{formatDateTime(entry.created_at)}</span>
      </div>

      {entry.text.trim() && (
        <div className="mt-1 text-xs text-text-secondary">
          <ChatMarkdown content={entry.text} className="!m-0 text-xs" />
        </div>
      )}

      {entry.attachment_ids.length > 0 && (
        <ul className="mt-1.5 flex flex-wrap gap-1.5">
          {entry.attachment_ids.map((aid) => {
            const meta = attachments.find((a) => a.id === aid);
            return (
              <li key={aid}>
                <a
                  href={attachmentUrl(aid)}
                  target="_blank"
                  rel="noreferrer"
                  className="inline-flex max-w-[12rem] items-center gap-1 rounded border border-border-outer px-1.5 py-0.5 text-9 text-text-tertiary hover:border-[var(--color-accent)] hover:text-[var(--color-accent)]"
                  title={meta?.filename ?? aid}
                >
                  <span className="truncate">{meta?.filename ?? aid}</span>
                  {meta && <span className="shrink-0">{formatBytes(meta.size)}</span>}
                </a>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
