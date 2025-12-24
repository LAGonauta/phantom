use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::Mutex as TokioMutex;

pub struct ClientMap {
    pub idle_timeout: Duration,
    pub idle_check_interval: Duration,
    inner: Arc<TokioMutex<HashMap<String, ClientEntry>>>,
}

struct ClientEntry {
    socket: Arc<UdpSocket>,
    last_active: Instant,
}

impl ClientMap {
    pub fn new(idle_timeout: Duration, idle_check_interval: Duration) -> Self {
        let map = ClientMap {
            idle_timeout,
            idle_check_interval,
            inner: Arc::new(TokioMutex::new(HashMap::new())),
        };

        // Spawn cleanup task
        let inner_clone = map.inner.clone();
        let idle = map.idle_timeout;
        let interval = map.idle_check_interval;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let mut guard = inner_clone.lock().await;
                let now = Instant::now();
                let mut to_remove = Vec::new();
                for (k, v) in guard.iter() {
                    if v.last_active + idle < now {
                        to_remove.push(k.clone());
                    }
                }
                for k in to_remove {
                    guard.remove(&k);
                }
            }
        });

        map
    }

    // Get or create a connection to remote for a client address.
    // Returns (socket, created)
    pub async fn get(&self, client: SocketAddr, remote: SocketAddr) -> anyhow::Result<(Arc<UdpSocket>, bool)> {
        let key = client.to_string();

        let mut guard = self.inner.lock().await;

        if let Some(entry) = guard.get_mut(&key) {
            entry.last_active = Instant::now();
            return Ok((entry.socket.clone(), false));
        }

        // Create new socket bound to ephemeral port
        let local = match client.ip() {
            std::net::IpAddr::V4(_) => "0.0.0.0:0",
            std::net::IpAddr::V6(_) => "[::]:0",
        };

        let sock = UdpSocket::bind(local).await?;
        sock.connect(remote).await?;
        let arc = Arc::new(sock);

        guard.insert(
            key,
            ClientEntry {
                socket: arc.clone(),
                last_active: Instant::now(),
            },
        );

        Ok((arc, true))
    }
}
