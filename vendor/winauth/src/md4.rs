///! lightweight MD4 implementation
use crate::hmac::Hash;

/// derived from the RSA Data Security, Inc. MD4 Message-Digest Algorithm (RFC1320)
/// https://tools.ietf.org/html/rfc1320
pub struct Md4 {
    state: [u32; 4],
    count: [u32; 2],
    buffer: [u8; 64],
}

const S11: u32 = 3;
const S12: u32 = 7;
const S13: u32 = 11;
const S14: u32 = 19;
const S21: u32 = 3;
const S22: u32 = 5;
const S23: u32 = 9;
const S24: u32 = 13;
const S31: u32 = 3;
const S32: u32 = 9;
const S33: u32 = 11;
const S34: u32 = 15;

static PADDING: &'static [u8; 64] = &[
    0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0,
];

impl Hash for Md4 {
    #[inline]
    fn hash(bytes: &[u8]) -> Vec<u8> {
        let mut md4 = Md4::new();
        md4.update(&bytes);
        let ret = md4.finish();
        ret.to_vec()
    }

    fn block_size() -> usize {
        16
    }

    fn hash_size() -> usize {
        16
    }
}

fn encode(output: &mut [u8], input: &[u32]) {
    debug_assert_eq!(output.len() % 4, 0);
    for i in 0..input.len() {
        let idx = 4 * i;
        output[idx] = (input[i] & 0xff) as u8;
        output[idx + 1] = ((input[i] >> 8) & 0xff) as u8;
        output[idx + 2] = ((input[i] >> 16) & 0xff) as u8;
        output[idx + 3] = ((input[i] >> 24) & 0xff) as u8;
    }
}

fn decode(output: &mut [u32], bytes: &[u8]) {
    debug_assert_eq!(bytes.len() % 4, 0);
    for i in 0..(bytes.len() / 4) {
        let idx = 4 * i;
        output[i] = bytes[idx] as u32
            | ((bytes[idx + 1] as u32) << 8)
            | ((bytes[idx + 2] as u32) << 16)
            | ((bytes[idx + 3] as u32) << 24);
    }
}

#[allow(non_snake_case)]
#[inline]
fn F(x: u32, y: u32, z: u32) -> u32 {
    (((x) & (y)) | ((!x) & (z)))
}

#[allow(non_snake_case)]
#[inline]
fn G(x: u32, y: u32, z: u32) -> u32 {
    (((x) & (y)) | ((x) & (z)) | ((y) & (z)))
}

#[allow(non_snake_case)]
#[inline]
fn H(x: u32, y: u32, z: u32) -> u32 {
    ((x) ^ (y) ^ (z))
}

macro_rules! FF {
    ($a:expr, $b:expr, $c: expr, $d: expr, $x:expr, $s:expr) => {
        $a = $a.wrapping_add(F($b, $c, $d).wrapping_add($x));
        $a = $a.rotate_left($s);
    };
}

macro_rules! GG {
    ($a:expr, $b:expr, $c: expr, $d: expr, $x:expr, $s:expr) => {
        $a = $a.wrapping_add(G($b, $c, $d).wrapping_add($x).wrapping_add(0x5a827999u32));
        $a = $a.rotate_left($s);
    };
}

macro_rules! HH {
    ($a:expr, $b:expr, $c: expr, $d: expr, $x:expr, $s:expr) => {
        $a = $a.wrapping_add(H($b, $c, $d).wrapping_add($x).wrapping_add(0x6ed9eba1u32));
        $a = $a.rotate_left($s);
    };
}

impl Md4 {
    /// begins a MD4 operation
    fn new() -> Md4 {
        Md4 {
            count: [0u32; 2],
            // magic initialization constants
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            buffer: [0u8; 64],
        }
    }

    fn update(&mut self, input: &[u8]) {
        // compute number of bytes mod 64
        let mut index = ((self.count[0] >> 3) & 0x3f) as usize;

        // update number of bits
        self.count[0] += (input.len() << 3) as u32;
        if self.count[0] < (input.len() << 3) as u32 {
            self.count[1] += 1;
        }
        self.count[1] += input.len() as u32 >> 29;

        let part_len = 64 - index;

        // transform as many times as possible
        let mut i = part_len;
        if input.len() >= part_len {
            self.buffer[index..(index + part_len)].copy_from_slice(&input[..part_len]);
            self.transform(None);

            while i + 63 < input.len() {
                self.transform(Some(&input[i..]));
                i += 64;
            }
            index = 0;
        } else {
            i = 0;
        }

        // buffer the remaining input
        let count = input.len() - i;
        self.buffer[index..index + count].copy_from_slice(&input[i..i + count]);
    }

    fn transform(&mut self, input: Option<&[u8]>) {
        let input = match input {
            None => &self.buffer,
            Some(input) => input,
        };
        debug_assert_eq!(input.len(), 64);
        let mut x = [0u32; 16];
        decode(&mut x, &input[..64]);

        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];

        /* Round 1 */
        FF!(a, b, c, d, x[0], S11); /* 1 */
        FF!(d, a, b, c, x[1], S12); /* 2 */
        FF!(c, d, a, b, x[2], S13); /* 3 */
        FF!(b, c, d, a, x[3], S14); /* 4 */
        FF!(a, b, c, d, x[4], S11); /* 5 */
        FF!(d, a, b, c, x[5], S12); /* 6 */
        FF!(c, d, a, b, x[6], S13); /* 7 */
        FF!(b, c, d, a, x[7], S14); /* 8 */
        FF!(a, b, c, d, x[8], S11); /* 9 */
        FF!(d, a, b, c, x[9], S12); /* 10 */
        FF!(c, d, a, b, x[10], S13); /* 11 */
        FF!(b, c, d, a, x[11], S14); /* 12 */
        FF!(a, b, c, d, x[12], S11); /* 13 */
        FF!(d, a, b, c, x[13], S12); /* 14 */
        FF!(c, d, a, b, x[14], S13); /* 15 */
        FF!(b, c, d, a, x[15], S14); /* 16 */

        /* Round 2 */
        GG!(a, b, c, d, x[0], S21); /* 17 */
        GG!(d, a, b, c, x[4], S22); /* 18 */
        GG!(c, d, a, b, x[8], S23); /* 19 */
        GG!(b, c, d, a, x[12], S24); /* 20 */
        GG!(a, b, c, d, x[1], S21); /* 21 */
        GG!(d, a, b, c, x[5], S22); /* 22 */
        GG!(c, d, a, b, x[9], S23); /* 23 */
        GG!(b, c, d, a, x[13], S24); /* 24 */
        GG!(a, b, c, d, x[2], S21); /* 25 */
        GG!(d, a, b, c, x[6], S22); /* 26 */
        GG!(c, d, a, b, x[10], S23); /* 27 */
        GG!(b, c, d, a, x[14], S24); /* 28 */
        GG!(a, b, c, d, x[3], S21); /* 29 */
        GG!(d, a, b, c, x[7], S22); /* 30 */
        GG!(c, d, a, b, x[11], S23); /* 31 */
        GG!(b, c, d, a, x[15], S24); /* 32 */

        /* Round 3 */
        HH!(a, b, c, d, x[0], S31); /* 33 */
        HH!(d, a, b, c, x[8], S32); /* 34 */
        HH!(c, d, a, b, x[4], S33); /* 35 */
        HH!(b, c, d, a, x[12], S34); /* 36 */
        HH!(a, b, c, d, x[2], S31); /* 37 */
        HH!(d, a, b, c, x[10], S32); /* 38 */
        HH!(c, d, a, b, x[6], S33); /* 39 */
        HH!(b, c, d, a, x[14], S34); /* 40 */
        HH!(a, b, c, d, x[1], S31); /* 41 */
        HH!(d, a, b, c, x[9], S32); /* 42 */
        HH!(c, d, a, b, x[5], S33); /* 43 */
        HH!(b, c, d, a, x[13], S34); /* 44 */
        HH!(a, b, c, d, x[3], S31); /* 45 */
        HH!(d, a, b, c, x[11], S32); /* 46 */
        HH!(c, d, a, b, x[7], S33); /* 47 */
        HH!(b, c, d, a, x[15], S34); /* 48 */

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }

    fn finish(mut self) -> [u8; 16] {
        let mut bits = [0u8; 8];

        // save number of bits
        encode(&mut bits, &self.count);

        // pad out to 56 mod 64
        let index = (self.count[0] >> 3) & 0x3f;
        let pad_len = if index < 56 { 56 - index } else { 120 - index } as usize;
        self.update(&PADDING[..pad_len]);

        // append length (before padding)
        self.update(&bits);

        // store state
        let mut digest = [0u8; 16];
        encode(&mut digest, &self.state);
        digest
    }
}

#[cfg(test)]
mod tests {
    use super::Md4;
    use crate::hmac::Hash;

    #[test]
    fn test_md4_fox() {
        assert_eq!(
            Md4::hash(b"The quick brown fox jumps over the lazy dog"),
            vec![
                0x1b, 0xee, 0x69, 0xa4, 0x6b, 0xa8, 0x11, 0x18, 0x5c, 0x19, 0x47, 0x62, 0xab, 0xae,
                0xae, 0x90
            ]
        );
    }

    #[test]
    fn test_md4_empty() {
        assert_eq!(
            Md4::hash(b""),
            vec![
                0x31, 0xd6, 0xcf, 0xe0, 0xd1, 0x6a, 0xe9, 0x31, 0xb7, 0x3c, 0x59, 0xd7, 0xe0, 0xc0,
                0x89, 0xc0
            ]
        );
    }
}
