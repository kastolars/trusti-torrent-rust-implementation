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

    pub fn calculate_piece_size(self, index: usize) -> usize {
        let begin = index * self.piece_length as usize;
        let mut end = begin + self.piece_length as usize;
        if end > self.length as usize {
            end = self.length as usize
        }
        return end - begin;
    }
}


impl Clone for Info {
    fn clone(&self) -> Self {
        return Info {
            name: self.name.clone(),
            piece_length: self.piece_length,
            pieces: self.pieces.clone(),
            length: self.length,
        };
    }

    fn clone_from(&mut self, source: &Self) {
        self.name = source.name.clone();
        self.pieces = source.pieces.clone();
        self.piece_length = source.piece_length;
        self.length = source.length;
    }
}