//! Concrete `SearchService` implementation — linear scan over the tree.

use async_trait::async_trait;
use tokio::fs;

use crate::error::Result;
use crate::service::search::SearchService;
use crate::store::library::LibraryStore;
use crate::types::{DocMeta, SearchHit, ROOT_DIR_ID};

pub struct LibrarySearchService {
    store: LibraryStore,
}

impl LibrarySearchService {
    pub fn new(store: LibraryStore) -> Self {
        Self { store }
    }
}

/// Case-insensitive substring match.
fn contains_ci(haystack: &str, needle: &str) -> bool {
    let hay = haystack.to_lowercase();
    let n = needle.to_lowercase();
    hay.contains(&n)
}

/// Build a short snippet around the first match in `body`.
fn snippet(body: &str, needle: &str, radius: usize) -> String {
    let lower = body.to_lowercase();
    let n = needle.to_lowercase();
    match lower.find(&n) {
        Some(pos) => {
            // Snap to char boundary; `pos` from the lowercased string can
            // differ in byte offsets if the needle case-fold changes the
            // length (rare: only for non-ASCII case folds). Guard with a
            // saturating scan so we never panic slicing mid-char.
            let start = body[..body.len().min(pos)].char_indices().count().min(pos);
            let s = pos.saturating_sub(radius).min(start);
            let e = (pos + n.len() + radius).min(body.len());
            // Round to char boundary before slicing.
            let s = body[..s].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
            let e = body[..e].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
            let mut out = body[s..e].to_string();
            if s > 0 {
                out.insert(0, '…');
            }
            if e < body.len() {
                out.push('…');
            }
            out
        }
        None => String::new(),
    }
}

impl LibrarySearchService {
    /// Score a document: title match counts heavily, each body occurrence
    /// counts 1. Returns `None` when neither matches.
    async fn score(
        &self,
        dir_id: &str,
        meta: &DocMeta,
        needle: &str,
    ) -> Result<Option<(i32, String)>> {
        let dir_path = self.store.dir_path(dir_id)?;
        let body_path = dir_path.join(format!("{}.md", meta.name));
        let mut score = 0;
        let mut snip = String::new();
        if contains_ci(&meta.name, needle) {
            score += 10;
        }
        if body_path.exists() {
            let body = fs::read_to_string(&body_path).await?;
            if contains_ci(&body, needle) {
                let count = body
                    .to_lowercase()
                    .match_indices(&needle.to_lowercase())
                    .count();
                score += count as i32;
                if snip.is_empty() {
                    snip = snippet(&body, needle, 40);
                }
            }
        }
        if score == 0 {
            Ok(None)
        } else {
            Ok(Some((score, snip)))
        }
    }
}

#[async_trait]
impl SearchService for LibrarySearchService {
    async fn search(&self, keyword: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Ok(vec![]);
        }
        let mut hits = Vec::new();
        let indexes = self.store.list_all_indexes().await?;
        for (idx_path, idx) in indexes {
            let dir_id = idx.dir_id.clone();
            let dir_rel = if dir_id == ROOT_DIR_ID {
                String::new()
            } else {
                // The dir_id *is* the folder name on disk, so the path is
                // relative to the library root already.
                dir_id.clone()
            };
            for meta in &idx.files {
                if meta.deleted {
                    continue;
                }
                if let Some((score, snip)) = self.score(&dir_id, meta, keyword).await? {
                    let path = if dir_rel.is_empty() {
                        format!("{}.md", meta.name)
                    } else {
                        format!("{}/{}.md", dir_rel, meta.name)
                    };
                    hits.push(SearchHit {
                        doc_id: meta.doc_id.clone(),
                        name: meta.name.clone(),
                        path,
                        snippet: snip,
                        score,
                    });
                }
            }
            let _ = idx_path; // silence unused
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
        hits.truncate(limit);
        Ok(hits)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::service::document::{CreateDocumentInput, DocumentService};
    use crate::service::document_impl::LibraryDocumentService;
    use crate::service::directory::{CreateDirectoryInput, DirectoryService};
    use crate::service::directory_impl::LibraryDirectoryService;
    use tempfile::TempDir;

    async fn setup() -> (
        TempDir,
        Arc<LibraryDocumentService>,
        Arc<LibraryDirectoryService>,
        Arc<LibrarySearchService>,
    ) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let root_store = || LibraryStore::new(tmp.path().to_path_buf());
        let docs = Arc::new(LibraryDocumentService::new(root_store()));
        let dirs = Arc::new(LibraryDirectoryService::new(root_store()));
        let search = Arc::new(LibrarySearchService::new(root_store()));
        (tmp, docs, dirs, search)
    }

    #[test]
    fn snippet_keeps_char_boundaries() {
        let s = "第一行你好世界 second line here";
        let snip = snippet(s, "你好", 3);
        assert!(!snip.is_empty());
        // No panic means the slice stayed on a boundary.
        assert!(snip.contains("你"));
    }

    #[test]
    fn contains_ci_handles_unicode_and_case() {
        assert!(contains_ci("Hello World", "world"));
        assert!(contains_ci("RustDoc", "rustdoc"));
        assert!(contains_ci("产品方案", "方案"));
        assert!(!contains_ci("hello", "bye"));
    }

    #[tokio::test]
    async fn search_finds_title_and_content_matches() {
        let (_tmp, docs, dirs, search) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "项目A".into(),
                creator: None,
            })
            .await
            .unwrap();
        // Title hit (score 10) + body hit.
        docs.create(CreateDocumentInput {
            parent_dir_id: sub.dir_id.clone(),
            title: "PRD-方案".into(),
            content: "这里包含路由方案细节与评审".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        // Body-only hit.
        docs.create(CreateDocumentInput {
            parent_dir_id: sub.dir_id.clone(),
            title: "会议纪要".into(),
            content: "讨论了部署方案".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        // No match.
        docs.create(CreateDocumentInput {
            parent_dir_id: sub.dir_id.clone(),
            title: "无关".into(),
            content: "今天天气不错".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        let hits = search.search("方案", 10).await.unwrap();
        assert_eq!(hits.len(), 2);
        // Title hit ranks first.
        assert_eq!(hits[0].name, "PRD-方案");
        assert_eq!(hits[1].name, "会议纪要");
        assert!(!hits[0].path.is_empty());
    }

    #[tokio::test]
    async fn search_respects_limit_and_empty_keyword() {
        let (_tmp, docs, _dirs, search) = setup().await;
        for i in 0..5 {
            docs.create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: format!("shared-{i}"),
                content: "公共内容 keyword".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        }
        let hits = search.search("keyword", 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        let none = search.search("", 5).await.unwrap();
        assert!(none.is_empty());
    }
}
