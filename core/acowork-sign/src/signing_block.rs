//! Signing block data structures and binary serialization
//!
//! The Signing Block is inserted into the .bot ZIP file before the
//! Central Directory, similar to Android's APK Signature Scheme v2.
//!
//! ## V2 Binary Format (current, APK v2 style)
//!
//! Inserted between local file entries and the Central Directory:
//! ```text
//! [magic: 16 bytes]            — "RBSign Block 42\0"
//! [size_prefix: u64 LE]        — block content size
//! [block_content: variable]    — signers data (see below)
//! [size_suffix: u64 LE]        — same as size_prefix
//! [magic: 16 bytes]            — repeated for backward scanning
//! ```
//!
//! Block content (signers data):
//! ```text
//! [signers_count: u32 BE]      — number of signers
//! For each signer:
//!   [certs_count: u32 BE]
//!   For each certificate:
//!     [cert_len: u32 BE]
//!     [cert_data: bytes]
//!     [identity_type: u8]       — 0=Developer, 1=Platform, 2=CaIssued
//!   [digest_algo: u8]           — 0=SHA256
//!   [digests_count: u32 BE]
//!   For each digest:
//!     [section_name_len: u16 BE]
//!     [section_name: bytes]
//!     [digest_len: u16 BE]
//!     [digest: bytes]
//!   [signature_len: u32 BE]
//!   [signature: bytes]
//!   [signed_attrs_len: u32 BE]
//!   [signed_attrs: bytes (JSON)]
//! ```
//!
//! ## V1 Legacy Format (ZIP entry)
//!
//! Stored as a ZIP entry named "META-INF/SIGNING.BLOCK":
//! ```text
//! [block_size: u32 BE]         — total size of the block body
//! [magic: 8 bytes]             — "ROLLBLLS"
//! [signers data]               — same as V2 block content
//! [block_size: u32 BE]         — repeated at end
//! ```

use serde::{Deserialize, Serialize};

/// Magic bytes for ACowork Signing Block (V1, legacy ZIP entry format)
pub const SIGNING_BLOCK_MAGIC: &[u8; 8] = b"ROLLBLLS";

/// Magic bytes for ACowork Signing Block (V2, APK v2 style binary format)
/// 16 bytes: "RBSign Block 42" + null terminator
pub const SIGNING_BLOCK_MAGIC_V2: [u8; 16] = *b"RBSign Block 42\0";

/// Complete signing block inserted into ZIP (before Central Directory)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningBlock {
    pub signers: Vec<Signer>,
}

/// Signer information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signer {
    pub certificates: Vec<Certificate>,
    pub digest_algorithm: DigestAlgorithm,
    pub digests: Vec<SectionDigest>,
    pub signature: Vec<u8>,
    pub signed_attrs: SignedAttributes,
}

/// X.509 Certificate
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Certificate {
    pub data: Vec<u8>,
    pub identity: SignerIdentity,
}

/// Signer identity type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignerIdentity {
    Developer,
    Platform,
    CaIssued,
}

impl SignerIdentity {
    fn to_u8(self) -> u8 {
        match self {
            SignerIdentity::Developer => 0,
            SignerIdentity::Platform => 1,
            SignerIdentity::CaIssued => 2,
        }
    }

    fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(SignerIdentity::Developer),
            1 => Some(SignerIdentity::Platform),
            2 => Some(SignerIdentity::CaIssued),
            _ => None,
        }
    }
}

/// Digest algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DigestAlgorithm {
    Sha256,
}

impl DigestAlgorithm {
    fn to_u8(self) -> u8 {
        match self {
            DigestAlgorithm::Sha256 => 0,
        }
    }

    fn from_u8(val: u8) -> Option<Self> {
        match val {
            0 => Some(DigestAlgorithm::Sha256),
            _ => None,
        }
    }
}

/// Section digest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionDigest {
    pub section_name: String,
    pub digest: Vec<u8>,
}

/// Signed attributes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedAttributes {
    pub signing_time: String,
}

impl SigningBlock {
    // ------------------------------------------------------------------
    // V2 Binary Format (APK v2 style, current)
    // ------------------------------------------------------------------

    /// Serialize to V2 binary format for embedding before the Central Directory
    ///
    /// Format: `[magic:16][size_prefix:u64 LE][content][size_suffix:u64 LE][magic:16]`
    pub fn to_binary(&self) -> Vec<u8> {
        let content = self.encode_content();
        let content_size = content.len() as u64;

        let mut result = Vec::with_capacity(16 + 8 + content.len() + 8 + 16);
        // Leading magic
        result.extend_from_slice(&SIGNING_BLOCK_MAGIC_V2);
        // Size prefix (u64 LE)
        result.extend_from_slice(&content_size.to_le_bytes());
        // Block content
        result.extend_from_slice(&content);
        // Size suffix (u64 LE)
        result.extend_from_slice(&content_size.to_le_bytes());
        // Trailing magic
        result.extend_from_slice(&SIGNING_BLOCK_MAGIC_V2);
        result
    }

    /// Deserialize from V2 binary format
    ///
    /// Expects the full binary framing: `[magic:16][size_prefix:8][content][size_suffix:8][magic:16]`
    pub fn from_binary(data: &[u8]) -> Result<Self, SigningBlockError> {
        // Minimum size: 16 (magic) + 8 (size_prefix) + 8 (size_suffix) + 16 (magic) = 48
        if data.len() < 48 {
            return Err(SigningBlockError::TooShort {
                expected: 48,
                actual: data.len(),
            });
        }

        // Check leading magic
        if data[0..16] != SIGNING_BLOCK_MAGIC_V2 {
            return Err(SigningBlockError::InvalidMagic);
        }

        // Read size prefix (u64 LE at offset 16)
        let size_prefix =
            u64::from_le_bytes(data[16..24].try_into().expect("slice has correct length")) as usize;

        // Validate total length
        let total_expected = 16 + 8 + size_prefix + 8 + 16;
        if data.len() < total_expected {
            return Err(SigningBlockError::TooShort {
                expected: total_expected,
                actual: data.len(),
            });
        }

        // Check trailing magic
        if data[total_expected - 16..total_expected] != SIGNING_BLOCK_MAGIC_V2 {
            return Err(SigningBlockError::InvalidMagic);
        }

        // Read size suffix and verify consistency
        let suffix_offset = 16 + 8 + size_prefix;
        let size_suffix = u64::from_le_bytes(
            data[suffix_offset..suffix_offset + 8]
                .try_into()
                .expect("slice has correct length"),
        ) as usize;

        if size_prefix != size_suffix {
            return Err(SigningBlockError::SizeMismatch {
                prefix: size_prefix,
                suffix: size_suffix,
            });
        }

        // Parse block content
        let content = &data[24..24 + size_prefix];
        Self::decode_content(content)
    }

    // ------------------------------------------------------------------
    // Shared content encoding (used by both V1 and V2)
    // ------------------------------------------------------------------

    /// Encode signers data (block content without framing)
    fn encode_content(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.signers.len() as u32).to_be_bytes());
        for signer in &self.signers {
            encode_signer(&mut buf, signer);
        }
        buf
    }

    /// Decode signers data (block content without framing)
    fn decode_content(data: &[u8]) -> Result<Self, SigningBlockError> {
        let mut cursor = 0usize;
        let signers_count = read_u32(data, &mut cursor)?;

        let mut signers = Vec::with_capacity(signers_count as usize);
        for _ in 0..signers_count {
            signers.push(decode_signer(data, &mut cursor)?);
        }

        Ok(Self { signers })
    }

    // ------------------------------------------------------------------
    // V1 Legacy Format (ZIP entry, kept for backward compatibility)
    // ------------------------------------------------------------------

    /// Serialize to V1 legacy binary format (stored as a ZIP entry)
    ///
    /// Format: `[size_prefix:u32 BE][body][size_suffix:u32 BE]`
    /// where body = `[magic:8][content]`
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut body = Vec::new();
        // V1 magic
        body.extend_from_slice(SIGNING_BLOCK_MAGIC);
        // Signers content
        body.extend_from_slice(&self.encode_content());

        // Wrap with size prefix and suffix
        let block_size = body.len() as u32;
        let mut result = Vec::with_capacity(4 + body.len() + 4);
        result.extend_from_slice(&block_size.to_be_bytes());
        result.extend_from_slice(&body);
        result.extend_from_slice(&block_size.to_be_bytes());
        result
    }

    /// Deserialize from V1 legacy binary format (ZIP entry)
    pub fn from_bytes(data: &[u8]) -> Result<Self, SigningBlockError> {
        if data.len() < 12 {
            return Err(SigningBlockError::TooShort {
                expected: 12,
                actual: data.len(),
            });
        }

        // Read size prefix
        let prefix_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;

        // Verify size suffix
        let suffix_offset = 4 + prefix_size;
        if data.len() < suffix_offset + 4 {
            return Err(SigningBlockError::TooShort {
                expected: suffix_offset + 4,
                actual: data.len(),
            });
        }

        let suffix_size = u32::from_be_bytes([
            data[suffix_offset],
            data[suffix_offset + 1],
            data[suffix_offset + 2],
            data[suffix_offset + 3],
        ]) as usize;

        if prefix_size != suffix_size {
            return Err(SigningBlockError::SizeMismatch {
                prefix: prefix_size,
                suffix: suffix_size,
            });
        }

        let body = &data[4..4 + prefix_size];

        // Check V1 magic
        if body.len() < 8 || &body[0..8] != SIGNING_BLOCK_MAGIC {
            return Err(SigningBlockError::InvalidMagic);
        }

        // Parse content (after the 8-byte magic)
        let content = &body[8..];
        Self::decode_content(content)
    }
}

fn encode_signer(buf: &mut Vec<u8>, signer: &Signer) {
    // Certificates count
    buf.extend_from_slice(&(signer.certificates.len() as u32).to_be_bytes());

    for cert in &signer.certificates {
        // Cert data length
        buf.extend_from_slice(&(cert.data.len() as u32).to_be_bytes());
        // Cert data
        buf.extend_from_slice(&cert.data);
        // Identity type
        buf.push(cert.identity.to_u8());
    }

    // Digest algorithm
    buf.push(signer.digest_algorithm.to_u8());

    // Digests count
    buf.extend_from_slice(&(signer.digests.len() as u32).to_be_bytes());

    for digest in &signer.digests {
        // Section name length (u16)
        let name_bytes = digest.section_name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
        // Section name
        buf.extend_from_slice(name_bytes);
        // Digest length (u16)
        buf.extend_from_slice(&(digest.digest.len() as u16).to_be_bytes());
        // Digest
        buf.extend_from_slice(&digest.digest);
    }

    // Signature length
    buf.extend_from_slice(&(signer.signature.len() as u32).to_be_bytes());
    // Signature
    buf.extend_from_slice(&signer.signature);

    // Signed attrs (JSON encoded)
    let attrs_json = serde_json::to_vec(&signer.signed_attrs).unwrap_or_default();
    buf.extend_from_slice(&(attrs_json.len() as u32).to_be_bytes());
    buf.extend_from_slice(&attrs_json);
}

fn decode_signer(body: &[u8], cursor: &mut usize) -> Result<Signer, SigningBlockError> {
    // Certificates count
    let certs_count = read_u32(body, cursor)? as usize;
    let mut certificates = Vec::with_capacity(certs_count);

    for _ in 0..certs_count {
        let cert_len = read_u32(body, cursor)? as usize;
        if *cursor + cert_len + 1 > body.len() {
            return Err(SigningBlockError::UnexpectedEof);
        }
        let cert_data = body[*cursor..*cursor + cert_len].to_vec();
        *cursor += cert_len;
        let identity_byte = body[*cursor];
        *cursor += 1;
        let identity = SignerIdentity::from_u8(identity_byte)
            .ok_or(SigningBlockError::InvalidIdentityType(identity_byte))?;
        certificates.push(Certificate {
            data: cert_data,
            identity,
        });
    }

    // Digest algorithm
    if *cursor >= body.len() {
        return Err(SigningBlockError::UnexpectedEof);
    }
    let digest_algo_byte = body[*cursor];
    *cursor += 1;
    let digest_algorithm = DigestAlgorithm::from_u8(digest_algo_byte)
        .ok_or(SigningBlockError::InvalidDigestAlgorithm(digest_algo_byte))?;

    // Digests count
    let digests_count = read_u32(body, cursor)? as usize;
    let mut digests = Vec::with_capacity(digests_count);

    for _ in 0..digests_count {
        let name_len = read_u16(body, cursor)? as usize;
        if *cursor + name_len > body.len() {
            return Err(SigningBlockError::UnexpectedEof);
        }
        let section_name = String::from_utf8(body[*cursor..*cursor + name_len].to_vec())
            .map_err(|_| SigningBlockError::InvalidUtf8)?;
        *cursor += name_len;

        let digest_len = read_u16(body, cursor)? as usize;
        if *cursor + digest_len > body.len() {
            return Err(SigningBlockError::UnexpectedEof);
        }
        let digest = body[*cursor..*cursor + digest_len].to_vec();
        *cursor += digest_len;

        digests.push(SectionDigest {
            section_name,
            digest,
        });
    }

    // Signature
    let sig_len = read_u32(body, cursor)? as usize;
    if *cursor + sig_len > body.len() {
        return Err(SigningBlockError::UnexpectedEof);
    }
    let signature = body[*cursor..*cursor + sig_len].to_vec();
    *cursor += sig_len;

    // Signed attributes
    let attrs_len = read_u32(body, cursor)? as usize;
    if *cursor + attrs_len > body.len() {
        return Err(SigningBlockError::UnexpectedEof);
    }
    let signed_attrs: SignedAttributes =
        serde_json::from_slice(&body[*cursor..*cursor + attrs_len])
            .map_err(SigningBlockError::JsonParse)?;
    *cursor += attrs_len;

    Ok(Signer {
        certificates,
        digest_algorithm,
        digests,
        signature,
        signed_attrs,
    })
}

fn read_u32(data: &[u8], cursor: &mut usize) -> Result<u32, SigningBlockError> {
    if *cursor + 4 > data.len() {
        return Err(SigningBlockError::UnexpectedEof);
    }
    let val = u32::from_be_bytes([
        data[*cursor],
        data[*cursor + 1],
        data[*cursor + 2],
        data[*cursor + 3],
    ]);
    *cursor += 4;
    Ok(val)
}

fn read_u16(data: &[u8], cursor: &mut usize) -> Result<u16, SigningBlockError> {
    if *cursor + 2 > data.len() {
        return Err(SigningBlockError::UnexpectedEof);
    }
    let val = u16::from_be_bytes([data[*cursor], data[*cursor + 1]]);
    *cursor += 2;
    Ok(val)
}

/// Signing block serialization errors
#[derive(Debug, thiserror::Error)]
pub enum SigningBlockError {
    #[error("Data too short: expected {expected} bytes, got {actual}")]
    TooShort { expected: usize, actual: usize },
    #[error("Size mismatch: prefix={prefix}, suffix={suffix}")]
    SizeMismatch { prefix: usize, suffix: usize },
    #[error("Invalid magic bytes")]
    InvalidMagic,
    #[error("Unexpected end of data")]
    UnexpectedEof,
    #[error("Invalid identity type: {0}")]
    InvalidIdentityType(u8),
    #[error("Invalid digest algorithm: {0}")]
    InvalidDigestAlgorithm(u8),
    #[error("Invalid UTF-8 in section name")]
    InvalidUtf8,
    #[error("JSON parse error: {0}")]
    JsonParse(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_signing_block() -> SigningBlock {
        SigningBlock {
            signers: vec![Signer {
                certificates: vec![Certificate {
                    data: vec![1, 2, 3, 4],
                    identity: SignerIdentity::Developer,
                }],
                digest_algorithm: DigestAlgorithm::Sha256,
                digests: vec![
                    SectionDigest {
                        section_name: "manifest".into(),
                        digest: vec![0xAA; 32], // SHA-256 = 32 bytes
                    },
                    SectionDigest {
                        section_name: "prompts".into(),
                        digest: vec![0xBB; 32],
                    },
                ],
                signature: vec![0xCC; 64], // Ed25519 signature = 64 bytes
                signed_attrs: SignedAttributes {
                    signing_time: "2026-04-17T12:00:00Z".into(),
                },
            }],
        }
    }

    #[test]
    fn test_signing_block_roundtrip() {
        let block = sample_signing_block();
        let bytes = block.to_bytes();
        let decoded = SigningBlock::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.signers.len(), 1);
        assert_eq!(decoded.signers[0].certificates.len(), 1);
        assert_eq!(
            decoded.signers[0].certificates[0].identity,
            SignerIdentity::Developer
        );
        assert_eq!(decoded.signers[0].digest_algorithm, DigestAlgorithm::Sha256);
        assert_eq!(decoded.signers[0].digests.len(), 2);
        assert_eq!(decoded.signers[0].digests[0].section_name, "manifest");
        assert_eq!(decoded.signers[0].signature.len(), 64);
        assert_eq!(
            decoded.signers[0].signed_attrs.signing_time,
            "2026-04-17T12:00:00Z"
        );
    }

    #[test]
    fn test_signing_block_size_prefix_suffix() {
        let block = sample_signing_block();
        let bytes = block.to_bytes();

        // First 4 bytes = size prefix
        let prefix = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);

        // Last 4 bytes = size suffix
        let len = bytes.len();
        let suffix = u32::from_be_bytes([
            bytes[len - 4],
            bytes[len - 3],
            bytes[len - 2],
            bytes[len - 1],
        ]);

        assert_eq!(prefix, suffix);
    }

    #[test]
    fn test_signing_block_magic() {
        let block = sample_signing_block();
        let bytes = block.to_bytes();
        // Bytes 4..12 should be the magic
        assert_eq!(&bytes[4..12], SIGNING_BLOCK_MAGIC);
    }

    #[test]
    fn test_signing_block_invalid_magic() {
        let mut data = vec![0u8; 20];
        // Set a valid size prefix
        let size = 8u32; // just the magic
        data[0..4].copy_from_slice(&size.to_be_bytes());
        // Invalid magic
        data[4..12].copy_from_slice(b"BADBADAD");
        data[12..16].copy_from_slice(&size.to_be_bytes());
        // Extend to required length
        data.resize(20, 0);

        let result = SigningBlock::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_signing_block_too_short() {
        let result = SigningBlock::from_bytes(&[0u8; 5]);
        assert!(result.is_err());
    }

    #[test]
    fn test_signing_block_multiple_signers() {
        let block = SigningBlock {
            signers: vec![
                Signer {
                    certificates: vec![Certificate {
                        data: vec![1],
                        identity: SignerIdentity::Developer,
                    }],
                    digest_algorithm: DigestAlgorithm::Sha256,
                    digests: vec![SectionDigest {
                        section_name: "manifest".into(),
                        digest: vec![0xAA; 32],
                    }],
                    signature: vec![0xCC; 64],
                    signed_attrs: SignedAttributes {
                        signing_time: "2026-04-17T12:00:00Z".into(),
                    },
                },
                Signer {
                    certificates: vec![Certificate {
                        data: vec![2],
                        identity: SignerIdentity::Platform,
                    }],
                    digest_algorithm: DigestAlgorithm::Sha256,
                    digests: vec![SectionDigest {
                        section_name: "manifest".into(),
                        digest: vec![0xDD; 32],
                    }],
                    signature: vec![0xEE; 64],
                    signed_attrs: SignedAttributes {
                        signing_time: "2026-04-17T13:00:00Z".into(),
                    },
                },
            ],
        };

        let bytes = block.to_bytes();
        let decoded = SigningBlock::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.signers.len(), 2);
        assert_eq!(
            decoded.signers[1].certificates[0].identity,
            SignerIdentity::Platform
        );
    }

    // ------------------------------------------------------------------
    // V2 Binary Format Tests (S1.3)
    // ------------------------------------------------------------------

    #[test]
    fn test_signing_block_serialize_deserialize() {
        let block = sample_signing_block();
        let bytes = block.to_binary();
        let decoded = SigningBlock::from_binary(&bytes).unwrap();

        assert_eq!(decoded.signers.len(), 1);
        assert_eq!(decoded.signers[0].certificates.len(), 1);
        assert_eq!(
            decoded.signers[0].certificates[0].identity,
            SignerIdentity::Developer
        );
        assert_eq!(decoded.signers[0].digest_algorithm, DigestAlgorithm::Sha256);
        assert_eq!(decoded.signers[0].digests.len(), 2);
        assert_eq!(decoded.signers[0].digests[0].section_name, "manifest");
        assert_eq!(decoded.signers[0].digests[1].section_name, "prompts");
        assert_eq!(decoded.signers[0].signature.len(), 64);
        assert_eq!(
            decoded.signers[0].signed_attrs.signing_time,
            "2026-04-17T12:00:00Z"
        );
    }

    #[test]
    fn test_signing_block_magic_correct() {
        let block = sample_signing_block();
        let bytes = block.to_binary();

        // Leading magic (first 16 bytes)
        assert_eq!(&bytes[0..16], SIGNING_BLOCK_MAGIC_V2);
        // Trailing magic (last 16 bytes)
        assert_eq!(
            &bytes[bytes.len() - 16..bytes.len()],
            SIGNING_BLOCK_MAGIC_V2
        );
    }

    #[test]
    fn test_signing_block_size_fields_consistent() {
        let block = sample_signing_block();
        let bytes = block.to_binary();

        // Size prefix at offset 16 (u64 LE)
        let size_prefix = u64::from_le_bytes(bytes[16..24].try_into().unwrap());

        // Size suffix: after content, before trailing magic
        let suffix_offset = 24 + size_prefix as usize;
        let size_suffix =
            u64::from_le_bytes(bytes[suffix_offset..suffix_offset + 8].try_into().unwrap());

        assert_eq!(size_prefix, size_suffix);
    }

    #[test]
    fn test_signing_block_binary_invalid_magic() {
        let mut data = vec![0u8; 48];
        // Invalid leading magic
        data[0..16].copy_from_slice(b"XXXXXXXXXXXXXXXX");
        let result = SigningBlock::from_binary(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_signing_block_binary_too_short() {
        let result = SigningBlock::from_binary(&[0u8; 20]);
        assert!(result.is_err());
    }

    #[test]
    fn test_v1_and_v2_produce_same_signers() {
        let block = sample_signing_block();
        let v1_bytes = block.to_bytes();
        let v2_bytes = block.to_binary();

        let v1_decoded = SigningBlock::from_bytes(&v1_bytes).unwrap();
        let v2_decoded = SigningBlock::from_binary(&v2_bytes).unwrap();

        // Both formats should decode to the same signing block
        assert_eq!(v1_decoded.signers.len(), v2_decoded.signers.len());
        assert_eq!(
            v1_decoded.signers[0].certificates[0].identity,
            v2_decoded.signers[0].certificates[0].identity
        );
        assert_eq!(
            v1_decoded.signers[0].digest_algorithm,
            v2_decoded.signers[0].digest_algorithm
        );
        assert_eq!(
            v1_decoded.signers[0].digests.len(),
            v2_decoded.signers[0].digests.len()
        );
        assert_eq!(
            v1_decoded.signers[0].signature,
            v2_decoded.signers[0].signature
        );
    }
}
