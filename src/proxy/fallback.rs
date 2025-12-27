use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::{error, info, trace, warn};
use std::net::{SocketAddr, ToSocketAddrs};
use std::rc::Rc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::task::spawn_local;

use crate::config::{Args, MAX_MTU};
use crate::proto;
use crate::proxy::clientmap::ClientMap;

pub async fn start(args: Args) -> anyhow::Result<()> {
    info!(
        "Using hashmap-based UDP proxy with remote server IP: {}",
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
        let socket = UdpSocket::bind(addr).await?;
        info!("Listening on {}", socket.local_addr()?);
        sockets.push(Rc::new(socket));
    }

    let client_map = ClientMap::new(Duration::from_secs(args.timeout));

    // Unique server id
    let server_id: i64 = rand::random::<i64>();

    let read_loops = sockets.into_iter().map(|socket| {
        let remote_addr = remote_addr.clone();
        let client_map = &client_map;
        let server_id = server_id;
        let remove_ports = args.remove_ports;
        read_loop(socket, client_map, remote_addr, server_id, remove_ports)
    });

    // Removes unused connections periodically
    let mut cleanup_timer = tokio::time::interval(Duration::from_secs(args.timeout));

    let mut read_loops = FuturesUnordered::from_iter(read_loops);

    loop {
        select! {
            _ = cleanup_timer.tick() => {
                trace!("Running client map cleanup");
                client_map.cleanup();
            },
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
                    "Received {} bytes from client {} through {}, sending to {}",
                    len,
                    addr,
                    listener.local_addr().unwrap(),
                    remote
                );

                let data = buf[..len].to_vec();

                match client_map.get(addr, remote).await {
                    Ok((server_socket, created)) => {
                        if created {
                            // spawn reader for server->client
                            let server_socket = server_socket.clone();
                            let listener = listener.clone();
                            spawn_local(async move {
                                proxy_server_reader(server_socket, addr, listener).await;
                            });
                        }

                        if let Some(packet_id) = buf.get(0) {
                            if *packet_id == proto::UNCONNECTED_PING_ID {
                                info!("Received LAN ping from client {}", addr);
                            }
                        }

                        // TODO: logic to figure out if server is offline and respond with empty pong if necessary

                        let _ = server_socket.send(&data).await;
                    }
                    Err(e) => {
                        // Mighty happen if we exhaust the server resources (e.g. ports)
                        warn!("Failed to get/create client mapping: {}", e);
                        if let Some(packet_id) = buf.get(0) {
                            if *packet_id == proto::UNCONNECTED_PING_ID {
                                info!("Received LAN ping from client {} but server cannot be reached, rewritting pong", addr);
                                match proto::rewrite_unconnected_pong(
                                    &listener,
                                    server_id,
                                    remove_ports,
                                    &proto::OFFLINE_PONG,
                                ) {
                                    Ok(pong) => {
                                        let _ = listener.send_to(&pong, addr).await;
                                    }
                                    Err(e) => {
                                        warn!("Failed to build pong response: {}", e);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                error!("Unable to receive data from listener: {}", e);
                break;
            }
        }
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
