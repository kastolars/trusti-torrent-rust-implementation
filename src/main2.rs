use std::{cmp, fs, thread};
use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use serde_bencode::{de};
use serde_derive::{Serialize, Deserialize};
use serde_bytes::ByteBuf;
use sha1::{Sha1};
use urlencoding::{encode_binary};
use anyhow::{anyhow, bail, ensure};
use crossbeam_channel::{Receiver, Sender};
use num_enum::TryFromPrimitive;
use num_enum::IntoPrimitive;
use std::convert::TryFrom;
use std::ptr::hash;

const MSG_INTERESTED: u8 = 2;

#[derive(Debug, Eq, PartialEq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
enum MessageId {
    Choke = 0,
    UnChoke = 1,
    Interested = 2,
    NotInterested = 3,
    Have = 4,
    BitField = 5,
    Request = 6,
    Piece = 7,
    Cancel = 8,
    KeepAlive = 9,
}

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


impl Info {
    fn hash(&self) -> anyhow::Result<[u8; 20]> {
        let serialized_info = serde_bencode::to_bytes(&self)?;
        let mut hasher = Sha1::new();
        hasher.update(serialized_info.as_slice());
        Ok(hasher.digest().bytes())
    }

    fn piece_hashes(&self) -> Vec<&[u8]> {
        let pieces = self.pieces.as_ref();
        return pieces.chunks(20).collect();
    }
}

impl From<&str> for Torrent {
    fn from(path: &str) -> Self {
        let contents = fs::read(path).expect("Something went wrong reading the file");
        return de::from_bytes::<Torrent>(&contents).unwrap();
    }
}

fn peer_to_socket_addr(chunk: &[u8]) -> SocketAddr {
    // Split chunk into ip octet and big endian port
    let (ip_octets, port) = chunk.split_at(4);

    // Get the ip address
    let ip_slice_to_array: [u8; 4] = ip_octets.try_into().unwrap();
    let ip_addr = Ipv4Addr::from(ip_slice_to_array);

    // Get the port
    let mut rdr = Cursor::new(port);
    let port_fixed = rdr.read_u16::<BigEndian>().unwrap();

    // Create socket address
    return SocketAddr::new(IpAddr::V4(ip_addr), port_fixed);
}

fn build_tracker_url(torrent: Torrent, info_hash: &[u8], peer_id: &[u8]) -> String {
    let announce = torrent.announce.as_str();
    let ih = encode_binary(&info_hash);
    let pid = encode_binary(&peer_id);
    let left = torrent.info.length.to_string();
    return format!("{}?compact=1&downloaded=0&port=6881&uploaded=0&info_hash={}&peer_id={}&left={}", announce, ih.as_ref(), pid.as_ref(), left.as_str());
}

fn create_handshake(info_hash: &[u8], peer_id: &[u8]) -> Vec<u8> {
    let mut handshake: Vec<u8> = Vec::new();
    let protocol_id = "BitTorrent protocol";
    let protocol_id_length: u8 = protocol_id.len().try_into().unwrap();
    handshake.push(protocol_id_length);
    handshake.extend(protocol_id.bytes());
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


struct PieceResult {
    index: usize,
    buf: Vec<u8>,
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
    // Hardcoded local filepath
    let path = "C:\\Users\\kasto\\IdeaProjects\\trusti\\src\\debian-11.2.0-amd64-netinst.iso.torrent";

    // Parse the torrent file
    let contents = fs::read(path).expect("Something went wrong reading the file");
    let torrent = de::from_bytes::<Torrent>(&contents)?;

    // Info hash
    let serialized_info = serde_bencode::to_bytes(&torrent.info)?;
    let mut hasher = Sha1::new();
    hasher.update(serialized_info.as_slice());
    let info_hash = hasher.digest().bytes();

    // Peer id
    let random_bytes: Vec<u8> = (0..20).map(|_| { rand::random::<u8>() }).collect();
    let peer_id: [u8; 20] = <[u8; 20]>::try_from(random_bytes.as_slice())?;

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
    let vec: Vec<u8> = tracker.peers.into_vec();
    let chunks: Vec<&[u8]> = vec.chunks(6).collect();
    let peers: Vec<SocketAddr> = chunks.iter().map(|chunk| peer_to_socket_addr(chunk)).collect();

    // Get piece hashes
    let piece_hashes = torrent.info.piece_hashes();

    // Get piece_hashes
    let (piece_request_sender, piece_request_receiver): (Sender<PieceRequest>, Receiver<PieceRequest>) = crossbeam_channel::bounded(piece_hashes.len());
    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let vec = piece_hash.to_vec();
        let length = calculate_piece_size(index, torrent.info.piece_length as usize, torrent.info.length as usize);
        let piece_req = PieceRequest {
            index,
            piece_hash: vec,
            length,
        };
        piece_request_sender.send(piece_req)?;
    }

    // Iterate over peers
    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    for &peer in &peers {
        let piece_request_receiver_copy = piece_request_receiver.clone();
        let piece_request_sender_copy = piece_request_sender.clone();
        let handle = thread::spawn(move || {
            let _ = start_worker(peer, info_hash, peer_id, piece_request_receiver_copy, piece_request_sender_copy);
        });
        handles.push(handle);
    }

    // Join handles
    for handle in handles { let _ = handle.join(); }

    Ok(())
}

#[derive(Debug, Clone)]
struct PieceRequest {
    piece_hash: Vec<u8>,
    index: usize,
    length: usize,
}

fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<Connection> {
    // Initialize connection
    let mut stream = TcpStream::connect_timeout(&peer, Duration::from_secs(3))?;

    // Create handshake
    let mut handshake: Vec<u8> = Vec::new();
    let protocol_id = "BitTorrent protocol";
    let protocol_id_length = protocol_id.len() as u8;
    handshake.push(protocol_id_length);
    handshake.extend(protocol_id.bytes());
    handshake.extend([0u8; 8]);
    handshake.extend(info_hash);
    handshake.extend(peer_id);

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write(&handshake)?;

    // Read Pstrlen
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut pstrlen_buf = [0u8; 1];
    stream.read(&mut pstrlen_buf)?;
    let pstrlen = pstrlen_buf[0] as usize;
    ensure!(pstrlen != 0, "PstrLen cannot be 0!");

    // Get Pstr
    let mut pstr_buf = vec![0u8; pstrlen];
    stream.read(&mut pstr_buf)?;
    let peer_protocol = String::from_utf8(pstr_buf)?;
    ensure!(peer_protocol == protocol_id, "Must use BitTorrent protocol");

    // Get info hash and peer_id
    let mut handshake_buf = vec![0u8; 48];
    stream.read(&mut handshake_buf)?;
    let peer_info_hash = &handshake_buf[8..8 + 20];
    let seeder_peer_id = String::from_utf8(handshake_buf[8 + 20..].to_vec())?;

    // Compare info hashes
    ensure!(compare_hashes(&info_hash, peer_info_hash).is_eq(), "Mismatched info hashes");
    // println!("Completed handshake with peer {:?}", seeder_peer_id);

    // Receive bitfield
    let bitfield_msg = read_message(&stream)?;
    ensure!(bitfield_msg.id == MessageId::BitField, "Was expecting a bitfield");

    // Create the connection
    Ok(Connection {
        choked: true,
        bitfield: bitfield_msg.payload,
        peer_id: seeder_peer_id,
        stream,
    })
}

const MAX_BLOCK_SIZE: u32 = 16384;
const MAX_PIPELINED_REQUESTS: i8 = 5;


fn start_worker(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20], piece_request_receiver: Receiver<PieceRequest>, piece_request_sender: Sender<PieceRequest>) -> anyhow::Result<()> {
    // Establish connection
    let mut conn = connect_to_peer(peer, info_hash, peer_id)?;

    // Send Unchoke
    let unchoke_buf: [u8; 5] = [0, 0, 0, 1, 1];
    conn.stream.write(&unchoke_buf)?;

    // Send Interested
    let interested_buf: [u8; 5] = [0, 0, 0, 1, MSG_INTERESTED];
    conn.stream.write(&interested_buf)?;

    // Loop over pieces
    for piece_request in piece_request_receiver {
        // Check if peer has piece
        let has_piece = bitfield_contains_index(&conn.bitfield, piece_request.index as isize);

        // If they don't put it back in the queue
        if !has_piece {
            piece_request_sender.send(piece_request)?;
            continue;
        }

        if let Err(e) = download_piece(&mut conn, &piece_request) {
            println!("{:?}", e);
        }

        // Download
        // if download_piece(&piece_request, &mut conn).is_err() {
        //     piece_request_sender.send(piece_request.clone())?;
        //     println!("Failed to download piece {:?} from {:?}", piece_request.index, conn.peer_id);
        //     return Err(anyhow!("Failed to download piece"));
        // }


        // Receive messages
        //     let msg = read_message(&conn.stream)?;
        //     match msg.id {
        //         MessageId::UnChoke => { conn.choked = false; }
        //         MessageId::Choke => { conn.choked = true; }
        //         MessageId::Piece => {
        //             if msg.payload.len() < 8 {
        //                 println!("Payload must be at least size 8");
        //                 return Err(anyhow!("Payload must be at least size 8"));
        //             }
        //             println!("Received {:?} message from {:?}", msg.id, conn.peer_id);
        //             let mut crsr = Cursor::new(msg.payload);
        //             let piece_index = crsr.read_u32::<BigEndian>()? as usize;
        //             if piece_index != piece_request.index {
        //                 println!("Mismatched piece index");
        //                 return Err(anyhow!("Mismatched piece index."));
        //             }
        //             let begin = crsr.read_u32::<BigEndian>()? as usize;
        //             if begin >= piece_buf.len() {
        //                 println!("Begin must be less than the length of the buffer");
        //                 return Err(anyhow!("Begin must be less than the length of the buffer"));
        //             }
        //             let data = &crsr.get_ref()[8..];
        //             piece_buf.splice(begin..begin, data.iter().cloned());
        //             num_pipelined_requests -= 1;
        //             num_bytes_downloaded += data.len();
        //         }
        //         _ => {}
        //     }
        // }

        // Check integrity - put it back on the work queue if it's incorrect
        // let mut hasher = Sha1::new();
        // hasher.update(piece_buf.as_ref());
        // let result = hasher.digest().bytes();
        // if !compare_hashes(piece_request.piece_hash.as_ref(), result.as_slice()).is_eq() {
        //     piece_request_sender.send(piece_request.clone())?;
        //     continue;
        // }

        // TODO: Send Have message
        // let mut crsr = Cursor::new(vec![]);
        // crsr.write_u32::<BigEndian>(1 + 4)?;
        // crsr.write_u8(MessageId::Have.into())?;
        // crsr.write_u32::<BigEndian>(piece_request.index as u32)?;
        // let have_message = crsr.get_mut();
        // conn.stream.write(have_message)?;

        // TODO: Send to results collection
    }

    Ok(())
}

fn bitfield_contains_index(bitfield: &Vec<u8>, index: isize) -> bool {
    let byte_index = index / 8;
    let offset = index % 8;
    if byte_index < 0 || byte_index >= bitfield.len() as isize {
        return false;
    }
    return bitfield[byte_index as usize] >> (7 - offset) & 1 != 0;
}

fn read_message(mut stream: &TcpStream) -> anyhow::Result<Message> {
    let mut message_length_buf = [0u8; 4];
    stream.read(&mut message_length_buf)?;
    let mut crsr = Cursor::new(message_length_buf);
    let message_length = crsr.read_u32::<BigEndian>().unwrap() as usize;
    if message_length == 0 {
        return Ok(Message {
            id: MessageId::KeepAlive,
            payload: vec![],
        });
    }
    let mut message_buf = vec![0u8; message_length];
    stream.read(&mut message_buf)?;
    let message_id = message_buf[0];
    let payload = message_buf[1..].to_vec();
    return Ok(Message {
        id: MessageId::try_from(message_id)?,
        payload,
    });
}

struct Connection {
    choked: bool,
    bitfield: Vec<u8>,
    peer_id: String,
    stream: TcpStream,
}

struct Message {
    id: MessageId,
    payload: Vec<u8>,
}

struct PieceState {}

fn download_piece(conn: &mut Connection, piece_request: &PieceRequest) -> anyhow::Result<()> {
    let mut num_bytes_requested = 0u32;
    let mut num_bytes_downloaded = 0usize;
    let mut num_pipelined_requests = 0i8;
    conn.stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    conn.stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    while num_bytes_downloaded < piece_request.length {
        // println!("Piece {:?} progress: {:?} / {:?}", piece_request.index, num_bytes_downloaded, piece_request.length);
        if !conn.choked {
            while num_pipelined_requests < MAX_PIPELINED_REQUESTS && num_bytes_requested < piece_request.length as u32 {
                let mut block_size = MAX_BLOCK_SIZE;
                if piece_request.length - (num_bytes_requested as usize) < block_size as usize {
                    block_size = (piece_request.length - num_bytes_requested as usize) as u32
                }
                send_request_message(conn, piece_request.index as u32, num_bytes_requested, block_size)?;
                num_bytes_requested += block_size;
                num_pipelined_requests += 1;
            }
        }
        let msg = read_message(&conn.stream);
        if msg.is_err() {
            continue
        }
        let msg = msg.unwrap();
        // println!("Received message: {:?}", msg.id);

        match msg.id {
            MessageId::UnChoke => { conn.choked = false; }
            MessageId::Choke => { conn.choked = true; }
            MessageId::Piece => {
                let block = &msg.payload[8..];
                num_bytes_downloaded += block.len();
                num_pipelined_requests -= 1;
            }
            MessageId::Have => {}
            _ => {}
        }
    }
    println!("Finished downloading piece {:?}", piece_request.index);
    Ok(())
}

fn download_piece2(piece_request: &PieceRequest, conn: &mut Connection) -> anyhow::Result<()> {
    let mut num_bytes_requested = 0usize;
    let mut num_bytes_downloaded = 0usize;
    let mut num_pipelined_requests = 0i8;
    conn.stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    conn.stream.set_write_timeout(Some(Duration::from_secs(30)))?;
    while num_bytes_downloaded < piece_request.length {
        // println!("Peer: {:?} - choked: {:?} - piece index: {:?} - num bytes: {:?} / {:?}", conn.peer_id, conn.choked, piece_request.index, num_bytes_downloaded, piece_request.length);
        let mut block_size = MAX_BLOCK_SIZE;
        if piece_request.length - num_bytes_requested < block_size as usize {
            block_size = (piece_request.length - num_bytes_requested) as u32
        }
        if !conn.choked {
            while num_pipelined_requests < MAX_PIPELINED_REQUESTS && num_bytes_requested < piece_request.length {
                // Send request for block
                send_request_message(conn, piece_request.index as u32, num_bytes_requested as u32, block_size);

                // Dynamic variables
                num_bytes_requested += block_size as usize;
                num_pipelined_requests += 1;
            }
        }
        let msg = read_message(&conn.stream)?;
        match msg.id {
            MessageId::UnChoke => { conn.choked = false; }
            MessageId::Choke => { conn.choked = true; }
            MessageId::Piece => {
                num_bytes_downloaded += (msg.payload[8..]).len();
                num_pipelined_requests -= 1;
            }
            MessageId::Have => {
                println!("Received Have message");
            }
            _ => {}
        }
    }
    // println!("Finished downloading piece {:?} from peer {:?}", piece_request.index, conn.peer_id);
    Ok(())
}

fn send_request_message(mut conn: &mut Connection, index: u32, num_bytes_requested: u32, block_size: u32) -> anyhow::Result<()> {
    let mut crsr = Cursor::new(vec![]);
    crsr.write_u32::<BigEndian>(1 + 4 * 3)?;
    crsr.write_u8(MessageId::Request.into())?;
    crsr.write_u32::<BigEndian>(index)?;
    crsr.write_u32::<BigEndian>(num_bytes_requested)?;
    crsr.write_u32::<BigEndian>(block_size)?;
    let request_message = crsr.get_mut();
    conn.stream.write(request_message)?;
    Ok(())
}