use ironrdp_server::{
    BoundConnection, ConnectionBinder, ConnectionHandler, ConnectionPolicy, Credentials,
    DesktopSize, KeyboardEvent, MouseEvent, PostConnectionAction, RdpServer, RdpServerDisplay,
    RdpServerDisplayUpdates, RdpServerInputHandler, ServerError, ServerResult, TlsIdentityCtx,
};
use niri_rdp_bridge::{
    MicrophoneMode,
    broker::Broker,
    listen_addr,
    microphone::Microphone,
    performance::{ConnectionStats, connection as connection_stats},
    server_error,
    video::Video,
    wayland::Desktop,
};
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
    clipboard: niri_rdp_bridge::clipboard::Clipboard,
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
        desktop.attach_clipboard(self.clipboard.clone());
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
    clipboard: niri_rdp_bridge::clipboard::Clipboard,
    video: Option<Video>,
    stats: &'static ConnectionStats,
    /// Where the one disconnect summary line goes (stderr, so the journal).
    emit: fn(&str),
}
fn emit_to_journal(line: &str) {
    eprintln!("{line}");
}
impl ConnectionHandler for Lifecycle {
    fn on_accept(&mut self, p: SocketAddr) -> bool {
        let accepted = !self.loopback_only || p.ip().is_loopback();
        // Connection rejection happens before this hook, so an accepted peer
        // is the one connection whose credentials are validated next.
        if accepted && let Some(broker) = &self.broker {
            broker.begin_connection(Some(p.ip()));
        }
        accepted
    }
    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&ServerError>,
    ) -> PostConnectionAction {
        self.clipboard.clear();
        if let Some(error) = error {
            eprintln!(
                "RDP connection {peer} ended after {:.2}s: {}",
                duration.as_secs_f64(),
                error.report()
            );
        }
        // Exactly one payload-free summary per served connection: the server
        // calls this hook once after every connection, however it ended.
        let avc444 = self
            .video
            .as_ref()
            .is_some_and(|video| video.avc444_negotiated());
        (self.emit)(
            &self
                .stats
                .take()
                .line(peer, duration, error.is_some(), avc444),
        );
        if let Some(active) = &self.active
            && let Some(desktop) = active.lock().unwrap().take()
        {
            desktop.stop();
        }
        if let Some(b) = &self.broker {
            b.discard();
            b.begin_connection(None);
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
    let clipboard = niri_rdp_bridge::clipboard::Clipboard::default();
    let mut audio = niri_rdp_bridge::audio::Audio::default();
    if let Some(video) = &video {
        audio = audio.with_video(video.clone());
    }
    // Separate from playback: its own channel, thread, queue and socket.
    let microphone = match MicrophoneMode::from_env()? {
        MicrophoneMode::Auto => Some(Microphone::default()),
        MicrophoneMode::Off => None,
    };
    let mut server = if mode == "broker" {
        let broker = Arc::new(
            Broker::new(
                PathBuf::from("/run/niri-rdp/broker.sock"),
                output,
                video.clone(),
            )
            .with_audio(audio.clone())
            .with_microphone(microphone.clone())
            .with_clipboard(clipboard.clone())
            .with_login_limits(niri_rdp_bridge::login::LoginLimits::from_env()?),
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
            .with_rdpeai_factory(
                microphone
                    .as_ref()
                    .map(|microphone| Box::new(microphone.factory()) as _),
            )
            .with_cliprdr_factory(Some(Box::new(clipboard.clone())))
            .with_connection_policy(ConnectionPolicy::Reject)
            .with_connection_handler(Some(Box::new(Lifecycle {
                broker: Some(broker.clone()),
                active: None,
                loopback_only: false,
                clipboard: clipboard.clone(),
                video: video.clone(),
                stats: connection_stats(),
                emit: emit_to_journal,
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
            .with_cliprdr_factory(Some(Box::new(clipboard.clone())))
            .with_connection_policy(ConnectionPolicy::Reject)
            .with_connection_handler(Some(Box::new(Lifecycle {
                broker: None,
                active: Some(active.clone()),
                loopback_only: true,
                clipboard: clipboard.clone(),
                video: video.clone(),
                stats: connection_stats(),
                emit: emit_to_journal,
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
            clipboard: clipboard.clone(),
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
    server.set_tcp_socket_config(Some(niri_rdp_bridge::network::configure_tcp_liveness));
    server.enable_heartbeat(ironrdp_server::heartbeat::HeartbeatConfig {
        period_secs: 5,
        warning_count: 3,
        reconnect_count: 3,
    });
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
            clipboard: Default::default(),
            video: None,
            stats: Box::leak(Box::default()),
            emit: |_| {},
        };
        let remote = "192.168.1.50:50000".parse().unwrap();
        assert!(lifecycle.on_accept(remote));
        lifecycle.loopback_only = true;
        assert!(!lifecycle.on_accept(remote));
        assert!(lifecycle.on_accept("127.0.0.1:50000".parse().unwrap()));
        assert!(lifecycle.on_accept("[::1]:50000".parse().unwrap()));
    }

    #[test]
    fn lifecycle_hands_the_served_peer_to_the_broker_and_clears_it() {
        let broker = Arc::new(Broker::new(
            PathBuf::from("/nonexistent-niri-rdp-test.sock"),
            "HEADLESS-1".into(),
            None,
        ));
        let mut lifecycle = Lifecycle {
            active: None,
            broker: Some(broker.clone()),
            loopback_only: false,
            clipboard: Default::default(),
            video: None,
            stats: Box::leak(Box::default()),
            emit: |_| {},
        };
        let peer: SocketAddr = "192.168.1.50:50000".parse().unwrap();
        assert!(lifecycle.on_accept(peer));
        assert_eq!(broker.current_peer(), Some(peer.ip()));
        lifecycle.on_disconnected(peer, Duration::from_secs(1), None);
        assert_eq!(broker.current_peer(), None);
        lifecycle.loopback_only = true;
        assert!(!lifecycle.on_accept(peer));
        assert_eq!(broker.current_peer(), None);
    }

    thread_local! {
        static SUMMARIES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    fn record_summary(line: &str) {
        SUMMARIES.with(|lines| lines.borrow_mut().push(line.to_owned()));
    }
    fn summaries() -> Vec<String> {
        SUMMARIES.with(|lines| lines.borrow().clone())
    }
    fn recording_lifecycle(stats: &'static ConnectionStats) -> Lifecycle {
        Lifecycle {
            active: None,
            broker: None,
            loopback_only: true,
            clipboard: Default::default(),
            video: None,
            stats,
            emit: record_summary,
        }
    }

    #[test]
    fn every_disconnect_emits_one_summary_and_resets_its_counters() {
        use niri_rdp_bridge::performance::Counter;
        let stats: &'static ConnectionStats = Box::leak(Box::default());
        let mut lifecycle = recording_lifecycle(stats);
        let peer: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        stats.add(Counter::VideoFrames, 3);
        stats.add(Counter::ClipboardToClient, 2);
        lifecycle.on_disconnected(peer, Duration::from_secs(2), None);
        let lines = summaries();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with(
            "RDP connection summary peer=127.0.0.1:50000 duration_s=2.0 result=ok video_codec=h264"
        ));
        assert!(lines[0].contains(" frames=3 ") && lines[0].contains(" clipboard_to_client=2 "));
        // An abrupt or failed connection still gets exactly one line, and the
        // previous connection's counters do not leak into it.
        let error = server_error("connection reset");
        lifecycle.on_disconnected(peer, Duration::from_millis(300), Some(&error));
        let lines = summaries();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains(" result=error video_codec=bitmap "));
        assert!(lines[1].contains(" frames=0 ") && lines[1].contains(" clipboard_to_client=0 "));
        assert!(
            !lines[1].contains("connection reset"),
            "the summary carries no error text"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abruptly_dropped_tcp_connections_each_emit_one_summary() {
        use std::io::Write;
        let stats: &'static ConnectionStats = Box::leak(Box::default());
        tokio::task::LocalSet::new()
            .run_until(async move {
                let mut server = RdpServer::builder()
                    .with_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                    .with_no_security()
                    .with_no_input()
                    .with_no_display()
                    .with_connection_handler(Some(Box::new(recording_lifecycle(stats))))
                    .build();
                let events = server.event_sender().clone();
                let run = tokio::task::spawn_local(async move {
                    let _ = Box::pin(server.run()).await;
                });
                let addr = loop {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = events.send(ironrdp_server::ServerEvent::GetLocalAddr(tx));
                    if let Ok(Some(addr)) = rx.await {
                        break addr;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                for expected in 1..=2 {
                    let mut client = std::net::TcpStream::connect(addr).unwrap();
                    // A truncated X.224 header, then an abrupt close.
                    client.write_all(&[0x03, 0x00]).unwrap();
                    drop(client);
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while summaries().len() < expected && std::time::Instant::now() < deadline {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    assert_eq!(summaries().len(), expected);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                let lines = summaries();
                assert_eq!(lines.len(), 2, "no duplicate summary lines");
                assert!(lines.iter().all(|line| line.contains(" result=error ")));
                run.abort();
            })
            .await;
    }
}
