//! SSH-key authentication, with no secret stored on the server.
//!
//! The client signs `hypr-rdp:<user>:<unix minute>` with `ssh-keygen -Y sign -n hypr-rdp` and
//! sends the raw Ed25519 signature, base64-encoded, as the RDP password. The server rebuilds the
//! data OpenSSH signed (PROTOCOL.sshsig) and checks it against the desktop user's authorized keys.
//! A signature is accepted for the current minute and one either side, and only once.
use std::collections::HashMap;
use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
use sha2::{Digest as _, Sha512};

/// The `ssh-keygen -Y sign -n` namespace; signatures made for other purposes cannot log in.
pub(crate) const NAMESPACE: &str = "hypr-rdp";
const KEY_TYPE: &str = "ssh-ed25519";
/// Minutes either side of the server's clock that a signature may carry.
const CLOCK_SKEW_MINUTES: u64 = 1;
const PASSWD_BUFFER_LIMIT: usize = 65_536;

/// The text a client signs to log in as `username` during `minute` (Unix time / 60).
pub(crate) fn message(username: &str, minute: u64) -> String {
    format!("{NAMESPACE}:{username}:{minute}")
}

fn put_ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// The bytes `ssh-keygen -Y sign` signs: the SSHSIG preamble with a SHA-512 message hash.
fn signed_data(message: &str) -> Vec<u8> {
    let mut data = b"SSHSIG".to_vec();
    put_ssh_string(&mut data, NAMESPACE.as_bytes());
    put_ssh_string(&mut data, b"");
    put_ssh_string(&mut data, b"sha512");
    put_ssh_string(&mut data, &Sha512::digest(message.as_bytes()));
    data
}

fn take_ssh_string(blob: &[u8]) -> Option<(&[u8], &[u8])> {
    let length = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    let rest = &blob[4..];
    Some((rest.get(..length)?, rest.get(length..)?))
}

/// The Ed25519 keys of an authorized_keys file. Other key types, comments and options are
/// skipped; SSH-only options such as `command=` have no meaning for RDP.
fn parse_authorized_keys(text: &str) -> Vec<VerifyingKey> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            words.find(|word| *word == KEY_TYPE)?;
            let blob = STANDARD.decode(words.next()?).ok()?;
            let (kind, rest) = take_ssh_string(&blob)?;
            let (key, _) = take_ssh_string(rest)?;
            if kind != KEY_TYPE.as_bytes() {
                return None;
            }
            VerifyingKey::from_bytes(key.try_into().ok()?).ok()
        })
        .collect()
}

fn decode_signature(password: &str) -> Option<Signature> {
    let bytes = STANDARD.decode(password.trim()).ok()?;
    Some(Signature::from_bytes(bytes.as_slice().try_into().ok()?))
}

fn current_minute() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() / 60)
}

/// Whether one of `keys` signed the login message for a minute around `now`.
fn verify(keys: &[VerifyingKey], username: &str, signature: &Signature, now: u64) -> bool {
    (now.saturating_sub(CLOCK_SKEW_MINUTES)..=now + CLOCK_SKEW_MINUTES).any(|minute| {
        let data = signed_data(&message(username, minute));
        keys.iter()
            .any(|key| key.verify_strict(&data, signature).is_ok())
    })
}

fn load_keys(path: &Path) -> Result<Vec<VerifyingKey>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read authorized keys {}", path.display()))?;
    Ok(parse_authorized_keys(&text))
}

/// The unprivileged user hypr-rdp runs as: only that account can log in, as with PAM.
fn desktop_user() -> Result<(String, PathBuf)> {
    // SAFETY: the UID getters have no pointer arguments or preconditions.
    let (real, effective) = unsafe { (libc::getuid(), libc::geteuid()) };
    anyhow::ensure!(
        real != 0 && real == effective,
        "SSH-key mode requires an unprivileged desktop user"
    );
    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all storage is valid for the call, and getpwuid_r initializes record on
        // success. The name and home are copied out before buffer is dropped.
        let status = unsafe {
            libc::getpwuid_r(
                real,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < PASSWD_BUFFER_LIMIT {
            size *= 2;
            continue;
        }
        anyhow::ensure!(
            status == 0 && !result.is_null(),
            "Linux account lookup failed"
        );
        // SAFETY: a successful getpwuid_r with a non-null result initialized record, and its
        // string fields point into buffer, which is still alive here.
        let record = unsafe { record.assume_init() };
        let (name, home) = unsafe {
            (
                CStr::from_ptr(record.pw_name),
                CStr::from_ptr(record.pw_dir),
            )
        };
        return Ok((
            name.to_str()
                .context("account name is not UTF-8")?
                .to_owned(),
            PathBuf::from(OsStr::from_bytes(home.to_bytes())),
        ));
    }
}

pub(super) struct SshKeyValidator {
    username: String,
    authorized_keys: PathBuf,
    /// Signatures already used, with the minute they were accepted; each logs in once.
    used: Mutex<HashMap<[u8; 64], u64>>,
}

impl SshKeyValidator {
    pub fn new(authorized_keys: Option<PathBuf>) -> Result<Self> {
        let (username, home) = desktop_user()?;
        let authorized_keys = authorized_keys.unwrap_or_else(|| home.join(".ssh/authorized_keys"));
        anyhow::ensure!(
            !load_keys(&authorized_keys)?.is_empty(),
            "no ssh-ed25519 key in {}",
            authorized_keys.display()
        );
        Ok(Self {
            username,
            authorized_keys,
            used: Mutex::new(HashMap::new()),
        })
    }

    fn decide(
        &self,
        keys: &[VerifyingKey],
        credentials: &Credentials,
        now: u64,
    ) -> CredentialDecision {
        let Some(signature) = decode_signature(&credentials.password) else {
            return CredentialDecision::Reject;
        };
        if credentials.username != self.username || !verify(keys, &self.username, &signature, now) {
            return CredentialDecision::Reject;
        }
        let mut used = self
            .used
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        used.retain(|_, minute| *minute + 2 * CLOCK_SKEW_MINUTES >= now);
        if used.insert(signature.to_bytes(), now).is_some() {
            return CredentialDecision::Reject;
        }
        CredentialDecision::Accept
    }
}

#[async_trait]
impl CredentialValidator for SshKeyValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> std::result::Result<CredentialDecision, CredentialValidationError> {
        // Read the keys on every attempt, so removing a key revokes it without a restart.
        let keys = load_keys(&self.authorized_keys).map_err(|error| {
            CredentialValidationError::new(std::io::Error::other(error.to_string()))
        })?;
        Ok(self.decide(&keys, credentials, current_minute()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Made with OpenSSH: `printf 'hypr-rdp:alice:29573100' | ssh-keygen -Y sign -n hypr-rdp`,
    // keeping the last 64 bytes of the signature blob, as a client does.
    const PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6+y9fWu8t2cRpSwOfCGBbMKvHJkoQJQI661gCSUatP hypr-rdp-test";
    const SIGNATURE: &str =
        "3+Zd5R1tgcMhtbgooPxmI/CMzFXkMPu80habBSNesu4ae2+YCjf5ykZ1Mpv43JAkOZ3wo5/uyNJfBG7JowWxAQ==";
    const MINUTE: u64 = 29_573_100;

    fn validator() -> SshKeyValidator {
        SshKeyValidator {
            username: "alice".into(),
            authorized_keys: PathBuf::new(),
            used: Mutex::new(HashMap::new()),
        }
    }

    fn login(username: &str, password: &str) -> Credentials {
        Credentials {
            username: username.into(),
            password: password.into(),
            domain: None,
        }
    }

    fn keys() -> Vec<VerifyingKey> {
        parse_authorized_keys(&format!(
            "# a comment\nssh-rsa AAAA other\nrestrict {PUBLIC_KEY}\n"
        ))
    }

    #[test]
    fn reads_only_ed25519_keys_and_tolerates_options() {
        assert_eq!(keys().len(), 1);
        assert!(parse_authorized_keys("ssh-ed25519 not-base64 x").is_empty());
    }

    #[test]
    fn accepts_an_openssh_signature_within_the_clock_window() {
        for now in [MINUTE - 1, MINUTE, MINUTE + 1] {
            assert_eq!(
                validator().decide(&keys(), &login("alice", SIGNATURE), now),
                CredentialDecision::Accept,
                "now = {now}"
            );
        }
    }

    #[test]
    fn rejects_old_wrong_user_unknown_key_and_garbage() {
        let v = validator();
        assert_eq!(
            v.decide(&keys(), &login("alice", SIGNATURE), MINUTE + 2),
            CredentialDecision::Reject
        );
        assert_eq!(
            v.decide(&keys(), &login("bob", SIGNATURE), MINUTE),
            CredentialDecision::Reject
        );
        assert_eq!(
            v.decide(&[], &login("alice", SIGNATURE), MINUTE),
            CredentialDecision::Reject
        );
        assert_eq!(
            v.decide(&keys(), &login("alice", "not a signature"), MINUTE),
            CredentialDecision::Reject
        );
    }

    #[test]
    fn a_signature_logs_in_only_once() {
        let v = validator();
        assert_eq!(
            v.decide(&keys(), &login("alice", SIGNATURE), MINUTE),
            CredentialDecision::Accept
        );
        assert_eq!(
            v.decide(&keys(), &login("alice", SIGNATURE), MINUTE),
            CredentialDecision::Reject
        );
    }
}
