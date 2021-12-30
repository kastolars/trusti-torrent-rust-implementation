use std::io::Read;
use std::net::TcpStream;
use anyhow::ensure;
use crate::compare_byte_slices;

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

pub fn compare_handshakes(mut stream: &TcpStream, hs: Handshake) -> anyhow::Result<String >{
    let protocol = "BitTorrent protocol";
    let mut pstrlen_buf = [0u8; 1];
    stream.read_exact(&mut pstrlen_buf)?;
    let pstrlen: usize = pstrlen_buf[0].into();
    ensure!(pstrlen != 0, "Pstrlen cannot be zero");
    let mut pstr_buf = vec![0u8; pstrlen];
    stream.read_exact(&mut pstr_buf)?;
    let pstr = String::from_utf8(pstr_buf)?;
    ensure!(pstr == protocol, "Peer does not use BitTorrent protocol");
    let mut handshake_buf = [0u8; 48];
    stream.read_exact(&mut handshake_buf)?;
    let peer_info_hash = &handshake_buf[8..8 + 20];
    let peer_id = &handshake_buf[8 + 20..];
    ensure!(compare_byte_slices(&hs.info_hash, peer_info_hash), "Info hashes do not match");
    let peer_string_id = String::from_utf8(peer_id.to_vec())?;
    Ok(peer_string_id)
}


#[cfg(test)]
mod tests {
    use crate::Handshake;

    #[test]
    fn test_serialize() {
        let hs = Handshake {
            pstr: "BitTorrent protocol".to_string(),
            info_hash: [2u8; 20],
            peer_id: [3u8; 20],
        };

        let serialized = hs.serialize();
        assert_eq!(serialized[0], 19);
        let pstr = String::from_utf8(serialized[1..20].to_owned()).unwrap();
        assert_eq!(pstr, "BitTorrent protocol");
        assert_eq!(serialized[20..28], [0u8; 8]);
        assert_eq!(serialized[28..48], [2u8; 20]);
        assert_eq!(serialized[48..68], [3u8; 20]);
    }
}
