use std::io::{Cursor, Read};
use std::net::TcpStream;
use anyhow::bail;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

pub type MessageId = u8;

pub const MSG_CHOKE: MessageId = 0;
pub const MSG_UNCHOKE: MessageId = 1;
pub const MSG_INTERESTED: MessageId = 2;
pub const MSG_NOT_INTERESTED: MessageId = 3;
pub const MSG_HAVE: MessageId = 4;
pub const MSG_BITFIELD: MessageId = 5;
pub const MSG_REQUEST: MessageId = 6;
pub const MSG_PIECE: MessageId = 7;
pub const MSG_CANCEL: MessageId = 8;

pub struct Message {
    pub id: u8,
    pub payload: Vec<u8>,
}

pub fn read_message(mut stream: &TcpStream) -> anyhow::Result<Message> {
    let mut message_length_buf = [0u8; 4];
    stream.read_exact(&mut message_length_buf)?;
    let mut rdr = Cursor::new(message_length_buf);
    let message_length = rdr.read_u32::<BigEndian>().unwrap() as usize;
    if message_length == 0 {
        return Ok(Message { id: 255, payload: vec![] }); // Keep-Alive
    }
    let mut message_buf = vec![0u8; message_length];
    stream.read_exact(&mut message_buf)?;
    let id = message_buf[0];
    let payload = &message_buf[1..];
    Ok(Message { id, payload: payload.to_vec() })
}

pub fn build_request_message(index: u32, num_bytes_requested: u32, block_size: u32) -> anyhow::Result<Vec<u8>> {
    let mut crsr = Cursor::new(vec![]);
    crsr.write_u32::<BigEndian>(1 + 4 + 4 + 4)?;
    crsr.write_u8(MSG_REQUEST)?;
    crsr.write_u32::<BigEndian>(index)?;
    crsr.write_u32::<BigEndian>(num_bytes_requested)?;
    crsr.write_u32::<BigEndian>(block_size)?;
    Ok(crsr.into_inner())
}


fn build_interested_message() -> anyhow::Result<Vec<u8>> {
    let mut wtr = Cursor::new(vec![0u8; 5]);
    wtr.write_u32::<BigEndian>(1)?;
    wtr.write_u8(MSG_INTERESTED)?;
    Ok(wtr.into_inner())
}

fn build_unchoke_message() -> anyhow::Result<Vec<u8>> {
    let mut wtr = Cursor::new(vec![0u8; 5]);
    wtr.write_u32::<BigEndian>(1)?;
    wtr.write_u8(MSG_UNCHOKE)?;
    Ok(wtr.into_inner())
}

pub fn build_message(id: MessageId) -> anyhow::Result<Vec<u8>> {
    match id {
        MSG_INTERESTED => return build_interested_message(),
        MSG_UNCHOKE => return build_unchoke_message(),
        _ => bail!("Not implemented: {:?}", id)
    }
}