use std::fmt::{Debug, Formatter};
use std::io::{Cursor, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;
use anyhow::{bail, ensure};
use byteorder::{BigEndian, ReadBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
use crate::{build_message, build_request_message, DownloadedPiece, Handshake, MessageId, PieceRequest};
use crate::handshake::compare_handshakes;
use crate::message::{MSG_BITFIELD, MSG_CANCEL, MSG_CHOKE, MSG_HAVE, MSG_NOT_INTERESTED, MSG_PIECE, MSG_UNCHOKE, read_message};


const DOWNLOAD_DEADLINE: u64 = 30;
const MAX_PIPELINED_REQUESTS: u8 = 5;
const MAX_BLOCK_SIZE: usize = 16384;
const HANDSHAKE_TIMEOUT: u64 = 5;


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


impl Connection {
    fn send_request_message(&mut self, index: u32, num_bytes_requested: u32, block_size: u32) -> anyhow::Result<()> {
        let request_message = build_request_message(index, num_bytes_requested, block_size)?;
        self.stream.write(request_message.as_ref())?;
        Ok(())
    }

    pub fn send_message(&mut self, id: MessageId) -> anyhow::Result<()> {
        let message = build_message(id)?;
        self.stream.write(message.as_ref())?;
        Ok(())
    }

    pub fn download_piece(&mut self, piece_request: PieceRequest) -> anyhow::Result<DownloadedPiece> {
        let (deadline_sender, deadline_receiver): (Sender<bool>, Receiver<bool>) = bounded(1);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(DOWNLOAD_DEADLINE));
            let _ = deadline_sender.send(true);
        });
        let mut num_bytes_requested = 0u32;
        let mut num_bytes_downloaded = 0usize;
        let mut num_pipelined_requests = 0u8;
        let mut piece_buf = vec![0u8; piece_request.size];
        while num_bytes_downloaded < piece_request.size {
            if deadline_receiver.is_full() {
                bail!("Deadline reached.")
            }
            if !self.choked {
                while num_pipelined_requests < MAX_PIPELINED_REQUESTS && num_bytes_requested < piece_request.size as u32 {
                    let mut block_size = MAX_BLOCK_SIZE as u32;
                    if piece_request.size - (num_bytes_requested as usize) < block_size as usize {
                        block_size = (piece_request.size - num_bytes_requested as usize) as u32;
                    }
                    self.send_request_message(piece_request.index as u32, num_bytes_requested, block_size)?;
                    num_bytes_requested += block_size;
                    num_pipelined_requests += 1;
                }
            }
            let msg = read_message(&self.stream)?;
            match msg.id {
                MSG_CHOKE => self.choked = true,
                MSG_UNCHOKE => self.choked = false,
                MSG_PIECE => {
                    ensure!(msg.payload.len() >= 8, "Payload must be at least 8 bytes.");
                    let mut rdr = Cursor::new(msg.payload);
                    let piece_index = rdr.read_u32::<BigEndian>()? as usize;
                    ensure!(piece_index == piece_request.index, "Mismatched piece indexes.");
                    let start = rdr.read_u32::<BigEndian>()? as usize;
                    ensure!(start < piece_buf.len(), "Start of piece is out of bounds of write buffer");
                    let block = &rdr.get_ref()[8..];
                    piece_buf.splice(start..(start + block.len()), block.iter().cloned());
                    num_bytes_downloaded += block.len();
                    num_pipelined_requests -= 1;
                }
                MSG_NOT_INTERESTED => {}
                MSG_HAVE => {}
                MSG_CANCEL => {}
                _ => {}
            }
        }
        let downloaded_piece = DownloadedPiece {
            buf: piece_buf,
            index: piece_request.index,
        };
        Ok(downloaded_piece)
    }
}

pub fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<Connection> {
    let mut stream = TcpStream::connect_timeout(&peer, Duration::from_secs(HANDSHAKE_TIMEOUT))?;

    // Set timeouts
    stream.set_write_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT)))?;
    stream.set_read_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT)))?;


    // Create handshake
    let handshake_struct = Handshake {
        pstr: "BitTorrent protocol".to_string(),
        info_hash,
        peer_id,
    };
    let handshake = handshake_struct.serialize();

    // Send handshake
    stream.write(&handshake)?;

    // Receive handshake
    let remote_peer_id_string = compare_handshakes(&stream, handshake_struct)?;

    // Receive bitfield
    let bitfield_msg = read_message(&stream)?;
    ensure!(bitfield_msg.id == MSG_BITFIELD, "Was expecting a bitfield");

    return Ok(Connection {
        choked: true,
        stream,
        peer_id: remote_peer_id_string,
        bitfield: bitfield_msg.payload,
    });
}


