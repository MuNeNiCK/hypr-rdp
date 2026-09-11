//! Authentication policy for access to the existing desktop.
mod pam;

use crate::config::{AuthConfig, ConfigCredentials};
use ironrdp_server::{CredentialValidator, Credentials};
use std::sync::Arc;

pub(crate) use pam::{dispatch_helper, valid_pam_service};

pub(super) struct PreparedAuthentication {
    pub security: ServerSecurityMode,
    pub credentials: Option<Credentials>,
    pub validator: Option<Arc<dyn CredentialValidator>>,
}

impl PreparedAuthentication {
    pub fn new(config: AuthConfig) -> anyhow::Result<Self> {
        match config {
            AuthConfig::Configured(creds) => {
                let credentials = ironrdp_credentials(creds);
                Ok(Self {
                    security: security_mode_for_credentials(&credentials),
                    credentials,
                    validator: None,
                })
            }
            AuthConfig::Pam { service } => {
                let validator = pam::PamValidator::new(service)?;
                Ok(Self {
                    security: ServerSecurityMode::Tls,
                    credentials: None,
                    validator: Some(Arc::new(validator)),
                })
            }
        }
    }
}

pub(super) fn ironrdp_credentials(credentials: Option<ConfigCredentials>) -> Option<Credentials> {
    credentials.map(|credentials| Credentials {
        username: credentials.username,
        password: credentials.password,
        domain: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServerSecurityMode {
    Tls,
    Hybrid,
}

impl ServerSecurityMode {
    pub(super) fn allows_authenticated_replacement(self) -> bool {
        // IronRDP authenticates Hybrid candidates through CredSSP before eviction.
        // TLS alone only proves the handshake, so keep its existing queue policy.
        matches!(self, Self::Hybrid)
    }
}

pub(super) fn security_mode_for_credentials(
    credentials: &Option<Credentials>,
) -> ServerSecurityMode {
    if credentials.is_some() {
        ServerSecurityMode::Hybrid
    } else {
        ServerSecurityMode::Tls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_authentication_keeps_nla_and_no_auth_modes_separate() {
        let none = PreparedAuthentication::new(AuthConfig::Configured(None)).unwrap();
        assert_eq!(none.security, ServerSecurityMode::Tls);
        assert!(none.validator.is_none());
        let fixed = PreparedAuthentication::new(AuthConfig::Configured(Some(ConfigCredentials {
            username: "user".into(),
            password: "secret".into(),
        })))
        .unwrap();
        assert_eq!(fixed.security, ServerSecurityMode::Hybrid);
        assert!(fixed.credentials.is_some());
        assert!(fixed.validator.is_none());
    }

    #[test]
    fn missing_pam_service_is_a_startup_error_not_no_auth() {
        assert!(PreparedAuthentication::new(AuthConfig::Pam {
            service: "hypr-rdp-nonexistent-test-service".into()
        })
        .is_err());
    }
}
