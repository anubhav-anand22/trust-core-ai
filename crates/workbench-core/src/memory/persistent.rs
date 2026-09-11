//! Encrypted long-term memory (`persistent_memory.json`).
//!
//! Blueprint requirement: user-level memory "encrypted at rest using AES-GCM …
//! with keys derived from local machine parameters". Layout:
//!
//! ```text
//! key      = PBKDF2-HMAC-SHA256(machine_id, APP_SALT, 200_000) -> 32 bytes
//! cipher   = AES-256-GCM, fresh random 96-bit nonce per write
//! on disk  = base64( nonce[12] || ciphertext || tag )
//! ```
//!
//! **Threat model — read this before relying on it.** The key is reconstructible
//! by anything that can run this app on this machine, so this is *obfuscation-
//! grade*: it defeats casual disk inspection, backup leakage and copy-to-another-
//! machine, not a determined local attacker. It is deliberately keyless-at-rest
//! (nothing to steal from a config file) at the cost of that ceiling.

use std::path::Path;

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{CoreError, Result};

/// Namespacing salt: binds the derived key to (this app, this purpose) so it can't
/// collide with any other PBKDF2 use of the same machine id.
const APP_SALT: &[u8] = b"sovereign-workbench/persistent-memory/v1";
const PBKDF2_ROUNDS: u32 = 200_000;
const NONCE_LEN: usize = 12;

/// Long-term, user-level memory. Written on session end and app reset; read once
/// at startup.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistentMemory {
    /// Facility metadata accumulated across sessions (unit names, plant sections,
    /// equipment registers…).
    #[serde(default)]
    pub facility_metadata: serde_json::Map<String, serde_json::Value>,
    /// Themes that recur across sessions, e.g. "pump-cavitation", "flange-leak".
    #[serde(default)]
    pub recurrent_tags: Vec<String>,
    /// Compressed digest of historical context.
    #[serde(default)]
    pub history_digest: String,
}

impl PersistentMemory {
    /// Load and decrypt. A missing / empty / corrupt / wrong-machine file returns a
    /// fresh default (with a warning) — long-term memory loss must never stop a
    /// session from starting.
    pub fn load(path: &Path) -> Self {
        match Self::try_load(path) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, ?path, "persistent memory unreadable; starting fresh");
                Self::default()
            }
        }
    }

    fn try_load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let armored = std::fs::read_to_string(path)?;
        let armored = armored.trim();
        if armored.is_empty() {
            return Ok(Self::default());
        }

        let blob = base64::engine::general_purpose::STANDARD
            .decode(armored)
            .map_err(|e| CoreError::Crypto(format!("base64 decode: {e}")))?;
        if blob.len() <= NONCE_LEN {
            return Err(CoreError::Crypto("stored blob is too short".into()));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);

        let plaintext = Self::cipher()
            .decrypt(Nonce::from_slice(nonce_bytes), ciphertext)
            .map_err(|_| {
                CoreError::Crypto("decryption failed (file tampered or from another machine)".into())
            })?;
        Ok(serde_json::from_slice(&plaintext)?)
    }

    /// Encrypt and atomically write (temp file + rename).
    pub fn persist(&self, path: &Path) -> Result<()> {
        let plaintext = serde_json::to_vec(self)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = Self::cipher()
            .encrypt(&nonce, plaintext.as_ref())
            .map_err(|_| CoreError::Crypto("encryption failed".into()))?;

        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(nonce.as_slice());
        blob.extend_from_slice(&ciphertext);
        let armored = base64::engine::general_purpose::STANDARD.encode(&blob);

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, armored.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// A short human-readable view for role C's "facility memory" prompt slot.
    pub fn context_blob(&self) -> String {
        let mut out = String::new();
        if !self.facility_metadata.is_empty() {
            out.push_str("FACILITY METADATA: ");
            out.push_str(&serde_json::to_string(&self.facility_metadata).unwrap_or_default());
            out.push('\n');
        }
        if !self.recurrent_tags.is_empty() {
            out.push_str("RECURRENT THEMES: ");
            out.push_str(&self.recurrent_tags.join(", "));
            out.push('\n');
        }
        if !self.history_digest.is_empty() {
            out.push_str("HISTORY: ");
            out.push_str(&self.history_digest);
        }
        out
    }

    /// Derive the machine-bound AES-256-GCM cipher.
    ///
    /// `machine_uid::get()` reads a stable per-OS identifier (Windows
    /// `MachineGuid`, Linux `/etc/machine-id`, macOS `IOPlatformUUID`). If that
    /// fails we fall back to a constant so the app still runs — memory is simply
    /// portable in that degraded case.
    fn cipher() -> Aes256Gcm {
        let machine_id =
            machine_uid::get().unwrap_or_else(|_| "workbench-fallback-machine-id".to_string());
        let key_bytes = pbkdf2::pbkdf2_hmac_array::<Sha256, 32>(
            machine_id.as_bytes(),
            APP_SALT,
            PBKDF2_ROUNDS,
        );
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persistent_memory.json");

        let mut mem = PersistentMemory::default();
        mem.recurrent_tags.push("flange-leak".into());
        mem.history_digest = "Unit-2 crude column inspected 2026-08.".into();
        mem.facility_metadata
            .insert("plant".into(), serde_json::json!("MRPL Phase-3"));
        mem.persist(&path).unwrap();

        // On-disk content must not contain the plaintext.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("flange-leak"));
        assert!(!raw.contains("MRPL"));

        let loaded = PersistentMemory::load(&path);
        assert_eq!(loaded.recurrent_tags, vec!["flange-leak".to_string()]);
        assert_eq!(loaded.history_digest, mem.history_digest);
        assert_eq!(
            loaded.facility_metadata.get("plant").unwrap(),
            &serde_json::json!("MRPL Phase-3")
        );
    }

    #[test]
    fn missing_file_is_fresh_default() {
        let dir = tempfile::tempdir().unwrap();
        let mem = PersistentMemory::load(&dir.path().join("nope.json"));
        assert!(mem.recurrent_tags.is_empty());
        assert!(mem.history_digest.is_empty());
    }

    #[test]
    fn corrupt_file_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persistent_memory.json");
        std::fs::write(&path, "this is not base64 ciphertext!!!").unwrap();
        let mem = PersistentMemory::load(&path); // must not panic
        assert!(mem.history_digest.is_empty());
    }
}
