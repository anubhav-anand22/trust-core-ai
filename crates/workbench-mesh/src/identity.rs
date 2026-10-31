//! Each device's long-term key, and the devices it has paired with.
//!
//! Pairing (code + PAKE) happens once. After that both sides remember the other's
//! public key, and a reconnect is a plain Noise handshake that only succeeds if
//! the key the other side proves it holds is one we stored (MESH-13).

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{MeshError, Result};

/// The Noise pattern the static keys are generated for. X25519 keys are the same
/// for the pairing (`XXpsk3`) and reconnect (`XX`) handshakes.
pub(crate) const KEY_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Short, stable id for a public key: the first 8 bytes of its SHA-256, in hex.
/// What the UI and the logs call a device.
pub fn device_id(public_key: &[u8]) -> String {
    Sha256::digest(public_key)[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// This device's key pair and display name.
#[derive(Clone)]
pub struct DeviceIdentity {
    pub name: String,
    pub(crate) private_key: Vec<u8>,
    pub public_key: Vec<u8>,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("name", &self.name)
            .field("device_id", &self.device_id())
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    name: String,
    private_key: String,
    public_key: String,
}

impl DeviceIdentity {
    pub fn generate(name: impl Into<String>) -> Result<Self> {
        let params = KEY_PARAMS.parse().map_err(MeshError::Noise)?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        Ok(Self { name: name.into(), private_key: kp.private, public_key: kp.public })
    }

    pub fn device_id(&self) -> String {
        device_id(&self.public_key)
    }

    /// Load the key from `path`, or create and save one there on first run.
    /// The file holds a private key: it is written owner-only on Unix.
    pub fn load_or_create(path: &Path, name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        if let Ok(raw) = std::fs::read_to_string(path) {
            let file: IdentityFile = serde_json::from_str(&raw)?;
            let decode = |s: &str| B64.decode(s).map_err(|e| MeshError::Store(e.to_string()));
            return Ok(Self {
                // The display name can change (the user renames the device); the key can't.
                name,
                private_key: decode(&file.private_key)?,
                public_key: decode(&file.public_key)?,
            });
        }
        let identity = Self::generate(name)?;
        let file = IdentityFile {
            name: identity.name.clone(),
            private_key: B64.encode(&identity.private_key),
            public_key: B64.encode(&identity.public_key),
        };
        write_private(path, &serde_json::to_vec_pretty(&file)?)?;
        Ok(identity)
    }
}

/// One device this device has paired with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub device_id: String,
    pub name: String,
    /// Base64 X25519 public key.
    pub public_key: String,
    pub paired_at_unix: u64,
}

/// The paired devices, saved as JSON (`mesh_devices.json`).
#[derive(Debug, Default)]
pub struct TrustStore {
    path: Option<PathBuf>,
    devices: Vec<TrustedDevice>,
}

impl TrustStore {
    /// Not saved anywhere. For tests and for a first run before a path exists.
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// A missing file is an empty store. A corrupt one is an error, not silently
    /// "nobody is trusted", so the user finds out why reconnects stopped working.
    pub fn load(path: &Path) -> Result<Self> {
        let devices = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: Some(path.to_path_buf()), devices })
    }

    pub fn devices(&self) -> &[TrustedDevice] {
        &self.devices
    }

    pub fn trusts(&self, public_key: &[u8]) -> Option<&TrustedDevice> {
        let key = B64.encode(public_key);
        self.devices.iter().find(|d| d.public_key == key)
    }

    /// Remember a device (replacing an older entry for the same key) and save.
    pub fn add(&mut self, name: &str, public_key: &[u8]) -> Result<TrustedDevice> {
        let key = B64.encode(public_key);
        self.devices.retain(|d| d.public_key != key);
        let device = TrustedDevice {
            device_id: device_id(public_key),
            name: name.to_string(),
            public_key: key,
            paired_at_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        self.devices.push(device.clone());
        self.save()?;
        Ok(device)
    }

    /// Forget a device. Its next reconnect is refused; it has to pair again.
    pub fn remove(&mut self, device_id: &str) -> Result<bool> {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        let removed = self.devices.len() != before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        match &self.path {
            Some(path) => write_private(path, &serde_json::to_vec_pretty(&self.devices)?),
            None => Ok(()),
        }
    }
}

/// Write via a temp file and rename, so a crash never leaves half a key file.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_identity_is_created_once_and_reloaded_after() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh_identity.json");
        let first = DeviceIdentity::load_or_create(&path, "laptop").unwrap();
        let again = DeviceIdentity::load_or_create(&path, "renamed").unwrap();
        assert_eq!(first.public_key, again.public_key);
        assert_eq!(again.name, "renamed");
        assert_eq!(first.public_key.len(), 32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "private key is owner-only");
        }
    }

    #[test]
    fn the_trust_store_survives_a_restart_and_forgets_on_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh_devices.json");
        let key = DeviceIdentity::generate("w").unwrap().public_key;

        let mut store = TrustStore::load(&path).unwrap();
        let added = store.add("workstation", &key).unwrap();
        store.add("workstation (renamed)", &key).unwrap();
        assert_eq!(store.devices().len(), 1, "same key, one entry");

        let mut reloaded = TrustStore::load(&path).unwrap();
        assert_eq!(reloaded.trusts(&key).unwrap().name, "workstation (renamed)");
        assert!(reloaded.remove(&added.device_id).unwrap());
        assert!(TrustStore::load(&path).unwrap().trusts(&key).is_none());
    }

    #[test]
    fn a_corrupt_store_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mesh_devices.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(TrustStore::load(&path).is_err());
    }
}
