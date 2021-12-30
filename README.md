# TRustI - Torrent Rust Implementation

## Inspiration

This project was inspired by [Jesse Li's blog post](https://blog.jse.li/posts/torrent/)  where he built a torrent client
using Golang.

## Sources used

- [Jesse Li's Blog Post](https://blog.jse.li/posts/torrent/)
- [Bram Cohen's Protocol Specification from 2008](https://www.bittorrent.org/beps/bep_0003.html)
- [Kristen Widman's Blog Post](http://www.kristenwidman.com/blog/71/how-to-write-a-bittorrent-client-part-2/)

## Walkthrough

### Parse torrent file

These are structs I created to deserialize the torrent file. It's how .torrent files structure
their [bencoded](https://en.wikipedia.org/wiki/Bencode) data.

```
// torrent.rs

#[derive(Deserialize, Debug)]
pub struct Torrent {
    pub info: Info,
    pub announce: String,
}
```

```
// metainfo.rs

#[derive(Serialize, Deserialize, Debug)]
pub struct Info {
    pub name: String,
    #[serde(rename = "piece length")]
    pub piece_length: u64,
    pub pieces: ByteBuf,
    pub length: u64,
}
```

### Info hash and peer ID

Next we hash the metainfo portion of the torrent file to get the Sha-1 20 byte hash. We need this to perform our
handshake with peers.

```
// metainfo.rs

impl Info {
    pub fn hash(&self) -> anyhow::Result<[u8; 20]> {
        let serialized_info = serde_bencode::to_bytes(&self)?;
        let mut hasher = Sha1::new();
        hasher.update(serialized_info.borrow());
        Ok(hasher.digest().bytes())
    }
    ...
}
```

Here we generate 20 random bytes to represent our identity we share with peers.

```
// main.rs

let random_bytes: Vec<u8> = (0..20).map(|_| { rand::random::<u8>() }).collect();
let peer_id: [u8; 20] = <[u8; 20]>::try_from(random_bytes.borrow())?;
```

### Get peers

To find out who is serving the file we are attempting to download, we need to make a GET request to the tracker URL. We
can build the tracker URL using information provided in the torrent file.

```
// torrent.rs

impl Torrent {
    pub fn build_tracker_url(&self, info_hash: [u8; 20], peer_id: [u8; 20]) -> String {
        let announce = self.announce.as_str();
        let info_hash = encode_binary(&info_hash);
        let peer_id = encode_binary(&peer_id);
        let left = self.info.length.to_string();
        format!("{}?compact=1&downloaded=0&port=6881&uploaded=0&info_hash={}&peer_id={}&left={}", announce, info_hash.as_ref(), peer_id.as_ref(), left.as_str())
    }
}
```

A successful call to the tracker server will return a list of peers that we can attempt to make connections with.

Here's an example tracker URL:

```
http://bttracker.debian.org:6969/announce?compact=1&downloaded=0&port=6881&uploaded=0&info_hash=%28%C5Q%96%F5wS%C4%0A%CE%B6%FBXa~i%95%A7%ED%DB&peer_id=5%14%D7%BF%BFY%5B~V%10-%8CKCk%16%E5%92%87%AA&left=396361728
```

The peer list in the response body is returned as a list of 6 byte IPv4 address/port combinations. We iterate over each
chunk and convert it to a `SocketAddr`:

```
// tracker.rs

fn bytes_to_socket_addr(chunk: &[u8]) -> anyhow::Result<SocketAddr> {
    // Split chunk into ip octet and big endian port
    let (ip, port) = chunk.split_at(4);

    // Get the ip address
    let ip: [u8; 4] = ip.try_into()?;
    let ip = Ipv4Addr::from(ip);

    // Get the port
    let mut rdr = Cursor::new(port);
    let port = rdr.read_u16::<BigEndian>()?;

    // Create socket address
    Ok(SocketAddr::new(IpAddr::V4(ip), port))
}
```

### Handshake

Now we need to identify which peer we want to request file pieces from. We can do this by opening a TCP connection and
performing a handshake.

A serialized handshake is composed of the following:

- PstrLen - The 1 byte length of the protocol identifier we are using, in this case the value is 19.
- Pstr - The pstrlen-byte long protocol identifier. In this case it is "BitTorrent protocol"
- Reserve bytes - 8 reserve bytes, here they are zeroed out.
- Info Hash - The 20 byte hash of the metainfo struct we obtained earlier.
- Peer Id - The 20 byte peer id we generated earlier.

We then receive a handshake from our peer. After that we can compare handshake fields. The protocol identifiers must be
the same, and we can confirm the peer has the file we need by comparing info hashes.

### Messages

Upon completing a successful handshake, we can now start sending/receiving messages. Each message from hereon out is
structured the same:

- Length - The 4 byte big endian length of the message. The value is at least one, because not all messages have
  payloads but all have a message ID.
- ID - The 1 byte message ID.
- Payload - The (length - 1) byte-sized data to follow.

The message types are mapped to message ID's:

- 0: Choke
- 1: Unchoke
- 2: Interested
- 3: Not Interested
- 4: Have
- 5: Bitfield
- 6: Request
- 7: Piece
- 8: Cancel

A message with length 0 is interpreted as a "Keep-Alive" message intended to maintain the connection.

Here is the code to read messages from a tcp stream:

```
// message.rs

pub fn read_message(mut stream: &TcpStream) -> anyhow::Result<Message> {
    let mut message_length_buf = [0u8; 4];
    stream.read_exact(&mut message_length_buf)?;
    let mut rdr = Cursor::new(message_length_buf);
    let message_length = rdr.read_u32::<BigEndian>().unwrap() as usize;
    if message_length == 0 {
        return Ok(Message { id: 255, payload: vec![] }); // Keep-Alive
    }
    let mut message_buf = vec![0u8; message_length];
    stream.read_exact(&mut message_buf)?;
    let id = message_buf[0];
    let payload = &message_buf[1..];
    Ok(Message { id, payload: payload.to_vec() })
}
```

We will go over the important messages below.

### Bitfield

Upon completing a successful handshake, we can expect our peer to also send a bitfield.

A bitfield is a clever way to communicate which pieces of a file a peer has.

The bitfield message leverages big endian _bits_ to encode which pieces a peer has.

When parsing the torrent file, we get an array of 20 byte piece hashes in the metainfo struct. The bitfield payload has
a bit for each of the hashes in this array. A 1 bit means that a peer has the piece indexed at that bit's position in
the array.

An example would be:

```
10000101
^    ^ ^ 
```

This bitfield shows that the peer has the pieces indexed at 0, 2, and 7, but lacks the others.

The significance of the bitfield is we know exactly which pieces to request of a particular peer and to avoid needless
wasteful requests.

### Choke and Interested

Peers maintain a state on either side of the connection. Connections begin in a choked and uninterested state. In order
for data transfer to commence, both peers need to be unchoked.

We will send an unchoke message and an interested message to our peer first. Once we receive an unchoke message from the
peer, we can start making requests for pieces!

Keep in mind a peer can choke a connection at any time.

### Request and Piece

Now that we can make requests, we can start iterating over our piece hash collection and ask our peer for pieces. As we
iterate, we first perform a cross-check on the bitfield we received from the peer and only request the ones they have.

Assuming a peer has the piece, they will respond with a Piece message containing a fragment of the piece.

Once we have downloaded a piece, we will want to verify its integrity by comparing the sha-1 hash of it with the
corresponding piece hash we retrieved from the .torrent file.

### Blocks

Pieces can be quite large, and their size is contained in the metainfo file. For this reason, we ask for fragments or "
blocks" of pieces in our Request message.

Traditionally we will limit the request block sizes to 16,384 bytes. We will make requests for blocks until we have
downloaded the whole piece. It is recommended from the white paper to pipeline our requests to improve throughput. We
will limit our number of active pipelined requests to 5 to not risk getting choked.

```
// connection.rs

impl Connection {
    ...
    pub fn download_piece(&mut self, piece_request: PieceRequest) -> anyhow::Result<DownloadedPiece> {
        ...
        let mut num_bytes_requested = 0u32;
        let mut num_bytes_downloaded = 0usize;
        let mut num_pipelined_requests = 0u8;
        ...
        while num_bytes_downloaded < piece_request.size {
            ...
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
            ...
    }
}
```

### Using threading and channels

Iterating over each peer sequentially would take a long time, especially for very large files. We will leverage
threading to handle each peer separately. The `crossbeam_channel` rust crate can be leveraged to queue up piece requests
and collect downloaded pieces.

```
// main.rs

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

...

// Start threads

...

// Collect downloaded pieces
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
```

Once a piece has been downloaded, we can write it to a file immediately using a seek operation to make sure the file
pointer is in the correct spot.

```
...
Downloaded 99.60%
Downloaded 99.67%
Downloaded 99.74%
Downloaded 99.80%
Downloaded 99.87%
Downloaded 99.93%
Downloaded 100.00%
Completed download.
```

And we're done!

## Instructions

### Torrent file

I have hardcoded the `.torrent` file for this project. You can download it
here: https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/debian-11.2.0-amd64-netinst.iso.torrent

Make sure you move it to the root of the project, level with the `src` directory.

### Build

`cargo build --package trusti --bin trusti`

### Run

`cargo run --package trusti --bin trusti`

### Verification

The above webpage also provides a Sha-256 hash where you can verify and compare the hash of the downloaded file.