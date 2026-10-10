use ironrdp_server::{Credentials, RdpServer, TlsIdentityCtx};
use niri_rdp_transport_poc::{LISTEN_ADDR, SyntheticBackend};
use std::path::PathBuf;

fn required_env(name: &str) -> anyhow::Result<String> {
    let value = std::env::var(name).map_err(|_| anyhow::anyhow!("set {name} explicitly"))?;
    anyhow::ensure!(!value.is_empty(), "{name} must not be empty");
    Ok(value)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // No CLI password arguments, defaults, global logging subscriber, or key log output.
    anyhow::ensure!(
        std::env::args_os().len() == 1,
        "configure with POC_* environment variables; no arguments accepted"
    );
    anyhow::ensure!(
        std::env::var_os("SSLKEYLOGFILE").is_none(),
        "unset SSLKEYLOGFILE for this prototype"
    );
    let cert = PathBuf::from(required_env("POC_CERT")?);
    let key = PathBuf::from(required_env("POC_KEY")?);
    let username = required_env("POC_USERNAME")?;
    let password = required_env("POC_PASSWORD")?;
    let identity = TlsIdentityCtx::init_from_paths(&cert, &key)?;
    let acceptor = identity.make_acceptor()?;
    let backend = SyntheticBackend::default();
    let mut server = RdpServer::builder()
        .with_addr(LISTEN_ADDR)
        .with_hybrid(acceptor, identity.pub_key)
        .with_input_handler(backend.clone())
        .with_display_handler(backend.clone())
        .with_connection_handler(Some(Box::new(backend.clone())))
        .build();
    server.set_credentials(Some(Credentials {
        username,
        password,
        domain: None,
    }));
    println!(
        "Synthetic RDP test card on {LISTEN_ADDR}; Ctrl+C to stop. No Niri session is launched."
    );
    tokio::select! {
        result = server.run() => result?,
        result = tokio::signal::ctrl_c() => result?,
    }
    backend.0.lock().unwrap().release_all();
    Ok(())
}
