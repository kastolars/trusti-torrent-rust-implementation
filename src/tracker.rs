use serde_bytes::ByteBuf;
use serde_derive::Deserialize;


#[derive(Deserialize, Debug)]
pub struct Tracker {
    pub peers: ByteBuf,
}


