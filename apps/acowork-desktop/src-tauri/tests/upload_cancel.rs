//! The upload-cancel contract (ADR-122 §3, desktop side).
//!
//! A cancel cannot abort a Tauri `invoke` already on the wire, so the command
//! only records the intent and `upload_file_chunked` reads it between chunks.
//! The registry is keyed by the token the *caller* mints for the transfer, and
//! these are the properties that choice buys: a cancel can be pressed before
//! the transfer has a server-side id, and it can never reach a different
//! upload.

use acowork_desktop::upload_cancel::UploadCancels;

const TOKEN: &str = "2f0c5a1e-upload-a";

#[test]
fn a_cancel_names_one_transfer() {
    let cancels = UploadCancels::default();
    // No request yet: an upload in progress must keep going.
    assert!(!cancels.take(TOKEN), "nothing was requested, nothing to consume");

    cancels.request(TOKEN);
    assert!(cancels.take(TOKEN), "the upload loop must see the request");
    assert!(!cancels.take(TOKEN), "one request stops one chunk loop");
}

#[test]
fn a_cancel_cannot_reach_another_transfer() {
    let cancels = UploadCancels::default();
    cancels.request(TOKEN);

    assert!(
        !cancels.take("2f0c5a1e-upload-b"),
        "another transfer's cancel is not this one's business"
    );
    assert!(cancels.take(TOKEN));
}

/// The retry-after-cancel case, which is why the key is a per-transfer token
/// rather than the file path: the upload clears its own token on every exit
/// path, so a click that landed too late leaves nothing behind for the retry.
#[test]
fn an_upload_forgets_its_token_however_it_ends() {
    let cancels = UploadCancels::default();
    {
        let _slot = cancels.hold(TOKEN);
        cancels.request(TOKEN); // the click landed too late
    } // …and the upload ended anyway

    assert!(
        !cancels.take(TOKEN),
        "the retry of the same file must start clean"
    );
}