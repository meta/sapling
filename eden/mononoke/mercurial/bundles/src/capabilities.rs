/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Unpacking capabilities

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::Error;
use anyhow::Result;
use bytes::Bytes;
use bytes::BytesMut;
use mercurial_types::utils::percent_encode;
use percent_encoding::percent_decode;
use tokio_util::codec::Decoder;

#[derive(Debug, PartialEq, Eq)]
pub struct Capabilities {
    caps: HashMap<String, Vec<String>>,
}

impl Capabilities {
    pub fn new(caps: HashMap<String, Vec<String>>) -> Self {
        Self { caps }
    }
}

/// This is a tokio_io Decoder for capabilities used f.e. in "replycaps" part of bundle2
///
/// The format is as follows:
/// ```text
/// <capabilities> := '' EOF | <line> ['\n' <line>]* EOF
/// <line> := <key> | <key> '=' <values>
/// <values> := '' | <value> [',' <value>]*
/// <key> := `url encoded key`
/// <value> := `url encoded value`
/// ```
///
/// Notice that this decoder needs to reach EOF and it decodes everything as a single item
pub struct CapabilitiesUnpacker;

impl Decoder for CapabilitiesUnpacker {
    type Item = Capabilities;
    type Error = Error;

    fn decode(&mut self, _buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        Ok(None) // This unpacker unpacks single element, wait for EOF
    }

    fn decode_eof(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        let mut caps = HashMap::new();
        for kv in buf.as_ref().split(|b| b == &b'\n') {
            let mut kv = kv.splitn(2, |b| b == &b'=');
            let key = percent_decode(kv.next().expect("must have at least 1 element"))
                .decode_utf8()?
                .into_owned();
            let values = {
                match kv.next() {
                    None => Vec::new(),
                    Some(values) => {
                        let res: Result<Vec<_>, _> = values
                            .split(|b| b == &b',')
                            .filter(|v| !v.is_empty())
                            .map(|v| percent_decode(v).decode_utf8().map(Cow::into_owned))
                            .collect();
                        res?
                    }
                }
            };
            caps.insert(key, values);
        }

        buf.clear(); // all buf was consumed

        Ok(Some(Capabilities { caps }))
    }
}

pub fn encode_capabilities(caps: Capabilities) -> Bytes {
    let mut res = vec![];
    for (key, values) in caps.caps {
        let key = percent_encode(&key);
        let values = itertools::join(values.into_iter().map(|v| percent_encode(&v)), ",");
        res.push(format!("{key}={values}"));
    }
    Bytes::from(itertools::join(res, "\n"))
}

#[cfg(test)]
mod test {
    use maplit::hashmap;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn caps_roundtrip() {
        let caps = hashmap! {
            "key1".to_string() => vec!["value11".to_string(), "value12".to_string()],
            "key_empty".to_string() => vec![],
            "key2".to_string() => vec!["value22".to_string()],
            "weirdkey,=,=".to_string() => vec!["weirdvalue,==,".to_string(), "value".to_string()],
        };

        let encoded = encode_capabilities(Capabilities::new(caps.clone()));
        let mut unpacker = CapabilitiesUnpacker;

        let decoded = unpacker
            .decode_eof(&mut BytesMut::from(&encoded[..]))
            .unwrap()
            .unwrap();
        assert_eq!(decoded.caps, caps);
    }
}
