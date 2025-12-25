use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::spawn_local;

#[derive(Clone)]
pub struct ClientMap {
    pub idle_timeout: Duration,
    pub idle_check_interval: Duration,
    inner: Rc<RefCell<HashMap<String, ClientEntry>>>,
}

struct ClientEntry {
    socket: Rc<UdpSocket>,
    last_active: Instant,
}

impl ClientMap {
    pub fn new(idle_timeout: Duration, idle_check_interval: Duration) -> Self {
        let map = ClientMap {
            idle_timeout,
            idle_check_interval,
            inner: Rc::new(RefCell::new(HashMap::new())),
        };

        // Spawn cleanup task
        spawn_local({
            let map = map.clone();
            async move {
                loop {
                    tokio::time::sleep(map.idle_check_interval).await;
                    let now = Instant::now();
                    let mut to_remove = Vec::new();
                    for (k, v) in map.inner.borrow().iter() {
                        if v.last_active + map.idle_timeout < now {
                            to_remove.push(k.clone());
                        }
                    }
                    let mut inner = map.inner.borrow_mut();
                    for k in to_remove {
                        inner.remove(&k);
                    }
                }
            }
        });

        map
    }

    // Get or create a connection to remote for a client address.
    // Returns (socket, created)
    pub async fn get(
        &self,
        client: SocketAddr,
        remote: SocketAddr,
    ) -> anyhow::Result<(Rc<UdpSocket>, bool)> {
        let key = client.to_string();

        let mut inner = self.inner.borrow_mut();
        if let Some(entry) = inner.get_mut(&key) {
            entry.last_active = Instant::now();
            return Ok((entry.socket.clone(), false));
        }

        // Create new socket bound to ephemeral port
        let local = match client.ip() {
            std::net::IpAddr::V4(_) => "0.0.0.0:0",
            std::net::IpAddr::V6(_) => "[::]:0",
        };

        drop(inner); // Release borrow before await

        let sock = UdpSocket::bind(local).await?;
        sock.connect(remote).await?;
        let arc = Rc::new(sock);

        self.inner.borrow_mut().insert(
            key,
            ClientEntry {
                socket: arc.clone(),
                last_active: Instant::now(),
            },
        );

        Ok((arc, true))
    }
}
