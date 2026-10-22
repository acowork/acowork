//! Concrete `RequestService` implementation backed by `RequestsStore` +
//! `DocumentService`.
//!
//! Review transitions are serialised through an in-process `tokio::sync::Mutex`
//! (`review_gate`) so two concurrent approves of the same request cannot both
//! observe `pending` and merge twice. Document writes themselves stay guarded
//! by `DocumentService`'s own optimistic `base_version` check — the gate only
//! protects the request-file state transition, which the version check cannot
//! see.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use tokio::sync::Mutex;

use crate::error::{DocError, Result};
use crate::path::validate_doc_id;
use crate::service::document::{DocumentService, UpdateDocumentInput};
use crate::service::request::{ApproveOutcome, RequestService, SubmitRequestInput};
use crate::store::requests::RequestsStore;
use crate::types::{generate_request_id, RequestStatus, UpdateRequest};

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Production `RequestService` over `.requests/` + the live document store.
pub struct LibraryRequestService {
    store: RequestsStore,
    docs: Arc<dyn DocumentService>,
    clock: Clock,
    ttl: Duration,
    /// Serialises review transitions (approve / reject / sweep).
    review_gate: Mutex<()>,
}

impl LibraryRequestService {
    /// `ttl_hours` comes from `DocConfig::request_ttl_hours` (default 72).
    pub fn new(
        docs: Arc<dyn DocumentService>,
        store: RequestsStore,
        ttl_hours: u32,
    ) -> Self {
        let clock: Clock = Arc::new(Utc::now);
        Self {
            store,
            docs,
            clock,
            ttl: Duration::hours(i64::from(ttl_hours)),
            review_gate: Mutex::new(()),
        }
    }

    /// Inject a custom clock (used by tests for deterministic expiry).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    fn expired_cutoff(&self) -> DateTime<Utc> {
        self.now() - self.ttl
    }

    /// Lazily mark stale `pending` requests as `expired` (design §5.4).
    /// Idempotent and safe to call without the gate: concurrent sweeps
    /// write the same final state via atomic rename, last write wins.
    async fn sweep_expired(&self) -> Result<()> {
        for mut req in self.store.list().await? {
            if req.status == RequestStatus::Pending && req.created_at < self.expired_cutoff() {
                req.status = RequestStatus::Expired;
                self.store.write(&req).await?;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl RequestService for LibraryRequestService {
    async fn submit(&self, input: SubmitRequestInput) -> Result<UpdateRequest> {
        validate_doc_id(&input.doc_id)?;
        if input.content.trim().is_empty() {
            return Err(DocError::BadRequest("content must not be empty".into()));
        }
        if input.submitted_by.trim().is_empty() {
            return Err(DocError::BadRequest("submitted_by must not be empty".into()));
        }
        // Gate: the submission is checked against the live document.
        let read = self.docs.read(&input.doc_id).await?;
        if read.meta.version != input.base_version {
            return Err(DocError::VersionConflict {
                base_version: input.base_version,
                current_version: read.meta.version,
            });
        }
        let req = UpdateRequest {
            request_id: generate_request_id(),
            doc_id: input.doc_id.clone(),
            path: read.path.clone(),
            base_version: input.base_version,
            content: input.content,
            submitted_by: input.submitted_by,
            status: RequestStatus::Pending,
            created_at: self.now(),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
        };
        self.store.write(&req).await?;
        Ok(req)
    }

    async fn list(&self, status: Option<RequestStatus>) -> Result<Vec<UpdateRequest>> {
        self.sweep_expired().await?;
        let mut all = self.store.list().await?;
        if let Some(want) = status {
            all.retain(|r| r.status == want);
        }
        Ok(all)
    }

    async fn get(&self, request_id: &str) -> Result<UpdateRequest> {
        self.sweep_expired().await?;
        self.store.read(request_id).await
    }

    async fn list_for_doc(&self, doc_id: &str) -> Result<Vec<UpdateRequest>> {
        validate_doc_id(doc_id)?;
        self.sweep_expired().await?;
        let all = self.store.list().await?;
        Ok(all.into_iter().filter(|r| r.doc_id == doc_id).collect())
    }

    async fn approve(
        &self,
        request_id: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<ApproveOutcome> {
        let _gate = self.review_gate.lock().await;
        let mut req = self.store.read(request_id).await?;
        // Expiry is a terminal state: a stale request cannot be reviewed —
        // the agent must re-submit on the current base (design §5.4).
        if req.status == RequestStatus::Pending && req.created_at < self.expired_cutoff() {
            req.status = RequestStatus::Expired;
            self.store.write(&req).await?;
            return Err(DocError::RequestExpired(request_id.to_string()));
        }
        if req.status != RequestStatus::Pending {
            return Err(DocError::AlreadyReviewed(format!("{:?}", req.status)));
        }
        // Re-check base_version against the live document (design §5.4):
        // a concurrent human PUT or an earlier approved request bumped
        // the version since submission → refuse to merge (git push 被拒).
        let merged = self
            .docs
            .update(
                &req.doc_id,
                UpdateDocumentInput {
                    base_version: req.base_version,
                    title: None,
                    content: req.content.clone(),
                },
            )
            .await?;
        req.status = RequestStatus::Approved;
        req.reviewed_at = Some(self.now());
        req.reviewed_by = Some(reviewed_by.to_string());
        req.review_note = note.map(str::to_string);
        self.store.write(&req).await?;
        Ok(ApproveOutcome {
            request: req,
            doc_version: merged.version,
        })
    }

    async fn reject(
        &self,
        request_id: &str,
        reviewed_by: &str,
        note: Option<&str>,
    ) -> Result<UpdateRequest> {
        let _gate = self.review_gate.lock().await;
        let mut req = self.store.read(request_id).await?;
        // Same terminal-state rule as approve.
        if req.status == RequestStatus::Pending && req.created_at < self.expired_cutoff() {
            req.status = RequestStatus::Expired;
            self.store.write(&req).await?;
            return Err(DocError::RequestExpired(request_id.to_string()));
        }
        if req.status != RequestStatus::Pending {
            return Err(DocError::AlreadyReviewed(format!("{:?}", req.status)));
        }
        req.status = RequestStatus::Rejected;
        req.reviewed_at = Some(self.now());
        req.reviewed_by = Some(reviewed_by.to_string());
        req.review_note = note.map(str::to_string);
        self.store.write(&req).await?;
        Ok(req)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::document::{CreateDocumentInput, DocumentService};
    use crate::service::document_impl::LibraryDocumentService;
    use crate::service::request::RequestService;
    use crate::store::library::LibraryStore;
    use crate::types::ROOT_DIR_ID;
    use chrono::TimeZone;
    use tempfile::TempDir;

    /// Fresh library with one doc at version 1 + a request service whose
    /// clock starts at `base` (tests advance it to exercise TTL expiry).
    struct Harness {
        _tmp: TempDir,
        docs: Arc<LibraryDocumentService>,
        requests: LibraryRequestService,
        _offset: std::sync::Arc<std::sync::atomic::AtomicI64>,
        doc: String,
    }

    fn clock_at(base: DateTime<Utc>) -> (Clock, std::sync::Arc<std::sync::atomic::AtomicI64>) {
        let offset = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
        let offset2 = offset.clone();
        let clock: Clock = Arc::new(move || {
            base + Duration::seconds(offset2.load(std::sync::atomic::Ordering::Relaxed))
        });
        (clock, offset)
    }

    fn advance(offset: &std::sync::Arc<std::sync::atomic::AtomicI64>, seconds: i64) {
        offset.fetch_add(seconds, std::sync::atomic::Ordering::Relaxed);
    }

    async fn harness(ttl_hours: u32) -> Harness {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let docs = Arc::new(LibraryDocumentService::new(store.clone()));
        let created = docs
            .create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "协作文档".into(),
                content: "v1".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let (clock, offset) = clock_at(Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap());
        let requests = LibraryRequestService::new(
            docs.clone() as Arc<dyn DocumentService>,
            RequestsStore::new(store.root().to_path_buf()),
            ttl_hours,
        )
        .with_clock(clock);
        Harness {
            _tmp: tmp,
            docs,
            requests,
            _offset: offset,
            doc: created.doc_id,
        }
    }

    async fn submit_one(h: &Harness, content: &str) -> UpdateRequest {
        h.requests
            .submit(SubmitRequestInput {
                doc_id: h.doc.clone(),
                base_version: 1,
                content: content.into(),
                submitted_by: "agent:test-agent".into(),
            })
            .await
            .unwrap()
    }

    /// Simulate a human direct PUT that bumps the doc to v2 (used to
    /// pre-empt a pending request's base).
    async fn human_edit(h: &Harness, content: &str) {
        let read = h.docs.read(&h.doc).await.unwrap();
        h.docs
            .update(
                &h.doc,
                UpdateDocumentInput {
                    base_version: read.meta.version,
                    title: None,
                    content: content.into(),
                },
            )
            .await
            .unwrap();
    }

    async fn doc_version(h: &Harness) -> u64 {
        h.docs.read(&h.doc).await.unwrap().meta.version
    }

    async fn doc_content(h: &Harness) -> String {
        h.docs.read(&h.doc).await.unwrap().content
    }

    #[tokio::test]
    async fn submit_creates_pending_with_path_snapshot() {
        let h = harness(72).await;
        let req = submit_one(&h, "v2 提案").await;
        assert_eq!(req.status, RequestStatus::Pending);
        assert_eq!(req.base_version, 1);
        assert_eq!(req.doc_id, h.doc);
        assert_eq!(req.submitted_by, "agent:test-agent");
        assert!(req.request_id.starts_with("r-"));
        assert!(req.path.ends_with(".md"), "path snapshot: {}", req.path);
        assert!(req.reviewed_at.is_none());
    }

    #[tokio::test]
    async fn submit_with_stale_base_returns_version_conflict_and_creates_nothing() {
        let h = harness(72).await;
        let err = h
            .requests
            .submit(SubmitRequestInput {
                doc_id: h.doc.clone(),
                base_version: 0, // document is already at v1
                content: "based on nothing".into(),
                submitted_by: "agent:test-agent".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DocError::VersionConflict { base_version: 0, current_version: 1 }
        ));
        assert_eq!(h.requests.list(None).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn submit_rejects_empty_content_and_unknown_doc() {
        let h = harness(72).await;
        let err = h
            .requests
            .submit(SubmitRequestInput {
                doc_id: h.doc.clone(),
                base_version: 1,
                content: "   ".into(),
                submitted_by: "agent:x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::BadRequest(_)));

        let err = h
            .requests
            .submit(SubmitRequestInput {
                doc_id: "doc-000000000000".into(),
                base_version: 1,
                content: "x".into(),
                submitted_by: "agent:x".into(),
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::DocNotFound(_)));
    }

    #[tokio::test]
    async fn approve_merges_content_and_bumps_version() {
        let h = harness(72).await;
        let req = submit_one(&h, "# v2\n\n由 agent 提案的内容").await;
        let outcome = h
            .requests
            .approve(&req.request_id, "human:zhang", Some("内容合理"))
            .await
            .unwrap();
        assert_eq!(outcome.doc_version, 2);
        assert_eq!(outcome.request.status, RequestStatus::Approved);
        assert_eq!(outcome.request.reviewed_by.as_deref(), Some("human:zhang"));
        assert_eq!(outcome.request.review_note.as_deref(), Some("内容合理"));

        // The live document now carries the merged content at v2.
        assert_eq!(doc_version(&h).await, 2);
        assert!(doc_content(&h).await.contains("由 agent 提案的内容"));
    }

    #[tokio::test]
    async fn approve_preempted_base_returns_conflict_and_keeps_pending() {
        let h = harness(72).await;
        let req = submit_one(&h, "v2 提案").await;
        // A human directly edits the document first (bypassing review).
        human_edit(&h, "人类直接修改的内容").await;
        let err = h
            .requests
            .approve(&req.request_id, "human:zhang", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DocError::VersionConflict { base_version: 1, current_version: 2 }),
            "got: {err:?}"
        );
        // Request stays pending so the agent can rebase and re-submit.
        let after = h.requests.get(&req.request_id).await.unwrap();
        assert_eq!(after.status, RequestStatus::Pending);
    }

    #[tokio::test]
    async fn approve_twice_fails_with_already_reviewed() {
        let h = harness(72).await;
        let req = submit_one(&h, "v2").await;
        h.requests
            .approve(&req.request_id, "human:a", None)
            .await
            .unwrap();
        let err = h
            .requests
            .approve(&req.request_id, "human:b", None)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::AlreadyReviewed(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn reject_keeps_content_and_note() {
        let h = harness(72).await;
        let req = submit_one(&h, "v2 提案").await;
        let rejected = h
            .requests
            .reject(&req.request_id, "human:zhang", Some("与现状冲突"))
            .await
            .unwrap();
        assert_eq!(rejected.status, RequestStatus::Rejected);
        assert_eq!(rejected.content, "v2 提案");
        assert_eq!(rejected.review_note.as_deref(), Some("与现状冲突"));
        // Document untouched.
        assert_eq!(doc_version(&h).await, 1);
    }

    #[tokio::test]
    async fn expired_requests_are_swept_on_list_and_refused_on_review() {
        // TTL = 1h; submit at t0, advance 2h, then approve / list.
        let h = harness(1).await;
        let req = submit_one(&h, "v2 提案").await;
        advance(&h._offset, 2 * 3600);

        // Approving an expired request fails with RequestExpired and no
        // merge happened (document stays at v1).
        let err = h
            .requests
            .approve(&req.request_id, "human:zhang", None)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::RequestExpired(_)), "got: {err:?}");
        assert_eq!(doc_version(&h).await, 1);

        // The review attempt flipped it to Expired on disk; list reports
        // it as such and the pending filter is empty.
        let all = h.requests.list(None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].status, RequestStatus::Expired);
        let pending = h.requests.list(Some(RequestStatus::Pending)).await.unwrap();
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn list_filters_by_status_and_doc_history() {
        let h = harness(72).await;
        let r1 = submit_one(&h, "提案 A").await;
        advance(&h._offset, 1);
        let r2 = submit_one(&h, "提案 B").await;
        advance(&h._offset, 1);
        let r3 = submit_one(&h, "提案 C").await;
        h.requests.approve(&r1.request_id, "human:x", None).await.unwrap();
        h.requests.reject(&r2.request_id, "human:x", None).await.unwrap();

        let approved = h.requests.list(Some(RequestStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1);
        assert_eq!(approved[0].request_id, r1.request_id);
        let rejected = h.requests.list(Some(RequestStatus::Rejected)).await.unwrap();
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].request_id, r2.request_id);

        let hist = h.requests.list_for_doc(&h.doc).await.unwrap();
        assert_eq!(hist.len(), 3);
        // Newest first (created_at desc).
        assert_eq!(hist[0].request_id, r3.request_id);
        assert_eq!(hist[1].request_id, r2.request_id);
        assert_eq!(hist[2].request_id, r1.request_id);
    }

    #[tokio::test]
    async fn two_agents_submit_same_base_only_first_approve_wins() {
        let h = harness(72).await;
        let a = submit_one(&h, "agent A 的 v2").await;
        let b = submit_one(&h, "agent B 的 v2").await;
        let first = h.requests.approve(&a.request_id, "human:x", None).await.unwrap();
        assert_eq!(first.doc_version, 2);
        let err = h
            .requests
            .approve(&b.request_id, "human:x", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, DocError::VersionConflict { base_version: 1, current_version: 2 }),
            "got: {err:?}"
        );
        // B stays pending — agent B must rebase on v2 and re-submit.
        assert_eq!(h.requests.get(&b.request_id).await.unwrap().status, RequestStatus::Pending);
    }
}

