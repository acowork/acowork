//! ADR-122 §4.3 (D13): the receiving agent's side of an intent attachment.
//!
//! The producer half (`intent_send` → transfer store) and the store itself are
//! covered elsewhere. What this file pins is the consumer half over real HTTP:
//! the Runtime reads the declared list out of the turn's params, streams each
//! transfer into its workspace, and hands the turn a workspace reference the
//! model can open — with the peer's filename treated as hostile input, and a
//! failed pull leaving nothing behind.
//!
//! The pull takes the Gateway base URL as an argument (rather than looking up
//! a port), which is what lets a stand-in server stand in for the Gateway.

use acowork_runtime::agent::intent_turn::pull_intent_attachments;

#[tokio::test]
async fn an_intent_attachment_is_streamed_into_the_workspace() {
    let payload = b"%PDF-1.7 a peer's report\n".to_vec();
    let mut server = mockito::Server::new_async().await;
    let gateway = format!("http://{}", server.socket_address());

    let transfer_id = "11111111-1111-4111-8111-111111111111";
    let mock = server
        .mock("GET", format!("/api/transfers/{transfer_id}/content").as_str())
        .match_query(mockito::Matcher::UrlEncoded(
            "sender".into(),
            "inst-receiver".into(),
        ))
        .match_header(
            acowork_core::auth::NODE_TOKEN_HEADER,
            "node-token-1",
        )
        .with_status(200)
        .with_body(payload.clone())
        .create_async()
        .await;

    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    let params = serde_json::json!({
        acowork_core::protocol::INTENT_ATTACHMENTS_PARAM: [
            {
                "id": transfer_id,
                "name": "report.pdf",
                "mime": "application/pdf",
                "size": payload.len(),
                "sha256": "0".repeat(64),
            }
        ]
    });

    let items = pull_intent_attachments(
        &params,
        &work_dir,
        &gateway,
        "node-token-1",
        "inst-receiver",
    )
    .await;
    mock.assert_async().await;

    assert_eq!(items.len(), 1, "one declared attachment, one landed file");
    let landed = work_dir.join("intent-attachments").join(format!("{transfer_id}-report.pdf"));
    assert_eq!(
        std::fs::read(&landed).expect("the pulled bytes are on disk"),
        payload,
        "the streamed body is the file, unchanged"
    );
    let acowork_core::protocol::AttachedItem::AttachedFile { abs_path, name, .. } = &items[0]
    else {
        panic!("a landed file is a workspace reference, got {:?}", items[0]);
    };
    assert_eq!(name, "report.pdf");
    assert_eq!(
        std::path::Path::new(abs_path),
        landed.as_path(),
        "the reference the turn carries IS the file on disk"
    );
}

/// A name from another agent is input from an untrusted peer, not a path: no
/// traversal, no absolute path, no reserved characters.
#[tokio::test]
async fn a_peer_filename_cannot_walk_out_of_the_inbox() {
    let payload = b"x".to_vec();
    let mut server = mockito::Server::new_async().await;
    let gateway = format!("http://{}", server.socket_address());

    let transfer_id = "22222222-2222-4222-8222-222222222222";
    let _mock = server
        .mock("GET", format!("/api/transfers/{transfer_id}/content").as_str())
        // Every request here carries `?sender=`, and a mock with no query
        // matcher only answers a query-less request.
        .match_query(mockito::Matcher::Any)
        .with_status(200)
        .with_body(payload.clone())
        .create_async()
        .await;

    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    let params = serde_json::json!({
        acowork_core::protocol::INTENT_ATTACHMENTS_PARAM: [
            { "id": transfer_id, "name": "../../evil.txt", "mime": "text/plain",
              "size": payload.len(), "sha256": "" }
        ]
    });

    let items = pull_intent_attachments(&params, &work_dir, &gateway, "", "inst-receiver").await;
    assert_eq!(items.len(), 1);
    assert!(
        !work_dir.parent().unwrap().join("evil.txt").exists(),
        "nothing was written outside the inbox"
    );
    let entries: Vec<String> = std::fs::read_dir(work_dir.join("intent-attachments"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries.len(), 1, "one file, in the inbox: {entries:?}");
    assert!(
        entries[0].ends_with("-evil.txt") && !entries[0].contains(".."),
        "the traversal is flattened to its last segment: {entries:?}"
    );
}

/// The store refusing a transfer (expired, or the grant never landed) must not
/// leave a zero-byte file behind for the turn to open.
#[tokio::test]
async fn a_refused_transfer_leaves_no_file() {
    let mut server = mockito::Server::new_async().await;
    let gateway = format!("http://{}", server.socket_address());

    let transfer_id = "33333333-3333-4333-8333-333333333333";
    let _mock = server
        .mock("GET", format!("/api/transfers/{transfer_id}/content").as_str())
        .match_query(mockito::Matcher::Any)
        .with_status(404)
        .create_async()
        .await;

    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    let params = serde_json::json!({
        acowork_core::protocol::INTENT_ATTACHMENTS_PARAM: [
            { "id": transfer_id, "name": "gone.bin", "mime": "", "size": 10, "sha256": "" }
        ]
    });

    let items = pull_intent_attachments(&params, &work_dir, &gateway, "", "inst-receiver").await;
    assert!(items.is_empty(), "a refused transfer yields no reference");
    let inbox = work_dir.join("intent-attachments");
    let leftovers: Vec<String> = std::fs::read_dir(&inbox)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "no half-file left behind: {leftovers:?}");
}

/// A turn with no attachment key is the ordinary case (every older peer) and
/// must cost one map lookup, not a request.
#[tokio::test]
async fn an_intent_without_attachments_pulls_nothing() {
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    let items = pull_intent_attachments(
        &serde_json::json!({ "message": "hi" }),
        &work_dir,
        "http://127.0.0.1:1",
        "",
        "inst-receiver",
    )
    .await;
    assert!(items.is_empty());
    assert!(
        !work_dir.join("intent-attachments").exists(),
        "no inbox is created for a turn with no attachments"
    );
}