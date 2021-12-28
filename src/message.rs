use std::io::{Cursor, Read};
use std::net::TcpStream;
use byteorder::{BigEndian, ReadBytesExt};

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
        return Ok(Message { id: 255, payload: vec![] });
    }
    let mut message_buf = vec![0u8; message_length];
    stream.read_exact(&mut message_buf)?;
    let id = message_buf[0];
    let payload = &message_buf[1..];
    Ok(Message { id, payload: payload.to_vec() })
}