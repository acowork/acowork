/**
 * `docFocusDir.ts` — 上传落点规则。
 *
 * 这条规则错了不会报错，只会把文件悄悄放进「不是用户想放的地方」，所以给三条
 * 分支各钉一个断言：选目录 → 目录本身；选文档 → 它的父目录；没选 → 根。
 */
import { describe, expect, it } from "vitest";
import { focusedDocDirId } from "./docFocusDir";
import type { DocTreeNode } from "../../lib/doc-types";

function node(dirId: string, docIds: string[]): DocTreeNode {
  return {
    dir: { dir_id: dirId, name: dirId, parent_dir_id: null, deleted: false } as DocTreeNode["dir"],
    dirs: [],
    files: docIds.map(
      (id) => ({ doc_id: id, name: id, ext: null }) as DocTreeNode["files"][number],
    ),
  };
}

describe("focusedDocDirId", () => {
  const nodes = {
    root: node("root", ["a"]),
    "dir-1": node("dir-1", ["b"]),
    "dir-2": node("dir-2", []),
  };

  it("选中的目录就是上传目标", () => {
    expect(focusedDocDirId(nodes, { kind: "dir", id: "dir-2" })).toBe("dir-2");
  });

  it("选中文档时落到它所在的目录", () => {
    expect(focusedDocDirId(nodes, { kind: "doc", id: "b" })).toBe("dir-1");
  });

  it("什么都没选时落到根", () => {
    expect(focusedDocDirId(nodes, null)).toBe("root");
  });

  it("选中的文档尚未加载到任何一层时也落到根，而不是抛错", () => {
    expect(focusedDocDirId(nodes, { kind: "doc", id: "missing" })).toBe("root");
  });
});