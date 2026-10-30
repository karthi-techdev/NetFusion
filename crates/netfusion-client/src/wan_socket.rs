use log::info;
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::windows::io::{AsRawSocket, FromRawSocket};
use std::sync::Arc;
use tokio::net::UdpSocket;

/// Represents an individual bonded WAN uplink (e.g. Airtel, Jio) bound strictly
/// to its respective local IP interface address.
#[allow(dead_code)]
pub struct WanPathSocket {
    pub path_id: u8,
    pub name: String,
    pub local_ip: Ipv4Addr,
    pub server_endpoint: SocketAddrV4,
    pub socket: Arc<UdpSocket>,
}

#[allow(dead_code)]
impl WanPathSocket {
    /// Bind a UDP socket strictly to a specific local IPv4 network interface
    pub fn bind(
        path_id: u8,
        name: &str,
        local_ip: Ipv4Addr,
        server_endpoint: SocketAddrV4,
    ) -> Result<Self, String> {
        info!(
            "Binding WAN Path {} ({}) to source IP: {}",
            path_id, name, local_ip
        );

        let domain = Domain::IPV4;
        let socket_type = Type::DGRAM;
        let protocol = Some(Protocol::UDP);

        let socket2 = Socket::new(domain, socket_type, protocol)
            .map_err(|e| format!("Failed to create socket2 UDP: {}", e))?;

        socket2
            .set_nonblocking(true)
            .map_err(|e| format!("Failed to set nonblocking: {}", e))?;

        // Reusable address and port
        let _ = socket2.set_reuse_address(true);

        // Explicitly bind to local interface IPv4 and ephemeral port 0
        let bind_addr = SocketAddrV4::new(local_ip, 0);
        socket2
            .bind(&bind_addr.into())
            .map_err(|e| format!("Failed to bind WAN path to {}: {}", local_ip, e))?;

        // Convert into tokio::net::UdpSocket
        let std_socket: std::net::UdpSocket = unsafe {
            std::net::UdpSocket::from_raw_socket(socket2.as_raw_socket() as _)
        };
        // std_socket took ownership of raw handle, so forget socket2 to avoid double close
        std::mem::forget(socket2);

        let tokio_socket = UdpSocket::from_std(std_socket)
            .map_err(|e| format!("Failed to convert to tokio UDP socket: {}", e))?;

        Ok(Self {
            path_id,
            name: name.to_string(),
            local_ip,
            server_endpoint,
            socket: Arc::new(tokio_socket),
        })
    }

    /// Send raw datagram to the bonding server endpoint
    pub async fn send_to_server(&self, buf: &[u8]) -> Result<usize, std::io::Error> {
        self.socket
            .send_to(buf, SocketAddr::V4(self.server_endpoint))
            .await
    }
}
