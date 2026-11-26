//! Package verification (extract Signing Block + verify)
//!
//! Verifies a .bot package by:
//! 1. Extracting the SigningBlock (V2 binary format, or V1 legacy ZIP entry)
//! 2. Recomputing section digests
//! 3. Verifying the Ed25519 signature
//! 4. Validating certificate identity

use ed25519_dalek::{Signature, Verifier as Ed25519Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::error::{Result, SignError};
use crate::sign::create_signature_data;
use crate::signing_block::{SignerIdentity, SigningBlock};

/// Verify a .bot package signature from a file path.
///
/// This is the sole verification entry point. All callers (CLI install,
/// HTTP multipart upload, etc.) must write their input to a file first and
/// pass the path here.
pub fn verify_package(package_path: &Path) -> Result<VerificationResult> {
    let zip_data = fs::read(package_path)?;
    verify_package_impl(&zip_data)
}

/// Core verification logic operating on in-memory ZIP data.
fn verify_package_impl(zip_data: &[u8]) -> Result<VerificationResult> {
    let block = extract_signing_block(zip_data)?;

    if block.signers.is_empty() {
        return Err(SignError::InvalidPackage("No signers found".into()));
    }

    // Verify the first signer (Phase 1 supports single signer)
    let signer = &block.signers[0];

    // Recompute digests
    let computed_digests = recompute_digests(zip_data)?;

    // Verify digests match
    if signer.digests.len() != computed_digests.len() {
        return Err(SignError::VerificationFailed(format!(
            "Digest count mismatch: expected {}, got {}",
            signer.digests.len(),
            computed_digests.len()
        )));
    }

    for (expected, computed) in signer.digests.iter().zip(computed_digests.iter()) {
        if expected.section_name != computed.section_name {
            return Err(SignError::VerificationFailed(format!(
                "Section name mismatch: expected '{}', got '{}'",
                expected.section_name, computed.section_name
            )));
        }
        if expected.digest != computed.digest {
            return Err(SignError::VerificationFailed(format!(
                "Digest mismatch for section '{}'",
                expected.section_name
            )));
        }
    }

    // Verify signature
    if signer.certificates.is_empty() {
        return Err(SignError::VerificationFailed(
            "No certificates in signing block".into(),
        ));
    }

    let cert = &signer.certificates[0];
    let public_key_bytes: [u8; 32] = cert
        .data
        .clone()
        .try_into()
        .map_err(|_| SignError::InvalidPackage("Invalid public key length".into()))?;

    let verifying_key = VerifyingKey::from_bytes(&public_key_bytes)
        .map_err(|e| SignError::Ed25519(e.to_string()))?;

    let signature_bytes: [u8; 64] = signer
        .signature
        .clone()
        .try_into()
        .map_err(|_| SignError::InvalidPackage("Invalid signature length".into()))?;

    let signature = Signature::from_bytes(&signature_bytes);

    let signature_data = create_signature_data(&signer.digests, &signer.signed_attrs);

    verifying_key
        .verify(&signature_data, &signature)
        .map_err(|e| {
            SignError::VerificationFailed(format!("Signature verification failed: {e}"))
        })?;

    // Compute fingerprint
    let mut hasher = Sha256::new();
    hasher.update(public_key_bytes);
    let fingerprint = format!("{:x}", hasher.finalize());

    let signer_name = match cert.identity {
        SignerIdentity::Developer => "developer",
        SignerIdentity::Platform => "platform",
        SignerIdentity::CaIssued => "ca-issued",
    };

    Ok(VerificationResult {
        valid: true,
        signer: signer_name.to_string(),
        certificate_fingerprint: fingerprint,
        sections_count: signer.digests.len(),
    })
}

/// Extract the signing block from a .bot ZIP
///
/// Tries V2 binary format first (APK v2 style, embedded before CD),
/// then falls back to V1 legacy format (ZIP entry "META-INF/SIGNING.BLOCK").
fn extract_signing_block(zip_data: &[u8]) -> Result<SigningBlock> {
    // Try V2 binary format first
    if let Some(block) = extract_binary_signing_block(zip_data)? {
        return Ok(block);
    }

    // Fall back to V1 legacy ZIP entry format
    extract_legacy_signing_block(zip_data)
}

/// Extract signing block from V2 binary format (embedded before CD)
fn extract_binary_signing_block(zip_data: &[u8]) -> Result<Option<SigningBlock>> {
    match crate::zip_utils::find_binary_signing_block(
        zip_data,
        &crate::signing_block::SIGNING_BLOCK_MAGIC_V2,
    ) {
        Ok(Some(block_data)) => {
            let block = SigningBlock::from_binary(&block_data).map_err(|e| {
                SignError::InvalidPackage(format!("Invalid binary signing block: {e}"))
            })?;
            Ok(Some(block))
        }
        Ok(None) => Ok(None),
        Err(_) => Ok(None), // If ZIP structure is unreadable, try legacy
    }
}

/// Extract signing block from V1 legacy format (ZIP entry)
fn extract_legacy_signing_block(zip_data: &[u8]) -> Result<SigningBlock> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        if file.name() == "META-INF/SIGNING.BLOCK" {
            let mut block_data = Vec::new();
            file.read_to_end(&mut block_data)?;
            return SigningBlock::from_bytes(&block_data).map_err(|e| {
                SignError::InvalidPackage(format!("Invalid legacy signing block: {e}"))
            });
        }
    }

    Err(SignError::InvalidPackage(
        "No signing block found (neither V2 binary nor V1 ZIP entry)".into(),
    ))
}

/// Recompute SHA-256 digests for all files in the ZIP
///
/// Skips `META-INF/SIGNING.BLOCK` entries for legacy format compatibility.
/// In V2 format, the signing block is not a ZIP entry, so no skip is needed.
fn recompute_digests(zip_data: &[u8]) -> Result<Vec<crate::signing_block::SectionDigest>> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let mut digests = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        // Skip legacy signing block entry (no-op for V2 format)
        if name == "META-INF/SIGNING.BLOCK" {
            continue;
        }

        let mut content = Vec::new();
        file.read_to_end(&mut content)?;

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let digest = hasher.finalize().to_vec();

        digests.push(crate::signing_block::SectionDigest {
            section_name: name,
            digest,
        });
    }

    digests.sort_by(|a, b| a.section_name.cmp(&b.section_name));
    Ok(digests)
}

/// Verification result
#[derive(Debug)]
pub struct VerificationResult {
    /// Whether the signature is valid
    pub valid: bool,
    /// Signer identity (e.g., "developer", "platform")
    pub signer: String,
    /// SHA-256 fingerprint of the signer's certificate
    pub certificate_fingerprint: String,
    /// Number of sections verified
    pub sections_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keygen::KeyType;
    use std::io::Write;

    fn create_test_zip(path: &Path) {
        let file = fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        writer.start_file("manifest.toml", options).unwrap();
        writer.write_all(b"agent_id = \"com.test\"").unwrap();

        writer.start_file("prompts/system.md", options).unwrap();
        writer.write_all(b"You are a test agent.").unwrap();

        writer.finish().unwrap();
    }

    #[test]
    fn test_sign_and_verify_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-verify");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.bot");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.bot");
        crate::sign::sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify
        let result = verify_package(&signed_path).unwrap();
        assert!(result.valid);
        assert_eq!(result.signer, "developer");
        assert_eq!(result.sections_count, 2);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_unsigned_package() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-verify-unsigned");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let zip_path = tmp_dir.join("unsigned.bot");
        create_test_zip(&zip_path);

        let result = verify_package(&zip_path);
        assert!(result.is_err());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_tampered_package() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-verify-tampered");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.bot");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.bot");
        crate::sign::sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Tamper: create a new ZIP with modified content, then re-insert
        // the original signing block (which no longer matches the modified content)
        let signed_data = fs::read(&signed_path).unwrap();

        // Extract the binary signing block
        let block_data = crate::zip_utils::find_binary_signing_block(
            &signed_data,
            &crate::signing_block::SIGNING_BLOCK_MAGIC_V2,
        )
        .unwrap()
        .expect("Signing block should exist");

        // Read the signed ZIP and create a tampered version
        let reader = Cursor::new(&signed_data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        let tampered_zip_buffer = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(tampered_zip_buffer);
        let options = zip::write::SimpleFileOptions::default();

        for i in 0..archive.len() {
            let mut file = archive.by_index(i).unwrap();
            let name = file.name().to_string();

            if name == "manifest.toml" {
                // Tamper with content
                writer.start_file(&name, options).unwrap();
                writer.write_all(b"TAMPERED CONTENT").unwrap();
            } else {
                writer.start_file(&name, options).unwrap();
                std::io::copy(&mut file, &mut writer).unwrap();
            }
        }
        let tampered_zip_data = writer.finish().unwrap().into_inner();

        // Re-insert the original (now stale) signing block
        let tampered_data =
            crate::zip_utils::insert_block_before_cd(&tampered_zip_data, &block_data).unwrap();

        let tampered_path = tmp_dir.join("tampered.bot");
        fs::write(&tampered_path, tampered_data).unwrap();

        // Verify should fail
        let result = verify_package(&tampered_path);
        assert!(result.is_err(), "Tampered package should fail verification");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
