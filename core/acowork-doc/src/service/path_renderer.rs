//! 人读路径渲染器 —— 把 store 的 `{dir_id}/{file_name}` 布局翻译成
//! `研发/纪要.md` 的**唯一**一处实现。
//!
//! ## 为什么这是一个中间件，而不是一个辅助函数
//!
//! 「人读路径」是一个横切需求：REST（`GET /docs/{id}` 的 `path`）、MCP
//! （`doc_read` / `doc_stat` / `doc_list` / 搜索结果）、refs（`/refs/validate`、
//! `/refs/resolve`）三处边界都要它，而它自己不携带任何业务语义 —— 谁来问都
//! 是同一个答案。三处各写一遍的代价不是代码量，而是**它们会分别漂移**：一处
//! 修了 root 特例、一处漏了改写、一处把 `dir_id` 直接吐出去，而这类缺陷只在
//! 「根目录 + 改名」这种组合下才暴露。
//!
//! 所以它认识两件事，仅此两件：
//!
//! 1. 存储布局的形状（`raw.rsplit_once('/')` 出 `dir_id` + 文件名）；
//! 2. `DirectoryService::path_of` 的祖先链语义（root → `""`）。
//!
//! 它**不**知道任何业务词：不知道链接、不知道交付物、不知道审批。任何业务
//! 都可以整块复用它 —— 这就是它与业务模块的边界。
//!
//! ## 成本
//!
//! 每个目录一次 O(depth) 的祖先链上溯；同一次调用内按 `dir_id` 记忆化，
//! 所以「渲染 100 条搜索结果、分布在 3 个目录」只走 3 条链。记忆化是**每次
//! 调用**的，不是跨请求的：没有跨请求缓存，就没有 mkdir / rename / delete
//! 之后必须失效的那笔账 —— 那笔账一旦漏掉，症状是「移动后的文档显示旧路径」，
//! 而且只在长会话里偶发。

use std::collections::HashMap;

use crate::error::Result;
use crate::service::directory::DirectoryService;
use crate::service::document::DocumentService;
use crate::types::ROOT_DIR_ID;

/// 一次渲染期内的路径缓存。生命周期由调用方的作用域决定。
pub struct DocPathRenderer<'a> {
    dirs: &'a dyn DirectoryService,
    memo: HashMap<String, String>,
}

impl<'a> DocPathRenderer<'a> {
    pub fn new(dirs: &'a dyn DirectoryService) -> Self {
        let mut memo = HashMap::new();
        // 根目录没有名字：它的路径是空串，而不是 `/` 或 `dir-<root>`。
        memo.insert(ROOT_DIR_ID.to_string(), String::new());
        Self { dirs, memo }
    }

    /// 目录的人读路径（`研发/会议`）；根目录是 `""`。
    pub async fn dir_path(&mut self, dir_id: &str) -> Result<String> {
        if let Some(hit) = self.memo.get(dir_id) {
            return Ok(hit.clone());
        }
        let path = self.dirs.path_of(dir_id).await?;
        self.memo.insert(dir_id.to_string(), path.clone());
        Ok(path)
    }

    /// 已知目录 + 文件名时直接渲染（`doc_list` 这类逐条遍历的路径）。
    pub async fn entry_path(&mut self, dir_id: &str, file_name: &str) -> Result<String> {
        let dir = self.dir_path(dir_id).await?;
        Ok(join(&dir, file_name))
    }

    /// 由文档 id 渲染人读路径（`研发/纪要.md`）。
    ///
    /// 走 `DocumentService::path_of` 拿到存储布局的原始形状，再交给
    /// [`Self::humanize`] —— 拆分 `dir_id` 的逻辑因此只有一份。
    pub async fn doc_path(&mut self, docs: &dyn DocumentService, doc_id: &str) -> Result<String> {
        let raw = docs.path_of(doc_id).await?;
        self.humanize(&raw).await
    }

    /// 把存储布局的原始路径（`dir-4d3d/x.md`，或根目录下的 `x.md`）
    /// 翻译成人读路径（`研发/x.md` / `x.md`）。
    ///
    /// 公开是因为搜索结果、`DocStat` 这些对象在 service 层就已经带着原始
    /// 路径了，再按 id 回查一遍等于把同一件事做两遍。
    pub async fn humanize(&mut self, raw: &str) -> Result<String> {
        let (dir_id, file) = raw.rsplit_once('/').unwrap_or((ROOT_DIR_ID, raw));
        let dir = self.dir_path(dir_id).await?;
        Ok(join(&dir, file))
    }
}

/// 目录路径 + 条目名 → 路径。根目录下的条目没有前缀。
fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::directory::CreateDirectoryInput;
    use crate::service::directory_impl::LibraryDirectoryService;
    use crate::service::document::{CreateDocumentInput, CreateFileInput};
    use crate::service::document_impl::LibraryDocumentService;
    use crate::store::library::LibraryStore;
    use std::sync::Arc;
    use tempfile::TempDir;

    /// 真 store + 真 service（临时目录）：路径渲染的全部真相都在这两个
    /// trait 的语义里（root = `""`、祖先链上溯、`file_name()` 带真实扩展名），
    /// 用假实现测它等于把被测对象换成自己的假设。
    async fn setup() -> (TempDir, Arc<LibraryDirectoryService>, Arc<LibraryDocumentService>, String) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let dirs = Arc::new(LibraryDirectoryService::new(LibraryStore::new(
            tmp.path().to_path_buf(),
        )));
        let docs = Arc::new(LibraryDocumentService::new(LibraryStore::new(
            tmp.path().to_path_buf(),
        )));
        let dir = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "研发".into(),
                creator: None,
                icon: None,
                description: None,
            })
            .await
            .unwrap();
        (tmp, dirs, docs, dir.dir_id)
    }

    async fn mkdoc(docs: &LibraryDocumentService, dir_id: &str, title: &str) -> String {
        docs.create(CreateDocumentInput {
            parent_dir_id: dir_id.to_string(),
            title: title.into(),
            content: "x".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap()
        .doc_id
    }

    /// 根目录下的文档：路径就是文件名，没有前缀也没有 `/`。
    #[tokio::test]
    async fn root_level_doc_has_no_prefix() {
        let (_tmp, dirs, docs, _) = setup().await;
        let doc_id = mkdoc(&docs, ROOT_DIR_ID, "纪要").await;
        let mut r = DocPathRenderer::new(dirs.as_ref());
        assert_eq!(r.doc_path(docs.as_ref(), &doc_id).await.unwrap(), "纪要.md");
    }

    /// 一层目录 + 导入文件：扩展名来自 `DocMeta::file_name()`，不是硬编码 md。
    #[tokio::test]
    async fn nested_imported_entry_renders_its_real_file_name() {
        let (_tmp, dirs, docs, dir_id) = setup().await;
        let md_id = mkdoc(&docs, &dir_id, "纪要").await;
        let pdf = docs
            .create_file(CreateFileInput {
                parent_dir_id: dir_id.clone(),
                name: "设计".into(),
                ext: "pdf".into(),
                bytes: b"%PDF-1.4
".to_vec(),
                creator: None,
            })
            .await
            .unwrap();

        let mut r = DocPathRenderer::new(dirs.as_ref());
        assert_eq!(r.doc_path(docs.as_ref(), &md_id).await.unwrap(), "研发/纪要.md");
        assert_eq!(r.doc_path(docs.as_ref(), &pdf.doc_id).await.unwrap(), "研发/设计.pdf");
    }

    /// 记忆化：同一次渲染里，同一目录的祖先链只走一次 —— 用「目录被删掉后
    /// 缓存里的答案仍然可用」来证明它真的读了缓存，而不是碰巧算对。
    #[tokio::test]
    async fn directory_lookup_is_memoised_within_one_render() {
        let (_tmp, dirs, docs, dir_id) = setup().await;
        let doc_id = mkdoc(&docs, &dir_id, "一").await;
        let mut r = DocPathRenderer::new(dirs.as_ref());
        assert_eq!(r.doc_path(docs.as_ref(), &doc_id).await.unwrap(), "研发/一.md");
        // 目录连同文档一起进回收站，服务此刻已经找不到这条祖先链……
        dirs.delete(&dir_id).await.unwrap();
        assert!(dirs.path_of(&dir_id).await.is_err());
        // ……而同一个渲染器仍然给出答案：它读的是本次调用的记忆，不是服务。
        assert_eq!(r.dir_path(&dir_id).await.unwrap(), "研发", "第二次必须命中缓存");
    }

    /// 断链必须报错，且错误信息里不能出现内部 `dir_id` —— 把人读位置
    /// 退回成 `dir-xxx/name.md`，比这条路径渲染失败更糟（R5）。
    #[tokio::test]
    async fn broken_parent_chain_is_an_error_not_a_leak() {
        let (tmp, dirs, _docs, dir_id) = setup().await;
        let store = LibraryStore::new(tmp.path().to_path_buf());
        let mut idx = store.load(&dir_id).await.unwrap();
        idx.parent = Some("dir-0000000000ff".into());
        store.save(&idx).await.unwrap();

        let mut r = DocPathRenderer::new(dirs.as_ref());
        let err = r.dir_path(&dir_id).await.unwrap_err();
        assert!(
            !format!("{err}").contains(&dir_id),
            "错误信息里出现了内部 dir_id: {err}"
        );
    }
}
