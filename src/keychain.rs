//! Secrets in the macOS keychain, as generic passwords.
//!
//! Tests use an in-memory stand-in, so they don't touch the user's keychain.

use anyhow::Result;

#[cfg(test)]
use fake as backend;
#[cfg(not(test))]
use system as backend;

/// The item for `service` and `account`, if there is one.
pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>> {
    backend::get(service, account)
}

/// Adds the item, or replaces it.
pub fn set(service: &str, account: &str, secret: &[u8]) -> Result<()> {
    backend::set(service, account, secret)
}

/// Removes the item, if there is one.
pub fn delete(service: &str, account: &str) -> Result<()> {
    backend::delete(service, account)
}

mod system {
    use anyhow::{Context as _, Result};
    use security_framework::base::Error;
    use security_framework::passwords;

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        match passwords::get_generic_password(service, account) {
            Ok(secret) => Ok(Some(secret)),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(error) => Err(error).context("reading from the keychain"),
        }
    }

    pub fn set(service: &str, account: &str, secret: &[u8]) -> Result<()> {
        passwords::set_generic_password(service, account, secret).context("saving to the keychain")
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        match passwords::delete_generic_password(service, account) {
            Err(error) if error.code() != NOT_FOUND => {
                Err::<(), Error>(error).context("removing from the keychain")
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod fake {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use anyhow::Result;

    type Items = HashMap<(String, String), Vec<u8>>;

    static ITEMS: LazyLock<Mutex<Items>> = LazyLock::new(Default::default);

    fn items() -> MutexGuard<'static, Items> {
        ITEMS.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(service: &str, account: &str) -> Result<Option<Vec<u8>>> {
        Ok(items().get(&(service.into(), account.into())).cloned())
    }

    pub fn set(service: &str, account: &str, secret: &[u8]) -> Result<()> {
        items().insert((service.into(), account.into()), secret.to_vec());
        Ok(())
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        items().remove(&(service.into(), account.into()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::system;

    /// Uses the real keychain, so it's run only on request:
    /// `cargo test keychain -- --ignored`.
    #[test]
    #[ignore]
    fn stores_reads_and_removes_items() {
        let (service, account) = ("1k Words test", format!("{}", std::process::id()));
        system::delete(service, &account).unwrap();
        assert_eq!(system::get(service, &account).unwrap(), None);
        system::set(service, &account, b"one").unwrap();
        system::set(service, &account, b"two").unwrap();
        assert_eq!(
            system::get(service, &account).unwrap().as_deref(),
            Some(&b"two"[..])
        );
        system::delete(service, &account).unwrap();
        system::delete(service, &account).unwrap();
        assert_eq!(system::get(service, &account).unwrap(), None);
    }
}
