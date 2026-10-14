//! Disposable transport fixture. Run only inside a private network namespace.
use std::{io::Write, time::Instant};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpSocket,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket = TcpSocket::new_v4()?;
    niri_rdp_bridge::network::configure_tcp_liveness(&socket)?;
    socket.bind("127.0.0.1:0".parse()?)?;
    let listener = socket.listen(1)?;
    println!("ADDRESS {}", listener.local_addr()?);
    std::io::stdout().flush()?;
    let (mut connection, _) = listener.accept().await?;
    println!("ACCEPTED");
    std::io::stdout().flush()?;
    let started = Instant::now();
    let mut bytes = [0; 1];
    let busy = std::env::var_os("NIRI_RDP_NETWORK_PROBE_BUSY").is_some();
    let mut cadence = tokio::time::interval(std::time::Duration::from_millis(20));
    loop {
        let result = tokio::select! {
            result = connection.read(&mut bytes) => {
                match result {
                    Ok(0) => anyhow::bail!("fixture peer closed; a blackhole was required"),
                    Ok(_) => connection.write_all(&bytes).await,
                    Err(error) => Err(error),
                }
            },
            _ = cadence.tick(), if busy => connection.write_all(&[0x55; 256]).await,
        };
        if let Err(error) = result {
            println!(
                "DEAD {} {}",
                started.elapsed().as_millis(),
                error.raw_os_error().unwrap_or(0)
            );
            std::io::stdout().flush()?;
            break;
        }
    }
    drop(connection);
    let (mut resumed, _) = listener.accept().await?;
    resumed.read_exact(&mut bytes).await?;
    resumed.write_all(&bytes).await?;
    println!("RECONNECTED");
    Ok(())
}
