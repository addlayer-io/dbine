///! quick lightweight RC4 implementation

pub fn rc4(key: &[u8], message: &[u8]) -> Vec<u8> {
    // key scheduling
    let mut s = [0u8; 256];
    for i in 0..256 {
        s[i] = i as u8;
    }
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + s[i] as usize + key[i % key.len()] as usize) % 256;
        s.swap(i, j);
    }

    // PRGA
    let mut output = Vec::with_capacity(message.len());
    let mut i = 0usize;
    let mut j = 0usize;
    while output.capacity() > output.len() {
        i = (i + 1) % 256;
        j = (j + s[i] as usize) % 256;
        s.swap(i, j);
        let idx_k = (s[i] as usize + s[j] as usize) % 256;
        let k = s[idx_k as usize];
        let idx_msg = output.len();
        output.push(k ^ message[idx_msg]);
    }

    output
}

#[cfg(test)]
mod tests {
    use super::rc4;

    #[test]
    fn test_rc4() {
        assert_eq!(rc4(b"Wiki", b"pedia"), vec![0x10, 0x21, 0xBF, 0x04, 0x20]);
    }
}
