use std::borrow::Borrow;
use std::{cmp, fs};
use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;
use anyhow::ensure;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
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


fn main() -> anyhow::Result<()> {
    // Parse torrent
    let path = "C:\\Users\\kasto\\IdeaProjects\\trusti\\src\\debian-11.2.0-amd64-netinst.iso.torrent";
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

    for &peer in &peers {
        let _ = start_worker(peer, info_hash, peer_id, &piece_hashes);
    }

    Ok(())
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

fn receive_handshake(mut stream: &TcpStream, info_hash: [u8; 20]) -> anyhow::Result<String> {
    let protocol = "BitTorrent protocol";
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut pstrlen_buf = [0u8; 1];
    stream.read(&mut pstrlen_buf)?;
    let pstrlen: usize = pstrlen_buf[0].into();
    ensure!(pstrlen != 0, "Pstrlen cannot be zero");
    let mut pstr_buf = vec![0u8; pstrlen];
    stream.read(&mut pstr_buf)?;
    let pstr = String::from_utf8(pstr_buf)?;
    ensure!(pstr == protocol, "Peer does not use BitTorrent protocol");
    let mut handshake_buf = [0u8; 48];
    stream.read(&mut handshake_buf)?;
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
    stream.read(&mut message_length_buf)?;
    let mut rdr = Cursor::new(message_length_buf);
    let message_length = rdr.read_u32::<BigEndian>().unwrap() as usize;
    if message_length == 0 {
        return Ok(Message { id: 0, payload: vec![] });
    }
    let mut message_buf = vec![0u8; message_length];
    stream.read(&mut message_buf)?;
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

fn connect_to_peer(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20]) -> anyhow::Result<Connection> {
    let mut stream = TcpStream::connect_timeout(&peer, Duration::from_secs(3))?;

    // Create handshake
    let protocol = "BitTorrent protocol";
    let handshake = create_handshake(protocol, info_hash, peer_id);

    // Send handshake
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
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

fn start_worker(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20], piece_hashes: &Vec<&[u8]>) -> anyhow::Result<()> {
    // Connect and handshake with peer
    let mut conn = connect_to_peer(peer, info_hash, peer_id)?;
    println!("Successfully completed handshake with {:?}", peer.to_string());

    // Send Interested
    let mut wtr = Cursor::new(vec![0u8; 5]);
    wtr.write_u32::<BigEndian>(4)?;
    wtr.write_u8(2);
    conn.stream.write(wtr.get_ref())?;

    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let has_piece = bitfield_contains_index(&conn.bitfield, index as isize);
        if !has_piece {
            continue;
        }
    }


    Ok(())
}