use std::path::PathBuf;

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
    use std::os::unix::fs::OpenOptionsExt;

    let home = std::env::var("HOME").context("HOME not set")?;
    let config_dir = PathBuf::from(home).join(".config").join("hypr-rdp");
    let cert_path = config_dir.join("cert.pem");
    let key_path = config_dir.join("key.pem");

    if cert_path.exists() && key_path.exists() {
        tracing::info!(
            "Reusing existing TLS certificate from {}",
            config_dir.display()
        );
        return Ok((cert_path, key_path));
    }

    std::fs::create_dir_all(&config_dir).context("failed to create config directory")?;

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
