//! Intent module
//!
//! Cross-agent Intent delivery. Since the tech-debt plan (D1/D7) the
//! Gateway owns the only delivery entry: [`delivery::wake_and_deliver`]
//! plus the [`mailbox`] delivery buffer. The pre-ADR-033 `IntentRouter`
//! (gRPC-era dead code, never instantiated) was removed with D7.

pub mod accept;
pub mod chat_client;
pub mod chat_turn;
pub mod gate;
pub mod delivery;
pub mod mailbox;
pub mod sweeper;
pub mod privacy;

pub use delivery::{DeliveryContext, DeliveryError, wake_and_deliver};
pub use mailbox::{IntentMailbox, MailboxEntry};
