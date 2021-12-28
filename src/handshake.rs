pub struct Handshake {
    pub pstr: String,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        let pstrlen = self.pstr.len();
        buf.push(pstrlen as u8);
        buf.extend(self.pstr.bytes());
        buf.extend([0u8; 8]);
        buf.extend(self.info_hash);
        buf.extend(self.peer_id);
        buf
    }
}


