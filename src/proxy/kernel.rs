use anyhow::Result;
use flume::Sender;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::{debug, error, info, trace, warn};
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::task::spawn_local;
use tokio::time::Instant;

use crate::config::{Args, MAX_MTU};

pub async fn start(args: Args) -> Result<()> {
    info!(
        "Using kernel-based UDP proxy with remote server IP: {}",
        args.server
    );

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

        let s2 = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
        s2.set_reuse_address(true)?;
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
        read_loop(
            Duration::from_secs(args.timeout),
            socket,
            remote_addr,
            server_id,
            remove_ports,
        )
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

pub async fn has_support() -> bool {
    try_create_connected_socket(
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 0)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 0)),
    )
    .is_ok()
}

async fn read_loop(
    timeout: Duration,
    listener: Rc<UdpSocket>,
    remote: SocketAddr,
    server_id: i64,
    remove_ports: bool,
) {
    let mut buf = vec![0u8; MAX_MTU];

    let mut cleanup_timer = tokio::time::interval(timeout);
    let mut unconnection_map: HashMap<SocketAddr, (Sender<Vec<u8>>, Instant)> = HashMap::new();
    loop {
        select! {
            now = cleanup_timer.tick() => {
                unconnection_map.retain(|addr, (_sender, last_used)| {
                    if now.duration_since(*last_used) > timeout {
                        debug!("Removing inactive unconnected sender loop for client {}", addr);
                        false
                    } else {
                        true
                    }
                });
            },
            result = listener.recv_from(&mut buf) => match result {
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

                    let sender = match unconnection_map.entry(addr) {
                        Entry::Occupied(mut o) => {
                            trace!("Reusing existing proxy loop for client {}", addr);
                            o.get_mut().1 = Instant::now();
                            o.get().0.clone()
                        },
                        Entry::Vacant(v) => {
                            let (s, r) = flume::unbounded();
                            let client_socket =
                                try_create_connected_socket(listener.local_addr().unwrap(), addr)
                                    .unwrap();
                            spawn_local(proxy_loop(
                                timeout,
                                r,
                                client_socket,
                                remote,
                                server_id,
                                remove_ports,
                            ));
                            v.insert((s, Instant::now())).0.clone()
                        }
                    };
                    if let Err(e) = sender.send_async(buf[..len].to_vec()).await {
                        warn!("Failed to send data to proxy loop for {}: {}", addr, e);
                        unconnection_map.remove(&addr);
                    }
                }
                Err(e) => {
                    error!("Unable to receive data from listener: {}", e);
                    break;
                }
            }
        }
    }
}

async fn proxy_loop(
    timeout: Duration,
    unconnected_recv: flume::Receiver<Vec<u8>>,
    client_socket: UdpSocket,
    remote_addr: SocketAddr,
    server_id: i64,
    remove_ports: bool,
) -> anyhow::Result<()> {
    let client_addr = client_socket.peer_addr()?;
    let local = match remote_addr.ip() {
        std::net::IpAddr::V4(_) => "0.0.0.0:0",
        std::net::IpAddr::V6(_) => "[::]:0",
    };

    // TODO: error when cannot bind
    let remote_socket = UdpSocket::bind(local).await?;
    remote_socket.connect(remote_addr).await?;

    let mut client_buf = vec![0u8; MAX_MTU];
    let mut server_buf = vec![0u8; MAX_MTU];

    let mut cleanup_timer = tokio::time::interval(timeout);

    let mut last_client_message = Instant::now();
    let mut last_server_message = Instant::now();

    debug!(
        "Connected client {} to remote server {}",
        client_addr, remote_addr
    );
    loop {
        select! {
            now = cleanup_timer.tick() => {
                if now.duration_since(last_client_message) > timeout
                    && now.duration_since(last_server_message) > timeout
                {
                    // TODO: send unconnected pong when the server is offline, but break if the client is gone
                    debug!("No activity for {:#?}, closing read loop for client {}", timeout, client_addr);
                    break;
                }
            },
            // From the unconnected socket
            client_result = unconnected_recv.recv_async(), if !unconnected_recv.is_disconnected() => {
                match client_result {
                    Ok(data) => {
                        if data.len() == 0 {
                            continue;
                        }
                        trace!(
                            "Received {} bytes from client {} (channel), sending to {}",
                            data.len(),
                            client_addr,
                            remote_addr
                        );
                        let _ = remote_socket.send(&data).await;
                    }
                    Err(e) => {
                        warn!("Got unconnected recv error: {}", e);
                    }
                }
            },
            client_result = client_socket.recv_from(&mut client_buf) => {
                last_client_message = Instant::now();
                match client_result {
                    Ok((client_len, addr)) => {
                        if client_len == 0 {
                            continue;
                        }

                        // As per Cloudflare article, we might get packets from other addresses
                        // due to a possible race condition, so we need to verify the source address
                        // to ensure all is good
                        if addr != client_addr {
                            warn!("Received packet from unexpected address {} (expected {})", addr, client_addr);
                            continue;
                        }
                        let data = client_buf[..client_len].to_vec();
                        trace!(
                            "Received {} bytes from client {}, sending to {}",
                            client_len,
                            addr,
                            remote_addr
                        );
                        // TODO: to not block the loop
                        let _ = remote_socket.send_to(&data, remote_addr).await;
                    }
                    Err(e) => {
                        warn!("client read error: {}", e);
                        break;
                    }
                }
            },
            remote_result = remote_socket.recv(&mut server_buf) => {
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
                            client_addr,
                        );
                        // TODO: to not block the loop
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

// Tries to create a specialized "Connected" socket.
// If the OS doesn't support the required flags, it returns Err.
// https://blog.cloudflare.com/everything-you-ever-wanted-to-know-about-udp-sockets-but-were-afraid-to-ask-part-1/#established-over-unconnected-technique
fn try_create_connected_socket(
    listener_addr: SocketAddr,
    peer_addr: SocketAddr,
) -> Result<UdpSocket> {
    trace!(
        "Creating connected socket for peer {} while listening to {}",
        peer_addr,
        listener_addr
    );
    let socket = Socket::new(
        Domain::for_address(listener_addr),
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;

    socket.set_nonblocking(true)?;
    socket.set_reuse_address(true)?;

    // 2. Bind to the same listener
    socket.bind(&listener_addr.into())?;

    // 3. Connect to the specific client
    // This effectively "filters" this socket to only receive packets from this peer
    socket.connect(&peer_addr.into())?;

    // Convert back to std::net::UdpSocket (which can be turned into Tokio/Async socket)
    Ok(UdpSocket::from_std(socket.into())?)
}
