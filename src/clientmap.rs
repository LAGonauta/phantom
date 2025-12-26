use log::trace;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::task::spawn_local;

#[derive(Clone)]
pub struct ClientMap {
    idle_timeout: Duration,
    inner: Rc<RefCell<HashMap<String, ClientEntry>>>,
}

struct ClientEntry {
    socket: Rc<UdpSocket>,
    last_active: Instant,
}

impl ClientMap {
    pub fn new(idle_timeout: Duration) -> Self {
        ClientMap {
            idle_timeout,
            inner: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn cleanup(&self) {
        let now = Instant::now();
        self.inner.borrow_mut().retain(|k, val| {
            if val.last_active + self.idle_timeout < now {
                trace!("Removing idle client connection: {}", k);
                false
            } else {
                true
            }
        });
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

        // Release borrow before await
        drop(inner);

        let remote_sock = UdpSocket::bind(local).await?;
        remote_sock.connect(remote).await?;
        let remote_sock = Rc::new(remote_sock);

        self.inner.borrow_mut().insert(
            key,
            ClientEntry {
                socket: remote_sock.clone(),
                last_active: Instant::now(),
            },
        );

        Ok((remote_sock, true))
    }
}
