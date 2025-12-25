use clap::Parser;
use log::{debug, error, info, warn};
use rand::Rng;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::Mutex;
use tokio::task::spawn_local;
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
    let server_socket = Rc::new(UdpSocket::bind(bind_socket_addr).await?);

    // Ping server socket on port 19132 (IPv4)
    let ping_socket = UdpSocket::bind("0.0.0.0:19132").await?;

    // Optional IPv6 ping
    let ping_socket_v6 = if args.ipv6 {
        match UdpSocket::bind("[::]:19133").await {
            Ok(s) => Some(s),
            Err(e) => {
                warn!("Failed to bind IPv6 ping listener: {}", e);
                None
            }
        }
    } else {
        None
    };

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
            server_socket.clone(),
            &client_map,
            remote_addr,
            server_id,
            remove_ports,
        )
    };

    // if let Some(pv6) = &ping_socket_v6 {
    //     let server_socket = server_socket.clone();
    //     let remote_addr = remote_addr.clone();
    //     let remove_ports = args.remove_ports;
    //     let server_id = server_id;
    //     let client_map = &client_map;
    //     spawn_local(async move {
    //         read_loop(
    //             pv6,
    //             server_socket,
    //             client_map,
    //             remote_addr,
    //             server_id,
    //             remove_ports,
    //         )
    //         .await;
    //     });
    // }

    let proxy_task = {
        let remote_addr = remote_addr.clone();
        let remove_ports = args.remove_ports;
        let server_id = server_id;
        proxy_listen_loop(
            server_socket,
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
    server_socket: Rc<UdpSocket>,
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

                let data = buf[..len].to_vec();
                debug!("ping recv {} from {}", len, addr);

                // Handle unconnected ping/pong based on proto
                if let Ok(Some(ping)) = proto::read_unconnected_ping(&data) {
                    // If needed, we could respond directly; for simplicity pass through to proxy flow
                    debug!("Parsed unconnected ping from {}", addr);
                }

                // Use same proxy logic as main listener: forward to server via per-client socket
                let (sock, created) = client_map.get(addr, remote).await.unwrap();

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
    listener: Rc<UdpSocket>,
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

                let data = buf[..len].to_vec();
                debug!("client recv: {} from {}", len, addr);

                let (sock, created) = client_map.get(addr, remote).await.unwrap();

                if created {
                    let sock_clone = sock.clone();
                    let server_listener = listener.clone();
                    spawn_local(async move {
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

async fn proxy_server_reader(
    server_conn: Rc<UdpSocket>,
    client: SocketAddr,
    server_listener: Rc<UdpSocket>,
) {
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
