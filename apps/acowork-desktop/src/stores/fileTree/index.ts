/**
 * `__seedTreeNode` is a test-only escape hatch (writes straight into the
 * cache, bypassing fetch/dedup/abort/SWR). The `__` prefix + this doc are
 * the guard rails: production code MUST NOT import it. If a lint rule is
 * added later, block `__*` imports from non-test files here.
 */
export { useFileTreeStore, fetchTreeNode, refreshTreeNode, __seedTreeNode } from "./fileTreeStore";
export { getCachedWorkspaceRoot, getCachedFileVersion } from "./lookup";
export type { TreeNode, TreeCacheKey, TreeEntry, TreeResponse, TreeError, TreeCachePolicy } from "./types";
export { treeKey, splitTreeKey, workspaceRootKey, DEFAULT_TREE_POLICY, isReadyNode, isLoading } from "./types";
export type { TreeCache } from "./treeCache";
