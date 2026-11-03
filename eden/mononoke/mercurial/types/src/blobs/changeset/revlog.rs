/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::io;
use std::io::Write;
use std::str;
use std::str::FromStr;

use anyhow::Context;
use anyhow::Error;
use anyhow::Result;
use anyhow::bail;
use blobstore::KeyedBlobstore;
use bytes::Bytes;
use context::CoreContext;
use mononoke_types::DateTime;

use crate::HgBlob;
use crate::HgBlobNode;
use crate::HgChangesetEnvelope;
use crate::HgChangesetId;
use crate::HgManifestId;
use crate::HgNodeHash;
use crate::HgParents;
use crate::NULL_HASH;
use crate::NonRootMPath;

// The `user` and `comments` fields are expected to be utf8 encoded, but
// some older commits might be corrupted. We handle them as pure binary here
// and higher levels can convert to utf8 as needed.
// See https://www.mercurial-scm.org/wiki/EncodingStrategy for details.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RevlogChangeset {
    pub p1: Option<HgNodeHash>,
    pub p2: Option<HgNodeHash>,
    pub manifestid: HgManifestId,
    pub user: Bytes,
    pub time: DateTime,
    pub extra: Extra,
    pub files: Vec<NonRootMPath>,
    pub message: Bytes,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Default)]
pub struct Extra(pub(super) BTreeMap<Bytes, Bytes>);

impl AsRef<BTreeMap<Bytes, Bytes>> for Extra {
    fn as_ref(&self) -> &BTreeMap<Bytes, Bytes> {
        &self.0
    }
}

fn parseline<'a, I, F, T>(lines: &mut I, parse: F) -> Result<T>
where
    I: Iterator<Item = &'a [u8]>,
    F: Fn(&'a [u8]) -> Result<T>,
{
    match lines.next() {
        Some(s) => parse(s),
        None => bail!("premature end"),
    }
}

pub(super) fn escape<'a, S: IntoIterator<Item = &'a u8>>(s: S) -> Vec<u8> {
    let mut ret = Vec::new();

    for c in s.into_iter() {
        match *c {
            b'\0' => ret.extend_from_slice(&b"\\0"[..]),
            b'\n' => ret.extend_from_slice(&b"\\n"[..]),
            b'\r' => ret.extend_from_slice(&b"\\r"[..]),
            b'\\' => ret.extend_from_slice(&b"\\\\"[..]),
            c => ret.push(c),
        }
    }

    ret
}

pub(super) fn unescape<S: AsRef<[u8]>>(s: S) -> Bytes {
    let s = s.as_ref();
    let mut ret = Vec::with_capacity(s.len());
    let mut quote = false;

    for c in s.iter() {
        match *c {
            b'0' if quote => {
                quote = false;
                ret.push(b'\0');
            }
            b'n' if quote => {
                quote = false;
                ret.push(b'\n');
            }
            b'r' if quote => {
                quote = false;
                ret.push(b'\r');
            }
            b'\\' if quote => {
                quote = false;
                ret.push(b'\\');
            }
            c if quote => {
                quote = false;
                ret.push(b'\\');
                ret.push(c)
            }
            b'\\' => {
                assert!(!quote);
                quote = true;
            }
            c => {
                quote = false;
                ret.push(c);
            }
        }
    }

    ret.into()
}

impl Extra {
    pub fn new(extra: BTreeMap<Bytes, Bytes>) -> Self {
        Extra(extra)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn from_slice<S: AsRef<[u8]>>(s: Option<S>) -> Result<Extra> {
        let mut ret = BTreeMap::new();

        if let Some(s) = s {
            let s = s.as_ref();

            for kv in s.split(|c| *c == b'\0') {
                let kv: Vec<_> = kv.splitn(2, |c| *c == b':').collect();
                if kv.len() == 2 {
                    ret.insert(unescape(kv[0]), unescape(kv[1]));
                }
            }
        }

        Ok(Extra(ret))
    }
}

fn try_get<T>(v: &[T], idx: usize) -> Option<&T> {
    if idx < v.len() { Some(&v[idx]) } else { None }
}

// Time has the format: time tz extra\n
// "date (time is int or float, timezone is int)"
//     - in what units? time is seconds from epoch?
//     - what's TZ? seconds offset from UTC?
//
// Extra is key:value, \0 separated, with \\, \0, \n escaped
fn parsetimeextra<S: AsRef<[u8]>>(s: S) -> Result<(DateTime, Extra)> {
    let s = s.as_ref();
    let parts: Vec<_> = s.splitn(3, |c| *c == b' ').collect();

    if parts.len() < 2 {
        bail!("not enough parts");
    }
    let time: i64 = str::from_utf8(parts[0])?
        .parse::<i64>()
        .context("can't parse time")?;
    let tz: i32 = str::from_utf8(parts[1])?
        .parse::<i32>()
        .context("can't parse tz")?;

    let extras = Extra::from_slice(try_get(parts.as_ref(), 2))?;

    Ok((DateTime::from_timestamp(time, tz)?, extras))
}

impl RevlogChangeset {
    pub fn new_from_parts(
        // XXX replace parents with p1 and p2
        parents: HgParents,
        manifestid: HgManifestId,
        user: Bytes,
        time: DateTime,
        extra: BTreeMap<Bytes, Bytes>,
        files: Vec<NonRootMPath>,
        message: Bytes,
    ) -> Self {
        let (p1, p2) = parents.get_nodes();
        Self {
            p1,
            p2,
            manifestid,
            user,
            time,
            extra: Extra(extra),
            files,
            message,
        }
    }

    pub fn new(node: HgBlobNode) -> Result<Self> {
        let (p1, p2) = node.parents().get_nodes();
        Self::parse(node.as_blob().clone(), p1, p2)
    }

    pub fn from_envelope(envelope: HgChangesetEnvelope) -> Result<Self> {
        let envelope = envelope.into_mut();
        Self::parse(
            envelope.contents.into(),
            envelope.p1.map(HgChangesetId::into_nodehash),
            envelope.p2.map(HgChangesetId::into_nodehash),
        )
    }

    pub async fn load<'a, B: KeyedBlobstore>(
        ctx: &'a CoreContext,
        blobstore: &'a B,
        changesetid: HgChangesetId,
    ) -> Result<Option<RevlogChangeset>> {
        if changesetid == HgChangesetId::new(NULL_HASH) {
            let revlogcs = RevlogChangeset::new_null();
            Ok(Some(revlogcs))
        } else {
            let key = changesetid.blobstore_key();

            async {
                let got = blobstore.get(ctx, &key).await?;
                match got {
                    None => Ok(None),
                    Some(bytes) => {
                        let envelope = HgChangesetEnvelope::from_blob(bytes.into())?;
                        if changesetid != envelope.node_id() {
                            bail!(
                                "Changeset ID mismatch (requested: {}, got: {})",
                                changesetid,
                                envelope.node_id()
                            );
                        }
                        let revlogcs = RevlogChangeset::from_envelope(envelope)?;
                        Ok(Some(revlogcs))
                    }
                }
            }
            .await
            .with_context(move || {
                format!("Error while deserializing changeset retrieved from key '{key}'")
            })
        }
    }

    pub fn new_null() -> Self {
        Self {
            p1: None,
            p2: None,
            manifestid: HgManifestId::new(NULL_HASH),
            user: Bytes::new(),
            time: DateTime::from_timestamp(0, 0).expect("this is a valid DateTime"),
            extra: Extra(BTreeMap::new()),
            files: Vec::new(),
            message: Bytes::new(),
        }
    }

    // format used:
    // nodeid\n        : manifest node in ascii
    // user\n          : user, no \n or \r allowed
    // time tz extra\n : date (time is int or float, timezone is int)
    //                 : extra is metadata, encoded and separated by '\0'
    //                 : older versions ignore it
    // files\n\n       : files modified by the cset, no \n or \r allowed
    // (.*)            : comment (free text, ideally utf-8)
    //
    // changelog v0 doesn't use extra
    //
    // XXX Any constraints on/syntax of "user"?
    // XXX time units? tz meaning?
    // XXX Files sorted? No escaping?
    // XXX "extra" - how sorted? What encoding?
    // XXX "comment" - line endings normalized at all?
    pub(super) fn parse(
        blob: HgBlob,
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
    ) -> Result<Self> {
        // This is awkward - we want to store the node in the resulting
        // RevlogChangeset but we need to borrow from it to parse its data. Set up a
        // partially initialized RevlogChangeset then fill it in as we go.
        let mut ret = Self {
            p1,
            p2,
            manifestid: HgManifestId::new(NULL_HASH),
            user: Bytes::new(),
            time: DateTime::from_timestamp(0, 0).expect("this is a valid DateTime"),
            extra: Extra(BTreeMap::new()),
            files: Vec::new(),
            message: Bytes::new(),
        };

        {
            let data = blob.as_slice();
            let mut lines = data.split(|b| *b == b'\n');

            let nodehash = parseline(&mut lines, |l| HgNodeHash::from_str(str::from_utf8(l)?))
                .context("can't get hash")?;
            ret.manifestid = HgManifestId::new(nodehash);
            ret.user = parseline(&mut lines, |u| Ok::<_, Error>(Bytes::copy_from_slice(u)))
                .context("can't get user")?;
            let (time, extra) =
                parseline(&mut lines, parsetimeextra).context("can't get time/extra")?;

            ret.time = time;
            ret.extra = extra;

            let mut files = Vec::new();
            let mut message = Vec::new();

            // List of files followed by the message. The file list is one entry
            // per line, with a blank line delimiting the end. The message is a single
            // binary blob with no internal structure, but we've already split it on '\n'
            // boundaries, so we can glue it back together to re-create the original content.
            //
            // XXX: We assume the comment is utf-8. Is this a good assumption?
            let mut dofiles = true;
            for line in lines {
                if dofiles {
                    if line.is_empty() {
                        dofiles = false;
                        continue;
                    }
                    files.push(NonRootMPath::new(line).context("invalid path in changelog")?)
                } else {
                    message.push(line);
                }
            }

            ret.files = files;
            ret.message = message.join(&b'\n').into();
        }

        Ok(ret)
    }

    /// Generate a serialized changeset. This is the counterpart to parse, and generates
    /// in the same format as Mercurial. It should be bit-for-bit identical in fact.
    pub fn generate<W: Write>(&self, out: &mut W) -> Result<()> {
        serialize_cs(self, out)
    }

    /// Generate bytes where SHA1(bytes) is expected to match `get_node().nodeid()`.
    ///
    /// By Mercurial definition, `bytes` is `min(p1, p2) + max(p1, p2) + data`.
    ///
    /// This includes the `data` that `generate()` produces, with the extra
    /// header so it's possible to check data integrity against commit hashes.
    ///
    /// Do NOT use the first 40 bytes for "parents" information! Parent order
    /// should always be preserved and this function does not preserve the order.
    pub fn generate_for_hash_verification<W: Write>(&self, out: &mut W) -> Result<()> {
        serialize_sorted_parents(self, out)?;
        self.generate(out)
    }

    pub fn get_node(&self) -> Result<HgBlobNode> {
        let mut v = Vec::new();
        self.generate(&mut v)?;
        Ok(HgBlobNode::new(Bytes::from(v), self.p1(), self.p2()))
    }

    pub fn manifestid(&self) -> HgManifestId {
        self.manifestid
    }

    pub fn user(&self) -> &[u8] {
        &self.user
    }

    pub fn extra(&self) -> &BTreeMap<Bytes, Bytes> {
        &self.extra.0
    }

    pub fn message(&self) -> &[u8] {
        self.message.as_ref()
    }

    pub fn files(&self) -> &[NonRootMPath] {
        self.files.as_ref()
    }

    pub fn time(&self) -> &DateTime {
        &self.time
    }

    #[inline]
    pub fn parents(&self) -> HgParents {
        HgParents::new(self.p1(), self.p2())
    }

    #[inline]
    pub fn p1(&self) -> Option<HgNodeHash> {
        self.p1
    }

    #[inline]
    pub fn p2(&self) -> Option<HgNodeHash> {
        self.p2
    }
}

/// Generate a serialized changeset. This is the counterpart to parse, and generates
/// in the same format as Mercurial. It should be bit-for-bit identical in fact.
pub fn serialize_cs<W: Write>(cs: &RevlogChangeset, out: &mut W) -> Result<()> {
    write!(out, "{}\n", cs.manifestid().into_nodehash())?;
    out.write_all(cs.user())?;
    out.write_all(b"\n")?;
    write!(
        out,
        "{} {}",
        cs.time().timestamp_secs(),
        cs.time().tz_offset_secs()
    )?;

    if !cs.extra().is_empty() {
        write!(out, " ")?;
        serialize_extras(&cs.extra, out)?;
    }

    write!(out, "\n")?;
    for f in cs.files() {
        f.generate(out)?;
        out.write_all(b"\n")?;
    }
    write!(out, "\n")?;
    out.write_all(cs.message())?;

    Ok(())
}

fn serialize_sorted_parents<W: Write>(cs: &RevlogChangeset, out: &mut W) -> Result<()> {
    let p1 = cs.p1().unwrap_or(NULL_HASH);
    let p2 = cs.p2().unwrap_or(NULL_HASH);
    let p1: &[u8] = p1.as_bytes();
    let p2: &[u8] = p2.as_bytes();
    out.write_all(p1.min(p2))?;
    out.write_all(p1.max(p2))?;
    Ok(())
}

pub fn serialize_extras<W: Write>(extras: &Extra, out: &mut W) -> io::Result<()> {
    // assume BTreeMap is sorted enough
    let kv: Vec<_> = extras
        .0
        .iter()
        .map(|(k, v)| {
            let mut vec = Vec::new();
            vec.extend_from_slice(k);
            vec.push(b':');
            vec.extend_from_slice(v);
            escape(&vec)
        })
        .collect();
    out.write_all(kv.join(&b'\0').as_slice())
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;
    use mononoke_types::sha1_hash;
    use quickcheck::quickcheck;

    use super::*;

    quickcheck! {
        fn escape_roundtrip(input: Vec<u8>) -> bool {
            let result = escape(input.iter());
            unescape(result.iter()) == input
        }
    }

    #[mononoke::test]
    fn unescape_example_roundtrip() {
        let input = b"\x0c\\r\x90\x0c\x01\\n";
        let result = unescape(input.iter());
        assert_eq!(escape(result.iter()), input);
    }

    #[mononoke::test]
    fn sha1_bytes_match_hashes() {
        // Commit raw data created by: `hg init`, `hg commit -m y --config ui.allowemptycommit=1`,
        // then "[cl.revision(i) for i in cl]" in `hg dbsh`.
        let raw_texts = [
            b"0000000000000000000000000000000000000000\nJun Wu <quark@fb.com>\n1586367898 25200\n\nx",
            b"0000000000000000000000000000000000000000\nJun Wu <quark@fb.com>\n1586367993 25200\n\ny",
            b"0000000000000000000000000000000000000000\nJun Wu <quark@fb.com>\n1586375872 25200\n\nz",
            b"0000000000000000000000000000000000000000\nJun Wu <quark@fb.com>\n1586375896 25200\n\nm",
        ];

        // Commit hashes generated by Mercurial `[cl.node(i) for i in cl]` in `hg dbsh`.
        let hashes = [
            make_hash(b"6u'\\!,\xcf\xf7\xa2\xc8\x96\xf2\xf0\xc5\xa6\xa3\xfa\x1ay\xd8"),
            make_hash(b"\xc5(\xa0\xd0\xef\xce\xa2\xd9yI\x95\x87\xda\xa1\xeb\xa1\xdc\xb0>\xe6"),
            make_hash(b"s\xd2='\x11\xe1\xbdT\xdbgm\r\x89\x06\x95\x97\xab\xba\x0c\x8c"),
            make_hash(b"\xd8\xc7f/a2:\xa7l\xef\xfe+}4U\xb9\xcc_\x88\xab"),
        ];

        // Prepare linear changesets.
        let changesets = [
            make_changeset(raw_texts[0], None, None),
            make_changeset(raw_texts[1], Some(hashes[0]), None),
            make_changeset(raw_texts[2], None, None),
            make_changeset(raw_texts[3], Some(hashes[2]), Some(hashes[1])),
        ];

        // Commit hashes by the get_node().nodeid() API.
        let hashes_via_get_node: Vec<HgNodeHash> = changesets
            .iter()
            .map(|c| c.get_node().unwrap().nodeid())
            .collect();

        // Commit hashes by the sha1(generate_for_hash_verification()) API.
        let hashes_via_sha1_bytes: Vec<HgNodeHash> = changesets
            .iter()
            .map(|c| {
                let mut buf = Vec::new();
                c.generate_for_hash_verification(&mut buf).unwrap();
                let mut hash_context = sha1_hash::Context::new();
                hash_context.update(buf);
                let sha1 = hash_context.finish();
                HgNodeHash::new(sha1)
            })
            .collect();

        assert_eq!(&hashes[..], &hashes_via_get_node[..]);
        assert_eq!(&hashes[..], &hashes_via_sha1_bytes[..]);
    }

    fn make_hash(hash: &[u8]) -> HgNodeHash {
        HgNodeHash::from_bytes(hash).unwrap()
    }

    fn make_changeset(
        text: &'static [u8],
        p1: Option<HgNodeHash>,
        p2: Option<HgNodeHash>,
    ) -> RevlogChangeset {
        RevlogChangeset::parse(HgBlob::new(Bytes::from_static(text)), p1, p2).unwrap()
    }
}
