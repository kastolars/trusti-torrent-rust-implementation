use core::result::Result::Ok;
use std::borrow::Borrow;
use serde_bytes::ByteBuf;
use sha1::Sha1;
use serde_derive::{Deserialize, Serialize};


#[derive(Serialize, Deserialize, Debug)]
pub struct Info {
    pub name: String,
    #[serde(rename = "piece length")]
    pub piece_length: u64,
    pub pieces: ByteBuf,
    pub length: u64,
}

impl Info {
    pub fn hash(&self) -> anyhow::Result<[u8; 20]> {
        let serialized_info = serde_bencode::to_bytes(&self)?;
        let mut hasher = Sha1::new();
        hasher.update(serialized_info.borrow());
        Ok(hasher.digest().bytes())
    }
}
