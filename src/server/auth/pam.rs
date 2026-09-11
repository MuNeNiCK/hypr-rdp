//! Same-UID PAM authentication, isolated from the server's async runtime.
//!
//! Validation diagnostics emit only fixed static reasons; credentials,
//! usernames, domains, and secret lengths are never logged.
use std::ffi::{CStr, CString};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use ironrdp_server::{
    CredentialDecision, CredentialValidationError, CredentialValidator, Credentials,
};
use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;

const HELPER_ARG: &str = "--internal-pam-auth";
const PAYLOAD_LIMIT: usize = 65_536;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

// No Debug: this structure holds a password.
#[derive(Deserialize, Serialize)]
struct Request {
    service: String,
    username: String,
    password: String,
}

pub(crate) fn valid_pam_service(service: &str) -> bool {
    !service.is_empty()
        && service.len() <= 64
        && service.as_bytes()[0].is_ascii_alphanumeric()
        && service
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

fn valid_credentials(username: &str, password: &str) -> bool {
    !username.is_empty()
        && username.len() <= 256
        && !username.contains('\0')
        && !password.is_empty()
        && password.len() <= 4096
        && !password.contains('\0')
}

// Deterministic validator-entry reason for rejecting a presented credential
// set. None means the set passes the syntactic checks and proceeds to PAM.
fn credential_rejection_reason(username: &str, password: &str) -> Option<&'static str> {
    if username.is_empty() {
        Some("missing username")
    } else if password.is_empty() {
        Some("missing password")
    } else if valid_credentials(username, password) {
        None
    } else {
        Some("invalid credentials")
    }
}

fn service_exists(service: &str) -> bool {
    valid_pam_service(service)
        && ["/etc/pam.d", "/usr/lib/pam.d"]
            .iter()
            .any(|root| Path::new(root).join(service).is_file())
}

fn process_uid() -> Result<libc::uid_t> {
    // SAFETY: the UID getters have no pointer arguments or preconditions.
    let (real, effective) = unsafe { (libc::getuid(), libc::geteuid()) };
    anyhow::ensure!(
        real != 0 && real == effective,
        "PAM mode requires an unprivileged desktop user"
    );
    Ok(real)
}

fn user_uid(username: &str) -> Result<Option<libc::uid_t>> {
    let name = CString::new(username)?;
    let mut size = 1024;
    loop {
        let mut buffer = vec![0u8; size];
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all storage is valid for the call, and getpwnam_r initializes
        // record on success. Only the copied integer UID outlives buffer.
        let status = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < PAYLOAD_LIMIT {
            size *= 2;
            continue;
        }
        anyhow::ensure!(status == 0, "Linux account lookup failed");
        if result.is_null() {
            return Ok(None);
        }
        // SAFETY: a successful getpwnam_r with a non-null result initialized record.
        return Ok(Some(unsafe { record.assume_init().pw_uid }));
    }
}

struct PasswordConversation {
    username: CString,
    password: CString,
    answered_password: bool,
}

impl ConversationHandler for PasswordConversation {
    fn prompt_echo_on(&mut self, _: &CStr) -> std::result::Result<CString, ErrorCode> {
        Ok(self.username.clone())
    }
    fn prompt_echo_off(&mut self, _: &CStr) -> std::result::Result<CString, ErrorCode> {
        if self.answered_password {
            return Err(ErrorCode::CONV_ERR);
        }
        self.answered_password = true;
        Ok(self.password.clone())
    }
    fn radio_prompt(&mut self, _: &CStr) -> std::result::Result<bool, ErrorCode> {
        Err(ErrorCode::CONV_ERR)
    }
    fn binary_prompt(&mut self, _: u8, _: &[u8]) -> std::result::Result<(u8, Vec<u8>), ErrorCode> {
        Err(ErrorCode::CONV_ERR)
    }
    fn text_info(&mut self, _: &CStr) {}
    fn error_msg(&mut self, _: &CStr) {}
}

fn authenticate(request: Request) -> Result<bool> {
    let uid = process_uid()?;
    if !service_exists(&request.service)
        || !valid_credentials(&request.username, &request.password)
        || user_uid(&request.username)? != Some(uid)
    {
        return Ok(false);
    }
    let conversation = PasswordConversation {
        username: CString::new(request.username.as_str())?,
        password: CString::new(request.password.as_str())?,
        answered_password: false,
    };
    let mut pam = Context::new(&request.service, Some(&request.username), conversation)
        .map_err(|_| anyhow::anyhow!("PAM initialization failed"))?;
    if pam.authenticate(Flag::DISALLOW_NULL_AUTHTOK).is_err()
        || pam.acct_mgmt(Flag::DISALLOW_NULL_AUTHTOK).is_err()
    {
        return Ok(false);
    }
    let authenticated_user = pam
        .user()
        .map_err(|_| anyhow::anyhow!("PAM identity unavailable"))?;
    // Modules may canonicalize/remap PAM_USER. Authorization is on the final UID.
    Ok(user_uid(&authenticated_user)? == Some(uid))
}

fn read_request(input: impl Read) -> Result<Request> {
    let mut payload = Vec::new();
    input
        .take((PAYLOAD_LIMIT + 1) as u64)
        .read_to_end(&mut payload)?;
    anyhow::ensure!(payload.len() <= PAYLOAD_LIMIT, "PAM request exceeds limit");
    serde_json::from_slice(&payload).context("invalid PAM request")
}

/// This unprivileged helper never initializes Wayland or the RDP listener.
/// Credentials arrive only on stdin. No PAM output/errors are printed.
pub(crate) fn dispatch_helper() -> Option<i32> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(HELPER_ARG)) {
        return None;
    }
    if std::env::args_os().count() != 2 {
        return Some(2);
    }
    Some(
        match read_request(std::io::stdin().lock()).and_then(authenticate) {
            Ok(true) => 0,
            Ok(false) => 1,
            Err(_) => 2,
        },
    )
}

pub(super) struct PamValidator {
    service: String,
    executable: PathBuf,
    slots: Semaphore,
}

impl PamValidator {
    pub fn new(service: String) -> Result<Self> {
        process_uid()?;
        anyhow::ensure!(
            service_exists(&service),
            "PAM service file not found; install /etc/pam.d/{service}"
        );
        Ok(Self {
            service,
            executable: std::env::current_exe()?,
            slots: Semaphore::new(1),
        })
    }
}

#[async_trait]
impl CredentialValidator for PamValidator {
    async fn validate(
        &self,
        credentials: &Credentials,
    ) -> std::result::Result<CredentialDecision, CredentialValidationError> {
        if let Some(reason) =
            credential_rejection_reason(&credentials.username, &credentials.password)
        {
            tracing::warn!(reason, "PAM credentials rejected at validator entry");
            return Ok(CredentialDecision::Reject);
        }
        let Ok(_slot) = self.slots.try_acquire() else {
            tracing::warn!(reason = "PAM validation busy", "PAM credentials rejected");
            return Ok(CredentialDecision::Reject);
        };
        let request = Request {
            service: self.service.clone(),
            username: credentials.username.clone(),
            password: credentials.password.clone(),
        };
        // Domain does not select a separate provider: PAM resolves a Linux name.
        let payload = serde_json::to_vec(&request).map_err(|error| {
            tracing::warn!(reason = "PAM helper failure", "PAM credentials rejected");
            CredentialValidationError::new(error)
        })?;
        let status = run_helper(&self.executable, &[HELPER_ARG], &payload, AUTH_TIMEOUT)
            .await
            .map_err(|_| {
                tracing::warn!(
                    reason = "PAM helper failure or timeout",
                    "PAM credentials rejected"
                );
                CredentialValidationError::new(std::io::Error::other(
                    "PAM helper failed or timed out",
                ))
            })?;
        match status.code() {
            Some(0) => {
                tracing::info!("PAM credentials accepted");
                Ok(CredentialDecision::Accept)
            }
            Some(1) => {
                tracing::warn!(
                    reason = "authentication or account policy rejected",
                    "PAM credentials rejected"
                );
                Ok(CredentialDecision::Reject)
            }
            _ => {
                tracing::warn!(
                    reason = "PAM backend unavailable",
                    "PAM credentials rejected"
                );
                Err(CredentialValidationError::new(std::io::Error::other(
                    "PAM backend unavailable",
                )))
            }
        }
    }
}

struct PamChild {
    child: Child,
    group: i32,
}

impl PamChild {
    fn kill_group(&self) {
        if self.group > 0 {
            // SAFETY: this is the new process group created for our own child.
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
    }
}
impl Drop for PamChild {
    fn drop(&mut self) {
        // Also covers cancellation; Tokio's kill_on_drop handles child reaping.
        self.kill_group();
    }
}

async fn run_helper(
    executable: &Path,
    args: &[&str],
    payload: &[u8],
    deadline: Duration,
) -> Result<ExitStatus> {
    anyhow::ensure!(payload.len() <= PAYLOAD_LIMIT, "PAM request exceeds limit");
    let child = Command::new(executable)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let group = child.id().context("PAM helper has no process id")? as i32;
    let mut guard = PamChild { child, group };
    let outcome = tokio::time::timeout(deadline, async {
        let mut stdin = guard
            .child
            .stdin
            .take()
            .context("PAM helper stdin unavailable")?;
        stdin.write_all(payload).await?;
        drop(stdin);
        Ok::<_, anyhow::Error>(guard.child.wait().await?)
    })
    .await;
    guard.kill_group();
    let status = guard.child.wait().await;
    guard.group = 0;
    outcome.context("PAM helper timeout")??;
    Ok(status?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn pam_rejects_empty_nul_and_oversized_credentials() {
        for (user, password) in [
            ("", "secret"),
            ("alice", ""),
            ("a\0b", "secret"),
            ("alice", "s\0s"),
        ] {
            assert!(!valid_credentials(user, password));
        }
        assert!(!valid_credentials(&"a".repeat(257), "secret"));
        assert!(!valid_credentials("alice", &"x".repeat(4097)));
        assert!(valid_credentials("alice", "a password with spaces"));
    }

    #[test]
    fn pam_credential_rejection_reasons_are_deterministic() {
        assert_eq!(
            credential_rejection_reason("", "secret"),
            Some("missing username")
        );
        assert_eq!(
            credential_rejection_reason("", ""),
            Some("missing username")
        );
        assert_eq!(
            credential_rejection_reason("alice", ""),
            Some("missing password")
        );
        assert_eq!(
            credential_rejection_reason("a\0b", "secret"),
            Some("invalid credentials")
        );
        assert_eq!(
            credential_rejection_reason("alice", "s\0s"),
            Some("invalid credentials")
        );
        assert_eq!(
            credential_rejection_reason(&"a".repeat(257), "secret"),
            Some("invalid credentials")
        );
        assert_eq!(
            credential_rejection_reason("alice", &"x".repeat(4097)),
            Some("invalid credentials")
        );
        assert_eq!(credential_rejection_reason("alice", "secret"), None);
    }

    proptest! {
        #[test]
        fn generated_pam_service_cannot_escape_policy_directory(value in ".{0,100}") {
            if valid_pam_service(&value) {
                prop_assert!(value.len() <= 64);
                prop_assert_eq!(Path::new(&value).components().count(), 1);
                prop_assert!(!value.contains('/') && !value.contains('\0'));
                prop_assert!(value.as_bytes()[0].is_ascii_alphanumeric());
            }
        }
    }

    #[test]
    fn pam_request_parser_is_bounded_and_does_not_accept_partial_json() {
        assert!(read_request(&b"{}"[..]).is_err());
        assert!(read_request(&vec![b' '; PAYLOAD_LIMIT + 1][..]).is_err());
        let request = Request {
            service: "hypr-rdp".into(),
            username: "alice".into(),
            password: "secret".into(),
        };
        let payload = serde_json::to_vec(&request).unwrap();
        assert_eq!(read_request(payload.as_slice()).unwrap().username, "alice");
    }

    #[test]
    fn password_conversation_rejects_additional_secret_challenges() {
        let mut conversation = PasswordConversation {
            username: CString::new("alice").unwrap(),
            password: CString::new("secret").unwrap(),
            answered_password: false,
        };
        let prompt = CString::new("Password:").unwrap();
        assert!(conversation.prompt_echo_off(&prompt).is_ok());
        assert!(conversation.prompt_echo_off(&prompt).is_err());
        assert!(conversation.radio_prompt(&prompt).is_err());
        assert!(conversation.binary_prompt(0, &[]).is_err());
    }

    #[tokio::test]
    async fn pam_helper_failure_and_timeout_allow_the_next_request() {
        let shell = Path::new("/bin/sh");
        let failed = run_helper(
            shell,
            &["-c", "cat >/dev/null; exit 1"],
            b"request",
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(failed.code(), Some(1));
        let started = std::time::Instant::now();
        assert!(run_helper(
            shell,
            &["-c", "cat >/dev/null; sleep 30 & wait"],
            b"request",
            Duration::from_millis(100)
        )
        .await
        .is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(run_helper(
            shell,
            &["-c", "cat >/dev/null; exit 0"],
            b"request",
            Duration::from_secs(2)
        )
        .await
        .unwrap()
        .success());
    }

    #[tokio::test]
    async fn pam_helper_cancellation_kills_process_group() {
        let directory =
            std::env::temp_dir().join(format!("hypr-rdp-pam-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let pid_file = directory.join("pids");
        // The path is passed as a shell positional argument, never shell source.
        let task_path = pid_file.clone();
        let task = tokio::spawn(async move {
            run_helper(
                Path::new("/bin/sh"),
                &[
                    "-c",
                    "cat >/dev/null; sleep 30 & echo $$ $! > \"$1\"; wait",
                    "pam-test",
                    task_path.to_str().unwrap(),
                ],
                b"request",
                Duration::from_secs(30),
            )
            .await
        });
        let pids = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&pid_file) {
                    let pids: Vec<u32> = value
                        .split_whitespace()
                        .filter_map(|v| v.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        break pids;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let parent_reaped = !Path::new(&format!("/proc/{}", pids[0])).exists();
                let descendant_stopped = std::fs::read_to_string(format!("/proc/{}/stat", pids[1]))
                    .map(|stat| stat.rsplit_once(") ").unwrap().1.starts_with('Z'))
                    .unwrap_or(true);
                if parent_reaped && descendant_stopped {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled PAM helper or its descendant survived");
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn oversized_helper_input_is_refused_before_spawning() {
        assert!(run_helper(
            Path::new("/does/not/exist"),
            &[],
            &vec![0; PAYLOAD_LIMIT + 1],
            Duration::from_secs(1)
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("exceeds limit"));
    }
}
