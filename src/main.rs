use std::{fs, thread};
use std::borrow::Borrow;
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::str::FromStr;
use std::thread::JoinHandle;
use std::time::Duration;
use byteorder::{BigEndian, ReadBytesExt};
use serde_bencode::de;
use serde_derive::{Serialize, Deserialize};
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;
use serde::Serialize;
use serde_bytes::ByteBuf;
use sha1::{Sha1};
use futures::executor::block_on;
use urlencoding::{encode, encode_binary};

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
    interval: u64,
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

fn main() {
    // Parse the torrent file
    let path = "C:\\Users\\kasto\\IdeaProjects\\trusti\\src\\debian-11.2.0-amd64-netinst.iso.torrent";
    let contents = fs::read(path).expect("Something went wrong reading the file");
    let torrent = de::from_bytes::<Torrent>(&contents).unwrap();

    // Peer Id
    let peer_id = generate_peer_id();
    let peer_id_encoded = encode_binary(&peer_id).to_string();


    // Info hash
    let info_hash = torrent.info.hash();
    let info_hash_encoded = encode_binary(&info_hash).to_string();

    // Build the url
    let mut url = String::new();
    url.push_str(torrent.announce.as_str());
    url.push_str("?compact=1");
    url.push_str("&downloaded=0");
    url.push_str("&info_hash=");
    url.push_str(info_hash_encoded.as_str());
    url.push_str("&left=");
    url.push_str(torrent.info.length.to_string().as_str());
    url.push_str("&peer_id=");
    url.push_str(peer_id_encoded.as_str());
    url.push_str("&port=6881");
    url.push_str("&uploaded=0");

    println!("Tracker url: \"{}\"", url);

    // Get tracker
    let resp = reqwest::blocking::get(url).unwrap().bytes().unwrap();
    let tracker = de::from_bytes::<Tracker>(&resp).unwrap();


    // Get peers
    let vec: Vec<u8> = tracker.peers.into_vec();
    let chunks: Vec<&[u8]> = vec.chunks(6).collect();
    let mut peers: Vec<String> = Vec::new();
    let mut sock_addrs: Vec<SocketAddr> = Vec::new();

    for chunk in chunks {
        // Split chunk into ip octet and big endian port
        let (ip_octets, port) = chunk.split_at(4);

        // Get the ip address
        let ip_slice_to_array: [u8; 4] = ip_octets.try_into().unwrap();
        let ip_addr = Ipv4Addr::from(ip_slice_to_array);

        // Get the port
        let mut rdr = Cursor::new(port);
        let port_fixed = rdr.read_u16::<BigEndian>().unwrap();

        // Create socket address
        let socket_addr = SocketAddr::new(IpAddr::V4(ip_addr), port_fixed);

        // Gather into socket collection
        sock_addrs.push(socket_addr);
    }

    // Create the handshake
    let mut handshake: Vec<u8> = Vec::new();

    let protocol_id = "BitTorrent protocol";
    let protocol_id_length: u8 = protocol_id.len().try_into().unwrap();

    handshake.push(protocol_id_length);
    handshake.extend(protocol_id.bytes());
    handshake.extend([0u8; 8]);
    handshake.extend(info_hash);
    handshake.extend(peer_id);


    // Make the handshake
    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    for &sock in &sock_addrs {
        let handshake_copy = handshake.clone();
        let handle = thread::spawn(move || {
            match TcpStream::connect_timeout(&sock, Duration::from_secs(3)) {
                Err(_) => println!("Could not connect to {:?}", sock.to_string()),
                Ok(mut stream) => {
                    stream.set_write_timeout(Some(Duration::from_secs(3)));
                    if let Ok(_) = stream.write(&handshake_copy) {
                        stream.set_read_timeout(Some(Duration::from_secs(3)));
                        let mut read_buff = vec![0; 68];
                        match stream.read(&mut read_buff) {
                            Err(_) => println!("Could not read from socket"),
                            Ok(_) => println!("{:?}", read_buff)
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
