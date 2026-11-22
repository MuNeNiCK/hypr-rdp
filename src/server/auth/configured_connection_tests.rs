//! Configured authentication through public TCP/TLS/CredSSP activation.
//! No compositor, shell hooks, custom PDUs or direct credential callbacks.

use super::{PreparedAuthentication, ServerSecurityMode};
use crate::config::{AuthConfig, ConfigCredentials};
use crate::input::RdpInputSessionSink;
use crate::server::{session_hooks_from_config, ClientConnectionHandler};
use anyhow::{Context, Result};
use ironrdp_connector::{ClientConnector, Config, ConnectionOutcome, ConnectorResult, Credentials};
use ironrdp_pdu::{gcc, rdp::capability_sets::MajorPlatformType};
use ironrdp_server::{
    DesktopSize, DisplayContext, DisplayUpdate, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, ServerEvent, ServerResult,
};
use ironrdp_tokio::{NetworkClient, TokioFramed};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

const USER: &str = "domain-user";
const PASSWORD: &str = "configured-auth-test-password";
const DOMAIN: &str = "ExampleDomain";
const DEADLINE: Duration = Duration::from_secs(10);

struct NoNetwork;
impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ConnectorResult<Vec<u8>> {
        panic!("configured loopback NTLM must not contact an external service")
    }
}

struct IdleAuthDisplay;
#[async_trait::async_trait]
impl RdpServerDisplay for IdleAuthDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: 64,
            height: 64,
        }
    }

    async fn updates(
        &mut self,
        _ctx: DisplayContext,
    ) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(Self))
    }
}
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for IdleAuthDisplay {
    async fn next_update(&mut self) -> ServerResult<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

struct AuthInputSink {
    activated: Arc<Semaphore>,
    disconnected: Arc<Semaphore>,
}
impl RdpInputSessionSink for AuthInputSink {
    fn set_keyboard_layout(&self, _: u32) {
        self.activated.add_permits(1);
    }
    fn session_ended(&self) {
        self.disconnected.add_permits(1);
    }
}

struct ConfiguredAuthServer {
    task: JoinHandle<ServerResult<()>>,
    events: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    addr: SocketAddr,
    tls: Arc<rustls::ClientConfig>,
    public_key: Vec<u8>,
    activated: Arc<Semaphore>,
    disconnected: Arc<Semaphore>,
}

impl ConfiguredAuthServer {
    async fn start(username: &str, domain: Option<&str>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let tls_server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let tls = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let activated = Arc::new(Semaphore::new(0));
        let disconnected = Arc::new(Semaphore::new(0));
        let authentication =
            PreparedAuthentication::new(AuthConfig::Configured(ConfigCredentials {
                username: username.into(),
                password: PASSWORD.into(),
                domain: domain.map(str::to_owned),
            }))
            .unwrap();
        assert_eq!(authentication.security, ServerSecurityMode::Hybrid);
        assert!(authentication.validator.is_none());
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_hybrid(
                tokio_rustls::TlsAcceptor::from(Arc::new(tls_server)),
                key.public_key_raw().to_vec(),
            )
            .with_no_input()
            .with_display_handler(IdleAuthDisplay)
            .with_connection_policy(authentication.security.connection_policy())
            .with_connection_handler(Some(Box::new(ClientConnectionHandler::new(
                Box::new(AuthInputSink {
                    activated: Arc::clone(&activated),
                    disconnected: Arc::clone(&disconnected),
                }),
                session_hooks_from_config(None, None, None),
            ))))
            .build();
        server.set_credentials(authentication.credentials);
        let events = server.event_sender().clone();
        let task = tokio::task::spawn_local(async move { server.run().await });
        let (tx, rx) = tokio::sync::oneshot::channel();
        events.send(ServerEvent::GetLocalAddr(tx)).unwrap();
        let addr = tokio::time::timeout(DEADLINE, rx)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        Self {
            task,
            events,
            addr,
            tls,
            public_key: key.public_key_raw().to_vec(),
            activated,
            disconnected,
        }
    }

    async fn connect(
        &self,
        username: &str,
        domain: Option<&str>,
        password: &str,
    ) -> Result<TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>> {
        tokio::time::timeout(DEADLINE, async {
            let tcp = TcpStream::connect(self.addr).await?;
            let mut connector =
                ClientConnector::new(client_config(username, domain, password), tcp.local_addr()?);
            let mut framed = TokioFramed::new(tcp);
            let upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector).await?;
            let stream = tokio_rustls::TlsConnector::from(Arc::clone(&self.tls))
                .connect(
                    "localhost".try_into().unwrap(),
                    framed.into_inner_no_leftover(),
                )
                .await?;
            let upgraded = ironrdp_tokio::mark_as_upgraded(upgrade, &mut connector);
            let mut framed = TokioFramed::new(stream);
            let outcome = ironrdp_tokio::connect_finalize(
                upgraded,
                connector,
                &mut framed,
                &mut NoNetwork,
                "localhost".into(),
                self.public_key.clone(),
                None,
            )
            .await?;
            anyhow::ensure!(
                matches!(outcome, ConnectionOutcome::Connected(_)),
                "configured-auth test requires activation, not a redirection"
            );
            // Client finalization can precede the server's connection-info path.
            self.activated.acquire().await?.forget();
            assert_eq!(self.activated.available_permits(), 0);
            Ok(framed)
        })
        .await
        .context("configured-auth connection timed out")?
    }

    async fn wait_for_disconnect(&self) {
        tokio::time::timeout(DEADLINE, self.disconnected.acquire())
            .await
            .expect("connection must finish server-side cleanup")
            .unwrap()
            .forget();
        assert_eq!(self.activated.available_permits(), 0);
        assert_eq!(self.disconnected.available_permits(), 0);
    }

    async fn stop(&mut self) {
        self.events
            .send(ServerEvent::Quit(
                "configured-auth regression complete".into(),
            ))
            .unwrap();
        tokio::time::timeout(DEADLINE, &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Drop for ConfiguredAuthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Public connector fixture with Hybrid/CredSSP required, not TLS-only admission.
fn client_config(username: &str, domain: Option<&str>, password: &str) -> Config {
    Config {
        desktop_size: DesktopSize {
            width: 64,
            height: 64,
        },
        monitor_layout: None,
        desktop_scale_factor: 0,
        enable_tls: false,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        credentials: Credentials::UsernamePassword {
            username: username.into(),
            password: password.into(),
        },
        domain: domain.map(str::to_owned),
        client_build: 0,
        client_name: "configured-auth-regression".into(),
        keyboard_type: gcc::KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0x0409,
        keyboard_functional_keys_count: 12,
        connection_type: gcc::ConnectionType::Lan,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".into(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        license_cache: None,
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: false,
        performance_flags: Default::default(),
        timezone_info: Default::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp_pdu::rdp::capability_sets::RailSupportLevel::SUPPORTED,
    }
}

#[tokio::test]
async fn configured_domain_admission_rejects_invalid_credentials_and_recovers() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut server = ConfiguredAuthServer::start(USER, Some(DOMAIN)).await;
            for (username, password) in
                [(USER, "wrong-password"), ("unknown-auth-account", PASSWORD)]
            {
                let rejection = server
                    .connect(username, Some(DOMAIN), password)
                    .await
                    .err()
                    .expect("invalid credentials must not reach activation");
                assert!(
                    !rejection.is::<tokio::time::error::Elapsed>(),
                    "credential rejection must finish before its deadline: {rejection:#}"
                );
                let connector_error = rejection
                    .downcast_ref::<ironrdp_connector::ConnectorError>()
                    .expect("expected a CredSSP authentication rejection");
                assert!(
                    matches!(
                        connector_error.kind(),
                        ironrdp_connector::ConnectorErrorKind::Credssp(error)
                            if error.nstatus == Some(
                                ironrdp_connector::sspi::credssp::NStatusCode::LOGON_FAILURE
                            )
                    ),
                    "expected CredSSP STATUS_LOGON_FAILURE, got {connector_error:?}"
                );
                server.wait_for_disconnect().await;
                assert_eq!(server.activated.available_permits(), 0);
            }
            let first = server.connect(USER, Some(DOMAIN), PASSWORD).await.unwrap();
            drop(first);
            server.wait_for_disconnect().await;
            let reconnected = server.connect(USER, Some(DOMAIN), PASSWORD).await.unwrap();
            drop(reconnected);
            server.wait_for_disconnect().await;
            server.stop().await;
        })
        .await;
}

#[tokio::test]
async fn configured_domain_and_existing_username_forms_reach_activation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            for (server_user, server_domain, client_user, client_domain) in [
                (USER, None, USER, None),
                (USER, Some(DOMAIN), USER, Some(DOMAIN)),
                ("ExampleDomain\\domain-user", None, USER, Some(DOMAIN)),
                (USER, None, "ExampleDomain\\domain-user", None),
                (USER, None, "domain-user@corp.example.com", None),
                (
                    "domain-user@corp.example.com",
                    None,
                    "domain-user@corp.example.com",
                    None,
                ),
                (USER, Some(DOMAIN), "ExampleDomain\\domain-user", None),
            ] {
                let mut server = ConfiguredAuthServer::start(server_user, server_domain).await;
                let client = server
                    .connect(client_user, client_domain, PASSWORD)
                    .await
                    .with_context(|| {
                        format!(
                            "server {server_user:?}/{server_domain:?}, client {client_user:?}/{client_domain:?}"
                        )
                    })
                    .unwrap();
                drop(client);
                server.wait_for_disconnect().await;
                server.stop().await;
            }
        })
        .await;
}
