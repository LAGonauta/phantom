use clap::Parser;
use log::{debug, error, info, trace, warn};
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::task::spawn_local;
use tokio::time::sleep;

mod clientmap;
mod proto;

use clientmap::ClientMap;

const MAX_MTU: usize = 1472;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 's', long = "server")]
    server: String,

    #[arg(long = "bind", default_value = "0.0.0.0")]
    bind: String,

    #[arg(long = "bind_port", default_value_t = 0)]
    bind_port: u16,

    #[arg(long = "timeout", default_value_t = 60)]
    timeout: u64,

    #[arg(long = "remove_ports")]
    remove_ports: bool,
}

#[tokio::main(flavor = "local")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    pretty_env_logger::init_timed();

    info!("Starting up with remote server IP: {}", args.server);

    // Resolve remote server
    let remote_addr: SocketAddr = args
        .server
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve server address"))?;

    // Determine bind port (randomize if 0 similar to Go)
    let mut bind_port = args.bind_port;
    if bind_port == 0 {
        bind_port = (rand::random::<u16>() % 14000) + 50000;
    }

    let bind_addr = format!("{}:{}", args.bind, bind_port);
    let bind_socket_addr: SocketAddr = bind_addr.to_socket_addrs()?.next().unwrap();

    // Create proxy socket (UDP) with reuse options
    let proxy_socket = Rc::new(UdpSocket::bind(bind_socket_addr).await?);

    // Ping proxy socket on port 19132 (IPv4)
    let ping_socket = UdpSocket::bind("0.0.0.0:19132").await?;

    let client_map = ClientMap::new(Duration::from_secs(args.timeout), Duration::from_secs(5));

    // Unique server id
    let server_id: i64 = rand::random::<i64>();

    // Spawn ping read loops task
    let ping_task = {
        let remote_addr = remote_addr.clone();
        let remove_ports = args.remove_ports;
        let server_id = server_id;
        read_loop(
            &ping_socket,
            proxy_socket.clone(),
            &client_map,
            remote_addr,
            server_id,
            remove_ports,
        )
    };

    let proxy_task = {
        let remote_addr = remote_addr.clone();
        let remove_ports = args.remove_ports;
        let server_id = server_id;
        read_loop(
            &proxy_socket,
            proxy_socket.clone(),
            &client_map,
            remote_addr,
            server_id,
            remove_ports,
        )
    };

    select! {
        _ = ping_task => {
            error!("Ping task exited unexpectedly");
        },
        _ = proxy_task => {
            error!("Proxy task exited unexpectedly");
        },
        _ = tokio::signal::ctrl_c() => {
            info!("Shutdown requested, exiting");
        },
    }

    Ok(())
}

async fn read_loop(
    listener: &UdpSocket,
    proxy_socket: Rc<UdpSocket>,
    client_map: &ClientMap,
    remote: SocketAddr,
    server_id: i64,
    remove_ports: bool,
) {
    let mut buf = vec![0u8; MAX_MTU];

    loop {
        match listener.recv_from(&mut buf).await {
            Ok((len, addr)) => {
                if len == 0 {
                    continue;
                }

                trace!(
                    "Received {} bytes from client {}, sending to {}",
                    len,
                    addr,
                    remote
                );

                let data = buf[..len].to_vec();

                // Use same proxy logic as main listener: forward to server via per-client socket
                let (server_socket, created) = client_map.get(addr, remote).await.unwrap();

                if created {
                    // spawn reader for server->client
                    let server_socket = server_socket.clone();
                    let proxy_socket = proxy_socket.clone();
                    spawn_local(async move {
                        proxy_server_reader(server_socket, addr, proxy_socket).await;
                    });
                }
                if let Some(packet_id) = buf.get(0) {
                    let server_offline = false; // TODO
                                                // If server is offline, respond with empty pong
                    if *packet_id == proto::UNCONNECTED_PING_ID {
                        info!("Received LAN ping from client: {}", addr);
                        if server_offline {
                            let pong = rewrite_unconnected_pong(
                                &proxy_socket,
                                server_id,
                                remove_ports,
                                &data,
                            );
                            let _ = proxy_socket.send(&pong.unwrap()).await;
                        }
                    }
                }

                let _ = server_socket.send(&data).await;
            }
            Err(e) => {
                warn!("ping listener error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

fn rewrite_unconnected_pong(
    proxy_socket: &Rc<UdpSocket>,
    server_id: i64,
    remove_ports: bool,
    data: &Vec<u8>,
) -> Option<Vec<u8>> {
    if let Ok(Some(mut ping)) = proto::read_unconnected_ping(data) {
        // Modify server ID
        ping.pong.server_id = server_id.to_string();

        if ping.pong.port4 != "" && !remove_ports {
            ping.pong.port4 = proxy_socket.local_addr().unwrap().port().to_string();
            ping.pong.port6 = ping.pong.port4.clone();
        } else if remove_ports {
            ping.pong.port4 = "".to_string();
            ping.pong.port6 = "".to_string();
        }

        Some(proto::build_unconnected_pong(&ping))
    } else {
        None
    }
}

async fn proxy_server_reader(
    server_socket: Rc<UdpSocket>,
    client: SocketAddr,
    proxy_socket: Rc<UdpSocket>,
) {
    let mut buf = vec![0u8; MAX_MTU];

    loop {
        match server_socket.recv(&mut buf).await {
            Ok(len) => {
                if len == 0 {
                    continue;
                }
                let data = buf[..len].to_vec();
                debug!(
                    "Received {} bytes from server {}, sending to {}",
                    len,
                    server_socket.peer_addr().unwrap(),
                    client
                );

                // send back to client using the main server listener socket
                let _ = proxy_socket.send_to(&data, client).await;
            }
            Err(e) => {
                warn!("server read error: {}", e);
                break;
            }
        }
    }

    // When server read loop exits, client mapping should be removed by cleanup loop
}
