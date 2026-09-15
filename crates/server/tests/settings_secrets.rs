use server::settings_secrets;
use store::Db;

/// Helper to create an in-memory database for testing
fn test_db() -> Db {
    Db::open(":memory:").expect("Failed to create test database")
}

#[test]
fn test_roundtrip_encryption() {
    let db = test_db();
    let plaintext = "test secret value";

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");
    let decrypted =
        settings_secrets::decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_multiple_encryptions_are_different() {
    let db = test_db();
    let plaintext = "same plaintext";

    let encrypted1 =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption 1 failed");
    let encrypted2 =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption 2 failed");

    // Different IVs mean different ciphertexts even for same plaintext
    assert_ne!(encrypted1, encrypted2);

    // But both decrypt to the same value
    let decrypted1 =
        settings_secrets::decrypt_for_storage(&db, &encrypted1).expect("Decryption 1 failed");
    let decrypted2 =
        settings_secrets::decrypt_for_storage(&db, &encrypted2).expect("Decryption 2 failed");

    assert_eq!(decrypted1, plaintext);
    assert_eq!(decrypted2, plaintext);
}

#[test]
fn test_corrupted_ciphertext_returns_none() {
    let db = test_db();
    let plaintext = "test secret";

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");

    // Corrupt the encrypted data
    let base64_chars: Vec<char> = encrypted.chars().collect();
    if base64_chars.len() > 10 {
        let mut corrupted = base64_chars;
        corrupted[10] = if corrupted[10] == 'A' { 'B' } else { 'A' };
        let corrupted_str: String = corrupted.iter().collect();

        let result = settings_secrets::decrypt_for_storage(&db, &corrupted_str);
        assert!(result.is_none(), "Should return None for corrupted base64");
    }
}

#[test]
fn test_wrong_key_decryption() {
    let db = test_db();
    let plaintext = "secret data";

    // Encrypt with one key
    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");

    // Create a new database (which will have a different key)
    let db2 = test_db();

    // Try to decrypt with wrong key - should fail
    let result = settings_secrets::decrypt_for_storage(&db2, &encrypted);
    assert!(result.is_none(), "Should return None for wrong key");
}

#[test]
fn test_invalid_base64() {
    let db = test_db();

    // Not valid base64
    let result = settings_secrets::decrypt_for_storage(&db, "not-valid-base64!!!");
    assert!(result.is_none(), "Should return None for invalid base64");
}

#[test]
fn test_too_short_ciphertext() {
    let db = test_db();

    // Ciphertext too short (less than 28 bytes when decoded)
    let short = "YQ=="; // decodes to "a" (1 byte)
    let result = settings_secrets::decrypt_for_storage(&db, short);
    assert!(
        result.is_none(),
        "Should return None for ciphertext < 28 bytes"
    );
}

#[test]
fn test_empty_plaintext() {
    let db = test_db();
    let plaintext = "";

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");
    let decrypted =
        settings_secrets::decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_long_plaintext() {
    let db = test_db();
    let plaintext = "a".repeat(10000);

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, &plaintext).expect("Encryption failed");
    let decrypted =
        settings_secrets::decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_special_characters() {
    let db = test_db();
    let plaintext = "!@#$%^&*(){}[]|:;<>?,./~`\n\t\r";

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");
    let decrypted =
        settings_secrets::decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_unicode_plaintext() {
    let db = test_db();
    let plaintext = "Hello, 世界! 🚀 مرحبا العالم";

    let encrypted =
        settings_secrets::encrypt_for_storage(&db, plaintext).expect("Encryption failed");
    let decrypted =
        settings_secrets::decrypt_for_storage(&db, &encrypted).expect("Decryption failed");

    assert_eq!(decrypted, plaintext);
}

#[test]
fn test_ts_compatibility_decrypt() {
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

    let decrypted = settings_secrets::decrypt_for_storage(&db, ts_ciphertext)
        .expect("Failed to decrypt TS ciphertext");

    assert_eq!(
        decrypted, ts_plaintext,
        "Decrypted value should match TS plaintext"
    );
}
