use std::borrow::{Borrow, BorrowMut};
use std::{cmp, fs, thread};
use std::cmp::Ordering;
use std::fmt::{Debug, Formatter};
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;
use anyhow::{bail, ensure};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use crossbeam_channel::{bounded, Receiver, Sender};
use serde_bencode::de;
use serde_bytes::ByteBuf;
use serde_derive::{Serialize, Deserialize};
use sha1::Sha1;
use urlencoding::encode_binary;


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

fn peer_to_socket_addr(chunk: &[u8]) -> anyhow::Result<SocketAddr> {
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


struct PieceRequest {
    hash: Vec<u8>,
    index: usize,
    length: usize,
}

impl Clone for PieceRequest {
    fn clone(&self) -> Self {
        let hash = self.hash.clone();
        return PieceRequest {
            hash,
            index: self.index,
            length: self.length,
        };
    }

    fn clone_from(&mut self, source: &Self) {
        self.hash = source.hash.clone();
        self.length = source.length;
        self.index = source.index;
    }
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
    // File hash
    let file_hash = "45c9feabba213bdc6d72e7469de71ea5aeff73faea6bfb109ab5bad37c3b43bd";

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
        let sock = peer_to_socket_addr(chunk)?;
        peers.push(sock);
    }

    // Piece hashes
    let pieces = torrent.info.pieces.as_ref();
    let piece_hashes: Vec<&[u8]> = pieces.chunks(20).collect();
    let (piece_request_sender, piece_request_receiver): (Sender<PieceRequest>, Receiver<PieceRequest>) = crossbeam_channel::bounded(piece_hashes.len());
    let (piece_collection_sender, piece_collection_receiver): (Sender<DownloadedPiece>, Receiver<DownloadedPiece>) = crossbeam_channel::unbounded();
    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let length = calculate_piece_size(index, torrent.info.piece_length as usize, torrent.info.length as usize);
        let piece_request = PieceRequest {
            index,
            hash: piece_hash.to_vec(),
            length,
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
    let mut file = vec![0u8; torrent.info.length as usize];
    while num_pieces_downloaded < piece_hashes.len() {
        let piece = piece_collection_receiver.recv()?;
        num_pieces_downloaded += 1;
        let percent_progress = (num_pieces_downloaded as f32 / piece_hashes.len() as f32) * 100f32;
        let (start, end) = calculate_bounds_for_piece(piece.index, piece.buf.len(), torrent.info.length as usize);
        file.splice(start..end, piece.buf);
        println!("Downloaded {:.2}%", percent_progress);
    }

    println!("Completed download. Verifying hash...");
    Ok(())
}

fn calculate_bounds_for_piece(index: usize, piece_length: usize, file_length: usize) -> (usize, usize) {
    let begin = index * piece_length;
    let mut end = begin + piece_length;
    if end > file_length {
        end = file_length;
    }
    return (begin, end);
}


fn create_handshake(pstr: &str, info_hash: [u8; 20], peer_id: [u8; 20]) -> Vec<u8> {
    let mut handshake: Vec<u8> = Vec::new();
    let pstrlen = pstr.len() as u8;
    handshake.push(pstrlen);
    handshake.extend(pstr.bytes());
    handshake.extend([0u8; 8]);
    handshake.extend(info_hash);
    handshake.extend(peer_id);
    return handshake;
}

fn compare_hashes(a: &[u8], b: &[u8]) -> cmp::Ordering {
    for (ai, bi) in a.iter().zip(b.iter()) {
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            ord => return ord
        }
    }

    a.len().cmp(&b.len())
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
    ensure!(compare_hashes(&info_hash, peer_info_hash).is_eq(), "Info hashes do not match");
    let peer_string_id = String::from_utf8(peer_id.to_vec())?;
    Ok(peer_string_id)
}

struct Message {
    id: u8,
    payload: Vec<u8>,
}

fn read_message(mut stream: &TcpStream) -> anyhow::Result<Message> {
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

struct Connection {
    stream: TcpStream,
    peer_id: String,
    bitfield: Vec<u8>,
    choked: bool,
}

impl Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let addr = self.stream.peer_addr().unwrap();
        write!(f, "{:?}-{:?}", self.peer_id, addr)
    }
}

fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<Connection> {
    let mut stream = TcpStream::connect_timeout(&peer, Duration::from_secs(HANDSHAKE_TIMEOUT))?;

    // Create handshake
    let protocol = "BitTorrent protocol";
    let handshake = create_handshake(protocol, info_hash, peer_id);

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(HANDSHAKE_TIMEOUT)))?;
    stream.write(&handshake)?;

    // Receive handshake
    let remote_peer_id_string = receive_handshake(&stream, info_hash)?;

    // Receive bitfield
    let bitfield_msg = read_message(&stream)?;
    ensure!(bitfield_msg.id == 5, "Was expecting a bitfield");

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

// TODO: type alias
const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_NOT_INTERESTED: u8 = 3;
const MSG_HAVE: u8 = 4;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;
const MSG_CANCEL: u8 = 8;
const MSG_KEEP_ALIVE: u8 = 255;

const MAX_BLOCK_SIZE: usize = 16384;

fn send_request_message(conn: &mut Connection, index: u32, num_bytes_requested: u32, block_size: u32) -> anyhow::Result<()> {
    let mut crsr = Cursor::new(vec![]);
    crsr.write_u32::<BigEndian>(1 + 4 + 4 + 4)?;
    crsr.write_u8(MSG_REQUEST)?;
    crsr.write_u32::<BigEndian>(index)?;
    crsr.write_u32::<BigEndian>(num_bytes_requested)?;
    crsr.write_u32::<BigEndian>(block_size)?;
    let request_message = crsr.get_mut();
    conn.stream.write(request_message)?;
    Ok(())
}

struct PieceMeta {
    length: usize,
    downloaded: usize,
    requested: usize,
    pipelined_requests: i8,
    index: usize,
}

const MAX_PIPELINED_REQUESTS: u8 = 5;
const DOWNLOAD_DEADLINE: u64 = 30;

fn download_piece(conn: &mut Connection, piece_request: PieceRequest) -> anyhow::Result<DownloadedPiece> {
    let (deadline_sender, deadline_receiver): (Sender<bool>, Receiver<bool>) = bounded(1);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(DOWNLOAD_DEADLINE));
        deadline_sender.send(true);
    });
    let mut num_bytes_requested = 0u32;
    let mut num_bytes_downloaded = 0usize;
    let mut num_pipelined_requests = 0u8;
    let mut piece_buf = vec![0u8; piece_request.length];
    while num_bytes_downloaded < piece_request.length {
        if deadline_receiver.is_full() {
            bail!("Deadline reached.")
        }
        if !conn.choked {
            while num_pipelined_requests < MAX_PIPELINED_REQUESTS && num_bytes_requested < piece_request.length as u32 {
                let mut block_size = MAX_BLOCK_SIZE as u32;
                if piece_request.length - (num_bytes_requested as usize) < block_size as usize {
                    block_size = (piece_request.length - num_bytes_requested as usize) as u32;
                }
                send_request_message(conn, piece_request.index as u32, num_bytes_requested, block_size)?;
                num_bytes_requested += block_size;
                num_pipelined_requests += 1;
            }
        }
        let msg = read_message(&conn.stream)?;
        match msg.id {
            MSG_CHOKE => {
                conn.choked = true
            }
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
            MSG_HAVE => { println!("Received HAVE message from {:?}", conn) }
            _ => {}
        }
    }
    let downloaded_piece = DownloadedPiece {
        buf: piece_buf,
        index: piece_request.index,
    };
    Ok(downloaded_piece)
}

fn send_interested(mut conn: &mut Connection) -> anyhow::Result<()> {
    let mut wtr = Cursor::new(vec![0u8; 5]);
    wtr.write_u32::<BigEndian>(1)?;
    wtr.write_u8(MSG_INTERESTED)?;
    conn.stream.write(wtr.get_ref())?;
    Ok(())
}

fn send_unchoke(mut conn: &mut Connection) -> anyhow::Result<()> {
    let mut wtr = Cursor::new(vec![0u8; 5]);
    wtr.write_u32::<BigEndian>(1)?;
    wtr.write_u8(MSG_UNCHOKE)?;
    conn.stream.write(wtr.get_ref())?;
    Ok(())
}

const DOWNLOAD_TIMEOUT: u64 = 5;


fn start_worker(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20], piece_request_receiver: Receiver<PieceRequest>, piece_request_sender: Sender<PieceRequest>, piece_collection_sender: Sender<DownloadedPiece>) -> anyhow::Result<()> {
    // Connect and handshake with peer
    let mut conn = connect_to_peer(peer, info_hash, peer_id)?;
    println!("Successfully completed handshake with {:?}", peer.to_string());

    // Send Unchoke
    send_unchoke(conn.borrow_mut());

    // Send Interested
    send_interested(conn.borrow_mut())?;

    // Read from receiver and download each piece
    for piece_request in piece_request_receiver {

        // Check if peer has the piece
        let has_piece = bitfield_contains_index(&conn.bitfield, piece_request.index as isize);
        if !has_piece {
            piece_request_sender.send(piece_request);
            continue;
        }

        // Attempt to download the piece
        conn.stream.set_read_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        conn.stream.set_write_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        match download_piece(conn.borrow_mut(), piece_request.clone()) {
            Err(e) => {
                conn.stream.shutdown(Shutdown::Both)?;
                piece_request_sender.send(piece_request.clone());
                bail!("Failed to download piece from {:?}: {:?}", conn.peer_id, e);
            }
            Ok(piece) => {
                // Check integrity - put it back on the work queue if it's incorrect
                let mut hasher = Sha1::new();
                hasher.update(piece.buf.as_ref());
                let result = hasher.digest().bytes();
                if !compare_hashes(piece_request.hash.as_ref(), result.as_slice()).is_eq() {
                    piece_request_sender.send(piece_request.clone())?;
                    continue;
                }
                piece_collection_sender.send(piece);
            }
        }
    }

    Ok(())
}