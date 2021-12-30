pub struct DownloadedPiece {
    pub buf: Vec<u8>,
    pub index: usize,
}

impl DownloadedPiece {
    pub fn get_start(&self) -> usize {
        return self.index * self.buf.len();
    }
}