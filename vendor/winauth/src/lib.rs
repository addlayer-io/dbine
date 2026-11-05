//! documentation references to sections etc. based on the [MS-NLMP specification]
//! (https://winprotocoldoc.blob.core.windows.net/productionwindowsarchives/MS-NLMP/[MS-NLMP].pdf)
//!
//! This provides the means to perform authentication using NTLMv2  
//! Older NTLM versions are not supported, since they are long deprecated  
//! (and known to be even more insecure as NTLMv2)
//!
//! [Channel Bindings](http://blogs.msdn.com/b/openspecification/archive/2013/03/26/ntlm-and-channel-binding-hash-aka-exteneded-protection-for-authentication.aspx)
//! are supported in this implementation and in the windows bindings
use std::borrow::Cow;
use std::convert::TryFrom;
use std::env;
use std::io::{self, Cursor, Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use bitflags::bitflags;
use byteorder::{ByteOrder, LittleEndian, ReadBytesExt, WriteBytesExt};
use rand::prelude::*;

mod hmac;
mod md4;
mod rc4;

pub mod http;

#[cfg(windows)]
pub mod windows;

use crate::hmac::{Hash, Md5};
use crate::md4::Md4;

static SIGNATURE: &'static [u8] = b"NTLMSSP\0";
const SIGNATURE_LEN: usize = 8;

/// Trait to convert a u8 to a `enum` representation
trait FromUint
where
    Self: Sized,
{
    fn from_u8(n: u8) -> Option<Self>;
    fn from_u16(n: u16) -> Option<Self>;
}

/// A generic challenge-response trait, that returns a response until authentication is established
pub trait NextBytes {
    fn next_bytes(&mut self, bytes: Option<&[u8]>) -> io::Result<Option<Vec<u8>>>;
}

macro_rules! uint_enum {
    ($( #[$gattr:meta] )* pub enum $ty:ident { $( $( #[$attr:meta] )* $variant:ident = $val:expr,)* }) => {
        uint_enum!($( #[$gattr ])* (pub) enum $ty { $( $( #[$attr] )* $variant = $val, )* });
    };
    ($( #[$gattr:meta] )* enum $ty:ident { $( $( #[$attr:meta] )* $variant:ident = $val:expr,)* }) => {
        uint_enum!($( #[$gattr ])* () enum $ty { $( $( #[$attr] )* $variant = $val, )* });
    };

    ($( #[$gattr:meta] )* ( $($vis:tt)* ) enum $ty:ident { $( $( #[$attr:meta] )* $variant:ident = $val:expr,)* }) => {
        #[derive(Debug, Copy, Clone, PartialEq)]
        $( #[$gattr] )*
        $( $vis )* enum $ty {
            $( $( #[$attr ])* $variant = $val, )*
        }

        impl ::std::convert::TryFrom<u8> for $ty {
            type Error = ();
            fn try_from(n: u8) -> ::std::result::Result<$ty, ()> {
                match n {
                    $( x if x == $ty::$variant as u8 => Ok($ty::$variant), )*
                    _ => Err(()),
                }
            }
        }

        impl ::std::convert::TryFrom<u16> for $ty {
            type Error = ();
            fn try_from(n: u16) -> ::std::result::Result<$ty, ()> {
                match n {
                    $( x if x == $ty::$variant as u16 => Ok($ty::$variant), )*
                    _ => Err(()),
                }
            }
        }
    }
}

bitflags! {
    /// as documented in 2.2.2.5 NEGOTIATE
    struct NegotiateFlags: u32 {
        /// W-bit
        /// requests 56-bit encryption
        const NTLMSSP_NEGOTIATE_56 = 1<<31;

        /// V-bit
        /// requests explicit key exchange
        const NTLMSSP_NEGOTIATE_KEY_EXCH = 1<<30;

        /// U-bit
        /// requests an 128 bit session key
        const NTLMSSP_NEGOTIATE_128 = 1<<29;
        // r1,r2,r3 (unused)

        /// T-bit
        /// requests the protocol version number
        const NTLMSSP_NEGOTIATE_VERSION = 1<<25;
        // r4

        /// S-bit
        const NTLMSSP_NEGOTIATE_TARGET_INFO = 1<<23;

        /// R-bit
        /// requests LMOW usage
        const NTLMSSP_REQUEST_NON_NT_SESSION_KEY = 1<<22;
        //r5

        /// Q-bit
        /// request identity level token
        const NTLMSSP_NEGOTIATE_IDENTIFY = 1<<20;

        /// P-bit
        /// NTLMv2 Session Security
        const NTLMSSP_NEGOTIATE_EXTENDED_SESSIONSECURITY = 1<<19;
        //r6

        /// O-bit
        const NTLMSSP_TARGET_TYPE_SERVER = 1<<17;

        /// N-bit
        const NTLMSSP_TARGET_TYPE_DOMAIN = 1<<16;

        /// M-bit
        /// requests a signature block
        const NTLMSSP_NEGOTIATE_ALWAYS_SIGN = 1<<15;
        // r7 (Negotiate Local Call, 1 << 14)

        /// L-bit
        const NTLMSSP_NEGOTIATE_OEM_WORKSTATION_SUPPLIED = 1<<13;

        /// K-bit
        const NTLMSSP_NEGOTIATE_OEM_DOMAIN_SUPPLIED = 1<<12;

        /// J-bit
        const NTLMSSP_ANONYMOUS_CONNECTION = 1<<11;
        // r8

        /// H-bit
        /// NTLMv1 Session Security, deprecated, insecure and not supported by us
        const NTLMSSP_NEGOTIATE_NTLM = 1<<9;
        // r9

        /// G-bit
        /// LM Session Security, deprecated, insecure and not supported by us
        const NTLMSSP_NEGOTIATE_LM_KEY = 1<<7;

        /// F-bit
        /// requests connectionless auth
        const NTLMSSP_NEGOTIATE_DATAGRAM = 1<<6;

        /// E-bit
        /// session key negotiation with message confidentiality
        const NTLMSSP_NEGOTIATE_SEAL = 1<<5;

        /// D-bit
        const NTLMSSP_NEGOTIATE_SIGN = 1<<4;
        // r10

        /// C-bit
        const NTLMSSP_REQUEST_TARGET = 1<<2;

        /// B-bit
        const NTLM_NEGOTIATE_OEM = 1<<1;

        /// A-bit
        const NTLMSSP_NEGOTIATE_UNICODE = 1<<0;
    }
}

/// specified in 2.2.1.1 NEGOTIATE_MESSAGE
struct NegotiateMessage {
    negotiate_flags: NegotiateFlags,
}

impl NegotiateMessage {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Cursor::new(Vec::with_capacity(40));

        bytes.write_all(SIGNATURE)?; // signature
        bytes.write_u32::<LittleEndian>(1)?; // message_type

        // make sure the negotiate flags do not contain workstation or domain flags
        let mut flags = self.negotiate_flags;
        flags.remove(NegotiateFlags::NTLMSSP_TARGET_TYPE_SERVER);
        flags.remove(NegotiateFlags::NTLMSSP_TARGET_TYPE_DOMAIN);

        bytes.write_u32::<LittleEndian>(flags.bits())?;

        // we write an empty domain and workspace name in the negotiate message, since we
        // cannot use unicode yet. and relying on OEM encoding is really really ugly.
        bytes.write_u64::<LittleEndian>(0)?;
        bytes.write_u64::<LittleEndian>(0)?;

        // we do not write version
        Ok(bytes.into_inner())
    }
}
uint_enum! {
    #[repr(u16)]
    enum AvId {
        MsvAvEOL = 0,
        MsvAvNbComputerName = 0x01,
        MsvAvNbDomainName = 0x02,
        MsvAvDnsComputerName = 0x03,
        MsvAvDnsDomainName = 0x04,
        MsvAvDnsTreeName = 0x05,
        MsvAvFlags = 0x06,
        MsvAvTimestamp = 0x07,
        MsvAvSingleHost = 0x08,
        MsvAvTargetName = 0x09,
        MsvChannelBindings = 0x0A,
    }
}

bitflags! {
    struct AvFlags: u32 {
        const AVF_ACCOUNT_AUTH_CONSTRAINED = 0x01;
        const AVF_MIC_FIELD_POPULATED = 0x02;
        const AVF_TARGET_SPN_UNTRUSTED_ORIGIN = 0x04;
    }
}

/// 2.2.2.1
#[derive(Debug)]
enum AvItem {
    NbComputerName(String),
    NbDomainName(String),
    DnsComputerName(String),
    DnsDomainName(String),
    DnsTreeName(String),
    Flags(AvFlags),
    /// [MS-DTYP] section 2.3.3: The FILETIME structure is a 64-bit value that represents the number of
    /// 100-nanosecond intervals that have elapsed since January 1, 1601, Coordinated Universal Time (UTC).
    Timestamp(u64),
    AvTargetName(String),
    /// A MD5 hash of gss_channel_bindingss_struct struct (RFC2744 3.1.1)  
    /// result of `make_sec_channel_bindings`
    ChannelBindings([u8; 16]),
}

trait EncodeExt {
    fn encode_unicode_string(&mut self, str_: &str) -> io::Result<()>;
    fn encode_av_pairs(&mut self, items: &[AvItem]) -> io::Result<()>;
}

trait DecodeExt {
    fn decode_unicode_string(&mut self, len: usize, name: &'static str) -> io::Result<String>;
    fn decode_av_pairs(&mut self) -> io::Result<Vec<AvItem>>;
}

impl<W: Write> EncodeExt for W {
    fn encode_unicode_string(&mut self, str_: &str) -> io::Result<()> {
        for chr in str_.encode_utf16() {
            self.write_u16::<LittleEndian>(chr)?;
        }
        Ok(())
    }

    fn encode_av_pairs(&mut self, items: &[AvItem]) -> io::Result<()> {
        for item in items {
            let (id, len, str_) = match *item {
                AvItem::NbComputerName(ref name) => (AvId::MsvAvNbComputerName, name.len(), name),
                AvItem::NbDomainName(ref name) => (AvId::MsvAvNbDomainName, name.len(), name),
                AvItem::DnsComputerName(ref name) => (AvId::MsvAvDnsComputerName, name.len(), name),
                AvItem::DnsDomainName(ref name) => (AvId::MsvAvDnsDomainName, name.len(), name),
                AvItem::DnsTreeName(ref name) => (AvId::MsvAvDnsTreeName, name.len(), name),
                AvItem::Flags(ref flags) => {
                    self.write_u16::<LittleEndian>(AvId::MsvAvFlags as u16)?;
                    self.write_u16::<LittleEndian>(4)?;
                    self.write_u32::<LittleEndian>(flags.bits())?;
                    continue;
                }
                AvItem::Timestamp(ts) => {
                    self.write_u16::<LittleEndian>(AvId::MsvAvTimestamp as u16)?;
                    self.write_u16::<LittleEndian>(8)?;
                    self.write_u64::<LittleEndian>(ts)?;
                    continue;
                }
                AvItem::AvTargetName(ref name) => (AvId::MsvAvTargetName, name.len(), name),
                AvItem::ChannelBindings(ref bindings) => {
                    self.write_u16::<LittleEndian>(AvId::MsvChannelBindings as u16)?;
                    self.write_u16::<LittleEndian>(bindings.len() as u16)?;
                    self.write_all(bindings)?;
                    continue;
                }
            };
            self.write_u16::<LittleEndian>(id as u16)?;
            self.write_u16::<LittleEndian>(2 * len as u16)?;
            self.encode_unicode_string(str_)?;
        }
        self.write_u16::<LittleEndian>(AvId::MsvAvEOL as u16)?;
        self.write_u16::<LittleEndian>(0)
    }
}

impl<R: Read> DecodeExt for R {
    fn decode_unicode_string(&mut self, len: usize, name: &'static str) -> io::Result<String> {
        if len % 2 > 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} unicode value has to be a multiple of 2 (utf16)", name),
            ));
        }
        let mut bytes: Vec<u16> = Vec::with_capacity(len as usize / 2);
        for _ in 0..len as usize / 2 {
            bytes.push(self.read_u16::<LittleEndian>()?);
        }
        let str_ = String::from_utf16(&bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("could not decode {} (utf16 decoding failure)", name),
            )
        })?;
        Ok(str_)
    }

    fn decode_av_pairs(&mut self) -> io::Result<Vec<AvItem>> {
        let mut pairs = Vec::with_capacity(4);
        loop {
            let id = AvId::try_from(self.read_u16::<LittleEndian>()?)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid av_id"))?;
            let len = self.read_u16::<LittleEndian>()? as usize;

            // TODO: length checks
            let item = match (id, len) {
                (AvId::MsvAvEOL, 0) => break,
                (AvId::MsvAvEOL, _) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "AV-eol length non-null",
                    ))
                }
                (AvId::MsvAvNbComputerName, _) => {
                    AvItem::NbComputerName(self.decode_unicode_string(len, "AV(nb_computer_name)")?)
                }
                (AvId::MsvAvNbDomainName, _) => {
                    AvItem::NbDomainName(self.decode_unicode_string(len, "AV(nb_domain_name)")?)
                }
                (AvId::MsvAvDnsComputerName, _) => AvItem::DnsComputerName(
                    self.decode_unicode_string(len, "AV(dns_computer_name)")?
                ),
                (AvId::MsvAvDnsDomainName, _) => {
                    AvItem::DnsDomainName(self.decode_unicode_string(len, "AV(dns_domain_name)")?)
                }
                (AvId::MsvAvDnsTreeName, _) => {
                    AvItem::DnsTreeName(self.decode_unicode_string(len, "AV(dns_tree_name)")?)
                }
                (AvId::MsvAvFlags, _) => {
                    AvItem::Flags(AvFlags::from_bits(self.read_u32::<LittleEndian>()?).ok_or(
                        io::Error::new(io::ErrorKind::InvalidData, "invalid AV flags"),
                    )?)
                }
                (AvId::MsvAvTimestamp, _) => {
                    // TODO: not sure this is correct, since FILETIME consists of dwLowDateTime/dwHighDateTime
                    // maybe we need:
                    // let low = try!(self.read_u32::<LittleEndian>());
                    // let high = try!(self.read_u32::<LittleEndian>());
                    // let ts = low as u64 | (high as u64) << 32;
                    AvItem::Timestamp(self.read_u64::<LittleEndian>()?)
                }
                (AvId::MsvAvSingleHost, _) => unimplemented!(),
                (AvId::MsvAvTargetName, _) => {
                    AvItem::AvTargetName(self.decode_unicode_string(len, "AV(target_name)")?)
                }
                (AvId::MsvChannelBindings, _) => {
                    let mut bytes = [0u8; 16];
                    self.read_exact(&mut bytes)?;
                    AvItem::ChannelBindings(bytes)
                }
            };
            pairs.push(item);
        }
        Ok(pairs)
    }
}

/// 2.2.1.2
#[derive(Debug)]
struct ChallengeMessage {
    negotiate_flags: NegotiateFlags,
    server_challenge: [u8; 8],
    target_name: Option<String>,
    av_pairs: Vec<AvItem>,
}

impl ChallengeMessage {
    fn decode<R: Read>(mut r: R) -> io::Result<Self> {
        let mut payload_offset = 48;
        let mut sig_bytes = [0u8; SIGNATURE_LEN];
        r.read_exact(&mut sig_bytes)?;
        if sig_bytes != SIGNATURE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "signature mismatch",
            ));
        }
        if r.read_u32::<LittleEndian>()? != 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid message type",
            ));
        }
        let target_name_len = r.read_u16::<LittleEndian>()?;
        if target_name_len != r.read_u16::<LittleEndian>()? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "target_name and target_name_max are not equal",
            ));
        }
        let target_name_offset = r.read_u32::<LittleEndian>()?;

        let raw_flags = r.read_u32::<LittleEndian>()?;
        let negotiate_flags = NegotiateFlags::from_bits(raw_flags).ok_or(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid negotiate flags",
        ))?;

        let mut server_challenge = [0u8; 8];
        r.read_exact(&mut server_challenge)?;
        // read reserved, reusing space
        r.read_exact(&mut sig_bytes)?;

        let target_info_len = r.read_u16::<LittleEndian>()?;
        if target_info_len != r.read_u16::<LittleEndian>()? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "target_info and target_info_max are not equal",
            ));
        }
        let target_info_offset = r.read_u32::<LittleEndian>()?;

        if negotiate_flags.contains(NegotiateFlags::NTLMSSP_NEGOTIATE_VERSION) {
            payload_offset += 8;
            // read version, only used for debug
            r.read_exact(&mut sig_bytes)?;
        }

        // start of payload parsing
        if target_name_len > 0 && target_name_offset < payload_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid target_info offset",
            ));
        }
        if target_info_len > 0 && target_info_offset < payload_offset {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid target_info offset",
            ));
        }
        // find the first payload element
        let payload_elements = match (target_name_len, target_info_len) {
            (0, 0) => 0,
            (0, _) => 1,
            (_, 0) => 1,
            (_, _) => 2,
        };

        let mut target_name: Option<String> = None;
        let mut av_pairs: Vec<AvItem> = vec![];

        for _ in 0..payload_elements {
            let parse_target_name = (target_info_offset > target_name_offset)
                && target_name.is_none()
                || !av_pairs.is_empty();
            let parse_av_pairs = (target_info_offset < target_name_offset) && av_pairs.is_empty()
                || target_name.is_some();

            if parse_target_name {
                let skip_bytes = target_name_offset - payload_offset;
                r.read_exact(&mut vec![0u8; skip_bytes as usize])?;
                target_name = Some(
                    r.decode_unicode_string(target_name_len as usize, "target_name")?
                );
                payload_offset += target_name_len as u32 + skip_bytes as u32;
            } else if parse_av_pairs {
                let skip_bytes = target_info_offset - payload_offset;
                r.read_exact(&mut vec![0u8; skip_bytes as usize])?;
                av_pairs = r.decode_av_pairs()?;
                payload_offset += target_info_len as u32 + skip_bytes as u32;
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid payload data",
                ));
            }
        }

        let ret = ChallengeMessage {
            negotiate_flags: negotiate_flags,
            server_challenge: server_challenge,
            target_name: target_name,
            av_pairs: av_pairs,
        };
        Ok(ret)
    }
}

/// 2.2.1.3
#[derive(Debug)]
struct AuthenticateMessage<'a> {
    /// The name of the (local) workstation (running this code)
    /// used by the server to determine if it can perform local auth.
    workstation: Option<Cow<'a, str>>,
    /// The domain name, if applicable, for the specified login user
    domain: Option<Cow<'a, str>>,
    user: Cow<'a, str>,
    nt_challenge_response: &'a [u8],
    encrypted_random_session_key: &'a [u8],
    exported_session_key: &'a [u8],
    /// concat of NEGOTIATE_MESSAGE, CHALLENGE_MESSAGE, AUTHENTICATE_MESSAGE
    mic_content: Option<&'a [u8]>,
    /// for connectionless connections only
    negotiate_flags: NegotiateFlags,
}

fn ntowfv2(user: &str, password: &str, domain: &str) -> Vec<u8> {
    let mut pwbuf: Vec<u8> = Vec::with_capacity(2 * password.len());
    let mut buf = [0u8; 2];
    for chr in password.encode_utf16() {
        LittleEndian::write_u16(&mut buf, chr);
        pwbuf.extend_from_slice(&buf);
    }

    let password_md4_hash = Md4::hash(&pwbuf);

    let message = {
        let mut message = user.to_uppercase();
        message.push_str(domain);
        let mut msg_buf = Vec::with_capacity(2 * message.len());
        for chr in message.encode_utf16() {
            LittleEndian::write_u16(&mut buf, chr);
            msg_buf.extend_from_slice(&buf);
        }
        msg_buf
    };

    Md5::hmac(&password_md4_hash, &message)
}

impl<'a> AuthenticateMessage<'a> {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut payload_offset: u64 = 88;

        let mut bytes = Cursor::new(Vec::with_capacity(256));
        bytes.write_all(SIGNATURE)?; // signature
        bytes.write_u32::<LittleEndian>(3)?; // message_type

        // LmChallengeResponse, deprecated so not supported by us (send Z(24) as the DOC says)
        bytes.write_u16::<LittleEndian>(24)?;
        bytes.write_u16::<LittleEndian>(24)?;
        bytes.write_u32::<LittleEndian>(payload_offset as u32)?;
        payload_offset += 24;

        // write NtChallengeResponse (2.2.2.8)
        {
            let len = self.nt_challenge_response.len() as u16;
            bytes.write_u16::<LittleEndian>(len)?;
            bytes.write_u16::<LittleEndian>(len)?;
            bytes.write_u32::<LittleEndian>(payload_offset as u32)?;

            let bak = bytes.position();
            bytes.set_position(payload_offset);
            bytes.write_all(&self.nt_challenge_response)?;
            payload_offset = bytes.position();
            bytes.set_position(bak);
        }

        // write domain and workstation name and user name
        let workstation = self.workstation.as_ref().map(|x| x.as_ref()).unwrap_or("");
        let domain = self.domain.as_ref().map(|x| x.as_ref()).unwrap_or("");

        for str_ in &[domain, self.user.as_ref(), workstation] {
            let field_len = 2 * str_.len();
            bytes.write_u16::<LittleEndian>(field_len as u16)?;
            bytes.write_u16::<LittleEndian>(field_len as u16)?;
            bytes.write_u32::<LittleEndian>(payload_offset as u32)?;

            let bak = bytes.position();
            bytes.set_position(payload_offset);
            assert_eq!(payload_offset % 2, 0);
            // this only works for "unicode" (ucs-2) encoding but using anything else is suicide
            for chr in str_.encode_utf16() {
                bytes.write_u16::<LittleEndian>(chr)?;
            }
            payload_offset = bytes.position();
            bytes.set_position(bak);
        }

        // write EncryptedRandomSessionKey (NTLMSSP_NEGOTIATE_KEY_EXCH)
        bytes.write_u16::<LittleEndian>(self.encrypted_random_session_key.len() as u16)?;
        bytes.write_u16::<LittleEndian>(self.encrypted_random_session_key.len() as u16)?;
        bytes.write_u32::<LittleEndian>(payload_offset as u32)?;
        let bak = bytes.position();
        bytes.set_position(payload_offset);
        bytes.write_all(&self.encrypted_random_session_key)?;
        bytes.set_position(bak);

        // write negotiate flags
        bytes.write_u32::<LittleEndian>(self.negotiate_flags.bits())?;

        bytes.write_u64::<LittleEndian>(0)?;
        assert_eq!(bytes.position(), 72);

        // WTF Microsoft: MIC is only checked when the flags we sent for THIS authentication message include SIGN/SEAL/ALWAYS_SIGN
        // MIC := HMAC_MD5(ExportedSessionKey, ConcatenationOf(NEGOTIATE_MESSAGE, CHALLENGE_MESSAGE, AUTHENTICATE_MESSAGE))
        if let Some(ref mic_content) = self.mic_content {
            let mut all_content = mic_content.to_vec();
            all_content.extend_from_slice(bytes.get_ref());
            let mic = Md5::hmac(&self.exported_session_key, &all_content);
            bytes.write_all(&mic)?;
        } else {
            bytes.write_all(&[0; 16])?;
        }

        Ok(bytes.into_inner())
    }
}

/// The encoded items of an ntlmv2 response
struct EncodedNtlmv2Response {
    encrypted_random_session_key: Vec<u8>,
    exported_session_key: [u8; 16],
    response: Vec<u8>,
}

/// A builder for an ntlmv2 response (2.2.2.8)
struct Ntlmv2Response<'a> {
    user: Cow<'a, str>,
    password: Cow<'a, str>,
    domain: Cow<'a, str>,
    timestamp: Option<u64>,
    server_challenge: &'a [u8],
    av_pairs: &'a [AvItem],
}

impl<'a> Ntlmv2Response<'a> {
    fn encode_ntlm2_client_challenge<W: Write>(&self, mut w: W) -> io::Result<()> {
        w.write_u8(1)?; // respType
        w.write_u8(1)?; // hiRespType
        w.write_u16::<LittleEndian>(0)?; //reserved1
        w.write_u32::<LittleEndian>(0)?; //reserved2
        let nano_seconds = if let Some(x) = self.timestamp {
            x
        } else {
            // nanoseconds since mindnight Jan. 1, 1601 (UTC) / 100
            let delta_time = 116444736000000000u64;
            let unix_time_delta = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "could not convert current systemtime",
                    ))
                }
                Ok(x) => x,
            };
            delta_time
                + (unix_time_delta.as_secs() as u64 * (1e9 as u64 / 100))
                + (unix_time_delta.subsec_nanos() as u64 / 100)
        };
        w.write_u64::<LittleEndian>(nano_seconds)?;
        let mut client_challenge = [0u8; 8];
        thread_rng().fill_bytes(&mut client_challenge);
        w.write_all(&client_challenge)?;
        w.write_u32::<LittleEndian>(0)?; //reserved3
        w.encode_av_pairs(&self.av_pairs)?;

        Ok(())
    }

    /// 2.2.2.8
    fn encode(&self) -> io::Result<EncodedNtlmv2Response> {
        // EncryptedRandomSessionKey (we only support NTLMv2 in the securest configuration):
        let mut temp = vec![];
        self.encode_ntlm2_client_challenge(&mut temp)?;
        let response_key_nt = ntowfv2(&self.user, &self.password, &self.domain);

        let nt_proof_str = {
            let mut tmp = self.server_challenge.to_vec();
            tmp.extend_from_slice(&temp);
            Md5::hmac(&response_key_nt, &tmp)
        };
        // SessionBaseKey / KeyExchangeKey
        let session_base_key = Md5::hmac(&response_key_nt, &nt_proof_str);
        // ExportedSessionKey := NONCE(16)
        let mut exported_session_key = [0u8; 16];
        thread_rng().fill_bytes(&mut exported_session_key);
        let encrypted_random_session_key = rc4::rc4(&session_base_key, &exported_session_key);

        let mut ntlmv2_response = nt_proof_str;
        ntlmv2_response.extend(temp);

        let ret = EncodedNtlmv2Response {
            exported_session_key: exported_session_key,
            encrypted_random_session_key: encrypted_random_session_key,
            response: ntlmv2_response,
        };
        Ok(ret)
    }
}

/// Builder for `NtlmV2Client` which provides configuration for it
pub struct NtlmV2ClientBuilder<'a> {
    target_spn: Option<Cow<'a, str>>,
    channel_bindings: Option<[u8; 16]>,
}

impl<'a> NtlmV2ClientBuilder<'a> {
    pub fn new() -> NtlmV2ClientBuilder<'a> {
        NtlmV2ClientBuilder {
            target_spn: None,
            channel_bindings: None,
        }
    }

    /// Set a target SPN. This requires a client to specify that it intends to identify against this SPN.  
    /// This limits replay attacks against the same server/service, since the SPN has to match.
    pub fn target_spn<S: Into<Cow<'a, str>>>(mut self, spn: S) -> NtlmV2ClientBuilder<'a> {
        self.target_spn = Some(spn.into());
        self
    }

    /// Set a channel binding. This limits client requests to the same channel.  
    /// This means e.g. that the authentication can only be successful over the same TLS connection.
    pub fn channel_bindings(mut self, binding: &[u8]) -> NtlmV2ClientBuilder<'a> {
        let mut binding_hash = [0u8; 16];
        let data = make_sec_channel_bindings(binding, true);
        binding_hash.copy_from_slice(&Md5::hash(&data));
        self.channel_bindings = Some(binding_hash);
        self
    }

    /// Build the resulting `NTLMv2Client`.
    ///
    /// # Warning
    /// This may be vulnerable to replay-attacks since it doesn't bind to any SPN  
    /// (would be accepted by ANY server) and to any channel  
    /// (can be relayed in general and isn't limited to the current connection)  
    /// So make sure to use `target_spn`/`channel_bindings`
    pub fn build<D, U, P>(self, domain: Option<D>, user: U, password: P) -> NtlmV2Client<'a>
    where
        D: Into<Cow<'a, str>>,
        U: Into<Cow<'a, str>>,
        P: Into<Cow<'a, str>>,
    {
        NtlmV2Client {
            target_spn: self.target_spn,
            channel_bindings: self.channel_bindings,
            ..NtlmV2Client::new(domain, user, password)
        }
    }
}

enum NtlmV2ClientState {
    Initial,
    Negotiated,
    Authenticated,
}

/// An authentication client to authenticate against a server (outbound)
pub struct NtlmV2Client<'a> {
    state: NtlmV2ClientState,
    workstation: Option<Cow<'a, str>>,
    domain: Option<Cow<'a, str>>,
    user: Cow<'a, str>,
    password: Cow<'a, str>,
    /// only allow authentication against this SPN (little protection)
    target_spn: Option<Cow<'a, str>>,
    /// MD5 hash of gss_channel_bindings_struct
    channel_bindings: Option<[u8; 16]>,
    all_bytes: Vec<u8>,
}

/// gss_channel_bindings_struct

///
/// # Really MS.
/// WinAPI uses SEC_CHANNEL_BINDINGS: https://msdn.microsoft.com/en-us/library/windows/desktop/dd919963(v=vs.85).aspx
///
/// For hashing the layout of gss_channel_bindings_struct (which is different to SEC_CHANNEL_BINDINGS) is used, as hinted here:
/// https://blogs.msdn.microsoft.com/openspecification/2013/03/26/ntlm-and-channel-binding-hash-aka-extended-protection-for-authentication/
///
/// 00 00 00 00 //initiator_addtype
/// 00 00 00 00 //initiator_address length
/// 00 00 00 00 //acceptor_addrtype
/// 00 00 00 00 //acceptor_address length
/// 35 00 00 00 //application_data length (53 bytes)
/// 74 6c 73 2d //application data, as calculated above
fn make_sec_channel_bindings(data: &[u8], hash: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(data.len() + 32);
    if hash {
        buf.extend_from_slice(&[0u8; 16])
    } else {
        buf.extend_from_slice(&[0u8; 24])
    };
    buf.write_u32::<LittleEndian>(data.len() as u32).unwrap();
    if !hash {
        buf.write_u32::<LittleEndian>(32).unwrap();
        assert_eq!(buf.len(), 32);
    }
    buf.extend_from_slice(&data);
    buf
}

impl<'a> NtlmV2Client<'a> {
    /// Construct a new authentication client that attempts to authenticate against target
    /// with the given user and password
    fn new<D, U, P>(domain: Option<D>, user: U, password: P) -> NtlmV2Client<'a>
    where
        D: Into<Cow<'a, str>>,
        U: Into<Cow<'a, str>>,
        P: Into<Cow<'a, str>>,
    {
        let workstation = match env::var("COMPUTERNAME") {
            Ok(x) => x.into(),
            Err(_) => "RUSTY_NTLM_CLIENT".into(),
        };
        NtlmV2Client {
            state: NtlmV2ClientState::Initial,
            workstation: Some(workstation),
            domain: domain.map(|x| x.into()),
            user: user.into(),
            password: password.into(),
            target_spn: None,
            channel_bindings: None,
            all_bytes: vec![],
        }
    }
}

impl<'a> NextBytes for NtlmV2Client<'a> {
    /// This returns the next bytes which have to be sent to the server.  
    /// The authentication is complete, when this returns `None`
    fn next_bytes(&mut self, bytes: Option<&[u8]>) -> io::Result<Option<Vec<u8>>> {
        let needed_flags = NegotiateFlags::NTLMSSP_NEGOTIATE_TARGET_INFO
            | NegotiateFlags::NTLMSSP_NEGOTIATE_128
            | NegotiateFlags::NTLMSSP_NEGOTIATE_KEY_EXCH
            | NegotiateFlags::NTLMSSP_NEGOTIATE_EXTENDED_SESSIONSECURITY
            | NegotiateFlags::NTLMSSP_NEGOTIATE_ALWAYS_SIGN
            | NegotiateFlags::NTLMSSP_NEGOTIATE_SEAL
            | NegotiateFlags::NTLMSSP_NEGOTIATE_UNICODE;
        match self.state {
            NtlmV2ClientState::Initial => {
                if bytes.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bytes given for initial ntlm client state (expected none)",
                    ));
                }
                let negotiate_msg = NegotiateMessage {
                    negotiate_flags: needed_flags,
                };
                let ret = negotiate_msg.encode()?;
                self.all_bytes.extend_from_slice(&ret);
                self.state = NtlmV2ClientState::Negotiated;
                Ok(Some(ret))
            }
            NtlmV2ClientState::Negotiated => {
                let bytes = if let Some(bytes) = bytes {
                    bytes
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bytes given for initial ntlm client state (expected none)",
                    ));
                };
                self.all_bytes.extend_from_slice(bytes.as_ref());
                let mut challenge_msg = ChallengeMessage::decode(bytes.as_ref())?;

                // check if the challenge contains the flags we previously requested
                if !challenge_msg.negotiate_flags.contains(needed_flags) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected requested flags",
                    ));
                }

                // set MIC bit in flags and channel binding (3.1.5.1.2)
                let mut has_flags = false;
                for item in &mut challenge_msg.av_pairs {
                    if let AvItem::Flags(ref mut flags) = *item {
                        flags.insert(AvFlags::AVF_MIC_FIELD_POPULATED);
                        has_flags = true;
                    }
                    if let AvItem::AvTargetName(_) = *item {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "unexpected targetname in challenge avitems",
                        ));
                    }
                }
                if !has_flags {
                    challenge_msg
                        .av_pairs
                        .push(AvItem::Flags(AvFlags::AVF_MIC_FIELD_POPULATED));
                }
                if let Some(channel_bindings) = self.channel_bindings {
                    challenge_msg
                        .av_pairs
                        .push(AvItem::ChannelBindings(channel_bindings));
                }
                if let Some(ref target_spn) = self.target_spn {
                    challenge_msg
                        .av_pairs
                        .push(AvItem::AvTargetName(target_spn.as_ref().into()));
                }

                // extract timestamp
                let timestamp = if !challenge_msg.av_pairs.is_empty() {
                    let (mut name, mut timestamp) = (false, None);
                    for item in &challenge_msg.av_pairs {
                        match *item {
                            AvItem::DnsComputerName(_) => name = true,
                            AvItem::Timestamp(ts) => timestamp = Some(ts),
                            _ => (),
                        }
                    }
                    if !name {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "expected DNSName for authenticate!",
                        ));
                    }
                    timestamp
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "expected AVpairs for authenticate!",
                    ));
                };

                // 3.1.5.1.2 Client Receives a CHALLENGE_MESSAGE
                let domain = match self.domain {
                    Some(ref domain) => domain,
                    None => "",
                };
                let ntlmv2_response = {
                    let ntlmv2_response = Ntlmv2Response {
                        user: self.user.as_ref().into(),
                        password: self.password.as_ref().into(),
                        domain: domain.into(),
                        timestamp: timestamp,
                        av_pairs: &challenge_msg.av_pairs,
                        server_challenge: &challenge_msg.server_challenge,
                    };
                    ntlmv2_response.encode()?
                };
                let auth_msg = AuthenticateMessage {
                    domain: self.domain.as_ref().map(|x| x.as_ref().into()),
                    workstation: self.workstation.as_ref().map(|x| x.as_ref().into()),
                    user: self.user.as_ref().into(),
                    nt_challenge_response: &ntlmv2_response.response,
                    exported_session_key: &ntlmv2_response.exported_session_key,
                    encrypted_random_session_key: &ntlmv2_response.encrypted_random_session_key,
                    negotiate_flags: needed_flags,
                    mic_content: Some(self.all_bytes.as_ref()),
                };
                let bytes = auth_msg.encode()?;
                self.state = NtlmV2ClientState::Authenticated;
                Ok(Some(bytes))
            }
            NtlmV2ClientState::Authenticated => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ntowfv2;
    use super::NtlmV2ClientBuilder;

    #[test]
    fn test_ntowfv2() {
        assert_eq!(
            ntowfv2("User", "Password", "Domain"),
            vec![
                0x0c, 0x86, 0x8a, 0x40, 0x3b, 0xfd, 0x7a, 0x93, 0xa3, 0x00, 0x1e, 0xf2, 0x2e, 0xf0,
                0x2e, 0x3f
            ]
        );
    }

    #[test]
    fn test_md5_gss_channel_bindings_struct() {
        let bindings = b"\x74\x6c\x73\x2d\x73\x65\x72\x76\x65\x72\x2d\x65\x6e\x64\x2d\x70\x6f\x69\x6e\x74\x3a\xea\x05\xfe\xfe\xcc\x6b\x0b\xd5\x71\xdb\xbc\x5b\xaa\x3e\xd4\x53\x86\xd0\x44\x68\x35\xf7\xb7\x4c\x85\x62\x1b\x99\x83\x47\x5f\x95";
        let expect = b"\x65\x86\xE9\x9D\x81\xC2\xFC\x98\x4E\x47\x17\x2F\xD4\xDD\x03\x10";

        let builder = NtlmV2ClientBuilder::new();
        assert_eq!(
            &builder.channel_bindings(bindings).channel_bindings.unwrap() as &[u8],
            expect
        );
    }
}
