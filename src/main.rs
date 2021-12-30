use std::borrow::Borrow;
use std::{fs, thread};
use std::cmp::Ordering;
use std::fs::File;
use std::io::{Seek, Write};
use std::net::{Shutdown, SocketAddr};
use std::time::Duration;
use anyhow::bail;
use crossbeam_channel::{Receiver, Sender};
use serde_bencode::de;
use sha1::Sha1;
use std::io::SeekFrom;
use tracker::Tracker;
use crate::connection::connect_to_peer;
use crate::downloaded_piece::DownloadedPiece;
use crate::handshake::Handshake;
use crate::message::{build_message, build_request_message, MessageId, MSG_INTERESTED, MSG_UNCHOKE};
use crate::metainfo::Info;
use crate::piece_request::PieceRequest;
use crate::torrent::Torrent;

mod piece_request;
mod handshake;
mod message;
mod connection;
mod metainfo;
mod tracker;
mod torrent;
mod downloaded_piece;
mod bitfield;

fn main() -> anyhow::Result<()> {
    // Parse torrent
    let path = "debian-11.2.0-amd64-netinst.iso.torrent";
    let contents = fs::read(path).expect("Something went wrong reading the file");
    let torrent = de::from_bytes::<Torrent>(&contents)?;

    // Info hash
    let info_hash = torrent.info.hash()?;

    // Peer id
    let random_bytes: Vec<u8> = (0..20).map(|_| { rand::random::<u8>() }).collect();
    let peer_id: [u8; 20] = <[u8; 20]>::try_from(random_bytes.borrow())?;

    // Tracker url
    let tracker_url = torrent.build_tracker_url(info_hash, peer_id);

    // Tracker
    let resp = reqwest::blocking::get(tracker_url).unwrap().bytes()?;
    let tracker = de::from_bytes::<Tracker>(&resp)?;

    // Get peers
    let peers = tracker.get_peers()?;

    // Piece hashes
    let pieces = torrent.info.pieces.as_ref();
    let piece_hashes: Vec<&[u8]> = pieces.as_ref().chunks(20).collect();

    // Channel initialization
    let (piece_request_sender, piece_request_receiver): (Sender<PieceRequest>, Receiver<PieceRequest>) = crossbeam_channel::bounded(piece_hashes.len());
    let (piece_collection_sender, piece_collection_receiver): (Sender<DownloadedPiece>, Receiver<DownloadedPiece>) = crossbeam_channel::unbounded();

    // Fill up request channel
    for (index, &piece_hash) in piece_hashes.iter().enumerate() {
        let metainfo = torrent.info.clone();
        let size = metainfo.calculate_piece_size(index);
        let piece_request = PieceRequest {
            index,
            hash: piece_hash.to_vec(),
            size,
        };
        piece_request_sender.send(piece_request)?;
    }

    // Start threads
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

    // Collect downloaded pieces
    let mut num_pieces_downloaded = 0usize;
    let mut file = File::create("debian-11.2.0-amd64-netinst.iso")?;
    while num_pieces_downloaded < piece_hashes.len() {
        let piece = piece_collection_receiver.recv()?;
        num_pieces_downloaded += 1;
        let percent_progress = (num_pieces_downloaded as f32 / piece_hashes.len() as f32) * 100f32;
        let start = piece.get_start();
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


const DOWNLOAD_TIMEOUT: u64 = 5;


fn start_worker(peer: SocketAddr, info_hash: [u8; 20], peer_id: [u8; 20], piece_request_receiver: Receiver<PieceRequest>, piece_request_sender: Sender<PieceRequest>, piece_collection_sender: Sender<DownloadedPiece>) -> anyhow::Result<()> {
    // Connect and handshake with peer
    let mut conn = connect_to_peer(peer, info_hash, peer_id)?;
    println!("Successfully completed handshake with {:?}", peer.to_string());

    // Send Unchoke
    conn.send_message(MSG_UNCHOKE)?;

    // Send Interested
    conn.send_message(MSG_INTERESTED)?;

    // Read from receiver and download each piece
    for piece_request in piece_request_receiver {

        // Check if peer has the piece
        let has_piece = bitfield::bitfield_contains_index(&conn.bitfield, piece_request.index as isize);
        if !has_piece {
            piece_request_sender.send(piece_request)?;
            continue;
        }

        // Attempt to download the piece
        conn.stream.set_read_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        conn.stream.set_write_timeout(Some(Duration::from_secs(DOWNLOAD_TIMEOUT)))?;
        match conn.download_piece(piece_request.clone()) {
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