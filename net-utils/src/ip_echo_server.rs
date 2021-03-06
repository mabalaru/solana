use {
    crate::{ip_echo_server_reply_length, HEADER_LENGTH},
    log::*,
    serde_derive::{Deserialize, Serialize},
    std::{io, net::SocketAddr, time::Duration},
    tokio::{
        net::{TcpListener, TcpStream},
        prelude::*,
        runtime::{self, Runtime},
        time::timeout,
    },
};

pub type IpEchoServer = Runtime;

pub const MAX_PORT_COUNT_PER_MESSAGE: usize = 4;

const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize, Default, Debug)]
pub(crate) struct IpEchoServerMessage {
    tcp_ports: [u16; MAX_PORT_COUNT_PER_MESSAGE], // Fixed size list of ports to avoid vec serde
    udp_ports: [u16; MAX_PORT_COUNT_PER_MESSAGE], // Fixed size list of ports to avoid vec serde
}

impl IpEchoServerMessage {
    pub fn new(tcp_ports: &[u16], udp_ports: &[u16]) -> Self {
        let mut msg = Self::default();
        assert!(tcp_ports.len() <= msg.tcp_ports.len());
        assert!(udp_ports.len() <= msg.udp_ports.len());

        msg.tcp_ports[..tcp_ports.len()].copy_from_slice(tcp_ports);
        msg.udp_ports[..udp_ports.len()].copy_from_slice(udp_ports);
        msg
    }
}

pub(crate) fn ip_echo_server_request_length() -> usize {
    const REQUEST_TERMINUS_LENGTH: usize = 1;
    HEADER_LENGTH
        + bincode::serialized_size(&IpEchoServerMessage::default()).unwrap() as usize
        + REQUEST_TERMINUS_LENGTH
}

async fn process_connection(mut socket: TcpStream, peer_addr: SocketAddr) -> io::Result<()> {
    info!("connection from {:?}", peer_addr);

    let mut data = vec![0u8; ip_echo_server_request_length()];
    let (mut reader, mut writer) = socket.split();

    let _ = timeout(IO_TIMEOUT, reader.read_exact(&mut data)).await??;
    drop(reader);

    let request_header: String = data[0..HEADER_LENGTH].iter().map(|b| *b as char).collect();
    if request_header != "\0\0\0\0" {
        // Explicitly check for HTTP GET/POST requests to more gracefully handle
        // the case where a user accidentally tried to use a gossip entrypoint in
        // place of a JSON RPC URL:
        if request_header == "GET " || request_header == "POST" {
            // Send HTTP error response
            timeout(
                IO_TIMEOUT,
                writer.write_all(b"HTTP/1.1 400 Bad Request\nContent-length: 0\n\n"),
            )
            .await??;
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("Bad request header: {}", request_header),
        ));
    }

    let msg =
        bincode::deserialize::<IpEchoServerMessage>(&data[HEADER_LENGTH..]).map_err(|err| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to deserialize IpEchoServerMessage: {:?}", err),
            )
        })?;

    trace!("request: {:?}", msg);

    // Fire a datagram at each non-zero UDP port
    match std::net::UdpSocket::bind("0.0.0.0:0") {
        Ok(udp_socket) => {
            for udp_port in &msg.udp_ports {
                if *udp_port != 0 {
                    match udp_socket.send_to(&[0], SocketAddr::from((peer_addr.ip(), *udp_port))) {
                        Ok(_) => debug!("Successful send_to udp/{}", udp_port),
                        Err(err) => info!("Failed to send_to udp/{}: {}", udp_port, err),
                    }
                }
            }
        }
        Err(err) => {
            warn!("Failed to bind local udp socket: {}", err);
        }
    }

    // Try to connect to each non-zero TCP port
    for tcp_port in &msg.tcp_ports {
        if *tcp_port != 0 {
            debug!("Connecting to tcp/{}", tcp_port);

            let tcp_stream = timeout(
                IO_TIMEOUT,
                TcpStream::connect(&SocketAddr::new(peer_addr.ip(), *tcp_port)),
            )
            .await??;

            debug!("Connection established to tcp/{}", *tcp_port);
            let _ = tcp_stream.shutdown(std::net::Shutdown::Both);
        }
    }

    // "\0\0\0\0" header is added to ensure a valid response will never
    // conflict with the first four bytes of a valid HTTP response.
    let mut bytes = vec![0u8; ip_echo_server_reply_length()];
    bincode::serialize_into(&mut bytes[HEADER_LENGTH..], &peer_addr.ip()).unwrap();
    trace!("response: {:?}", bytes);
    writer.write_all(&bytes).await
}

async fn run_echo_server(tcp_listener: std::net::TcpListener) {
    info!("bound to {:?}", tcp_listener.local_addr().unwrap());
    let tcp_listener =
        TcpListener::from_std(tcp_listener).expect("Failed to convert std::TcpListener");

    loop {
        match tcp_listener.accept().await {
            Ok((socket, peer_addr)) => {
                runtime::Handle::current().spawn(async move {
                    if let Err(err) = process_connection(socket, peer_addr).await {
                        info!("session failed: {:?}", err);
                    }
                });
            }
            Err(err) => warn!("listener accept failed: {:?}", err),
        }
    }
}

/// Starts a simple TCP server on the given port that echos the IP address of any peer that
/// connects.  Used by |get_public_ip_addr|
pub fn ip_echo_server(tcp_listener: std::net::TcpListener) -> IpEchoServer {
    tcp_listener.set_nonblocking(true).unwrap();

    let runtime = Runtime::new().expect("Failed to create Runtime");
    runtime.spawn(run_echo_server(tcp_listener));
    runtime
}
