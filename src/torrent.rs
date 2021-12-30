use crate::Info;
use serde_derive::Deserialize;
use urlencoding::encode_binary;


#[derive(Deserialize, Debug)]
pub struct Torrent {
    pub info: Info,
    pub announce: String,
}

impl Torrent {
    pub fn build_tracker_url(&self, info_hash: [u8; 20], peer_id: [u8; 20]) -> String {
        let announce = self.announce.as_str();
        let info_hash = encode_binary(&info_hash);
        let peer_id = encode_binary(&peer_id);
        let left = self.info.length.to_string();
        format!("{}?compact=1&downloaded=0&port=6881&uploaded=0&info_hash={}&peer_id={}&left={}", announce, info_hash.as_ref(), peer_id.as_ref(), left.as_str())
    }
}
