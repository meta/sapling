/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

// @lint-ignore-every SPELL

use std::fmt;
use std::fmt::Debug;
use std::fmt::Display;
use std::str::FromStr;

use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use ascii::AsciiStr;
use ascii::AsciiString;
use blake2::Blake2b;
use blake2::Blake2bMac;
use blake2::digest::Digest;
use blake2::digest::FixedOutput;
use blake2::digest::KeyInit;
use blake2::digest::Update;
use blake2::digest::typenum::U32;
use edenapi_types::Blake3 as EdenapiBlake3;
use edenapi_types::CommitId as EdenapiCommitId;
use edenapi_types::GitSha1 as EdenapiGitSha1;
use edenapi_types::Sha1 as EdenapiSha1;
use edenapi_types::Sha256 as EdenapiSha256;
use faster_hex::hex_decode;
use faster_hex::hex_encode;
use gix_hash::ObjectId;
use quickcheck::Arbitrary;
use quickcheck::Gen;
use quickcheck::empty_shrinker;
use sql::mysql;

use crate::errors::MononokeTypeError;
use crate::thrift;

// There is no NULL_HASH for Blake2 hashes. Any places that need a null hash should use an
// Option type, or perhaps a list as desired.

// Hash types deliberately do not derive serde `Serialize` or `Deserialize`,
// as the default implementation uses a list of integers.  If you need to
// serialize or deserialize these hashes, implement the traits directly.

pub const BLAKE2_HASH_LENGTH_BYTES: usize = 32;
pub const BLAKE2_HASH_LENGTH_HEX: usize = BLAKE2_HASH_LENGTH_BYTES * 2;

/// Raw BLAKE2b hash.
///
/// Mononoke's internal hashes are based on the BLAKE2b format, used to generate 256-bit (32-byte)
/// hashes.
///
/// This type is not used directly in most cases -- it is only used to build more specific typed
/// hashes.
///
/// For more on BLAKE2b, see <https://blake2.net/>
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(mysql::OptTryFromRowField)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct Blake2([u8; BLAKE2_HASH_LENGTH_BYTES]);

impl Blake2 {
    /// Construct a `Blake2` from an array of BLAKE2_HASH_LENGTH_BYTES bytes containing a
    /// BLAKE2b hash (ie, *not* a hash of the bytes).
    pub fn from_bytes<B: AsRef<[u8]>>(bytes: B) -> Result<Self> {
        let bytes = bytes.as_ref();
        if bytes.len() != BLAKE2_HASH_LENGTH_BYTES {
            bail!(MononokeTypeError::InvalidBlake2Input(format!(
                "need exactly {BLAKE2_HASH_LENGTH_BYTES} bytes"
            )));
        } else {
            let mut ret = [0; BLAKE2_HASH_LENGTH_BYTES];
            ret.copy_from_slice(bytes);
            Ok(Blake2(ret))
        }
    }

    /// Construct a `Blake2` from an array of BLAKE2_HASH_LENGTH_BYTES bytes.
    #[inline]
    pub const fn from_byte_array(arr: [u8; BLAKE2_HASH_LENGTH_BYTES]) -> Self {
        Blake2(arr)
    }

    #[inline]
    pub fn from_thrift(b: thrift::id::Blake2) -> Result<Self> {
        if b.0.len() != BLAKE2_HASH_LENGTH_BYTES {
            bail!(MononokeTypeError::InvalidThrift(
                "Blake2".into(),
                format!(
                    "wrong length: expected {}, got {}",
                    BLAKE2_HASH_LENGTH_BYTES,
                    b.0.len()
                )
            ));
        }
        // BigEndian here is matched with `to_thrift` below.
        let mut arr = [0u8; BLAKE2_HASH_LENGTH_BYTES];
        arr.copy_from_slice(&b.0[..]);
        Ok(Blake2(arr))
    }

    /// Construct a `Blake2` from a hex-encoded `AsciiStr`.
    #[inline]
    pub fn from_ascii_str(s: &AsciiStr) -> Result<Self> {
        Self::from_str(s.as_str())
    }

    pub fn to_hex(&self) -> AsciiString {
        let mut v = vec![0; BLAKE2_HASH_LENGTH_HEX];

        // This can only panic if buffer size of Vec isn't correct, which would be
        // a programming error.
        hex_encode(self.as_ref(), &mut v).expect("failed to hex encode");

        unsafe {
            // A hex string is always a pure ASCII string.
            AsciiString::from_ascii_unchecked(v)
        }
    }

    pub fn into_thrift(self) -> thrift::id::Blake2 {
        thrift::id::Blake2(self.0.into())
    }

    // Stable hash prefix for selection when sampling with modulus
    pub fn sampling_fingerprint(&self) -> u64 {
        let mut bytes: [u8; 8] = [0; 8];
        bytes.copy_from_slice(&self.0[0..8]);
        u64::from_le_bytes(bytes)
    }

    #[inline]
    pub fn into_inner(self) -> [u8; BLAKE2_HASH_LENGTH_BYTES] {
        self.0
    }
}

/// Context for incrementally computing a `Blake2` hash.
#[derive(Clone)]
pub enum Context {
    Unkeyed(Blake2b<U32>),
    Keyed(Blake2bMac<U32>),
}

impl Context {
    /// Construct a `Context`
    #[inline]
    pub fn new(key: &[u8]) -> Self {
        if key.is_empty() {
            Self::Unkeyed(Blake2b::new())
        } else {
            Self::Keyed(
                Blake2bMac::new_from_slice(key).expect("key should not be bigger than block size"),
            )
        }
    }

    #[inline]
    pub fn update<T>(&mut self, data: T)
    where
        T: AsRef<[u8]>,
    {
        match self {
            Self::Unkeyed(b2) => Digest::update(b2, data.as_ref()),
            Self::Keyed(b2) => b2.update(data.as_ref()),
        }
    }

    #[inline]
    pub fn finish(self) -> Blake2 {
        match self {
            Self::Unkeyed(b2) => Blake2(b2.finalize_fixed().into()),
            Self::Keyed(b2) => Blake2(b2.finalize_fixed().into()),
        }
    }
}

/// Get a reference to the underlying bytes of a `Blake2`
impl AsRef<[u8]> for Blake2 {
    fn as_ref(&self) -> &[u8] {
        &self.0[..]
    }
}

impl FromStr for Blake2 {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != BLAKE2_HASH_LENGTH_HEX {
            bail!(MononokeTypeError::InvalidBlake2Input(format!(
                "need exactly {BLAKE2_HASH_LENGTH_HEX} hex digits"
            )));
        }

        let mut ret = Blake2([0; BLAKE2_HASH_LENGTH_BYTES]);
        match hex_decode(s.as_bytes(), &mut ret.0) {
            Ok(_) => Ok(ret),
            Err(_) => bail!(MononokeTypeError::InvalidBlake2Input(
                "bad hex character".into()
            )),
        }
    }
}

impl Display for Blake2 {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.to_hex(), fmt)
    }
}

/// Custom `Debug` output for `Blake2` so it prints in hex.
impl Debug for Blake2 {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Blake2({self})")
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[derive(bincode::Encode, bincode::Decode)]
pub struct Blake2Prefix(Blake2, Blake2);

impl Blake2Prefix {
    /// Construct a `Blake2Prefix` from an array of bytes.
    pub fn from_bytes<B: AsRef<[u8]> + ?Sized>(bytes: &B) -> Result<Self> {
        let bytes = bytes.as_ref();
        if bytes.len() > BLAKE2_HASH_LENGTH_BYTES {
            bail!(MononokeTypeError::InvalidBlake2Input(format!(
                "prefix needs to be less or equal to {BLAKE2_HASH_LENGTH_BYTES} bytes"
            )))
        } else {
            let min_tail: Vec<u8> = vec![0x00; BLAKE2_HASH_LENGTH_BYTES - bytes.len()];
            let max_tail: Vec<u8> = vec![0xff; BLAKE2_HASH_LENGTH_BYTES - bytes.len()];
            Ok(Blake2Prefix(
                Blake2::from_bytes(bytes.iter().chain(&min_tail).cloned().collect::<Vec<_>>())?,
                Blake2::from_bytes(bytes.iter().chain(&max_tail).cloned().collect::<Vec<_>>())?,
            ))
        }
    }

    #[inline]
    /// Get a reference to the underlying bytes of the `Blake2` lower bound object.
    pub fn min_as_ref(&self) -> &[u8] {
        self.0.as_ref()
    }

    #[inline]
    /// Get a reference to the underlying bytes of the `Blake2` inclusive upper bound object.
    pub fn max_as_ref(&self) -> &[u8] {
        self.1.as_ref()
    }

    #[inline]
    /// Get the lower bound `Blake2`.
    pub fn min_bound(&self) -> Blake2 {
        self.0
    }

    #[inline]
    /// Get the inclusive upper bound `Blake2`.
    pub fn max_bound(&self) -> Blake2 {
        self.1
    }

    /// Convert into blake2 if it is the full prefix of the hash.
    #[inline]
    pub fn into_blake2(self) -> Option<Blake2> {
        if self.0 == self.1 { Some(self.0) } else { None }
    }

    pub fn to_hex(&self) -> AsciiString {
        let mut v_min_hex = vec![0; BLAKE2_HASH_LENGTH_HEX];
        hex_encode(self.0.as_ref(), &mut v_min_hex).expect("failed to hex encode");
        let mut v_max_hex = vec![0; BLAKE2_HASH_LENGTH_HEX];
        hex_encode(self.1.as_ref(), &mut v_max_hex).expect("failed to hex encode");
        for i in 0..BLAKE2_HASH_LENGTH_HEX {
            if v_min_hex[i] != v_max_hex[i] {
                v_min_hex.truncate(i);
                break;
            }
        }
        unsafe {
            // A hex string is always a pure ASCII string.
            AsciiString::from_ascii_unchecked(v_min_hex)
        }
    }
}

/// Construct a `Blake2Prefix` from a hex-encoded string.
impl FromStr for Blake2Prefix {
    type Err = Error;
    fn from_str(s: &str) -> Result<Blake2Prefix> {
        if s.len() > BLAKE2_HASH_LENGTH_HEX {
            bail!(MononokeTypeError::InvalidBlake2Input(format!(
                "prefix needs to be less or equal {BLAKE2_HASH_LENGTH_HEX} hex digits"
            )));
        }
        let min_tail: String = String::from_utf8(vec![b'0'; BLAKE2_HASH_LENGTH_HEX - s.len()])?;
        let max_tail: String = String::from_utf8(vec![b'f'; BLAKE2_HASH_LENGTH_HEX - s.len()])?;
        Ok(Blake2Prefix(
            Blake2::from_str(&(s.to_owned() + &min_tail))?,
            Blake2::from_str(&(s.to_owned() + &max_tail))?,
        ))
    }
}

impl Display for Blake2Prefix {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.to_hex(), fmt)
    }
}

/// Custom `Debug` output for `Blake2Prefix` so it prints in hex.
impl Debug for Blake2Prefix {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        write!(fmt, "Blake2Prefix({self})")
    }
}

macro_rules! impl_hash {
    ($type:ident, $size:literal, $error:ident) => {
        #[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
        #[derive(bincode::Encode, bincode::Decode)]
        pub struct $type([u8; $size]);

        impl $type {
            pub fn from_bytes(bytes: impl AsRef<[u8]>) -> Result<Self> {
                let bytes = bytes.as_ref();
                if bytes.len() != $size {
                    Err(MononokeTypeError::$error(format!("need exactly {} bytes", $size)).into())
                } else {
                    let mut ret = [0; $size];
                    ret.copy_from_slice(bytes);
                    Ok($type(ret))
                }
            }

            pub const fn from_byte_array(arr: [u8; $size]) -> Self {
                $type(arr)
            }

            pub fn to_hex(&self) -> AsciiString {
                let mut v = vec![0; $size * 2];

                // This can only panic if buffer size of Vec isn't correct, which would be
                // a programming error.
                hex_encode(self.as_ref(), &mut v).expect("failed to hex encode");

                unsafe { AsciiString::from_ascii_unchecked(v) }
            }

            pub fn to_brief(&self) -> AsciiString {
                self.to_hex().into_iter().take(8).collect()
            }

            #[inline]
            pub fn from_ascii_str(s: &AsciiStr) -> Result<Self> {
                Self::from_str(s.as_str())
            }

            pub fn into_thrift(self) -> thrift::id::$type {
                thrift::id::$type(self.0.into())
            }

            pub fn into_inner(self) -> [u8; $size] {
                self.0
            }

            /// Return a stable hash fingerprint that can be used for sampling
            #[inline]
            pub fn sampling_fingerprint(&self) -> u64 {
                let mut bytes: [u8; 8] = [0; 8];
                bytes.copy_from_slice(&&self.0[0..8]);
                u64::from_le_bytes(bytes)
            }
        }

        impl From<[u8; $size]> for $type {
            fn from(slice: [u8; $size]) -> Self {
                Self::from_byte_array(slice)
            }
        }

        impl Default for $type {
            fn default() -> Self {
                let bytes = [0; $size];
                $type(bytes)
            }
        }

        impl FromStr for $type {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self> {
                if s.len() != $size * 2 {
                    bail!(MononokeTypeError::$error(format!(
                        "must be {} hex digits",
                        $size * 2
                    )));
                }

                let mut ret = $type([0; $size]);

                let ret = match hex_decode(s.as_bytes(), &mut ret.0) {
                    Ok(_) => ret,
                    Err(_) => bail!(MononokeTypeError::$error("bad digit".into())),
                };

                Ok(ret)
            }
        }

        /// Get a reference to the underlying bytes of a `$type`
        impl AsRef<[u8]> for $type {
            fn as_ref(&self) -> &[u8] {
                &self.0[..]
            }
        }

        /// Custom `Debug` output for `$type` so it prints in hex.
        impl Debug for $type {
            fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
                write!(fmt, concat!(stringify!($type), "({})"), self.to_hex())
            }
        }

        /// Custom `Display` output for `$type` so it prints in hex.
        impl Display for $type {
            fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
                Display::fmt(&self.to_hex(), fmt)
            }
        }

        impl_arbitrary_for_hash!($type, $size);
    };
}

macro_rules! impl_arbitrary_for_hash {
    ($type:ident, $size:literal) => {
        impl Arbitrary for $type {
            fn arbitrary(g: &mut Gen) -> Self {
                let mut bytes = [0; $size];
                for b in bytes.iter_mut() {
                    *b = u8::arbitrary(g);
                }
                $type(bytes)
            }

            fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
                empty_shrinker()
            }
        }
    };
}

impl_hash!(Sha256, 32, InvalidSha256Input);
impl_hash!(Sha1, 20, InvalidSha1Input);
impl_hash!(GitSha1, 20, InvalidGitSha1Input);
impl_hash!(Blake3, 32, InvalidBlake3Input);
impl_arbitrary_for_hash!(Blake2, 32);

impl From<Sha1> for GitSha1 {
    fn from(value: Sha1) -> Self {
        Self(value.0)
    }
}

impl GitSha1 {
    pub fn to_object_id(&self) -> Result<ObjectId> {
        use anyhow::Context;
        ObjectId::from_hex(self.to_hex().as_bytes()).with_context(|| {
            format!(
                "Error in converting GitSha1 {:?} to GitObjectId",
                self.to_hex()
            )
        })
    }

    pub fn from_object_id(oid: &gix_hash::oid) -> Result<Self> {
        use anyhow::Context;
        Self::from_bytes(oid.as_bytes())
            .with_context(|| format!("Error in converting Git ObjectId {oid:?} to GitSha1"))
    }

    pub fn from_thrift(b: thrift::id::GitSha1) -> Result<Self> {
        Self::from_bytes(b.0)
    }
}

impl Blake3 {
    #[inline]
    pub fn from_thrift(b: thrift::id::Blake3) -> Result<Self> {
        Blake3::from_bytes(b.0)
    }
}

impl From<Blake3> for EdenapiBlake3 {
    fn from(v: Blake3) -> Self {
        EdenapiBlake3::from(v.0)
    }
}

impl From<EdenapiBlake3> for Blake3 {
    fn from(v: EdenapiBlake3) -> Self {
        Blake3::from_byte_array(v.into())
    }
}

/// Git-style content blob hashes. Same as SHA-1 but with `"<type> NNNN\0"` appended to the front,
/// where `<type>` is the object type (blob, tree, etc), and NNNN is the blob size as a decimal
/// string. Given that we know what the prefix is, we never explicitly store it so the objects
/// can be shared with non-Git uses.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RichGitSha1 {
    sha1: GitSha1,
    ty: &'static str,
    size: u64,
}

impl RichGitSha1 {
    pub fn is_blob(&self) -> bool {
        self.ty == "blob"
    }

    pub fn from_bytes(bytes: impl AsRef<[u8]>, ty: &'static str, size: u64) -> Result<Self> {
        Ok(Self::from_sha1(GitSha1::from_bytes(bytes)?, ty, size))
    }

    pub const fn from_byte_array(bytes: [u8; 20], ty: &'static str, size: u64) -> Self {
        Self::from_sha1(GitSha1::from_byte_array(bytes), ty, size)
    }

    pub const fn from_sha1(sha1: GitSha1, ty: &'static str, size: u64) -> Self {
        RichGitSha1 { sha1, ty, size }
    }

    pub fn sha1(&self) -> GitSha1 {
        self.sha1
    }

    pub fn ty(&self) -> &'static str {
        self.ty
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn to_hex(&self) -> AsciiString {
        self.sha1.to_hex()
    }

    pub fn to_object_id(&self) -> Result<ObjectId> {
        self.sha1.to_object_id()
    }

    /// Return the Git prefix bytes
    pub fn prefix(&self) -> Vec<u8> {
        format!("{} {}\0", self.ty, self.size).into_bytes()
    }

    pub fn into_thrift(self) -> thrift::id::GitSha1 {
        thrift::id::GitSha1(self.sha1.0.into())
    }
}

impl AsRef<[u8]> for RichGitSha1 {
    fn as_ref(&self) -> &[u8] {
        self.sha1.as_ref()
    }
}

impl Debug for RichGitSha1 {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("GitSha1")
            .field("sha1", &self.sha1.to_hex())
            .field("ty", &self.ty)
            .field("size", &self.size)
            .finish()
    }
}

impl Display for RichGitSha1 {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.to_hex(), fmt)
    }
}

impl From<GitSha1> for EdenapiGitSha1 {
    fn from(v: GitSha1) -> Self {
        EdenapiGitSha1::from(v.0)
    }
}

impl From<EdenapiGitSha1> for GitSha1 {
    fn from(v: EdenapiGitSha1) -> Self {
        GitSha1::from_byte_array(v.into())
    }
}

impl From<Sha1> for EdenapiSha1 {
    fn from(v: Sha1) -> Self {
        EdenapiSha1::from(v.0)
    }
}

impl From<EdenapiSha1> for Sha1 {
    fn from(v: EdenapiSha1) -> Self {
        Sha1::from_byte_array(v.into())
    }
}

impl From<Sha256> for EdenapiSha256 {
    fn from(v: Sha256) -> Self {
        EdenapiSha256::from(v.0)
    }
}

impl From<EdenapiSha256> for Sha256 {
    fn from(v: EdenapiSha256) -> Self {
        Sha256::from_byte_array(v.into())
    }
}

impl From<Sha256> for lfs_protocol::Sha256 {
    fn from(v: Sha256) -> Self {
        lfs_protocol::Sha256(v.0)
    }
}

impl From<lfs_protocol::Sha256> for Sha256 {
    fn from(v: lfs_protocol::Sha256) -> Self {
        Sha256::from_byte_array(v.0)
    }
}

impl From<GitSha1> for EdenapiCommitId {
    fn from(value: GitSha1) -> Self {
        EdenapiCommitId::GitSha1(value.into())
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MononokeDigest(pub Blake3, pub u64);

impl std::fmt::Display for MononokeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1)
    }
}

#[cfg(test)]
mod test {
    use mononoke_macros::mononoke;
    use quickcheck::TestResult;
    use quickcheck::quickcheck;

    use super::*;

    macro_rules! quickcheck_hash {
        ($type:ident, $size:literal) => {
            paste::item! {
                quickcheck! {
                    fn [<parse_roundtrip_for_ $type:lower>](v: Vec<u8>) -> TestResult {
                        if v.len() != $size {
                            return TestResult::discard()
                        }
                        let h = $type::from_bytes(v).unwrap();
                        let s = format!("{}", h);
                        let sh = s.parse().unwrap();

                        TestResult::from_bool(h == sh)
                    }

                    fn [<to_hex_roundtrip_for_ $type:lower>](h: $type) -> bool {
                        let v = h.to_hex();
                        let sh = $type::from_ascii_str(&v).unwrap();
                        h == sh
                    }

                    fn [<thrift_roundtrip_for_ $type:lower>](h: $type) -> bool {
                        let v = h.into_thrift();
                        let sh = $type::from_thrift(v).expect("converting a valid Thrift structure should always work");
                        h == sh
                    }
                }
            }
        };
    }

    // NULL is not exposed because no production code should use it.
    const NULL: Blake2 = Blake2([0; BLAKE2_HASH_LENGTH_BYTES]);

    // This hash is from https://asecuritysite.com/encryption/blake.
    #[rustfmt::skip]
    const NILHASH: Blake2 = Blake2([0x0e, 0x57, 0x51, 0xc0,
                                    0x26, 0xe5, 0x43, 0xb2,
                                    0xe8, 0xab, 0x2e, 0xb0,
                                    0x60, 0x99, 0xda, 0xa1,
                                    0xd1, 0xe5, 0xdf, 0x47,
                                    0x77, 0x8f, 0x77, 0x87,
                                    0xfa, 0xab, 0x45, 0xcd,
                                    0xf1, 0x2f, 0xe3, 0xa8]);

    #[mononoke::test]
    fn test_nil() {
        let context = Context::new(b"");
        let nil = context.finish();
        assert_eq!(nil, NILHASH);
    }

    #[mononoke::test]
    fn snapshot_hash() {
        let context = Context::new(b"abc");
        assert_eq!(
            context.finish(),
            Blake2::from_str("7a78f9455f438d36794c4adcf1a499856367dd403ceb8e9ca14a19a173b8f07b")
                .unwrap()
        );
    }

    #[mononoke::test]
    fn parse_ok() {
        assert_eq!(
            NULL,
            Blake2::from_str("0000000000000000000000000000000000000000000000000000000000000000")
                .unwrap()
        );
        assert_eq!(
            NILHASH,
            Blake2::from_str("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8")
                .unwrap()
        );
        assert_eq!(
            NILHASH,
            Blake2::from_str("0E5751C026E543B2E8AB2EB06099DAA1D1E5DF47778F7787FAAB45CDF12FE3A8")
                .unwrap()
        );
    }

    #[mononoke::test]
    fn parse_and_display_prefix_ok() {
        // max length
        assert_eq!(
            "0000000000000000000000000000000000000000000000000000000000000000",
            format!(
                "{}",
                Blake2Prefix::from_str(
                    "0000000000000000000000000000000000000000000000000000000000000000"
                )
                .unwrap()
            )
        );
        // empty
        assert_eq!("", format!("{}", Blake2Prefix::from_str("").unwrap()));
        // capital case
        assert_eq!(
            "0e5751c026",
            format!("{}", Blake2Prefix::from_str("0E5751C026").unwrap())
        );
        // odd length
        assert_eq!(
            "0e5751c02",
            format!("{}", Blake2Prefix::from_str("0e5751c02").unwrap())
        );
        // even length
        assert_eq!(
            "0e5751c0",
            format!("{}", Blake2Prefix::from_str("0e5751c0").unwrap())
        );
    }

    #[mononoke::test]
    fn parse_thrift() {
        let null_thrift = thrift::id::Blake2(vec![0; BLAKE2_HASH_LENGTH_BYTES].into());
        assert_eq!(NULL, Blake2::from_thrift(null_thrift.clone()).unwrap());
        assert_eq!(NULL.into_thrift(), null_thrift);

        let nil_thrift = thrift::id::Blake2(NILHASH.0.into());
        assert_eq!(NILHASH, Blake2::from_thrift(nil_thrift.clone()).unwrap());
        assert_eq!(NILHASH.into_thrift(), nil_thrift);
    }

    #[mononoke::test]
    fn parse_git_sha1_thrift() {
        let null_thrift = thrift::id::GitSha1(vec![0; 20].into());
        assert_eq!(
            GitSha1([0; 20]),
            GitSha1::from_thrift(null_thrift.clone()).unwrap()
        );
        assert_eq!(GitSha1([0; 20]).into_thrift(), null_thrift);
    }

    #[mononoke::test]
    fn test_display() {
        assert_eq!(
            format!("{NULL}"),
            "0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            format!("{NILHASH}"),
            "0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8"
        );
    }

    #[mononoke::test]
    fn parse_bad() {
        Blake2::from_str("").expect_err("unexpected OK - zero len");
        Blake2::from_str("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - trunc");
        Blake2::from_str("xe5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - badchar beginning");
        Blake2::from_str("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3x")
            .expect_err("unexpected OK - badchar end");
        Blake2::from_str("0e5751c026e543b2e8ab2eb06099daa1d1x5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - badchar middle");
    }

    #[mononoke::test]
    fn parse_blake3_bad() {
        Blake3::from_str("").expect_err("unexpected OK - zero len");
        Blake3::from_str("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - trunc");
        Blake3::from_str("xe5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - badchar beginning");
        Blake3::from_str("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3x")
            .expect_err("unexpected OK - badchar end");
        Blake3::from_str("0e5751c026e543b2e8ab2eb06099daa1d1x5df47778f7787faab45cdf12fe3a")
            .expect_err("unexpected OK - badchar middle");
    }

    #[mononoke::test]
    fn parse_thrift_bad() {
        Blake2::from_thrift(thrift::id::Blake2(vec![].into()))
            .expect_err("unexpected OK - zero len");
        Blake2::from_thrift(thrift::id::Blake2(vec![0; 31].into()))
            .expect_err("unexpected OK - too short");
        Blake2::from_thrift(thrift::id::Blake2(vec![0; 33].into()))
            .expect_err("unexpected Ok - too long");
    }

    #[mononoke::test]
    fn parse_blake3_thrift_bad() {
        Blake3::from_thrift(thrift::id::Blake3(vec![].into()))
            .expect_err("unexpected OK - zero len");
        Blake3::from_thrift(thrift::id::Blake3(vec![0; 31].into()))
            .expect_err("unexpected OK - too short");
        Blake3::from_thrift(thrift::id::Blake3(vec![0; 33].into()))
            .expect_err("unexpected Ok - too long");
    }

    quickcheck_hash!(Blake2, 32);
    quickcheck_hash!(Blake3, 32);

    #[mononoke::test]
    fn test_parse_sha1() {
        let sha1: Sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709".parse().unwrap();

        assert_eq!(
            sha1,
            Sha1::from_bytes([
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
                0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
            ])
            .unwrap()
        )
    }

    #[mononoke::test]
    fn test_parse_sha256() {
        let sha256: Sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .parse()
            .unwrap();

        assert_eq!(
            sha256,
            Sha256::from_bytes([
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ])
            .unwrap()
        )
    }

    #[mononoke::test]
    fn test_parse_blake3() {
        let blake3: Blake3 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            .parse()
            .unwrap();

        assert_eq!(
            blake3,
            Blake3::from_bytes([
                0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
                0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
                0x78, 0x52, 0xb8, 0x55,
            ])
            .unwrap()
        )
    }
}
