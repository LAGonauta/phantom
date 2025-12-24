use clap::Parser;
use log::{debug, info, warn};
use rand::Rng;
use tokio::task::spawn_local;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;
use tokio::time::sleep;

mod clientmap;
mod proto;
mod util;

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

    #[arg(long = "6")]
    ipv6: bool,

    #[arg(long = "remove_ports")]
    remove_ports: bool,

    #[arg(long = "workers", default_value_t = 1)]
    workers: u32,
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

    // Create server socket (UDP) with reuse options
    let server_socket = UdpSocket::bind(bind_socket_addr).await?;
    let server_socket = Arc::new(server_socket);

    // Ping server socket on port 19132 (IPv4)
    let ping_socket = UdpSocket::bind("0.0.0.0:19132").await?;
    let ping_socket = Arc::new(ping_socket);

    // Optional IPv6 ping
    let ping_socket_v6 = if args.ipv6 {
        match UdpSocket::bind("[::]:19133").await {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                warn!("Failed to bind IPv6 ping listener: {}", e);
                None
            }
        }
    } else {
        None
    };

    let client_map = Arc::new(Mutex::new(ClientMap::new(Duration::from_secs(args.timeout), Duration::from_secs(5))));

    // Unique server id
    let server_id: i64 = rand::random::<i64>();

    // Spawn ping read loops
    {
        let server_socket = server_socket.clone();
        let client_map = client_map.clone();
        let remote_addr = remote_addr.clone();
        let remove_ports = args.remove_ports;
        let server_id = server_id;
        spawn_local(async move {
            read_loop(ping_socket.clone(), server_socket, client_map, remote_addr, server_id, remove_ports).await;
        });
    }

    if let Some(pv6) = ping_socket_v6 {
        let server_socket = server_socket.clone();
        let client_map = client_map.clone();
        let remote_addr = remote_addr.clone();
        let remove_ports = args.remove_ports;
        let server_id = server_id;
        spawn_local(async move {
            read_loop(pv6, server_socket, client_map, remote_addr, server_id, remove_ports).await;
        });
    }

    // Start workers for main listener
    for i in 0..args.workers {
        let server_socket = server_socket.clone();
        let client_map = client_map.clone();
        let remote_addr = remote_addr.clone();
        let server_id = server_id;
        let remove_ports = args.remove_ports;

        if i < args.workers - 1 {
            spawn_local(async move {
                proxy_listen_loop(server_socket, client_map, remote_addr, server_id, remove_ports).await;
            });
        } else {
            proxy_listen_loop(server_socket, client_map, remote_addr, server_id, remove_ports).await;
        }
    }

    // Keep the program alive until CTRL+C
    tokio::signal::ctrl_c().await?;
    info!("Shutdown requested, exiting");

    Ok(())
}

async fn read_loop(
    listener: Arc<UdpSocket>,
    server_socket: Arc<UdpSocket>,
    client_map: Arc<Mutex<ClientMap>>,
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

                let data = buf[..len].to_vec();
                debug!("ping recv {} from {}", len, addr);

                // Handle unconnected ping/pong based on proto
                if let Ok(Some(ping)) = proto::read_unconnected_ping(&data) {
                    // If needed, we could respond directly; for simplicity pass through to proxy flow
                    debug!("Parsed unconnected ping from {}", addr);
                }

                // Use same proxy logic as main listener: forward to server via per-client socket
                let cm = client_map.lock().await;
                let (sock, created) = cm.get(addr, remote).await.unwrap();
                drop(cm);

                if created {
                    // spawn reader for server->client
                    let sock_clone = sock.clone();
                    let server_socket = server_socket.clone();
                    spawn_local(async move {
                        proxy_server_reader(sock_clone, addr, server_socket).await;
                    });
                }

                let _ = sock.send(&data).await;
            }
            Err(e) => {
                warn!("ping listener error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn proxy_listen_loop(
    listener: Arc<UdpSocket>,
    client_map: Arc<Mutex<ClientMap>>,
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

                let data = buf[..len].to_vec();
                debug!("client recv: {} from {}", len, addr);

                let cm = client_map.lock().await;
                let (sock, created) = cm.get(addr, remote).await.unwrap();
                drop(cm);

                if created {
                    let sock_clone = sock.clone();
                    let server_listener = listener.clone();
                    tokio::spawn(async move {
                        proxy_server_reader(sock_clone, addr, server_listener).await;
                    });
                }

                // If this is an unconnected ping ID, we may want to handle offline pong rewriting later.

                let _ = sock.send(&data).await;
            }
            Err(e) => {
                warn!("proxy listener error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn proxy_server_reader(server_conn: Arc<UdpSocket>, client: std::net::SocketAddr, server_listener: Arc<UdpSocket>) {
    let mut buf = vec![0u8; MAX_MTU];

    loop {
        match server_conn.recv(&mut buf).await {
            Ok(len) => {
                if len == 0 {
                    continue;
                }
                let data = buf[..len].to_vec();
                debug!("server recv: {}", len);

                // send back to client using the main server listener socket
                let _ = server_listener.send_to(&data, client).await;
            }
            Err(e) => {
                warn!("server read error: {}", e);
                break;
            }
        }
    }

    // When server read loop exits, client mapping should be removed by cleanup loop
}
