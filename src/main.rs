use std::{cmp, fs, thread};
use std::cmp::Ordering;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::Duration;
use byteorder::{BigEndian, ReadBytesExt};
use serde_bencode::{de, Error};
use serde_derive::{Serialize, Deserialize};
use serde_bytes::ByteBuf;
use sha1::{Sha1};
use urlencoding::{encode_binary};

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
}

impl From<&str> for Torrent {
    fn from(path: &str) -> Self {
        let contents = fs::read(path).expect("Something went wrong reading the file");
        return de::from_bytes::<Torrent>(&contents).unwrap();
    }
}

fn get_peer(chunk: &[u8]) -> SocketAddr {
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

struct Handshake {
    protocol_id_length: usize,
    protocol_id: String,
    reserved_bytes: Vec<u8>,
    info_hash: Vec<u8>,
    peer_id: Vec<u8>,
}

fn parse_handshake(buf: Vec<u8>) -> Result<Handshake, Error> {
    let protocol_id_length: usize = buf[0].into();
    if protocol_id_length == 0 {
        // TODO: throw an error here
        return Err(Error::InvalidValue(String::from("Cannot be length 0")));
    }
    let protocol_id = String::from_utf8(buf[1..(protocol_id_length + 1)].to_vec()).unwrap();
    let reserved_bytes = buf[protocol_id_length + 1..protocol_id_length + 9].to_vec();
    let info_hash = buf[protocol_id_length + 9..protocol_id_length + 29].to_vec();
    let peer_id = buf[protocol_id_length + 29..protocol_id_length + 49].to_vec();
    return Ok(Handshake {
        protocol_id_length,
        protocol_id,
        reserved_bytes,
        info_hash,
        peer_id,
    });
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

fn main() {
    // Hardcoded local filepath
    let path = "C:\\Users\\kasto\\IdeaProjects\\trusti\\src\\debian-11.2.0-amd64-netinst.iso.torrent";

    // Parse the torrent file
    let torrent = Torrent::from(path);

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
    let peers: Vec<SocketAddr> = chunks.iter().map(|chunk| get_peer(chunk)).collect();

    // Create the handshake
    let handshake = create_handshake(&info_hash, &peer_id);

    // Make the handshake
    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    for &peer_addr in &peers {
        let handshake_copy = handshake.clone();
        let handle = thread::spawn(move || {
            match TcpStream::connect_timeout(&peer_addr, Duration::from_secs(3)) {
                Err(_) => println!("Could not connect to {:?}", peer_addr.to_string()),
                Ok(mut stream) => {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
                    if let Ok(_) = stream.write(&handshake_copy) {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                        let mut read_buff = vec![0; 68];
                        match stream.read(&mut read_buff) {
                            Err(_) => println!("Could not read from socket"),
                            Ok(_) => {
                                if let Ok(received_handshake) = parse_handshake(read_buff) {
                                    let peer_info_hash: [u8; 20] = received_handshake.info_hash.try_into().unwrap();
                                    let info_hashes_equal = compare_info_hashes(&info_hash, &peer_info_hash).is_eq();
                                    println!("{:?}", info_hashes_equal);
                                }
                            }
                        }
                    }
                }
            }
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap()
    }
}
