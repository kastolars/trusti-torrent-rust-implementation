use std::borrow::{Borrow, BorrowMut};
use std::{fs, thread};
use std::cmp::Ordering;
use std::fmt::{Debug};
use std::fs::File;
use std::io::{Cursor, Read, Seek, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::time::Duration;
use anyhow::{bail, ensure};
use byteorder::{BigEndian, ReadBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
use serde_bencode::de;
use serde_bytes::ByteBuf;
use serde_derive::{Serialize, Deserialize};
use sha1::Sha1;
use urlencoding::encode_binary;
use std::io::SeekFrom;
use crate::connection::Connection;
use crate::handshake::Handshake;
use crate::message::{MSG_BITFIELD, read_message, MSG_UNCHOKE, MSG_CHOKE, MSG_PIECE, MSG_NOT_INTERESTED, MSG_HAVE, MSG_CANCEL, build_request_message, MessageId, build_message, MSG_INTERESTED};
use crate::piece_request::PieceRequest;

mod piece_request;
mod handshake;
mod message;
mod connection;

#[derive(Serialize, Deserialize, Debug)]
struct Info {
    name: String,
    #[serde(rename = "piece length")]
    piece_length: u64,
    pieces: ByteBuf,
    length: u64,
}

#[derive(Deserialize, Debug)]
struct Torrent {
    info: Info,
    announce: String,
}

#[derive(Deserialize, Debug)]
struct Tracker {
    peers: ByteBuf,
}

fn bytes_to_socket_addr(chunk: &[u8]) -> anyhow::Result<SocketAddr> {
    // Split chunk into ip octet and big endian port
    let (ip_octets, port) = chunk.split_at(4);

    // Get the ip address
    let ip_slice_to_array: [u8; 4] = ip_octets.try_into()?;
    let ip_addr = Ipv4Addr::from(ip_slice_to_array);

    // Get the port
    let mut rdr = Cursor::new(port);
    let port_fixed = rdr.read_u16::<BigEndian>()?;

    // Create socket address
    Ok(SocketAddr::new(IpAddr::V4(ip_addr), port_fixed))
}


struct DownloadedPiece {
    buf: Vec<u8>,
    index: usize,
}

fn calculate_piece_size(index: usize, piece_length: usize, length: usize) -> usize {
    let begin = index * piece_length;
    let mut end = begin + piece_length;
    if end > length {
        end = length
    }
    return end - begin;
}

fn main() -> anyhow::Result<()> {
    // Parse torrent
    let path = "debian-11.2.0-amd64-netinst.iso.torrent";
    let contents = fs::read(path).expect("Something went wrong reading the file");
    let torrent = de::from_bytes::<Torrent>(&contents)?;

    // Info hash
    let serialized_info = serde_bencode::to_bytes(&torrent.info)?;
    let mut hasher = Sha1::new();
    hasher.update(serialized_info.borrow());
    let info_hash = hasher.digest().bytes();

    // Peer id
    let random_bytes: Vec<u8> = (0..20).map(|_| { rand::random::<u8>() }).collect();
    let peer_id: [u8; 20] = <[u8; 20]>::try_from(random_bytes.borrow())?;

    // Tracker url
    let announce = torrent.announce.as_str();
    let ih = encode_binary(&info_hash);
    let pid = encode_binary(&peer_id);
    let left = torrent.info.length.to_string();
    let tracker_url = format!("{}?compact=1&downloaded=0&port=6881&uploaded=0&info_hash={}&peer_id={}&left={}", announce, ih.as_ref(), pid.as_ref(), left.as_str());

    // Tracker
    let resp = reqwest::blocking::get(tracker_url).unwrap().bytes()?;
    let tracker = de::from_bytes::<Tracker>(&resp)?;

    // Get peers
    let tracker_vec: Vec<u8> = tracker.peers.into_vec();
    let chunks: Vec<&[u8]> = tracker_vec.chunks(6).collect();
    let mut peers = Vec::<SocketAddr>::new();
    for chunk in chunks {
        let sock = bytes_to_socket_addr(chunk)?;
        peers.push(sock);
    }

    // Piece hashes
    let pieces = torrent.info.pieces.as_ref();
    let piece_hashes: Vec<&[u8]> = pieces.as_ref().chunks(20).collect();
    let (piece_request_sender, piece_request_receiver): (Sender<PieceRequest>, Receiver<PieceRequest>) = crossbeam_channel::bounded(piece_hashes.len());
    let (piece_collection_sender, piece_collection_receiver): (Sender<DownloadedPiece>, Receiver<DownloadedPiece>) = crossbeam_channel::unbounded();
    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let size = calculate_piece_size(index, torrent.info.piece_length as usize, torrent.info.length as usize);
        let piece_request = PieceRequest {
            index,
            hash: piece_hash.to_vec(),
            size,
        };
        piece_request_sender.send(piece_request)?;
    }

    // Initialize threads
    for &peer in &peers {
        let piece_request_receiver_copy = piece_request_receiver.clone();
        let piece_request_sender_copy = piece_request_sender.clone();
        let piece_collection_sender_copy = piece_collection_sender.clone();
        thread::spawn(move || {
            if let Err(e) = start_worker(
                peer,
                info_hash,
                peer_id,
                piece_request_receiver_copy,
                piece_request_sender_copy,
                piece_collection_sender_copy,
            ) {
                println!("Disconnected from worker {:?}: {:?}", peer.to_string(), e);
            }
        });
    }

    let mut num_pieces_downloaded = 0usize;
    let mut file = File::create("debian-11.2.0-amd64-netinst.iso")?;
    while num_pieces_downloaded < piece_hashes.len() {
        let piece = piece_collection_receiver.recv()?;
        num_pieces_downloaded += 1;
        let percent_progress = (num_pieces_downloaded as f32 / piece_hashes.len() as f32) * 100f32;
        let start = piece.index * piece.buf.len();
        file.seek(SeekFrom::Start(start as u64))?;
        file.write(piece.buf.as_ref())?;
        println!("Downloaded {:.2}%", percent_progress);
    }

    println!("Completed download.");

    Ok(())
}

fn compare_byte_slices(a: &[u8], b: &[u8]) -> bool {
    for (ai, bi) in a.iter().zip(b.iter()) {
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            ord => return ord.is_eq()
        }
    }

    a.len().cmp(&b.len()).is_eq()
}

const HANDSHAKE_TIMEOUT: u64 = 5;

fn receive_handshake(mut stream: &TcpStream, info_hash: [u8; 20]) -> anyhow::Result<String> {
    let protocol = "BitTorrent protocol";
    stream.set_read_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT)))?;
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
    ensure!(compare_byte_slices(&info_hash, peer_info_hash), "Info hashes do not match");
    let peer_string_id = String::from_utf8(peer_id.to_vec())?;
    Ok(peer_string_id)
}


fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<Connection> {
    let mut stream = TcpStream::connect_timeout(&peer, Duration::from_secs(HANDSHAKE_TIMEOUT))?;

    // Create handshake
    let handshake = Handshake {
        pstr: "BitTorrent protocol".to_string(),
        info_hash,
        peer_id,
    };
    let handshake = handshake.serialize();

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT)))?;
    stream.write(&handshake)?;

    // Receive handshake
    let remote_peer_id_string = receive_handshake(&stream, info_hash)?;

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

fn bitfield_contains_index(bitfield: &Vec<u8>, index: isize) -> bool {
    let byte_index = index / 8;
    let offset = index % 8;
    if byte_index < 0 || byte_index >= bitfield.len() as isize {
        return false;
    }
    return bitfield[byte_index as usize] >> (7 - offset) & 1 != 0;
}

const MAX_BLOCK_SIZE: usize = 16384;

fn send_request_message(conn: &mut Connection, index: u32, num_bytes_requested: u32, block_size: u32) -> anyhow::Result<()> {
    let request_message = build_request_message(index, num_bytes_requested, block_size)?;
    conn.stream.write(request_message.as_ref())?;
    Ok(())
}

const MAX_PIPELINED_REQUESTS: u8 = 5;
const DOWNLOAD_DEADLINE: u64 = 30;

fn download_piece(conn: &mut Connection, piece_request: PieceRequest) -> anyhow::Result<DownloadedPiece> {
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
        if !conn.choked {
            while num_pipelined_requests < MAX_PIPELINED_REQUESTS && num_bytes_requested < piece_request.size as u32 {
                let mut block_size = MAX_BLOCK_SIZE as u32;
                if piece_request.size - (num_bytes_requested as usize) < block_size as usize {
                    block_size = (piece_request.size - num_bytes_requested as usize) as u32;
                }
                send_request_message(conn, piece_request.index as u32, num_bytes_requested, block_size)?;
                num_bytes_requested += block_size;
                num_pipelined_requests += 1;
            }
        }
        let msg = read_message(&conn.stream)?;
        match msg.id {
            MSG_CHOKE => conn.choked = true,
            MSG_UNCHOKE => conn.choked = false,
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

fn send_message(conn: &mut Connection, id: MessageId) -> anyhow::Result<()> {
    let message = build_message(id)?;
    conn.stream.write(message.as_ref())?;
    Ok(())
}

const DOWNLOAD_TIMEOUT: u64 = 5;


fn start_worker(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20], piece_request_receiver: Receiver<PieceRequest>, piece_request_sender: Sender<PieceRequest>, piece_collection_sender: Sender<DownloadedPiece>) -> anyhow::Result<()> {
    // Connect and handshake with peer
    let mut conn = connect_to_peer(peer, info_hash, peer_id)?;
    println!("Successfully completed handshake with {:?}", peer.to_string());

    // Send Unchoke
    send_message(conn.borrow_mut(), MSG_UNCHOKE)?;

    // Send Interested
    send_message(conn.borrow_mut(), MSG_INTERESTED)?;

    // Read from receiver and download each piece
    for piece_request in piece_request_receiver {

        // Check if peer has the piece
        let has_piece = bitfield_contains_index(&conn.bitfield, piece_request.index as isize);
        if !has_piece {
            piece_request_sender.send(piece_request)?;
            continue;
        }

        // Attempt to download the piece
        conn.stream.set_read_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        conn.stream.set_write_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        match download_piece(conn.borrow_mut(), piece_request.clone()) {
            Err(e) => {
                conn.stream.shutdown(Shutdown::Both)?;
                piece_request_sender.send(piece_request.clone())?;
                bail!("Failed to download piece from {:?}: {:?}", conn.peer_id, e);
            }
            Ok(piece) => {
                // Check integrity - put it back on the work queue if it's incorrect
                let mut hasher = Sha1::new();
                hasher.update(piece.buf.as_ref());
                let result = hasher.digest().bytes();
                if !compare_byte_slices(piece_request.hash.as_ref(), result.as_slice()) {
                    piece_request_sender.send(piece_request.clone())?;
                    continue;
                }
                piece_collection_sender.send(piece)?;
            }
        }
    }

    Ok(())
}