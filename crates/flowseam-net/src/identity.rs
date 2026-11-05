//! Self-signed identity and its SHA-256 fingerprint (design §7: pinning is the
//! only trust anchor; there is no CA).

use std::fs;
use std::path::Path;

use rcgen::{generate_simple_self_signed, CertifiedKey};
use sha2::{Digest, Sha256};

use crate::NetError;

pub type Fingerprint = [u8; 32];

#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    /// PKCS#8 DER.
    pub key_der: Vec<u8>,
}

impl Identity {
    pub fn generate() -> Result<Self, NetError> {
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["kmshare.local".to_string()])
                .map_err(|e| NetError::Cert(e.to_string()))?;
        Ok(Identity {
            cert_der: cert.der().to_vec(),
            key_der: key_pair.serialize_der(),
        })
    }

    /// Load the identity from `dir`, creating it on first run. A directory that
    /// holds only one of the two files is an error, never silently regenerated:
    /// regenerating would change the fingerprint every peer has pinned.
    pub fn load_or_create(dir: &Path) -> Result<Self, NetError> {
        let cert_path = dir.join("cert.der");
        let key_path = dir.join("key.der");
        match (cert_path.exists(), key_path.exists()) {
            (true, true) => Ok(Identity {
                cert_der: read(&cert_path)?,
                key_der: read(&key_path)?,
            }),
            (false, false) => {
                let id = Identity::generate()?;
                create_private_dir(dir)?;
                write_private(&key_path, &id.key_der)?;
                write_private(&cert_path, &id.cert_der)?;
                Ok(id)
            }
            _ => Err(NetError::Identity(format!(
                "partial identity in {}: exactly one of cert.der / key.der exists",
                dir.display()
            ))),
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        fingerprint(&self.cert_der)
    }
}

pub fn fingerprint(cert_der: &[u8]) -> Fingerprint {
    Sha256::digest(cert_der).into()
}

pub fn to_hex(fp: &Fingerprint) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Fingerprint> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let txt = std::str::from_utf8(chunk).ok()?;
        out[i] = u8::from_str_radix(txt, 16).ok()?;
    }
    Some(out)
}

fn read(p: &Path) -> Result<Vec<u8>, NetError> {
    fs::read(p).map_err(NetError::Io)
}

// 權限在建立當下就設為 0600（create_new 同時防止覆寫既有檔），
// 不先用預設 umask 寫出再 chmod，避免私鑰有一段時間可被其他使用者讀取。
fn write_private(p: &Path, bytes: &[u8]) -> Result<(), NetError> {
    use std::io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(p).map_err(NetError::Io)?;
    f.write_all(bytes).map_err(NetError::Io)?;
    Ok(())
}

// 私鑰目錄只給擁有者進入。
fn create_private_dir(dir: &Path) -> Result<(), NetError> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir).map_err(NetError::Io)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    // 真的呼叫 load_or_create 並讀回檔案權限：私鑰必須是 0600、目錄必須是 0700。
    #[test]
    fn key_material_is_owner_only_from_creation() {
        let base = std::env::temp_dir().join(format!("flowseam-net-id-{}-perm", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("identity");
        Identity::load_or_create(&dir).expect("create identity");

        let key_mode = fs::metadata(dir.join("key.der")).unwrap().permissions().mode() & 0o777;
        let cert_mode = fs::metadata(dir.join("cert.der")).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(key_mode, 0o600, "key.der 權限應為 0600");
        assert_eq!(cert_mode, 0o600, "cert.der 權限應為 0600");
        assert_eq!(dir_mode, 0o700, "identity 目錄權限應為 0700");

        // create_new：既有私鑰不可被覆寫，只能走 load 路徑。
        let again = Identity::load_or_create(&dir).expect("reload identity");
        assert_eq!(again.key_der, fs::read(dir.join("key.der")).unwrap());
        let _ = fs::remove_dir_all(&base);
    }
}
