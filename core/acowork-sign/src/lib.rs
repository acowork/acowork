//! acowork-sign — .bot package signing/verification toolchain
//!
//! Provides three CLI commands:
//! - acowork-keygen: Generate Ed25519 key pairs
//! - acowork-sign: Sign .bot packages
//! - acowork-verify: Verify .bot package signatures

pub mod certificate;
pub mod error;
pub mod keygen;
pub mod packager;
pub mod sign;
pub mod signing_block;
pub mod verify;
pub mod zip_utils;
