//! Wire shape of `preview://download` (ADR-118 §D5).
//!
//! The event crosses into TypeScript, where `PreviewDownloadRequest` declares the
//! same two fields. Nothing in either language checks the other, so a rename on
//! one side compiles and produces a toast nobody sees — this test is the only
//! place the JSON keys are named literally.
//!
//! Why an integration test rather than a `#[cfg(test)]` module in `host.rs`: the
//! crate's lib-test harness needs the WebView2 runtime DLLs next to the test
//! binary, so a lib test cannot run outside a packaged app while this target can.

use acowork_desktop::browser::host::{download_request, PreviewDownloadDto};

#[test]
fn the_payload_carries_the_url_and_the_requesting_session() {
    let url: tauri::Url = "https://example.com/report.pdf".parse().unwrap();
    let payload = serde_json::to_value(download_request(&url, "preview-2")).unwrap();

    assert_eq!(payload["url"], "https://example.com/report.pdf");
    assert_eq!(payload["from"], "preview-2");
}

/// A page that builds a file in memory downloads a `blob:` URL. It means nothing
/// outside that page — which is exactly why the host forwards it verbatim instead
/// of filtering: the frontend has to see it to know the download URL is not the
/// one it should hand to the system browser.
#[test]
fn a_blob_download_is_forwarded_verbatim() {
    let url: tauri::Url = "blob:https://example.com/9f1e-2b".parse().unwrap();
    let payload = serde_json::to_value(download_request(&url, "preview-7")).unwrap();

    assert_eq!(payload["url"], "blob:https://example.com/9f1e-2b");
}

/// The TypeScript interface names these fields; a third field here would reach
/// the frontend unread, and a missing one would be `undefined` there.
#[test]
fn the_payload_has_exactly_two_fields() {
    let payload = serde_json::to_value(PreviewDownloadDto {
        url: "https://example.com/a".into(),
        from: "preview-1".into(),
    })
    .unwrap();

    let object = payload.as_object().expect("an object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["from", "url"]);
}