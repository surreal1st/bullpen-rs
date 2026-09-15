use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fs;

const SETTINGS_KEY_FILE_VAR: &str = "BULLPEN_SETTINGS_KEY_FILE";
const SETTINGS_KEY_VAR: &str = "BULLPEN_SETTINGS_KEY";
const SELF_MANAGED_KEY_ROW: &str = "secrets.settings_key";

/// Loads the external settings key from a file or environment variable.
/// Returns the key string (will be hashed to 32 bytes), or None if not configured.
fn load_external_settings_key() -> Option<String> {
    if let Ok(file) = std::env::var(SETTINGS_KEY_FILE_VAR)
        && !file.is_empty()
        && let Ok(value) = fs::read_to_string(&file)
    {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        // File not found or unreadable: fall through to inline check
    }

    std::env::var(SETTINGS_KEY_VAR).ok().and_then(|inline| {
        let trimmed = inline.trim();
        if !trimmed.is_empty() {
            Some(trimmed.to_string())
        } else {
            None
        }
    })
}

/// Gets the 32-byte AES-256-GCM key for settings encryption.
/// Prefers an external key file (hashed), falls back to a self-managed key stored in the database.
/// If neither exists, generates and stores a new key in the database.
pub fn get_settings_key(db: &store::Db) -> Result<Vec<u8>, String> {
    // Check for external key
    if let Some(external) = load_external_settings_key() {
        let mut hasher = Sha256::new();
        hasher.update(external.as_bytes());
        return Ok(hasher.finalize().to_vec());
    }

    // Check for self-managed key in database
    if let Ok(Some(hex_key)) = db.settings_get(SELF_MANAGED_KEY_ROW)
        && let Ok(key_bytes) = hex::decode(&hex_key)
        && key_bytes.len() == 32
    {
        return Ok(key_bytes);
    }

    // Generate and store a new key
    let mut key_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut key_bytes);
    let hex_key = hex::encode(key_bytes);
    db.settings_set(SELF_MANAGED_KEY_ROW, &hex_key)
        .map_err(|e| format!("Failed to store settings key: {}", e))?;

    Ok(key_bytes.to_vec())
}

/// Encrypts a plaintext string for storage in the database using AES-256-GCM.
/// Returns a base64-encoded string containing: IV || tag || ciphertext
/// Format matches TS: IV (12 bytes) || tag (16 bytes) || ciphertext
pub fn encrypt_for_storage(db: &store::Db, plaintext: &str) -> Result<String, String> {
    let key_bytes = get_settings_key(db)?;
    let key = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| format!("Invalid key: {}", e))?;

    // Generate a fresh 12-byte IV
    let mut iv_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut iv_bytes);
    let nonce = Nonce::from_slice(&iv_bytes);

    // Encrypt the plaintext - returns ciphertext with tag appended
    let ciphertext_with_tag = key
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("Encryption failed: {}", e))?;

    // The aes-gcm crate's encrypt returns: ciphertext || tag (16 bytes at end)
    // But TS format expects: IV || tag || ciphertext
    // So we need to extract the tag and rearrange
    if ciphertext_with_tag.len() < 16 {
        return Err("Encrypted output too short".to_string());
    }

    let split_point = ciphertext_with_tag.len() - 16;
    let ciphertext = &ciphertext_with_tag[..split_point];
    let tag = &ciphertext_with_tag[split_point..];

    // Build format: IV || tag || ciphertext
    let mut result = Vec::with_capacity(12 + 16 + ciphertext.len());
    result.extend_from_slice(&iv_bytes);
    result.extend_from_slice(tag);
    result.extend_from_slice(ciphertext);

    Ok(STANDARD.encode(&result))
}

/// Decrypts a ciphertext string encrypted with `encrypt_for_storage`.
/// Returns None if decryption fails (wrong key, tampered data, invalid format).
pub fn decrypt_for_storage(db: &store::Db, ciphertext: &str) -> Option<String> {
    // Decode from base64
    let raw = STANDARD.decode(ciphertext).ok()?;

    // Check minimum length: 12 bytes IV + 16 bytes tag + 0 bytes ciphertext minimum
    if raw.len() < 28 {
        return None;
    }

    // Extract components: IV (0..12) || tag (12..28) || ciphertext (28..)
    let iv_bytes = &raw[0..12];
    let tag = &raw[12..28];
    let encrypted = &raw[28..];

    // aes_gcm's decrypt method expects: ciphertext || tag (tag appended at end)
    let mut encrypted_with_tag = encrypted.to_vec();
    encrypted_with_tag.extend_from_slice(tag);

    // Get the key
    let key_bytes = get_settings_key(db).ok()?;
    let key = Aes256Gcm::new_from_slice(&key_bytes).ok()?;
    let nonce = Nonce::from_slice(iv_bytes);

    // Decrypt
    let plaintext = key.decrypt(nonce, encrypted_with_tag.as_ref()).ok()?;

    String::from_utf8(plaintext).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create an in-memory database for testing
    fn test_db() -> store::Db {
        store::Db::open(":memory:").expect("Failed to create test database")
    }

    #[test]
    fn test_roundtrip_encryption() {
        let db = test_db();
        let plaintext = "test secret value";

        let encrypted = encrypt_for_storage(&db, plaintext).expect("Encryption failed");
        let decrypted = decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_wrong_key_decryption() {
        let db = test_db();
        let plaintext = "test secret";

        let encrypted = encrypt_for_storage(&db, plaintext).expect("Encryption failed");

        // Corrupt the encrypted data to simulate wrong key
        // Change a byte in the ciphertext part
        let mut corrupted = STANDARD.decode(&encrypted).unwrap();
        if corrupted.len() > 28 {
            corrupted[28] ^= 0xFF; // Flip a bit in the ciphertext
        }
        let corrupted_b64 = STANDARD.encode(&corrupted);

        let result = decrypt_for_storage(&db, &corrupted_b64);
        assert!(
            result.is_none(),
            "Should return None for corrupted ciphertext"
        );
    }

    #[test]
    fn test_tampered_tag() {
        let db = test_db();
        let plaintext = "test secret";

        let encrypted = encrypt_for_storage(&db, plaintext).expect("Encryption failed");

        // Corrupt the tag (bytes 12-28)
        let mut tampered = STANDARD.decode(&encrypted).unwrap();
        if tampered.len() >= 13 {
            tampered[12] ^= 0xFF; // Flip a bit in the tag
        }
        let tampered_b64 = STANDARD.encode(&tampered);

        let result = decrypt_for_storage(&db, &tampered_b64);
        assert!(result.is_none(), "Should return None for tampered tag");
    }

    #[test]
    fn test_min_length_check() {
        let db = test_db();

        // Ciphertext too short (less than 28 bytes)
        let short = STANDARD.encode([0u8; 27]);
        let result = decrypt_for_storage(&db, &short);
        assert!(
            result.is_none(),
            "Should return None for ciphertext < 28 bytes"
        );
    }

    #[test]
    fn test_ts_compatibility() {
        // This test validates that Rust can decrypt ciphertext created by TS.
        // The test data was generated with TS using a throwaway key.
        let db = test_db();

        // Pre-set the self-managed key to match what TS used
        // Key hash (hex): SHA256("test-throwaway-key-12345")
        let ts_key_hex = "ecfdcdbeb7ca1791ed27a23e07bb8424985a5c51adb84d8f0d5ecfd8e7f4ee73";
        db.settings_set("secrets.settings_key", ts_key_hex)
            .expect("Failed to set test key");

        // Ciphertext: IV (12) || tag (16) || ciphertext, base64 encoded
        // Generated by TS with plaintext "Hello, World!" and key SHA256("test-throwaway-key-12345")
        let ts_plaintext = "Hello, World!";
        let ts_ciphertext = "W/4eYedWwzFbSzYznICUiOV/g48BNx1xyNKe6uU8g7D26z1ud1Wps2s=";

        let decrypted =
            decrypt_for_storage(&db, ts_ciphertext).expect("Failed to decrypt TS ciphertext");

        assert_eq!(
            decrypted, ts_plaintext,
            "Decrypted value should match TS plaintext"
        );
    }
}
