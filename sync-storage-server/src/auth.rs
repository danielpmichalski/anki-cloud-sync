use std::collections::HashMap;

use anyhow::{anyhow, Result};
use pbkdf2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use pbkdf2::Pbkdf2;
use sync_platform_api::AuthProvider;

/// Authenticates via `SYNC_USER*` env vars + PBKDF2. No DB required.
pub struct StandaloneAuthProvider {
    /// hkey → (email, pwhash)
    users: HashMap<String, (String, String)>,
}

impl StandaloneAuthProvider {
    pub fn from_env() -> Result<Self> {
        let mut idx = 1;
        let mut users = HashMap::new();
        loop {
            let envvar = format!("SYNC_USER{idx}");
            match std::env::var(&envvar) {
                Ok(val) => {
                    let (name, password) = val.split_once(':').ok_or_else(|| {
                        anyhow!("{envvar} should be in 'username:password' format")
                    })?;
                    let pwhash = if std::env::var("PASSWORDS_HASHED").is_ok() {
                        password.to_string()
                    } else {
                        Pbkdf2
                            .hash_password(
                                password.as_bytes(),
                                &SaltString::from_b64("tonuvYGpksNFQBlEmm3lxg").unwrap(),
                            )
                            .map_err(|e| anyhow!("hash password: {e}"))?
                            .to_string()
                    };
                    let hkey = anki::sync::http_server::derive_hkey(&format!("{name}:{password}"));
                    users.insert(hkey, (name.to_string(), pwhash));
                    idx += 1;
                }
                Err(_) => break,
            }
        }
        Ok(Self { users })
    }
}

impl AuthProvider for StandaloneAuthProvider {
    fn authenticate(&self, username: &str, password: &str) -> Result<(String, String)> {
        let hkey = anki::sync::http_server::derive_hkey(&format!("{username}:{password}"));
        let (email, pwhash) = self
            .users
            .get(&hkey)
            .ok_or_else(|| anyhow!("invalid user/pass"))?;
        let hash =
            PasswordHash::new(pwhash).map_err(|_| anyhow!("invalid pw hash in server config"))?;
        Pbkdf2
            .verify_password(password.as_bytes(), &hash)
            .map_err(|_| anyhow!("invalid user/pass"))?;
        Ok((hkey, email.clone()))
    }

    fn lookup_by_hkey(&self, hkey: &str) -> Result<String> {
        self.users
            .get(hkey)
            .map(|(email, _)| email.clone())
            .ok_or_else(|| anyhow!("unknown hkey"))
    }
}
