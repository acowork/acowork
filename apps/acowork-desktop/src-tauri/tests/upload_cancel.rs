//! The upload-cancel contract (ADR-122 §3, desktop side).
//!
//! A cancel cannot abort a Tauri `invoke` already on the wire, so the command
//! only records the intent and `upload_file_chunked` reads it between chunks.
//! What has to hold is the *consumption* rule: a request stops exactly one
//! upload. Get it wrong in one direction and "cancel, then retry" can never
//! start again (the stale flag kills the retry); get it wrong in the other and
//! the cancel is a no-op.

use acowork_desktop::gateway_client::UploadCancels;

const FILE: &str = "C:/videos/movie.mp4";

#[test]
fn a_cancel_is_consumed_by_exactly_one_upload() {
    let cancels = UploadCancels::default();
    // No request yet: an upload in progress must keep going.
    assert!(!cancels.take(FILE), "nothing was requested, nothing to consume");

    cancels.request(FILE);
    assert!(cancels.take(FILE), "the upload loop must see the request");
    assert!(
        !cancels.take(FILE),
        "a consumed request must not cancel the retry that follows"
    );
}

#[test]
fn a_cancel_names_one_file_only() {
    let cancels = UploadCancels::default();
    cancels.request("C:/videos/other.mp4");

    assert!(
        !cancels.take(FILE),
        "another file's cancel must not stop this upload"
    );
    assert!(cancels.take("C:/videos/other.mp4"));
}