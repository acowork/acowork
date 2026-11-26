/**
 * 文档库「当前焦点目录」——上传文件时落到哪里。
 *
 * 规则（与用户对树的直觉一致，不需要额外的选中态）：
 *   1. 选中的是目录 → 就是它；
 *   2. 选中的是文档 → 它所在的那个目录；
 *   3. 什么都没选 → 根（文档库列表层）。
 *
 * 文档条目本身不带 parent（`DocMeta` 没有父指针，树是懒加载的），所以第 2 条
 * 靠扫已加载的层来找。只有按下上传那一刻才算一次，代价可以忽略；真要 O(1)
 * 就得让服务端在 `DocMeta` 里带上 parent，为一个按钮改 wire 格式不划算。
 */
import { DOC_ROOT_DIR_ID } from "../../lib/doc-types";
import type { DocTreeNode } from "../../lib/doc-types";

export function focusedDocDirId(
  nodes: Record<string, DocTreeNode>,
  selected: { kind: "doc" | "dir"; id: string } | null,
): string {
  if (!selected) return DOC_ROOT_DIR_ID;
  if (selected.kind === "dir") return selected.id;
  for (const [dirId, node] of Object.entries(nodes)) {
    if ((node.files ?? []).some((f) => f.doc_id === selected.id)) return dirId;
  }
  return DOC_ROOT_DIR_ID;
}