use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(super) fn resolve_tls_paths(cert: Option<&str>, key: Option<&str>) -> Result<(String, String)> {
    match (cert, key) {
        (Some(c), Some(k)) => Ok((c.to_string(), k.to_string())),
        (Some(_), None) => anyhow::bail!("--cert provided without --key"),
        (None, Some(_)) => anyhow::bail!("--key provided without --cert"),
        (None, None) => {
            let (cert, key) =
                auto_generate_tls().context("auto TLS certificate generation failed")?;
            tracing::info!("Using auto-generated TLS certificate");
            Ok((
                cert.to_string_lossy().into_owned(),
                key.to_string_lossy().into_owned(),
            ))
        }
    }
}

/// Auto-generate a self-signed TLS certificate and persist it.
fn auto_generate_tls() -> Result<(PathBuf, PathBuf)> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("hypr-rdp");
    auto_generate_tls_in(&config_dir)
}

fn auto_generate_tls_in(config_dir: &Path) -> Result<(PathBuf, PathBuf)> {
    use std::os::unix::fs::OpenOptionsExt;

    let cert_path = config_dir.join("cert.pem");
    let key_path = config_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Reusing existing TLS certificate from {}",
            config_dir.display()
        );
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(config_dir).context("failed to create config directory")?;

    let lock_path = config_dir.join(".tls.lock");
    let lock_file = std::fs::File::create(&lock_path).context("failed to create TLS lock file")?;
    let lock_fd = std::os::fd::AsRawFd::as_raw_fd(&lock_file);
    let ret = unsafe { libc::flock(lock_fd, libc::LOCK_EX) };
    if ret != 0 {
        anyhow::bail!("failed to acquire TLS lock");
    }

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Reusing existing TLS certificate from {}",
            config_dir.display()
        );
        return Ok((cert_path, key_path));
    }

    let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(subject_alt_names)
            .context("failed to generate self-signed certificate")?;

    let tmp_key = config_dir.join(".key.pem.tmp");
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_key)
            .context("failed to create key.pem")?;
        std::io::Write::write_all(&mut f, key_pair.serialize_pem().as_bytes())
            .context("failed to write key.pem")?;
    }

    let tmp_cert = config_dir.join(".cert.pem.tmp");
    std::fs::write(&tmp_cert, cert.pem()).context("failed to write cert.pem")?;

    std::fs::rename(&tmp_key, &key_path).context("failed to finalize key.pem")?;
    std::fs::rename(&tmp_cert, &cert_path).context("failed to finalize cert.pem")?;

    tracing::info!(
        "Generated self-signed TLS certificate in {}",
        config_dir.display()
    );
    Ok((cert_path, key_path))
}

#[cfg(test)]
mod tests {
    use super::{auto_generate_tls_in, resolve_tls_paths};
    use std::os::unix::fs::PermissionsExt;

    use ironrdp_server::TlsIdentityCtx;

    #[test]
    fn resolve_tls_paths_requires_cert_and_key_pair() {
        let cert_only =
            resolve_tls_paths(Some("/tmp/cert.pem"), None).expect_err("cert without key must fail");
        assert!(format!("{cert_only:#}").contains("--cert provided without --key"));

        let key_only =
            resolve_tls_paths(None, Some("/tmp/key.pem")).expect_err("key without cert must fail");
        assert!(format!("{key_only:#}").contains("--key provided without --cert"));
    }

    #[test]
    fn resolve_tls_paths_preserves_custom_cert_and_key_paths() {
        let (cert, key) = resolve_tls_paths(Some("/tmp/cert.pem"), Some("/tmp/key.pem"))
            .expect("custom cert/key pair");

        assert_eq!(cert, "/tmp/cert.pem");
        assert_eq!(key, "/tmp/key.pem");
    }

    #[test]
    fn auto_generated_tls_identity_loads_and_reuses_existing_pair() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let dir = unique_temp_dir();
        std::fs::create_dir_all(&dir).expect("temp dir");

        let (cert, key) = auto_generate_tls_in(&dir).expect("generate TLS identity");
        assert!(cert.exists());
        assert!(key.exists());
        assert_eq!(cert, dir.join("cert.pem"));
        assert_eq!(key, dir.join("key.pem"));

        let key_mode = std::fs::metadata(&key)
            .expect("key metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(key_mode, 0o600);

        let tls_ctx =
            TlsIdentityCtx::init_from_paths(&cert, &key).expect("generated TLS identity loads");
        tls_ctx
            .make_acceptor()
            .expect("generated TLS acceptor is valid");

        let cert_metadata = std::fs::metadata(&cert).expect("cert metadata");
        let key_metadata = std::fs::metadata(&key).expect("key metadata");
        let reused = auto_generate_tls_in(&dir).expect("reuse TLS identity");
        assert_eq!(reused, (cert.clone(), key.clone()));
        assert_eq!(
            std::fs::metadata(&cert)
                .expect("reused cert metadata")
                .modified()
                .expect("reused cert modified time"),
            cert_metadata.modified().expect("cert modified time")
        );
        assert_eq!(
            std::fs::metadata(&key)
                .expect("reused key metadata")
                .modified()
                .expect("reused key modified time"),
            key_metadata.modified().expect("key modified time")
        );

        std::fs::remove_dir_all(&dir).expect("cleanup temp dir");
    }

    fn unique_temp_dir() -> std::path::PathBuf {
        let unique = format!(
            "hypr-rdp-tls-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }
}
