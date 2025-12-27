use anyhow::Result;
use clap::Parser;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::{error, info, trace, warn};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::task::{spawn_local, LocalSet};
use tokio::time::Instant;

mod proto;

const MAX_MTU: usize = 1472;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short = 's', long = "server")]
    server: String,

    #[arg(long = "timeout", default_value_t = 60)]
    timeout: u64,

    #[arg(long = "remove_ports")]
    remove_ports: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    pretty_env_logger::init_timed();

    LocalSet::new().run_until(start(args)).await
}

async fn start(args: Args) -> anyhow::Result<()> {
    info!("Starting up with remote server IP: {}", args.server);

    // Resolve remote server
    let remote_addr: SocketAddr = args
        .server
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve server address"))?;

    let local_addrs = vec!["[::]:19132".to_string(), "[::]:19133".to_string()]
        .into_iter()
        .map(|addr| addr.to_socket_addrs())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    let mut sockets = Vec::new();
    for addr in local_addrs {
        info!("Trying to listen on {}", addr);

        let s2 = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        )?;
        s2.set_reuse_address(true)?;
        s2.set_reuse_port(true)?;
        s2.set_nonblocking(true)?;
        s2.bind(&addr.into())?;

        let s = UdpSocket::from_std(s2.into())?;
        info!("Listening on {}", s.local_addr()?);
        sockets.push(Rc::new(s));
    }

    // Unique server id
    let server_id: i64 = rand::random::<i64>();

    let read_loops = sockets.into_iter().map(|socket| {
        let remote_addr = remote_addr.clone();
        let server_id = server_id;
        let remove_ports = args.remove_ports;
        read_loop(socket, remote_addr, server_id, remove_ports)
    });

    let mut read_loops = FuturesUnordered::from_iter(read_loops);

    loop {
        select! {
            _ = read_loops.next() => {
                error!("A read loop exited unexpectedly");
                break;
            },
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown requested, exiting");
                break;
            },
        }
    }

    Ok(())
}

async fn read_loop(
    listener: Rc<UdpSocket>,
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
                    "Received {} bytes from client {} through {}, sending to {}",
                    len,
                    addr,
                    listener.local_addr().unwrap(),
                    remote
                );

                let client_socket =
                    try_create_connected_socket(listener.local_addr().unwrap(), addr).unwrap();
                spawn_local(proxy_loop(client_socket, remote));

                //let data = buf[..len].to_vec();

                // match client_map.get(addr, remote).await {
                //     Ok((server_socket, created)) => {
                //         if created {
                //             // spawn reader for server->client
                //             let server_socket = server_socket.clone();
                //             let listener = listener.clone();
                //             spawn_local(async move {
                //                 proxy_server_reader(server_socket, addr, listener).await;
                //             });
                //         }

                //         if let Some(packet_id) = buf.get(0) {
                //             if *packet_id == proto::UNCONNECTED_PING_ID {
                //                 info!("Received LAN ping from client {}", addr);
                //             }
                //         }

                //         // TODO: logic to figure out if server is offline and respond with empty pong if necessary

                //         let _ = server_socket.send(&data).await;
                //     }
                //     Err(e) => {
                //         // Mighty happen if we exhaust the server resources (e.g. ports)
                //         warn!("Failed to get/create client mapping: {}", e);
                //         if let Some(packet_id) = buf.get(0) {
                //             if *packet_id == proto::UNCONNECTED_PING_ID {
                //                 info!("Received LAN ping from client {} but server cannot be reached, rewritting pong", addr);
                //                 match rewrite_unconnected_pong(
                //                     &listener,
                //                     server_id,
                //                     remove_ports,
                //                     &proto::OFFLINE_PONG,
                //                 ) {
                //                     Ok(pong) => {
                //                         let _ = listener.send_to(&pong, addr).await;
                //                     }
                //                     Err(e) => {
                //                         warn!("Failed to build pong response: {}", e);
                //                     }
                //                 }
                //             }
                //         }
                //     }
                // }
            }
            Err(e) => {
                error!("Unable to receive data from listener: {}", e);
                break;
            }
        }
    }
}

fn rewrite_unconnected_pong(
    proxy_socket: &Rc<UdpSocket>,
    server_id: i64,
    remove_ports: bool,
    data: &Vec<u8>,
) -> Result<Vec<u8>> {
    proto::read_unconnected_ping(data).map(|mut ping| {
        // Overwrite the server ID with one unique to this phantom instance.
        // If we don't do this, the client will get confused if you restart phantom.
        ping.pong.server_id = server_id.to_string();

        if ping.pong.port4 != "" && !remove_ports {
            ping.pong.port4 = proxy_socket.local_addr().unwrap().port().to_string();
            ping.pong.port6 = ping.pong.port4.clone();
        } else if remove_ports {
            ping.pong.port4 = "".to_string();
            ping.pong.port6 = "".to_string();
        }

        proto::build_unconnected_pong(&ping)
    })
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
                trace!(
                    "Received {} bytes from server {}, sending to {}",
                    len,
                    server_socket.peer_addr().unwrap(),
                    client
                );

                // send back to client using the main server listener socket
                let _ = proxy_socket.send_to(&data, client).await;
            }
            Err(e) => {
                // TODO
                // If server is offline, respond with empty pong
                warn!("server read error: {}", e);
                break;
            }
        }
    }

    // When server read loop exits, client mapping should be removed by cleanup loop
}

/// Tries to create a specialized "Connected" socket.
/// If the OS doesn't support the required flags, it returns None.
fn try_create_connected_socket(
    listener_addr: SocketAddr,
    peer_addr: SocketAddr,
) -> Result<UdpSocket> {
    trace!("Creating connected socket for peer {} while listening to {}", peer_addr, listener_addr);
    let socket = Socket::new(
        Domain::for_address(listener_addr),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;

    socket.set_nonblocking(true)?;

    // 1. Set the Platform-Specific Flags
    // On Unix/Linux, we need SO_REUSEPORT to share the port with the main listener.
    #[cfg(unix)]
    {
        socket.set_reuse_port(true)?;
        socket.set_reuse_address(true)?;
    }

    // On Windows, SO_REUSEADDR acts like SO_REUSEPORT
    #[cfg(windows)]
    socket.set_reuse_address(true)?;

    // 2. Bind to the same listener
    socket.bind(&listener_addr.into())?;

    // 3. Connect to the specific client
    // This effectively "filters" this socket to only receive packets from this peer
    socket.connect(&peer_addr.into())?;

    // Convert back to std::net::UdpSocket (which can be turned into Tokio/Async socket)
    Ok(UdpSocket::from_std(socket.into())?)
}

async fn proxy_loop(client_socket: UdpSocket, remote_addr: SocketAddr) -> anyhow::Result<()> {
    let local = match client_socket.local_addr().unwrap().ip() {
        std::net::IpAddr::V4(_) => "0.0.0.0:0",
        std::net::IpAddr::V6(_) => "[::]:0",
    };

    // TODO: error when cannot bind
    let remote_sock = UdpSocket::bind(local).await?;
    remote_sock.connect(remote_addr).await?;
    let remote_sock = Rc::new(remote_sock);

    let mut client_buf = vec![0u8; MAX_MTU];
    let mut server_buf = vec![0u8; MAX_MTU];

    // TODO: exit on timeout, use args.timeout
    let timeout = Duration::from_secs(60);
    let mut cleanup_timer = tokio::time::interval(timeout);

    let mut last_client_message = Instant::now();
    let mut last_server_message = Instant::now();
    loop {
        select! {
                now = cleanup_timer.tick() => {
                    if now.duration_since(last_client_message) > timeout
                        && now.duration_since(last_server_message) > timeout
                    {
                        // TODO: send unconnected pong when the server is offline, but break if the client is gone
                        trace!("No activity for {:#?}, closing read loop for client {}", timeout, client_socket.peer_addr().unwrap());
                        break;
                    }

                },
                client_result = client_socket.recv(&mut client_buf) => {
                    last_client_message = Instant::now();
                    match client_result {
                        Ok(client_len) => {
                            if client_len == 0 {
                                continue;
                            }
                            let data = client_buf[..client_len].to_vec();
                            trace!(
                                "Received {} bytes from client {}, sending to {}",
                                client_len,
                                client_socket.peer_addr().unwrap(),
                                remote_addr
                            );
                            let _ = remote_sock.send_to(&data, remote_addr).await;
                        }
                        Err(e) => {
                            warn!("client read error: {}", e);
                            break;
                        }
                    }
                },
                remote_result = remote_sock.recv(&mut server_buf) => {
                    last_server_message = Instant::now();
                    match remote_result {
                        Ok(server_len) => {
                            if server_len == 0 {
                                continue;
                            }
                            let data = server_buf[..server_len].to_vec();
                            trace!(
                                "Received {} bytes from server {}, sending to {}",
                                server_len,
                                remote_addr,
                                client_socket.peer_addr().unwrap(),
                            );
                            let _ = client_socket.send(&data).await;
                        }
                        Err(e) => {
                            warn!("server read error: {}", e);
                            break;
                        }
                    }
            }
        }
    }
    Ok(())
}
