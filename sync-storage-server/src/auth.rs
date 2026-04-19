use std::collections::HashMap;

use anyhow::{anyhow, Result};
use pbkdf2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use pbkdf2::Pbkdf2;
use sync_storage_api::AuthProvider;

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

/// Authenticates via SQLite DB with bcrypt. Persists session keys for cross-instance re-hydration.
/// Note: these methods are called from within `block_in_place` contexts in rslib, so sync DB
/// calls are safe here.
pub struct CloudAuthProvider;

impl AuthProvider for CloudAuthProvider {
    fn authenticate(&self, username: &str, password: &str) -> Result<(String, String)> {
        use sync_storage_config as ssc;
        // block_in_place: called from async context; sqlite is blocking I/O
        tokio::task::block_in_place(|| ssc::verify_sync_credentials(username, password))?;
        let hkey = anki::sync::http_server::derive_hkey(&format!("{username}:{password}"));
        tokio::task::block_in_place(|| ssc::store_sync_key(username, &hkey))?;
        Ok((hkey, username.to_string()))
    }

    fn lookup_by_hkey(&self, hkey: &str) -> Result<String> {
        use sync_storage_config as ssc;
        tokio::task::block_in_place(|| ssc::lookup_user_by_sync_key(hkey))
    }
}
