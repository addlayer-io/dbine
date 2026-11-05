use md5;
///! lightweight hmac implementation
use std::borrow::Cow;
use std::cmp;
use std::iter;

pub trait Hash {
    fn hash(bytes: &[u8]) -> Vec<u8>;
    fn block_size() -> usize;
    fn hash_size() -> usize;

    fn hmac(key: &[u8], message: &[u8]) -> Vec<u8> {
        let mut key = Cow::Borrowed(key);

        // short keys which are too long for the block size
        if key.len() > Self::block_size() {
            key = Cow::Owned(Self::hash(key.as_ref()));
        }

        // zero pad keys that are smaller than the block size
        if key.len() < Self::block_size() {
            key = Cow::Owned(
                key.iter()
                    .cloned()
                    .chain(iter::repeat(0u8).take(Self::block_size() - key.len()))
                    .collect(),
            );
        }

        let o_key = 0x5c;
        let i_key = 0x36;

        let tmp_size = key.len() + Self::hash_size();

        // build o_key_pad and append concat(i_key_pad, message) after it
        let mut output = Vec::with_capacity(cmp::max(2 * key.len() + message.len(), tmp_size));
        for i in key.as_ref() {
            output.push(i ^ o_key);
        }
        for i in key.as_ref() {
            output.push(i ^ i_key);
        }
        output.extend_from_slice(message);

        // calculate hash(concat(i_key_pad, message))
        {
            let i_part = Self::hash(&output[key.len()..]);
            // new size to store hash(concat(i_key_pad, message))
            output.truncate(tmp_size);
            output[key.len()..].clone_from_slice(&i_part);
        }

        Self::hash(&output)
    }
}

pub struct Md5;

impl Hash for Md5 {
    #[inline]
    fn hash(bytes: &[u8]) -> Vec<u8> {
        md5::compute(bytes).to_vec()
    }

    #[inline]
    fn block_size() -> usize {
        64
    }

    #[inline]
    fn hash_size() -> usize {
        16
    }
}

#[cfg(test)]
mod tests {
    use super::{Hash, Md5};

    #[test]
    fn test_hmac_empty_md5() {
        assert_eq!(
            Md5::hmac(b"", b""),
            vec![
                0x74, 0xe6, 0xf7, 0x29, 0x8a, 0x9c, 0x2d, 0x16, 0x89, 0x35, 0xf5, 0x8c, 0x00, 0x1b,
                0xad, 0x88
            ]
        );
    }

    #[test]
    fn test_hmac_md5() {
        assert_eq!(
            Md5::hmac(b"key", b"The quick brown fox jumps over the lazy dog"),
            vec![
                0x80, 0x07, 0x07, 0x13, 0x46, 0x3e, 0x77, 0x49, 0xb9, 0x0c, 0x2d, 0xc2, 0x49, 0x11,
                0xe2, 0x75
            ]
        );
    }
}
