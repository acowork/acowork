//! Cancelling an upload that is still running.
//!
//! The two producers cancel differently because they can: a browser `fetch`
//! takes an `AbortSignal` and stops on the spot, while a Tauri `invoke` is
//! already on the wire and cannot be aborted from the webview. So the Rust-side
//! command only *records* the intent here and `upload_file_chunked` reads it
//! between chunks — at most one chunk (2 MiB) away.
//!
//! **Keyed by the token the caller mints for the transfer, not by the file
//! path.** Two reasons, both of which the path-keyed first draft got wrong:
//!
//!   - the token exists before the request starts (the upload id does not, so
//!     an id-keyed registry cannot honour a cancel pressed during the digest or
//!     the create call) — and
//!   - a token is used by exactly one upload, so a request that arrives after
//!     that upload ended can never stop the next upload of the same file. The
//!     path-keyed draft needed a "consume once" rule to approximate that, and
//!     still leaked: a cancel landing after the last chunk left a flag that
//!     killed the retry.
//!
//! Chunks already on the server are released by the upload loop's own
//! `DELETE`; a process that dies before it gets there leaves them to the 24 h
//! TTL sweep (`UploadStore::sweep`), the same owner every abandoned upload has.

use std::collections::HashSet;
use std::sync::Mutex;

#[derive(Default)]
pub struct UploadCancels {
    tokens: Mutex<HashSet<String>>,
}

impl UploadCancels {
    /// Record that the user wants the upload carrying `token` stopped.
    pub fn request(&self, token: &str) {
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.insert(token.to_string());
        }
    }

    /// Whether a cancel was requested, consuming it: one request stops one
    /// chunk loop, never a loop that starts later.
    pub fn take(&self, token: &str) -> bool {
        self.tokens
            .lock()
            .map(|mut tokens| tokens.remove(token))
            .unwrap_or(false)
    }

    /// Forget `token` when the upload ends, however it ends.
    ///
    /// A guard rather than a call at each return: an upload leaves through the
    /// landed path, a refused chunk, a cancel and half a dozen `?`s, and a
    /// token left behind is exactly the bug this registry had when it keyed on
    /// the file path.
    pub fn hold<'a>(&'a self, token: &'a str) -> UploadSlot<'a> {
        UploadSlot {
            cancels: self,
            token,
        }
    }

    /// Forget `token`, whether or not it was cancelled — so a click that
    /// raced the final `complete` leaves nothing behind in the set.
    pub fn release(&self, token: &str) {
        if let Ok(mut tokens) = self.tokens.lock() {
            tokens.remove(token);
        }
    }
}

/// One upload's claim on a token; releases it on drop — see
/// [`UploadCancels::hold`].
pub struct UploadSlot<'a> {
    cancels: &'a UploadCancels,
    token: &'a str,
}

impl Drop for UploadSlot<'_> {
    fn drop(&mut self) {
        self.cancels.release(self.token);
    }
}
