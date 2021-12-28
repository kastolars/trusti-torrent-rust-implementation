use std::fmt::{Debug, Formatter};
use std::net::TcpStream;

pub struct Connection {
    pub stream: TcpStream,
    pub peer_id: String,
    pub bitfield: Vec<u8>,
    pub choked: bool,
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let addr = self.stream.peer_addr().unwrap();
        write!(f, "{:?}-{:?}", self.peer_id, addr)
    }
}