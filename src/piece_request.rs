pub struct PieceRequest {
    pub hash: Vec<u8>,
    pub index: usize,
    pub size: usize,
}

impl Clone for PieceRequest {
    fn clone(&self) -> Self {
        let hash = self.hash.clone();
        return PieceRequest {
            hash,
            index: self.index,
            size: self.size,
        };
    }

    fn clone_from(&mut self, source: &Self) {
        self.hash = source.hash.clone();
        self.size = source.size;
        self.index = source.index;
    }
}