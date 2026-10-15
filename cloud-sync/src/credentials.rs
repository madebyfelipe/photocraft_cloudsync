//! Optional OS-vault persistence. Failure leaves the caller's session credentials usable.
//! Calls wait at most three seconds; run them off the UI thread. A single detached
//! worker isolates OS unlock prompts and never blocks the sync service's lifecycle.
use std::{
    sync::{
        mpsc::{self, SyncSender},
        OnceLock,
    },
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    InvalidAccount,
    Unavailable,
}
impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidAccount => "Invalid cloud account for secure credential storage",
            Self::Unavailable => "Secure credential storage is unavailable or locked; credentials remain session-only",
        })
    }
}
impl std::error::Error for CredentialError {}
type Result<T> = std::result::Result<T, CredentialError>;
struct Account {
    service: String,
    user: String,
}
enum Operation {
    Save(String),
    Load,
    Delete,
}
struct Job {
    account: Account,
    operation: Operation,
    reply: mpsc::Sender<Result<Option<String>>>,
}
static WORKER: OnceLock<std::result::Result<SyncSender<Job>, ()>> = OnceLock::new();
const TIMEOUT: Duration = Duration::from_secs(3);

/// Saves in the OS vault, never a plaintext preferences file. On failure retain
/// the password only in memory and report session-only behavior.
pub fn save_credential(app: &str, endpoint: &str, user: &str, password: &str) -> Result<()> {
    if password.len() > 64 * 1024 {
        return Err(CredentialError::InvalidAccount);
    }
    request(account(app, endpoint, user)?, Operation::Save(password.into())).map(|_| ())
}
/// Loads a saved password; `Ok(None)` means there is no saved credential.
/// `Unavailable` means automatic sign-in cannot use the vault this session.
pub fn load_credential(app: &str, endpoint: &str, user: &str) -> Result<Option<String>> {
    request(account(app, endpoint, user)?, Operation::Load)
}
/// Removes only this app/account's credential. Missing entries are successful.
pub fn delete_credential(app: &str, endpoint: &str, user: &str) -> Result<()> {
    request(account(app, endpoint, user)?, Operation::Delete).map(|_| ())
}
fn account(app: &str, endpoint: &str, user: &str) -> Result<Account> {
    if !super::valid_label(app) || user.is_empty() || user.len() > 1024 || user.chars().any(char::is_control) {
        return Err(CredentialError::InvalidAccount);
    }
    let mut url = reqwest::Url::parse(endpoint).map_err(|_| CredentialError::InvalidAccount)?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"))))
    {
        return Err(CredentialError::InvalidAccount);
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(Account { service: format!("craft-cloud-sync.{app}"), user: super::hash(format!("{url}\n{user}").as_bytes()) })
}
fn request(account: Account, operation: Operation) -> Result<Option<String>> {
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = (account, operation);
        Err(CredentialError::Unavailable)
    }
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    {
        let sender = WORKER
            .get_or_init(|| {
                let (tx, rx) = mpsc::sync_channel::<Job>(1);
                std::thread::Builder::new()
                    .name("craft-credential-vault".into())
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            let result = backend(&job.account, job.operation);
                            let _ = job.reply.send(result);
                        }
                    })
                    .map_err(|_| ())?;
                Ok(tx)
            })
            .as_ref()
            .map_err(|_| CredentialError::Unavailable)?;
        dispatch(sender, account, operation, TIMEOUT)
    }
}
fn dispatch(sender: &SyncSender<Job>, account: Account, operation: Operation, timeout: Duration) -> Result<Option<String>> {
    let (reply, rx) = mpsc::channel();
    sender.try_send(Job { account, operation, reply }).map_err(|_| CredentialError::Unavailable)?;
    rx.recv_timeout(timeout).map_err(|_| CredentialError::Unavailable)?
}
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn backend(account: &Account, operation: Operation) -> Result<Option<String>> {
    let entry = keyring::Entry::new(&account.service, &account.user).map_err(|_| CredentialError::Unavailable)?;
    match operation {
        Operation::Save(password) => entry.set_password(&password).map(|_| None).map_err(|_| CredentialError::Unavailable),
        Operation::Load => match entry.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        },
        Operation::Delete => match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(CredentialError::Unavailable),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_identity_is_scoped_and_normalized() {
        let first = account("photo", "https://example.com/dav", "person").unwrap();
        let normalized = account("photo", "https://example.com/dav/", "person").unwrap();
        assert_eq!(first.user, normalized.user);
        assert_ne!(first.user, account("photo", "https://example.com/dav", "other").unwrap().user);
        assert_ne!(first.service, account("vector", "https://example.com/dav", "person").unwrap().service);
        assert_ne!(first.user, account("photo", "https://example.com/other", "person").unwrap().user);
        for bad in ["http://example.com/", "https://user:secret@example.com/", "https://example.com/?secret=x"] {
            assert!(account("photo", bad, "person").is_err())
        }
    }
    #[test]
    fn stalled_worker_and_full_queue_return_without_blocking() {
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let account = || account("photo", "https://example.com/", "person").unwrap();
        let start = std::time::Instant::now();
        assert_eq!(dispatch(&tx, account(), Operation::Load, Duration::from_millis(10)), Err(CredentialError::Unavailable));
        assert!(start.elapsed() < Duration::from_secs(1));
        let start = std::time::Instant::now();
        assert_eq!(dispatch(&tx, account(), Operation::Load, Duration::from_secs(3)), Err(CredentialError::Unavailable));
        assert!(start.elapsed() < Duration::from_secs(1));
        drop(rx);
    }
}
