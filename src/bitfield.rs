pub fn bitfield_contains_index(bitfield: &Vec<u8>, index: isize) -> bool {
    let byte_index = index / 8;
    let offset = index % 8;
    if byte_index < 0 || byte_index >= bitfield.len() as isize {
        return false;
    }
    return bitfield[byte_index as usize] >> (7 - offset) & 1 != 0;
}
