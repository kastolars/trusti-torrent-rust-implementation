use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use byteorder::{BigEndian, ReadBytesExt};
use serde_bytes::ByteBuf;
use serde_derive::Deserialize;


#[derive(Deserialize, Debug)]
pub struct Tracker {
    pub peers: ByteBuf,
}

fn bytes_to_socket_addr(chunk: &[u8]) -> anyhow::Result<SocketAddr> {
    // Split chunk into ip octet and big endian port
    let (ip, port) = chunk.split_at(4);

    // Get the ip address
    let ip: [u8; 4] = ip.try_into()?;
    let ip = Ipv4Addr::from(ip);

    // Get the port
    let mut rdr = Cursor::new(port);
    let port = rdr.read_u16::<BigEndian>()?;

    // Create socket address
    Ok(SocketAddr::new(IpAddr::V4(ip), port))
}


impl Tracker {
    pub fn get_peers(self) -> anyhow::Result<Vec<SocketAddr>> {
        let tracker_vec: Vec<u8> = self.peers.into_vec();
        let chunks: Vec<&[u8]> = tracker_vec.chunks(6).collect();
        let mut peers = Vec::<SocketAddr>::new();
        for chunk in chunks {
            let sock = bytes_to_socket_addr(chunk)?;
            peers.push(sock);
        }
        Ok(peers)
    }
}