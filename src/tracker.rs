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
    let (ip_octets, port) = chunk.split_at(4);

    // Get the ip address
    let ip_slice_to_array: [u8; 4] = ip_octets.try_into()?;
    let ip_addr = Ipv4Addr::from(ip_slice_to_array);

    // Get the port
    let mut rdr = Cursor::new(port);
    let port_fixed = rdr.read_u16::<BigEndian>()?;

    // Create socket address
    Ok(SocketAddr::new(IpAddr::V4(ip_addr), port_fixed))
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