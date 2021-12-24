use std::{cmp, fs, thread};
use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;
use byteorder::{BigEndian, ReadBytesExt};
use serde_bencode::{de};
use serde_derive::{Serialize, Deserialize};
use serde_bytes::ByteBuf;
use sha1::{Sha1};
use urlencoding::{encode_binary};
use std::error::Error as StdError;
use anyhow::{bail, ensure};
use crossbeam_channel as channel;
use crossbeam_channel::Receiver;
use num_enum::TryFromPrimitive;
use std::convert::TryFrom;

const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_NOT_INTERESTED: u8 = 3;
const MSG_HAVE: u8 = 4;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;
const MSG_CANCEL: u8 = 8;
const MSG_KEEP_ALIVE: u8 = 9;

#[derive(Debug, Eq, PartialEq, TryFromPrimitive)]
#[repr(u8)]
enum MessageId {
    Choke,
    UnChoke,
    Interested,
    NotInterested,
    Have,
    BitField,
    Request,
    Piece,
    Cancel,
    KeepAlive,
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

fn generate_peer_id() -> Vec<u8> {
    return (0..20).map(|_| { rand::random::<u8>() }).collect();
}

impl Info {
    fn hash(&self) -> [u8; 20] {
        let serialized_info = serde_bencode::to_bytes(&self).unwrap();
        let mut hasher = Sha1::new();
        hasher.update(serialized_info.as_slice());
        return hasher.digest().bytes();
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


fn compare_info_hashes(a: &[u8], b: &[u8]) -> cmp::Ordering {
    for (ai, bi) in a.iter().zip(b.iter()) {
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            ord => return ord
        }
    }

    a.len().cmp(&b.len())
}


fn handle_peer(peer_addr: SocketAddr, handshake_copy: Vec<u8>, info_hash: &[u8], piece_work_receiver: Receiver<PieceWork>) -> anyhow::Result<()> {
    // Connect to peer
    println!("Attempting to connect to peer: {:?}...", peer_addr.to_string());
    let mut stream = TcpStream::connect_timeout(&peer_addr, Duration::from_secs(3))?;
    println!("Successfully connected to peer {:?}!", peer_addr.to_string());

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write(&handshake_copy)?;

    // Receive handshake
    let mut pstrlen_buf = [0u8; 1];
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.read(&mut pstrlen_buf)?;

    // Get PstrLen
    let pstrlen = pstrlen_buf[0] as usize;
    if pstrlen == 0 {
        println!("Pstrlen cannot be 0");
        // TODO: Make this return an error
        return Ok(());
    }

    // Get info hash and peer id
    let mut handshake_buf = vec![0u8; pstrlen + 8 + 20 + 20];
    stream.read(&mut handshake_buf)?;
    let peer_info_hash = &handshake_buf[pstrlen + 8..pstrlen + 8 + 20];
    let _ = &handshake_buf[pstrlen + 8 + 20..];

    // Compare info hashes
    if !compare_info_hashes(&info_hash, &peer_info_hash).is_eq() {
        println!("Incompatible info hashes");
        // TODO: Make this return an error
        return Ok(());
    }

    // Get potential bitfield message length
    let mut message_length_buf = [0u8; 4];
    stream.read(&mut message_length_buf)?;
    let mut msg_length_cursor = Cursor::new(message_length_buf);
    let message_length = msg_length_cursor.read_u32::<BigEndian>().unwrap() as usize;
    if message_length == 0 {
        println!("Was not expecting a keep alive message");
        // TODO: Make this return an error
        return Ok(());
    }

    // Get bitfield message
    let mut bitfield_buf = vec![0u8; message_length];
    stream.read(&mut bitfield_buf)?;
    let message_id = bitfield_buf[0];
    if !message_id == MSG_BITFIELD {
        println!("Expected bitfield message, got {:?}", message_id);
        // TODO: Make this return an error
        return Ok(());
    }
    let bitfield = &bitfield_buf[1..];

    // Send Unchoke
    let unchoke_buf: [u8; 5] = [0, 0, 0, 1, MSG_UNCHOKE];
    stream.write(&unchoke_buf)?;

    // Send Interested
    let interested_buf: [u8; 5] = [0, 0, 0, 1, MSG_INTERESTED];
    stream.write(&interested_buf)?;

    for piece_work in piece_work_receiver {
        println!("{:?}", piece_work);
    }

    Ok(())
}

#[derive(Debug)]
struct PieceWork {
    index: usize,
    hash: Vec<u8>,
    length: usize,
}

struct PieceResult {
    index: usize,
    buf: Vec<u8>,
}

fn calculate_bounds_for_piece(index: usize, piece_length: usize, length: usize) -> (usize, usize) {
    let begin = index * piece_length;
    let mut end = begin + piece_length;
    if end > length {
        end = length
    }
    return (begin, end);
}

fn calculate_piece_size(index: usize, piece_length: u64, length: u64) -> usize {
    let (begin, end) = calculate_bounds_for_piece(index, piece_length as usize, length as usize);
    return end - begin;
}


fn original() -> Result<(), Box<dyn StdError>> {
    // Hardcoded local filepath
    let path = "C:\\Users\\kasto\\IdeaProjects\\trusti\\src\\debian-11.2.0-amd64-netinst.iso.torrent";

    // Parse the torrent file
    let torrent = Torrent::from(path);

    // Create the channels for maintaining work
    let piece_hashes = torrent.info.piece_hashes();
    let (piece_work_sender, piece_work_receiver): (crossbeam_channel::Sender<PieceWork>, crossbeam_channel::Receiver<PieceWork>) = channel::bounded(piece_hashes.len());
    let (result_sender, result_receiver): (crossbeam_channel::Sender<PieceResult>, crossbeam_channel::Receiver<PieceResult>) = channel::unbounded();

    let mut piece_work_collection: Vec<PieceWork> = Vec::new();
    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let length = calculate_piece_size(index, torrent.info.piece_length, torrent.info.length);
        let piece_work = PieceWork {
            index,
            hash: piece_hash.to_vec(),
            length,
        };
        // piece_work_sender.send(piece_work);
        piece_work_collection.push(piece_work);
    }


    // Peer Id
    let peer_id: [u8; 20] = generate_peer_id().try_into().unwrap();

    // Info hash
    let info_hash = torrent.info.hash();

    // Build the url
    let url = build_tracker_url(torrent, &info_hash, &peer_id);

    // Get tracker
    let resp = reqwest::blocking::get(url).unwrap().bytes().unwrap();
    let tracker = de::from_bytes::<Tracker>(&resp).unwrap();

    // Get peers
    let vec: Vec<u8> = tracker.peers.into_vec();
    let chunks: Vec<&[u8]> = vec.chunks(6).collect();
    let peers: Vec<SocketAddr> = chunks.iter().map(|chunk| peer_to_socket_addr(chunk)).collect();

    // Create the handshake
    let handshake = create_handshake(&info_hash, &peer_id);

    // Sequential version
    for &peer_addr in &peers {
        let handshake_copy = handshake.clone();
        match sequential_handle_peer(&peer_addr, handshake_copy, &info_hash, &piece_work_collection) {
            Ok(_) => {}
            Err(e) => println!("Failed to handle peer {:?}: {:?}", peer_addr.to_string(), e)
        }
    }


    // Make the handshake
    // let mut handles: Vec<JoinHandle<()>> = Vec::new();
    // for &peer_addr in &peers {
    //     let handshake_copy = handshake.clone();
    //     let piece_work_receiver_copy = piece_work_receiver.clone();
    //     let handle = thread::spawn(move || {
    //         let _ = handle_peer(peer_addr, handshake_copy, &info_hash, piece_work_receiver_copy);
    //     });
    //
    //     handles.push(handle);
    // }
    //
    // for handle in handles { handle.join().unwrap() }
    Ok(())
}

fn sequential_handle_peer(peer_addr: &SocketAddr, handshake: Vec<u8>, info_hash: &[u8], piece_work_collection: &Vec<PieceWork>) -> anyhow::Result<()> {
    // Connect to peer
    println!("Attempting to connect to peer: {:?}...", peer_addr.to_string());
    let mut stream = TcpStream::connect_timeout(&peer_addr, Duration::from_secs(3))?;
    println!("Successfully connected to peer {:?}!", peer_addr.to_string());

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    stream.write(&handshake)?;

    // Read PstrLen
    let mut pstrlen_buf = [0u8; 1];
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.read(&mut pstrlen_buf)?;
    let pstrlen = pstrlen_buf[0] as usize;
    ensure!(pstrlen != 0, "Pstrlen cannot be 0");

    // Get info hash and peer id
    let mut handshake_buf = vec![0u8; pstrlen + 8 + 20 + 20];
    stream.read(&mut handshake_buf)?;
    let peer_info_hash = &handshake_buf[pstrlen + 8..pstrlen + 8 + 20];
    let _ = &handshake_buf[pstrlen + 8 + 20..];

    // Compare info hashes
    ensure!(compare_info_hashes(&info_hash, &peer_info_hash).is_eq(), "Incompatible info hashes");

    // Get potential bitfield message length
    let mut message_length_buf = [0u8; 4];
    stream.read(&mut message_length_buf)?;
    let mut msg_length_cursor = Cursor::new(message_length_buf);
    let message_length = msg_length_cursor.read_u32::<BigEndian>().unwrap() as usize;
    ensure!(message_length != 0, "Was not expecting a keep alive message");

    // Get bitfield message
    let mut bitfield_buf = vec![0u8; message_length];
    stream.read(&mut bitfield_buf)?;
    let message_id = bitfield_buf[0];
    ensure!(message_id == MSG_BITFIELD, "Expected bitfield message, got {:?}", message_id);
    let peer_bitfield = &bitfield_buf[1..];

    // Send Unchoke
    let unchoke_buf: [u8; 5] = [0, 0, 0, 1, MSG_UNCHOKE];
    stream.write(&unchoke_buf)?;

    // Send Interested
    let interested_buf: [u8; 5] = [0, 0, 0, 1, MSG_INTERESTED];
    stream.write(&interested_buf)?;

    for piece_work in piece_work_collection {
        let byte_index = piece_work.index / 8;
        let offset = piece_work.index % 8;
        if byte_index < 0 || byte_index >= peer_bitfield.len() {
            continue;
        }
        if peer_bitfield[byte_index] >> (7 - offset) & 1 == 0 {
            continue;
        }
        stream.set_read_timeout(Some(Duration::from_secs(30)));
    }

    Ok(())
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

    // Iterate over peers
    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    for &peer in &peers {
        let handle = thread::spawn(move || {
            connect_to_peer(peer, info_hash, peer_id);
        });
        handles.push(handle);
    }

    for handle in handles { handle.join(); }

    Ok(())
}

fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<()> {
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
    stream.set_read_timeout(Some(Duration::from_secs(3)));
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
    ensure!(compare_info_hashes(&info_hash, peer_info_hash).is_eq(), "Mismatched info hashes");
    println!("Completed handshake with peer {:?}", seeder_peer_id);

    // Receive bitfield
    let bitfield_msg = read_message(&stream)?;
    ensure!(bitfield_msg.id == MessageId::BitField, "Was expecting a bitfield");

    // Create the connection
    let mut conn = Connection {
        choked: true,
        bitfield: bitfield_msg.payload,
        peer_id: seeder_peer_id,
    };

    // Send Unchoke
    let unchoke_buf: [u8; 5] = [0, 0, 0, 1, MSG_UNCHOKE];
    stream.write(&unchoke_buf)?;

    // Send Interested
    let interested_buf: [u8; 5] = [0, 0, 0, 1, MSG_INTERESTED];
    stream.write(&interested_buf)?;

    // Receive unchoke
    loop {
        let msg = read_message(&stream)?;
        println!("{:?} sent message: {:?}", conn.peer_id, msg.id);
        match msg.id {
            MessageId::UnChoke => { conn.choked = false; }
            MessageId::Choke => { conn.choked = true; }
            _ => {}
        }
    }


    Ok(())
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
}

struct Message {
    id: MessageId,
    payload: Vec<u8>,
}