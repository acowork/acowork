//! Package signing (insert Signing Block into ZIP)
//!
//! Signs a .agent package by:
//! 1. Reading the ZIP contents
//! 2. Computing SHA-256 digests of each section
//! 3. Creating a SigningBlock with Ed25519 signature
//! 4. Writing the signed ZIP with the SigningBlock binary-embedded
//!    before the Central Directory (APK v2 style)

use acowork_core::packaging::PackageOptions;
use ed25519_dalek::Signer as Ed25519Signer;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

use crate::error::{Result, SignError};
use crate::keygen::KeyType;
use crate::keygen::load_keypair;
use crate::signing_block::{
    Certificate, DigestAlgorithm, SectionDigest, SignedAttributes, Signer as BlockSigner,
    SigningBlock,
};

/// Sign a .agent package
///
/// Reads the input ZIP, computes digests, signs them, and produces
/// a signed ZIP with the SigningBlock inserted before the Central Directory.
pub fn sign_package(
    input_path: &Path,
    output_path: &Path,
    key_dir: &Path,
    key_type: KeyType,
) -> Result<()> {
    let keypair = load_keypair(key_dir, key_type)?;

    // Read the input ZIP
    let input_data = fs::read(input_path)?;

    // Compute section digests
    let digests = compute_digests(&input_data)?;

    // Create signing attributes
    let signed_attrs = SignedAttributes {
        signing_time: signing_time()?,
    };

    // Sign the concatenated digests
    let signature_data = create_signature_data(&digests, &signed_attrs);
    let signature = keypair.signing_key.sign(&signature_data);

    // Build the signing block
    let block = SigningBlock {
        signers: vec![BlockSigner {
            certificates: vec![Certificate {
                data: keypair.public_key_bytes().to_vec(),
                identity: match key_type {
                    KeyType::Developer => crate::signing_block::SignerIdentity::Developer,
                    KeyType::Platform => crate::signing_block::SignerIdentity::Platform,
                },
            }],
            digest_algorithm: DigestAlgorithm::Sha256,
            digests,
            signature: signature.to_bytes().to_vec(),
            signed_attrs,
        }],
    };

    // Write the signed ZIP
    write_signed_zip(&input_data, &block, output_path)?;

    Ok(())
}

/// Compute SHA-256 digests for each file in the ZIP
///
/// Skips `META-INF/SIGNING.BLOCK` entries for legacy re-signing support.
fn compute_digests(zip_data: &[u8]) -> Result<Vec<SectionDigest>> {
    let reader = Cursor::new(zip_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let mut digests = Vec::with_capacity(archive.len());

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        // Skip legacy signing block entry if re-signing
        if name == "META-INF/SIGNING.BLOCK" {
            continue;
        }

        let mut content = Vec::new();
        file.read_to_end(&mut content)?;

        let mut hasher = Sha256::new();
        hasher.update(&content);
        let digest = hasher.finalize().to_vec();

        digests.push(SectionDigest {
            section_name: name,
            digest,
        });
    }

    // Sort by section name for deterministic ordering
    digests.sort_by(|a, b| a.section_name.cmp(&b.section_name));

    Ok(digests)
}

/// Signature timestamp — wall clock, unless `SOURCE_DATE_EPOCH` pins it.
///
/// A code signature carrying the time it was made is correct behaviour, but it
/// also means re-signing one unchanged package yields different bytes, so
/// "same commit, same artifact" cannot be asserted by anyone: not a
/// reproducibility check, not a mirror diff, not a second builder on another
/// machine. Reproducible-build tooling standardised on `SOURCE_DATE_EPOCH` for
/// exactly this hole. Set it (CI, a determinism gate) and the whole package is
/// a pure function of its source tree; leave it unset and the stamp is the real
/// signing time, which is what a normal build wants.
fn signing_time() -> Result<String> {
    let pinned = std::env::var("SOURCE_DATE_EPOCH").ok().filter(|s| !s.trim().is_empty());
    match pinned {
        Some(raw) => {
            let epoch = raw.trim().parse::<i64>().map_err(|_| {
                SignError::InvalidPackage(format!("SOURCE_DATE_EPOCH is not an integer: {raw}"))
            })?;
            chrono::DateTime::from_timestamp(epoch, 0)
                .map(|dt| dt.to_rfc3339())
                .ok_or_else(|| {
                    SignError::InvalidPackage(format!("SOURCE_DATE_EPOCH out of range: {raw}"))
                })
        }
        None => Ok(chrono::Utc::now().to_rfc3339()),
    }
}

/// Create the data to be signed (concatenation of all digests + signed_attrs JSON)
pub fn create_signature_data(
    digests: &[SectionDigest],
    signed_attrs: &SignedAttributes,
) -> Vec<u8> {
    let mut data = Vec::new();

    for digest in digests {
        data.extend_from_slice(digest.section_name.as_bytes());
        data.extend_from_slice(&digest.digest);
    }

    let attrs_json = serde_json::to_vec(signed_attrs).unwrap_or_default();
    data.extend_from_slice(&attrs_json);

    data
}

/// Write a signed ZIP with the SigningBlock binary-embedded before the Central Directory
///
/// The .agent ZIP format (V2, APK v2 style):
/// ```text
/// [Local file headers + file data]     — original ZIP content
/// [SigningBlock (binary)]              — inserted before CD
/// [Central Directory]                   — original CD, updated offsets
/// [End of Central Directory Record]     — updated CD offset
/// ```
fn write_signed_zip(original_data: &[u8], block: &SigningBlock, output_path: &Path) -> Result<()> {
    // Step 1: Read input and write clean ZIP to buffer (without signing block)
    let reader = Cursor::new(original_data);
    let mut archive = zip::ZipArchive::new(reader)?;

    let clean_zip_buffer = Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(clean_zip_buffer);
    // Keep each entry's own timestamp instead of letting
    // `SimpleFileOptions::default()` stamp the wall clock: re-signing an
    // unchanged package would otherwise rewrite the time of all twelve entries
    // and change its bytes, undoing the packer's pin. The signature step must
    // not be a second source of nondeterminism.
    let fallback_time = crate::packager::archive_entry_time(&PackageOptions::default())?;
    let stored_options =
        |mod_time: zip::DateTime| zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(mod_time);

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;
        let name = file.name().to_string();

        // Skip existing signing block if re-signing
        if name == "META-INF/SIGNING.BLOCK" {
            continue;
        }

        let mod_time = file.last_modified().unwrap_or(fallback_time);
        writer.start_file(&name, stored_options(mod_time))?;
        std::io::copy(&mut file, &mut writer)?;
    }

    let clean_zip_buffer = writer.finish()?.into_inner();

    // Step 2: Insert binary signing block before CD
    let block_bytes = block.to_binary();
    let signed_data = crate::zip_utils::insert_block_before_cd(&clean_zip_buffer, &block_bytes)?;

    // Step 3: Write result
    fs::write(output_path, signed_data)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_compute_digests() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-sign");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let zip_data = fs::read(&zip_path).unwrap();
        let digests = compute_digests(&zip_data).unwrap();

        assert_eq!(digests.len(), 2);
        // Should be sorted by name
        assert!(digests[0].section_name < digests[1].section_name);
        // Each digest should be 32 bytes (SHA-256)
        for d in &digests {
            assert_eq!(d.digest.len(), 32);
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_package_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-sign-roundtrip");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create test ZIP
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        // Sign
        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify the signed ZIP exists
        assert!(signed_path.exists());

        // Verify the binary signing block is present (searched by zip_utils)
        let signed_data = fs::read(&signed_path).unwrap();
        let found = crate::zip_utils::find_binary_signing_block(
            &signed_data,
            &crate::signing_block::SIGNING_BLOCK_MAGIC_V2,
        )
        .unwrap();
        assert!(
            found.is_some(),
            "Binary signing block not found in signed ZIP"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // S1.3 Integration Tests
    // ------------------------------------------------------------------

    /// Sign a package using the legacy V1 format (ZIP entry) for backward compat testing
    #[cfg(test)]
    fn sign_package_legacy(
        input_path: &Path,
        output_path: &Path,
        key_dir: &Path,
        key_type: KeyType,
    ) -> Result<()> {
        let keypair = load_keypair(key_dir, key_type)?;
        let input_data = fs::read(input_path)?;
        let digests = compute_digests(&input_data)?;
        let signed_attrs = SignedAttributes {
            signing_time: chrono::Utc::now().to_rfc3339(),
        };
        let signature_data = create_signature_data(&digests, &signed_attrs);
        let signature = keypair.signing_key.sign(&signature_data);
        let block = SigningBlock {
            signers: vec![BlockSigner {
                certificates: vec![Certificate {
                    data: keypair.public_key_bytes().to_vec(),
                    identity: match key_type {
                        KeyType::Developer => crate::signing_block::SignerIdentity::Developer,
                        KeyType::Platform => crate::signing_block::SignerIdentity::Platform,
                    },
                }],
                digest_algorithm: DigestAlgorithm::Sha256,
                digests,
                signature: signature.to_bytes().to_vec(),
                signed_attrs,
            }],
        };

        // Write as legacy ZIP entry
        let reader = Cursor::new(input_data);
        let mut archive = zip::ZipArchive::new(reader)?;
        let output_file = fs::File::create(output_path)?;
        let mut writer = zip::ZipWriter::new(output_file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let name = file.name().to_string();
            if name == "META-INF/SIGNING.BLOCK" {
                continue;
            }
            writer.start_file(&name, options)?;
            std::io::copy(&mut file, &mut writer)?;
        }

        let block_bytes = block.to_bytes(); // V1 legacy format
        writer.start_file("META-INF/SIGNING.BLOCK", options)?;
        writer.write_all(&block_bytes)?;
        writer.finish()?;

        Ok(())
    }

    #[test]
    fn test_sign_binary_embed() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-binary-embed");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create test ZIP
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        // Sign with V2 binary format
        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify the binary signing block is embedded before CD
        let signed_data = fs::read(&signed_path).unwrap();
        let block_data = crate::zip_utils::find_binary_signing_block(
            &signed_data,
            &crate::signing_block::SIGNING_BLOCK_MAGIC_V2,
        )
        .unwrap()
        .expect("Binary signing block should be present");

        // The block should be parseable as V2 binary format
        let block = SigningBlock::from_binary(&block_data).unwrap();
        assert_eq!(block.signers.len(), 1);
        assert_eq!(block.signers[0].digests.len(), 2); // manifest.toml + prompts/system.md

        // The signing block should NOT be a ZIP entry
        let reader = Cursor::new(&signed_data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();
        for i in 0..archive.len() {
            let file = archive.by_index(i).unwrap();
            assert_ne!(
                file.name(),
                "META-INF/SIGNING.BLOCK",
                "V2 signing block should not be a ZIP entry"
            );
        }

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_binary_embed() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-verify-binary");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify with the verify module
        let result = crate::verify::verify_package(&signed_path).unwrap();
        assert!(result.valid);
        assert_eq!(result.signer, "developer");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_verify_roundtrip() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-binary-roundtrip");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create and sign
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify
        let result = crate::verify::verify_package(&signed_path).unwrap();
        assert!(result.valid, "Signature should be valid");
        assert_eq!(result.signer, "developer");
        assert_eq!(result.sections_count, 2);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_verify_legacy_format() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-legacy-verify");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create test ZIP
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        // Sign with legacy V1 format
        let signed_path = tmp_dir.join("signed_legacy.agent");
        sign_package_legacy(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify should succeed with backward-compatible verification
        let result = crate::verify::verify_package(&signed_path).unwrap();
        assert!(result.valid, "Legacy format should verify successfully");
        assert_eq!(result.signer, "developer");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_signed_zip_still_valid() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-zip-valid");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Generate key pair
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        // Create test ZIP
        let zip_path = tmp_dir.join("test.agent");
        create_test_zip(&zip_path);

        // Sign
        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // The signed file should still be a valid ZIP
        let signed_data = fs::read(&signed_path).unwrap();
        let reader = Cursor::new(&signed_data);
        let mut archive = zip::ZipArchive::new(reader).unwrap();

        // All original entries should be readable
        assert_eq!(archive.len(), 2);

        let mut names = Vec::new();
        for i in 0..archive.len() {
            let file = archive.by_index(i).unwrap();
            names.push(file.name().to_string());
        }
        names.sort();

        assert_eq!(names[0], "manifest.toml");
        assert_eq!(names[1], "prompts/system.md");

        // Read and verify content
        let mut manifest = String::new();
        archive
            .by_name("manifest.toml")
            .unwrap()
            .read_to_string(&mut manifest)
            .unwrap();
        assert_eq!(manifest, "agent_id = \"com.test\"");

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // Boundary / edge-case tests (review fix #3)
    // ------------------------------------------------------------------

    /// Helper: generate keypair, sign, verify, and check ZIP integrity
    fn sign_verify_and_check_zip(zip_path: &Path, tmp_dir: &Path, expected_file_count: usize) {
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();

        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify signature
        let result = crate::verify::verify_package(&signed_path).unwrap();
        assert!(result.valid, "Signature should be valid");
        assert_eq!(result.signer, "developer");
        assert_eq!(result.sections_count, expected_file_count);

        // Verify ZIP structural integrity with zip crate
        let signed_data = fs::read(&signed_path).unwrap();
        let reader = Cursor::new(&signed_data);
        let archive = zip::ZipArchive::new(reader).unwrap();
        assert_eq!(archive.len(), expected_file_count);
    }

    #[test]
    fn test_sign_verify_with_many_files() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-many-files");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create ZIP with 25 files
        let zip_path = tmp_dir.join("test.agent");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        for i in 0..25 {
            let name = format!("file_{:02}.txt", i);
            writer.start_file(&name, options).unwrap();
            writer
                .write_all(format!("Content of file {}", i).as_bytes())
                .unwrap();
        }
        writer.finish().unwrap();

        sign_verify_and_check_zip(&zip_path, &tmp_dir, 25);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_verify_with_large_file() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-large-file");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create ZIP with one 1MB+ file
        let zip_path = tmp_dir.join("test.agent");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        writer.start_file("large.bin", options).unwrap();
        // Write 1.5 MB of data
        let chunk = vec![0xAB_u8; 1024];
        for _ in 0..1536 {
            writer.write_all(&chunk).unwrap();
        }
        writer.finish().unwrap();

        sign_verify_and_check_zip(&zip_path, &tmp_dir, 1);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_verify_with_nested_dirs() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-nested-dirs");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create ZIP with deeply nested directory structure
        let zip_path = tmp_dir.join("test.agent");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        let nested_files = [
            "manifest.toml",
            "prompts/system.md",
            "prompts/default.md",
            "skills/search/skill.md",
            "skills/search/prompts/query.md",
            "skills/code/skill.md",
            "skills/code/prompts/review.md",
            "config/settings.toml",
            "config/defaults/profile.toml",
        ];

        for name in &nested_files {
            writer.start_file(name, options).unwrap();
            writer
                .write_all(format!("Content of {}", name).as_bytes())
                .unwrap();
        }
        writer.finish().unwrap();

        sign_verify_and_check_zip(&zip_path, &tmp_dir, nested_files.len());

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_sign_verify_with_empty_files() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-empty-files");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create ZIP with empty files alongside non-empty ones
        let zip_path = tmp_dir.join("test.agent");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        writer.start_file("manifest.toml", options).unwrap();
        writer.write_all(b"agent_id = \"com.test\"").unwrap();

        writer.start_file("empty.txt", options).unwrap();
        // Write nothing — empty file

        writer.start_file("prompts/system.md", options).unwrap();
        writer.write_all(b"You are a test agent.").unwrap();

        writer.start_file("also_empty.toml", options).unwrap();
        // Another empty file

        writer.finish().unwrap();

        sign_verify_and_check_zip(&zip_path, &tmp_dir, 4);

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_signed_zip_readable_by_zip_crate() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-zip-crate-readable");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        // Create ZIP with multiple entries
        let zip_path = tmp_dir.join("test.agent");
        let file = fs::File::create(&zip_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();

        let entries: Vec<(&str, &[u8])> = vec![
            ("manifest.toml", b"agent_id = \"com.zipcrate\""),
            ("prompts/system.md", b"System prompt."),
            ("skills/test/skill.md", b"A test skill."),
        ];

        for (name, content) in &entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();

        // Sign
        crate::keygen::generate_and_save(&tmp_dir.join("keys"), KeyType::Developer).unwrap();
        let signed_path = tmp_dir.join("signed.agent");
        sign_package(
            &zip_path,
            &signed_path,
            &tmp_dir.join("keys"),
            KeyType::Developer,
        )
        .unwrap();

        // Verify the signed ZIP can be opened by zip::ZipArchive
        let signed_data = fs::read(&signed_path).unwrap();
        let reader = Cursor::new(&signed_data);
        let mut archive =
            zip::ZipArchive::new(reader).expect("Signed ZIP should be openable by zip crate");

        // All original entries should be readable
        assert_eq!(archive.len(), entries.len());

        // Verify each entry's content matches
        let mut found_names = Vec::new();
        for i in 0..archive.len() {
            let mut f = archive.by_index(i).unwrap();
            let name = f.name().to_string();
            let mut content = String::new();
            f.read_to_string(&mut content).unwrap();
            found_names.push(name.clone());

            // Find expected content
            let expected = entries.iter().find(|(n, _)| *n == name).unwrap();
            assert_eq!(
                content.as_bytes(),
                expected.1,
                "Content mismatch for entry '{}'",
                name
            );
        }
        found_names.sort();

        // Verify CD offset is correct by checking the archive can be re-read
        let reader2 = Cursor::new(&signed_data);
        let archive2 = zip::ZipArchive::new(reader2)
            .expect("Re-reading signed ZIP should succeed (CD offset valid)");
        assert_eq!(archive2.len(), entries.len());

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
