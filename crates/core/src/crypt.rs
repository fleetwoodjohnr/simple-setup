//! Optional snapshot encryption with `age`. A random X25519 key encrypts the copied files and
//! the full manifest; the snapshot stores that key encrypted with the user's password (scrypt),
//! and the key itself is shown once as a recovery key. Everything stays a standard age file, so
//! `age -d` opens it with the password (the key file) or the recovery key (everything else).
//! Losing both means nobody can open the snapshot.

use std::fs;
use std::io::{Read, Write};
use std::iter;
use std::path::Path;

use age::secrecy::{ExposeSecret, SecretString};
use age::x25519;

use crate::{Error, IoContext, Result, invalid};

pub const KEY_FILE: &str = "keys/snapshot-key.age";
pub const SEALED_MANIFEST: &str = "simple-setup.json.age";
/// Recovery keys look like this.
const RECOVERY_PREFIX: &str = "AGE-SECRET-KEY-1";

#[derive(Clone)]
pub struct Key(x25519::Identity);

fn age_err(e: impl std::fmt::Display) -> Error {
    invalid(format!("Encryption error: {e}"))
}

impl Key {
    pub fn generate() -> Self {
        Self(x25519::Identity::generate())
    }

    /// Public half (`age1…`), recorded in the manifest.
    pub fn recipient(&self) -> String {
        self.0.to_public().to_string()
    }

    /// Opens the snapshot without the password. Show it once; never store it in the snapshot.
    pub fn recovery_key(&self) -> String {
        self.0.to_string().expose_secret().to_string()
    }

    /// This key as an age file encrypted with `password`.
    pub fn sealed_with(&self, password: &str) -> Result<Vec<u8>> {
        if password.chars().count() < 8 {
            return Err(invalid("Use a password of at least 8 characters."));
        }
        let enc = age::Encryptor::with_user_passphrase(SecretString::from(password.to_owned()));
        let mut out = Vec::new();
        let mut w = enc.wrap_output(&mut out).map_err(age_err)?;
        w.write_all(self.recovery_key().as_bytes())
            .map_err(age_err)?;
        w.finish().map_err(age_err)?;
        Ok(out)
    }

    /// The key of the snapshot at `bundle`, from its password or its recovery key. `recipient`
    /// is the public half the manifest records, if known; a key for another snapshot is refused.
    pub fn unlock(bundle: &Path, secret: &str, recipient: Option<&str>) -> Result<Self> {
        let secret = secret.trim();
        let text = if secret.to_ascii_uppercase().starts_with(RECOVERY_PREFIX) {
            secret.to_ascii_uppercase()
        } else {
            let path = bundle.join(KEY_FILE);
            let data = fs::read(&path).ctx(|| format!("Reading {}", path.display()))?;
            let dec = age::Decryptor::new(&data[..]).map_err(age_err)?;
            let id = age::scrypt::Identity::new(SecretString::from(secret.to_owned()));
            let mut r = dec
                .decrypt(iter::once(&id as &dyn age::Identity))
                .map_err(|_| invalid("Wrong password."))?;
            let mut text = String::new();
            r.read_to_string(&mut text).map_err(age_err)?;
            text.trim().to_string()
        };
        let key = Self(
            text.parse()
                .map_err(|_| invalid("That is not a valid recovery key."))?,
        );
        if recipient.is_some_and(|r| r != key.recipient()) {
            return Err(invalid("That key belongs to a different snapshot."));
        }
        Ok(key)
    }

    /// Wraps `w` so that what is written comes out encrypted; call `finish()` at the end.
    pub fn encrypt_to<W: Write>(&self, w: W) -> Result<age::stream::StreamWriter<W>> {
        let r = self.0.to_public();
        age::Encryptor::with_recipients(iter::once(&r as &dyn age::Recipient))
            .map_err(age_err)?
            .wrap_output(w)
            .map_err(age_err)
    }

    pub fn decrypt_from<R: Read>(&self, r: R) -> Result<impl Read> {
        age::Decryptor::new(r)
            .map_err(age_err)?
            .decrypt(iter::once(&self.0 as &dyn age::Identity))
            .map_err(|_| invalid("This file cannot be opened with the snapshot's key."))
    }

    /// A stable name for `path` that reveals nothing without this key (keyed SHA-256).
    pub fn opaque_name(&self, path: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(self.recovery_key().as_bytes());
        h.update([0]);
        h.update(path.as_bytes());
        format!("{:x}", h.finalize())[..40].to_string()
    }

    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut w = self.encrypt_to(&mut out)?;
        w.write_all(plain).map_err(age_err)?;
        w.finish().map_err(age_err)?;
        Ok(out)
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.decrypt_from(sealed)?
            .read_to_end(&mut out)
            .map_err(|_| invalid("Damaged encrypted data."))?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_and_recovery_key_open_the_same_key() {
        let t = tempfile::tempdir().unwrap();
        let k = Key::generate();
        fs::create_dir_all(t.path().join("keys")).unwrap();
        fs::write(
            t.path().join(KEY_FILE),
            k.sealed_with("correct horse").unwrap(),
        )
        .unwrap();
        let r = k.recipient();
        assert!(r.starts_with("age1"));
        let sealed = k.seal(b"hello").unwrap();
        assert!(!sealed.windows(5).any(|w| w == b"hello"));

        let by_pw = Key::unlock(t.path(), "correct horse", Some(&r)).unwrap();
        assert_eq!(by_pw.open(&sealed).unwrap(), b"hello");
        let by_recovery =
            Key::unlock(t.path(), &k.recovery_key().to_lowercase(), Some(&r)).unwrap();
        assert_eq!(by_recovery.open(&sealed).unwrap(), b"hello");

        assert_eq!(
            Key::unlock(t.path(), "wrong horse", Some(&r))
                .err()
                .unwrap()
                .to_string(),
            "Wrong password."
        );
        let other = Key::generate().recovery_key();
        assert!(Key::unlock(t.path(), &other, Some(&r)).is_err());
        assert!(k.sealed_with("short").is_err());
    }
}
