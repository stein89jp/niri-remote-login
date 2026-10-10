use ironrdp_server::{
    BoundConnection, ConnectionBinder, ConnectionHandler, ConnectionPolicy, Credentials,
    DesktopSize, KeyboardEvent, MouseEvent, PostConnectionAction, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerError, ServerResult, TlsIdentityCtx,
};
use niri_rdp_bridge::{broker::Broker, listen_addr, server_error, video::Video, wayland::Desktop};
use std::{
    net::SocketAddr,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
fn env(name: &str) -> anyhow::Result<String> {
    let v = std::env::var(name).map_err(|_| anyhow::anyhow!("set {name}"))?;
    anyhow::ensure!(!v.is_empty(), "empty {name}");
    Ok(v)
}
fn credential(name: &str, variable: &str) -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CREDENTIALS_DIRECTORY") {
        Ok(PathBuf::from(dir).join(name))
    } else {
        Ok(PathBuf::from(env(variable)?))
    }
}
#[derive(Clone)]
struct Placeholder;
impl RdpServerInputHandler for Placeholder {
    fn keyboard(&mut self, _: KeyboardEvent) {}
    fn mouse(&mut self, _: MouseEvent) {}
}
#[async_trait::async_trait]
impl RdpServerDisplay for Placeholder {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: 1920,
            height: 1080,
        }
    }
    async fn updates(&mut self) -> ServerResult<Box<dyn RdpServerDisplayUpdates>> {
        Err(server_error("no authenticated desktop binding"))
    }
}
struct Disposable {
    active: Arc<Mutex<Option<Desktop>>>,
    path: PathBuf,
    output: String,
    video: Option<Video>,
}
#[async_trait::async_trait]
impl ConnectionBinder for Disposable {
    async fn bind_connection(
        &self,
        _: &Credentials,
        size: DesktopSize,
    ) -> ServerResult<BoundConnection> {
        let path = self.path.clone();
        let output = self.output.clone();
        let desktop = tokio::task::spawn_blocking(move || {
            Desktop::start(UnixStream::connect(path)?, output, size, None)
        })
        .await
        .map_err(|_| server_error("desktop startup task failed"))?
        .map_err(|_| server_error("explicit disposable Wayland socket unavailable"))?;
        *self.active.lock().unwrap() = Some(desktop.clone());
        Ok(BoundConnection {
            display: match &self.video {
                Some(video) => video.display(desktop.clone()),
                None => Box::new(desktop.clone()),
            },
            input: Box::new(desktop),
        })
    }
}
struct Lifecycle {
    active: Option<Arc<Mutex<Option<Desktop>>>>,
    broker: Option<Arc<Broker>>,
    loopback_only: bool,
}
impl ConnectionHandler for Lifecycle {
    fn on_accept(&mut self, p: SocketAddr) -> bool {
        !self.loopback_only || p.ip().is_loopback()
    }
    fn on_disconnected(
        &mut self,
        _: SocketAddr,
        _: Duration,
        error: Option<&ServerError>,
    ) -> PostConnectionAction {
        if let Some(error) = error {
            eprintln!("RDP connection ended: {error}");
        }
        if let Some(active) = &self.active
            && let Some(desktop) = active.lock().unwrap().take()
        {
            desktop.stop();
        }
        if let Some(b) = &self.broker {
            b.discard();
        }
        PostConnectionAction::Continue
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::args_os().len() == 1,
        "configuration uses NIRI_RDP_* environment variables; no password arguments"
    );
    anyhow::ensure!(
        std::env::var_os("SSLKEYLOGFILE").is_none(),
        "unset SSLKEYLOGFILE"
    );
    let mode = std::env::var("NIRI_RDP_MODE").unwrap_or_else(|_| "broker".into());
    let configured_listen = match std::env::var("NIRI_RDP_LISTEN") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => anyhow::bail!("NIRI_RDP_LISTEN must contain valid Unicode"),
    };
    let listen = listen_addr(&mode, configured_listen.as_deref())?;
    let identity = TlsIdentityCtx::init_from_paths(
        &credential("tls-cert", "NIRI_RDP_CERT")?,
        &credential("tls-key", "NIRI_RDP_KEY")?,
    )?;
    let acceptor = identity.make_acceptor()?;
    let output = std::env::var("NIRI_RDP_OUTPUT").unwrap_or_else(|_| "HEADLESS-1".into());
    let video = Video::from_env()?;
    let audio = niri_rdp_bridge::audio::Audio::default();
    let mut server = if mode == "broker" {
        let broker = Arc::new(
            Broker::new(
                PathBuf::from("/run/niri-rdp/broker.sock"),
                output,
                video.clone(),
            )
            .with_audio(audio.clone()),
        );
        // PAM receives ClientInfo credentials within authenticated server TLS.
        // Arbitrary PAM passwords do not supply a server-side NLA/NTLM secret.
        let mut server = RdpServer::builder()
            .with_addr(listen)
            .with_tls(acceptor)
            .with_input_handler(Placeholder)
            .with_display_handler(Placeholder)
            .with_honor_client_desktop_size(Some(DesktopSize {
                width: 4096,
                height: 4096,
            }))
            .with_gfx_factory(video.as_ref().map(|v| Box::new(v.clone()) as _))
            .with_sound_factory(Some(Box::new(audio.factory())))
            .with_connection_policy(ConnectionPolicy::Reject)
            .with_connection_handler(Some(Box::new(Lifecycle {
                broker: Some(broker.clone()),
                active: None,
                loopback_only: false,
            })))
            .build();
        server.set_credential_validator(Some(broker.clone()));
        server.set_connection_binder(Some(broker));
        server
    } else if mode == "disposable" {
        let path = PathBuf::from(env("NIRI_RDP_WAYLAND_SOCKET")?);
        anyhow::ensure!(
            path.is_absolute(),
            "Wayland socket must be an explicit absolute path"
        );
        let active = Arc::new(Mutex::new(None));
        let mut server = RdpServer::builder()
            .with_addr(listen)
            .with_hybrid(acceptor, identity.pub_key)
            .with_input_handler(Placeholder)
            .with_display_handler(Placeholder)
            .with_honor_client_desktop_size(Some(DesktopSize {
                width: 4096,
                height: 4096,
            }))
            .with_gfx_factory(video.as_ref().map(|v| Box::new(v.clone()) as _))
            .with_connection_policy(ConnectionPolicy::Reject)
            .with_connection_handler(Some(Box::new(Lifecycle {
                broker: None,
                active: Some(active.clone()),
                loopback_only: true,
            })))
            .build();
        server.set_credentials(Some(Credentials {
            username: env("NIRI_RDP_USERNAME")?,
            password: env("NIRI_RDP_PASSWORD")?,
            domain: None,
        }));
        server.set_connection_binder(Some(Arc::new(Disposable {
            path,
            output,
            active,
            video: video.clone(),
        })));
        server
    } else {
        anyhow::bail!("NIRI_RDP_MODE must be broker or disposable");
    };
    // Reconnect cookies would bypass a fresh PAM policy/account decision.
    if let Some(video) = &video {
        video.set_suppressed(server.display_suppressed_handle());
    }
    server.set_auto_reconnect_cookie(None);
    println!("Niri RDP frontend on {listen} ({mode}); dedicated session only");
    tokio::select! {result=server.run()=>result?,result=tokio::signal::ctrl_c()=>result?}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_accepts_remote_broker_peers_but_keeps_disposable_peers_local() {
        let mut lifecycle = Lifecycle {
            active: None,
            broker: None,
            loopback_only: false,
        };
        let remote = "192.168.1.50:50000".parse().unwrap();
        assert!(lifecycle.on_accept(remote));
        lifecycle.loopback_only = true;
        assert!(!lifecycle.on_accept(remote));
        assert!(lifecycle.on_accept("127.0.0.1:50000".parse().unwrap()));
        assert!(lifecycle.on_accept("[::1]:50000".parse().unwrap()));
    }
}
