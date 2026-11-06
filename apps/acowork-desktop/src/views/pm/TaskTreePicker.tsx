/**
 * TaskTreePicker — 挑父任务 / 挑依赖的**结构**选择器。
 *
 * 为什么不是下拉框或复选列表：候选集是一棵树里的一块，不是一袋互不相干的
 * 条目。拍平成一维列表会同时丢掉两样东西 —— 深度（用户是靠"它在哪个枝上"
 * 找到目标任务的）和规模（N 个任务就要一次性挂上 N 个 option / 2N 个 DOM
 * 节点）。这里改成下钻：默认只画根这一层，点箭头才把那一枝的子节点挂上去，
 * **DOM 只包含展开的节点**，规模不再参与渲染成本。
 *
 * 排除是结构性的，且两种排除的视觉语义不同：
 * - 父任务：自己的整棵子树**不出现**。"我的后代"在"选父任务"的语境里本来
 *   就不是候选，没有解释的必要（`subtreeIds`）。
 * - 依赖：传递依赖自己的任务**出现但点不动**，并给出原因。用户常常是在
 *   它旁边找另一个，把它藏起来只会让人以为列表缺了东西
 *   （`dependentClosureIds`）。
 *
 * 两者都来自 `lib/taskTree`：本文件只负责画，规则都在那里，也只在那里测。
 */
import { useMemo, useState } from "react";
import { X as XIcon } from "lucide-react";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { useTranslation } from "../../i18n/useTranslation";
import type { PmTaskResponse } from "../../lib/pm-types";
import {
  buildTaskTree,
  dependentClosureIds,
  expandedForSelection,
  subtreeIds,
  type TaskTreeIndex,
} from "../../lib/taskTree";

/** 单选（父任务）还是多选（依赖）。只影响每行的控件形态。 */
export type TaskPickerMode = "single" | "multi";

interface TaskTreePickerProps {
  tree: TaskTreeIndex;
  mode: TaskPickerMode;
  selected: string[];
  onSelect: (id: string) => void;
  /** 画出来但点不动；值是该行显示的原因。 */
  blocked?: Map<string, string>;
  /** 连枝一起不画。 */
  hidden?: Set<string>;
  /** 单选模式下的"清空"行：文案在这里，选中时回传的空 id 由 `onSelect` 发出。 */
  emptyOption?: string;
  emptyLabel: string;
}

export function TaskTreePicker({
  tree,
  mode,
  selected,
  onSelect,
  blocked,
  hidden,
  emptyOption,
  emptyLabel,
}: TaskTreePickerProps) {
  const { t } = useTranslation();
  // 打开那一刻决定展开哪些枝：已选项的祖先路径。之后由用户自己控制 ——
  // 选择变化不再折叠/展开任何东西，否则刚点的那一行会被自己的重渲染弹走。
  const [expanded, setExpanded] = useState<Set<string>>(() =>
    expandedForSelection(tree, selected),
  );
  const chosen = useMemo(() => new Set(selected), [selected]);

  const toggleBranch = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const rows = tree.roots.filter((task) => !hidden?.has(task.id));

  const row = (task: PmTaskResponse, depth: number) => {
    const children = (tree.childrenByParent.get(task.id) ?? []).filter(
      (child) => !hidden?.has(child.id),
    );
    const open = expanded.has(task.id);
    const reason = blocked?.get(task.id);
    const isChosen = chosen.has(task.id);
    return (
      <li key={task.id}>
        <div
          className="flex items-center gap-1.5 rounded px-1.5 py-1"
          style={{ paddingLeft: 6 + depth * 14 }}
        >
          {children.length > 0 ? (
            <button
              type="button"
              onClick={() => toggleBranch(task.id)}
              aria-label={t(open ? "pm.task.collapse" : "pm.task.expand")}
              aria-expanded={open}
              className="w-4 shrink-0 text-center text-10 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300"
            >
              {open ? "▾" : "▸"}
            </button>
          ) : (
            <span className="w-4 shrink-0" aria-hidden />
          )}
          <label
            className={cn(
              "flex min-w-0 flex-1 items-center gap-1.5 rounded text-xs",
              reason
                ? "cursor-not-allowed text-text-tertiary"
                : cn("cursor-pointer text-text-secondary", listRowHover),
            )}
            title={reason}
          >
            <input
              type={mode === "single" ? "radio" : "checkbox"}
              name={mode === "single" ? "pm-task-parent" : undefined}
              checked={isChosen}
              disabled={!!reason}
              onChange={() => onSelect(task.id)}
              className="shrink-0 accent-[var(--color-accent)]"
            />
            <span className="min-w-0 flex-1 truncate">{task.title}</span>
            {/* 原因不藏在 title 里：悬停才看得见的解释等于没有解释。 */}
            {reason && <span className="shrink-0 text-10">{reason}</span>}
          </label>
        </div>
        {open && children.length > 0 && (
          <ul>{children.map((child) => row(child, depth + 1))}</ul>
        )}
      </li>
    );
  };

  return (
    <ul data-testid="task-tree" className="max-h-56 overflow-auto py-0.5">
      {emptyOption !== undefined && (
        <li>
          <div
            className="flex items-center gap-1.5 rounded px-1.5 py-1"
            style={{ paddingLeft: 6 }}
          >
            <span className="w-4 shrink-0" aria-hidden />
            <label className="flex min-w-0 flex-1 cursor-pointer items-center gap-1.5 rounded text-xs text-text-secondary">
              <input
                type="radio"
                name="pm-task-parent"
                checked={selected.length === 0}
                onChange={() => onSelect("")}
                className="shrink-0 accent-[var(--color-accent)]"
              />
              <span className="min-w-0 flex-1 truncate">{emptyOption}</span>
            </label>
          </div>
        </li>
      )}
      {rows.length === 0 && emptyOption === undefined ? (
        <li className="px-1.5 py-1 text-xs text-text-tertiary">{emptyLabel}</li>
      ) : (
        rows.map((task) => row(task, 0))
      )}
    </ul>
  );
}

/**
 * 父任务字段：一行"当前值 + 更换"，点开才长出树。
 *
 * 不常开的时候它占一行，跟别的字段一样高；要换的时候才把树展开。这不是为了
 * 省地方，是为了让"父任务"和"依赖"两个字段在视觉上同级 —— 一个是单值、一个
 * 是多值，选择过程都藏在点开之后。
 */
export function ParentTaskPicker({
  tasks,
  selfId,
  value,
  onChange,
}: {
  tasks: PmTaskResponse[];
  /** 当前这一行自己的 id —— 它的整棵子树都不是候选。 */
  selfId: string | null;
  value: string;
  onChange: (id: string) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const tree = useMemo(() => buildTaskTree(tasks), [tasks]);
  const hidden = useMemo(
    () => (selfId ? subtreeIds(tree, selfId) : new Set<string>()),
    [tree, selfId],
  );
  const current = value ? tree.byId.get(value) : null;

  return (
    <div className="space-y-1.5">
      <div className="flex items-center gap-2 text-xs">
        <span
          data-testid="parent-current"
          className={cn(
            "min-w-0 flex-1 truncate",
            current ? "text-text" : "text-text-tertiary",
          )}
        >
          {current ? current.title : t("pm.task.noParent")}
        </span>
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          className="shrink-0 inline-flex items-center rounded btn-solid px-2 py-1 text-11 font-medium"
        >
          {t("pm.task.change")}
        </button>
      </div>
      {open && (
        <div className="rounded-md border border-border-outer p-1">
          <TaskTreePicker
            tree={tree}
            mode="single"
            selected={value ? [value] : []}
            onSelect={(id) => {
              onChange(id);
              setOpen(false);
            }}
            hidden={hidden}
            emptyOption={t("pm.task.noParent")}
            emptyLabel={t("pm.task.noCandidates")}
          />
        </div>
      )}
    </div>
  );
}

/**
 * 依赖字段：已选 chips 常驻 + "添加依赖"点开树。
 *
 * chips 不随树的折叠消失 —— 已选项是**引用**，不是候选。候选可以收起来，
 * 引用的东西必须一直看得见，否则用户会以为它被删掉了。
 */
export function DependencyPicker({
  tasks,
  selfId,
  selected,
  onToggle,
}: {
  tasks: PmTaskResponse[];
  selfId: string | null;
  selected: string[];
  onToggle: (id: string) => void;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const tree = useMemo(() => buildTaskTree(tasks), [tasks]);
  const hidden = useMemo(
    () => (selfId ? new Set([selfId]) : new Set<string>()),
    [selfId],
  );
  // 传递依赖自己的任务：选它们会闭成环，画出来但点不动。
  const blocked = useMemo(() => {
    if (!selfId) return new Map<string, string>();
    const reason = t("pm.task.dependencyCycle");
    return new Map(
      [...dependentClosureIds(tree, selfId)].map((id) => [id, reason]),
    );
  }, [tree, selfId, t]);

  return (
    <div className="space-y-1.5">
      {selected.length > 0 && (
        <ul data-testid="dependency-chips" className="flex flex-wrap gap-1">
          {selected.map((id) => {
            const title = tree.byId.get(id)?.title ?? id;
            return (
              <li
                key={id}
                className="inline-flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-medium text-text-secondary dark:bg-zinc-700"
              >
                <span className="max-w-40 truncate">{title}</span>
                <button
                  type="button"
                  onClick={() => onToggle(id)}
                  // 无障碍名带上标题：多个 chip 的删除按钮不能都叫"移除"
                  aria-label={`${t("common.remove")} ${title}`}
                  className="shrink-0 text-text-tertiary hover:text-text"
                >
                  <XIcon className="h-2.5 w-2.5" />
                </button>
              </li>
            );
          })}
        </ul>
      )}
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="inline-flex items-center rounded btn-solid px-2 py-1 text-11 font-medium"
      >
        + {t("pm.task.addDependency")}
      </button>
      {open && (
        <div className="rounded-md border border-border-outer p-1">
          <TaskTreePicker
            tree={tree}
            mode="multi"
            selected={selected}
            onSelect={onToggle}
            hidden={hidden}
            blocked={blocked}
            emptyLabel={t("pm.task.noCandidates")}
          />
        </div>
      )}
    </div>
  );
}
