//! Real authenticated connections must execute the production shell-hook path.
//! No compositor, capture device, custom protocol parser or direct callback calls.

use super::*;
use ironrdp_connector::{ClientConnector, Config, ConnectorResult, Credentials};
use ironrdp_pdu::{gcc, rdp::capability_sets::MajorPlatformType};
use ironrdp_server::{
    DesktopSize, RdpServerDisplay, RdpServerDisplayUpdates, ServerEvent, ServerResult,
};
use ironrdp_tokio::{NetworkClient, TokioFramed};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

const USER: &str = "hook-user";
const PASSWORD: &str = "hook-test-password";
const DEADLINE: Duration = Duration::from_secs(10);

// NTLM with configured local credentials must never contact a KDC or other host.
struct NoNetwork;
impl NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ConnectorResult<Vec<u8>> {
        panic!("loopback NTLM authentication must not use an external service")
    }
}

struct IdleDisplay;
#[async_trait::async_trait]
impl RdpServerDisplay for IdleDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: 64,
            height: 64,
        }
    }

    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(Self))
    }
}
#[async_trait::async_trait]
impl RdpServerDisplayUpdates for IdleDisplay {
    async fn next_update(&mut self) -> ServerResult<Option<ironrdp_server::DisplayUpdate>> {
        std::future::pending().await
    }
}

struct InputSink(Arc<AtomicUsize>);
impl RdpInputSessionSink for InputSink {
    fn set_keyboard_layout(&self, _: u32) {}

    fn session_ended(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct HookServer {
    task: JoinHandle<ironrdp_server::ServerResult<()>>,
    events: tokio::sync::mpsc::UnboundedSender<ServerEvent>,
    addr: SocketAddr,
    tls: Arc<rustls::ClientConfig>,
    public_key: Vec<u8>,
    output: PathBuf,
    disconnects: Arc<AtomicUsize>,
}

impl HookServer {
    async fn start(name: &str) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let config = rustls::ServerConfig::builder()
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
        let output = session_hooks::test_support::hook_log_path(name);
        let hooks = session_hooks_from_config(
            Some(session_hooks::test_support::echo_to_log(&output, "start")),
            Some(session_hooks::test_support::echo_to_log(&output, "end")),
            None,
        );
        let disconnects = Arc::new(AtomicUsize::new(0));
        let authentication = PreparedAuthentication::new(crate::config::AuthConfig::Configured(
            Some(crate::config::ConfigCredentials {
                username: USER.into(),
                domain: None,
                password: PASSWORD.into(),
            }),
        ))
        .unwrap();
        assert_eq!(authentication.security, ServerSecurityMode::Hybrid);
        let mut server = RdpServer::builder()
            .with_addr(([127, 0, 0, 1], 0))
            .with_hybrid(
                tokio_rustls::TlsAcceptor::from(Arc::new(config)),
                key.public_key_raw().to_vec(),
            )
            .with_no_input()
            .with_display_handler(IdleDisplay)
            .with_connection_policy(authentication.security.connection_policy())
            .with_connection_handler(Some(Box::new(ClientConnectionHandler::new(
                Box::new(InputSink(Arc::clone(&disconnects))),
                hooks,
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
            output,
            disconnects,
        }
    }

    async fn connect(
        &self,
        password: &str,
    ) -> Result<TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>> {
        tokio::time::timeout(DEADLINE, async {
            let tcp = TcpStream::connect(self.addr).await?;
            let mut connector = ClientConnector::new(client_config(password), tcp.local_addr()?);
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
            ironrdp_tokio::connect_finalize(
                upgraded,
                connector,
                &mut framed,
                &mut NoNetwork,
                "localhost".into(),
                self.public_key.clone(),
                None,
            )
            .await?;
            Ok(framed)
        })
        .await
        .context("client connection timed out")?
    }

    async fn expect_output(&self, expected: &str) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let actual = std::fs::read_to_string(&self.output).unwrap_or_default();
                assert!(
                    expected.starts_with(&actual),
                    "unexpected hook output: {actual:?}"
                );
                if actual == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected hook output {expected:?}, got {:?}",
                std::fs::read_to_string(&self.output)
            )
        });
    }

    async fn stop(&mut self) {
        self.events
            .send(ServerEvent::Quit("hook regression complete".into()))
            .unwrap();
        tokio::time::timeout(DEADLINE, &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.output);
    }
}

// Matches the upstream public-connector handshake fixture; no wire construction.
fn client_config(password: &str) -> Config {
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
            username: USER.into(),
            password: password.into(),
        },
        domain: None,
        client_build: 0,
        client_name: "hook-regression".into(),
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
async fn authenticated_connections_run_session_hooks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut server = HookServer::start("authenticated-connections").await;
            let first = server.connect(PASSWORD).await.unwrap();
            server.expect_output("start\n").await;
            drop(first);
            server.expect_output("start\nend\n").await;
            let second = server.connect(PASSWORD).await.unwrap();
            server.expect_output("start\nend\nstart\n").await;
            let replacement = server.connect(PASSWORD).await.unwrap();
            server
                .expect_output("start\nend\nstart\nend\nstart\n")
                .await;
            drop(second);
            drop(replacement);
            server
                .expect_output("start\nend\nstart\nend\nstart\nend\n")
                .await;
            server.stop().await;
            assert_eq!(
                std::fs::read_to_string(&server.output).unwrap(),
                "start\nend\nstart\nend\nstart\nend\n"
            );
        })
        .await;
}

#[tokio::test]
async fn rejected_credentials_do_not_run_session_hooks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut server = HookServer::start("rejected-credentials").await;
            assert!(server.connect("wrong-password").await.is_err());
            tokio::time::timeout(DEADLINE, async {
                while server.disconnects.load(Ordering::SeqCst) == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("rejected connection must finish cleanup");
            assert_eq!(
                std::fs::read_to_string(&server.output).unwrap_or_default(),
                ""
            );
            let client = server.connect(PASSWORD).await.unwrap();
            server.expect_output("start\n").await;
            drop(client);
            server.expect_output("start\nend\n").await;
            server.stop().await;
            assert_eq!(
                std::fs::read_to_string(&server.output).unwrap(),
                "start\nend\n"
            );
        })
        .await;
}
