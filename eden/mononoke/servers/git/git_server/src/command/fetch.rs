/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use gix_hash::ObjectId;
use gix_object::Kind;
use gix_packetline::PacketLineRef;
use gix_packetline::blocking_io::StreamingPeekableIter;
use gix_transport::bstr::ByteSlice;
use packfile::pack::DeltaForm;
use protocol::types::ChainBreakingMode;
use protocol::types::FetchFilter;
use protocol::types::FetchRequest;
use protocol::types::PackfileConcurrency;
use protocol::types::RefsSource;
use protocol::types::ShallowInfoRequest;
use protocol::types::ShallowInfoResponse;
use protocol::types::ShallowVariant;

const DONE: &[u8] = b"done";
const THIN_PACK: &[u8] = b"thin-pack";
const NO_PROGRESS: &[u8] = b"no-progress";
const INCLUDE_TAG: &[u8] = b"include-tag";
const OFFSET_DELTA: &[u8] = b"ofs-delta";
const WAIT_FOR_DONE: &[u8] = b"wait-for-done";
const SIDEBAND_ALL: &[u8] = b"sideband-all";
const DEEPEN_RELATIVE: &[u8] = b"deepen-relative";

const WANT_PREFIX: &[u8] = b"want ";
const HAVE_PREFIX: &[u8] = b"have ";
const PACKFILE_URIS_PREFIX: &[u8] = b"packfile-uris ";
const WANT_REF_PREFIX: &[u8] = b"want-ref ";
const FILTER_PREFIX: &[u8] = b"filter ";
const DEEPEN_NOT_PREFIX: &[u8] = b"deepen-not ";
const DEEPEN_SINCE_PREFIX: &[u8] = b"deepen-since ";
const DEEPEN_PREFIX: &[u8] = b"deepen ";
const SHALLOW_PREFIX: &[u8] = b"shallow ";

const PACKFILE_URIS_SEPARATOR: &str = ",";

/// Arguments for `fetch` command
#[derive(Clone, Debug, Default)]
pub struct FetchArgs {
    /// Indicates to the server the objects which the client wants to
    /// retrieve
    wants: Vec<ObjectId>,
    /// Indicates to the server the objects which the client already has
    /// locally
    haves: Vec<ObjectId>,
    /// Indicates to the server that negotiation should terminate (or
    /// not even begin if performing a clone) and that the server should
    /// use the information supplied in the request to construct the packfile
    pub done: bool,
    /// Request that a thin pack be sent, which is a pack with deltas
    /// which reference base objects not contained within the pack (but
    /// are known to exist at the receiving end)
    pub thin_pack: bool,
    /// Request that progress information that would normally be sent on
    /// side-band channel 2, during the packfile transfer, should not be sent
    pub no_progress: bool,
    /// Request that annotated tags should be sent if the objects they
    /// point to are being sent.
    pub include_tag: bool,
    /// Indicate that the client understands PACKv2 with delta referring
    /// to its base by position in pack rather than by an oid
    pub ofs_delta: bool,
    /// List of object Ids representing the edge of the shallow history present
    /// at the client, i.e. the set of commits that the client knows about but
    /// does not have any of their parents and their ancestors
    pub shallow: Vec<ObjectId>,
    /// Requests that the fetch/clone should be shallow having a commit
    /// depth of "deepen" relative to the server
    pub deepen: Option<u32>,
    /// Requests that the semantics of the "deepen" command be changed
    /// to indicate that the depth requested is relative to the client's
    /// current shallow boundary, instead of relative to the requested commits.
    pub deepen_relative: bool,
    /// Requests that the shallow clone/fetch should be cut at a specific time,
    /// instead of depth. The timestamp provided should be in the same format
    /// as is expected for `git rev-list --max-age <timestamp>`
    pub deepen_since: Option<usize>,
    /// Requests that the shallow clone/fetch should be cut at a specific revision
    /// instead of a depth, i.e. the specified oid becomes the boundary at which the
    /// fetch or clone should stop at
    pub deepen_not: Vec<String>,
    /// Request that various objects from the packfile be omitted using
    /// one of several filtering techniques
    pub filter: Option<FilterArgs>,
    /// Indicates to the server that the client wants to retrieve a particular set of
    /// refs by providing the full name of the ref on the server
    pub want_refs: Vec<String>,
    /// Instruct the server to send the whole response multiplexed, not just the
    /// packfile section
    pub sideband_all: bool,
    /// Indicates to the server that the client is willing to receive URIs of any
    /// of the given protocols in place of objects in the sent packfile. Before
    /// performing the connectivity check, the client should download from all given URIs
    pub packfile_uris: Vec<String>,
    /// Indicates to the server that it should never send "ready", but should wait
    /// for the client to say "done" before sending the packfile
    pub wait_for_done: bool,
}

/// Argument for filtering objects during clone/fetch
#[derive(Clone, Debug)]
pub struct FilterArgs {
    max_blob_size: u64,
    max_tree_depth: u64,
    allowed_object_types: Vec<Kind>,
}

impl Default for FilterArgs {
    fn default() -> Self {
        Self {
            max_blob_size: u64::MAX,
            max_tree_depth: u64::MAX,
            allowed_object_types: vec![Kind::Blob, Kind::Tree, Kind::Commit, Kind::Tag],
        }
    }
}

impl FilterArgs {
    const COMBINE_PREFIX: &'static str = "combine:";
    const OBJECT_TYPE_PREFIX: &'static str = "object:type=";
    const BLOB_PREFIX: &'static str = "blob:";
    const TREE_PREFIX: &'static str = "tree:";
    const NO_BLOBS: &'static str = "none";
    const SIZE_LIMIT: &'static str = "limit=";
    const FILTER_SPLITTER: &'static str = "+";

    fn parse_size(size: &str) -> Result<u64> {
        const KB_SUFFIX: &str = "k";
        const MB_SUFFIX: &str = "m";
        const GB_SUFFIX: &str = "g";
        let mut multiplier = 1;
        let size = if let Some(size_num) = size.strip_suffix(KB_SUFFIX) {
            multiplier = 1024;
            size_num
        } else if let Some(size_num) = size.strip_suffix(MB_SUFFIX) {
            multiplier = 1024 * 1024;
            size_num
        } else if let Some(size_num) = size.strip_suffix(GB_SUFFIX) {
            multiplier = 1024 * 1024 * 1024;
            size_num
        } else {
            size
        };
        size.parse::<u64>()
            .map(|size| size * multiplier)
            .with_context(|| format!("Invalid blob size {size:?}"))
    }

    fn parse_from_spec(data: String) -> Result<Self> {
        let filter_set = if let Some(combined_filters) = data.strip_prefix(Self::COMBINE_PREFIX) {
            // There are multiple filters combined together
            combined_filters
                .split(Self::FILTER_SPLITTER)
                .map(String::from)
                .collect()
        } else {
            // There is only one filter
            vec![data]
        };
        let mut filter_args = FilterArgs::default();
        let mut allowed_type = None;
        for filter in filter_set {
            if let Some(object_type) = filter.strip_prefix(Self::OBJECT_TYPE_PREFIX) {
                let object_kind = Kind::from_bytes(object_type.as_bytes())
                    .with_context(|| format!("Invalid object type {object_type:?}"))?;
                // Git has this weird behavior if you specify multiple allowed object types
                // it just honors the first one it comes across. And no, there is no mention
                // of it in the docs. Found this out through code reading and trail-and-error :)
                if allowed_type.is_none() {
                    allowed_type = Some(object_kind);
                }
            }
            if let Some(blob_size) = filter.strip_prefix(Self::BLOB_PREFIX) {
                if blob_size == Self::NO_BLOBS {
                    filter_args.max_blob_size = 0;
                } else if let Some(blob_limit) = blob_size.strip_prefix(Self::SIZE_LIMIT) {
                    filter_args.max_blob_size = Self::parse_size(blob_limit)?;
                } else {
                    bail!("Invalid blob size {blob_size:?} in filter spec {filter}");
                }
            }
            if let Some(tree_depth) = filter.strip_prefix(Self::TREE_PREFIX) {
                let max_depth = tree_depth
                    .parse::<u64>()
                    .with_context(|| format!("Invalid tree depth {tree_depth:?}"))?;
                filter_args.max_tree_depth = max_depth;
            }
        }
        if let Some(allowed_type) = allowed_type {
            filter_args.allowed_object_types = vec![allowed_type];
        }
        Ok(filter_args)
    }

    fn into_fetch_filter(self) -> FetchFilter {
        FetchFilter {
            max_blob_size: self.max_blob_size,
            max_tree_depth: self.max_tree_depth,
            allowed_object_types: self.allowed_object_types,
        }
    }
}

pub fn parse_oid(data: &[u8], oid_type: &[u8]) -> Result<ObjectId> {
    ObjectId::from_hex(data).with_context(|| {
        format!("Invalid {oid_type:?}object id {data:?} received during fetch request")
    })
}

fn bytes_to_str<'a, 'b, 'c>(
    bytes: &'a [u8],
    bytes_type: &'b str,
    arg_type: &'c str,
) -> Result<&'a str> {
    std::str::from_utf8(bytes).with_context(|| {
        format!(
            "Invalid {bytes_type} bytes {bytes:?} received for {arg_type} during fetch command args parsing",
        )
    })
}

impl FetchArgs {
    /// Method determining if the fetch request is a shallow fetch request
    pub fn is_shallow(&self) -> bool {
        !self.shallow.is_empty()
            || self.deepen.is_some()
            || self.deepen_since.is_some()
            || !self.deepen_not.is_empty()
    }

    /// Method determining if the fetch request is a filter fetch request
    pub fn is_filter(&self) -> bool {
        self.filter.is_some()
    }

    fn validate(&self) -> Result<()> {
        if self.deepen.is_some() && self.deepen_since.is_some() {
            bail!(
                "deepen and deepen-since arguments cannot be provided at the same time for fetch command"
            )
        } else if self.deepen.is_some() && !self.deepen_not.is_empty() {
            bail!(
                "deepen and deepen-not arguments cannot be provided at the same time for fetch command"
            )
        } else {
            Ok(())
        }
    }

    pub fn haves(&self) -> &Vec<ObjectId> {
        &self.haves
    }

    pub fn wants(&self) -> &Vec<ObjectId> {
        &self.wants
    }

    pub fn parse_from_packetline(args: &[u8]) -> Result<Self> {
        let mut tokens = StreamingPeekableIter::new(args, &[PacketLineRef::Flush], true);
        let mut fetch_args = Self::default();
        while let Some(token) = tokens.read_line() {
            let token = token.context(
                "Failed to read line from packetline during fetch command args parsing",
            )??;
            if let PacketLineRef::Data(data) = token {
                let data = data.trim();
                if let Some(oid) = data.strip_prefix(WANT_PREFIX) {
                    fetch_args.wants.push(parse_oid(oid, WANT_PREFIX)?);
                } else if let Some(oid) = data.strip_prefix(HAVE_PREFIX) {
                    fetch_args.haves.push(parse_oid(oid, HAVE_PREFIX)?);
                } else if let Some(oid) = data.strip_prefix(SHALLOW_PREFIX) {
                    fetch_args.shallow.push(parse_oid(oid, SHALLOW_PREFIX)?);
                } else if let Some(depth) = data.strip_prefix(DEEPEN_PREFIX) {
                    let depth = bytes_to_str(depth, "depth", "deepen")?.parse::<u32>();
                    fetch_args.deepen = Some(depth.clone().with_context(|| {
                        format!(
                            "Invalid depth {depth:?} received during fetch command args parsing"
                        )
                    })?);
                } else if let Some(time_depth) = data.strip_prefix(DEEPEN_SINCE_PREFIX) {
                    let time_depth = bytes_to_str(time_depth, "depth", "deepen since")?.to_owned();
                    let parsed_time = time_depth.as_str().parse::<usize>()
                        .with_context(|| format!("Invalid time {time_depth:?} received for deepen since during fetch command args parsing"))?;
                    fetch_args.deepen_since = Some(parsed_time);
                } else if let Some(deepen_not_ref) = data.strip_prefix(DEEPEN_NOT_PREFIX) {
                    let deepen_not_ref =
                        bytes_to_str(deepen_not_ref, "deepen-not", "shallow")?.to_owned();
                    fetch_args.deepen_not.push(deepen_not_ref);
                } else if let Some(filter) = data.strip_prefix(FILTER_PREFIX) {
                    let filter_spec = bytes_to_str(filter, "filter_spec", "filter")?.to_owned();
                    fetch_args.filter = Some(FilterArgs::parse_from_spec(filter_spec)?);
                } else if let Some(want_ref) = data.strip_prefix(WANT_REF_PREFIX) {
                    let want_ref = bytes_to_str(want_ref, "want_ref", "want-ref")?.to_owned();
                    fetch_args.want_refs.push(want_ref);
                } else if let Some(packfile_uris) = data.strip_prefix(PACKFILE_URIS_PREFIX) {
                    let packfile_uris =
                        bytes_to_str(packfile_uris, "packfile_uris", "packfile-uris")?;
                    fetch_args.packfile_uris = Vec::from_iter(
                        packfile_uris
                            .split(PACKFILE_URIS_SEPARATOR)
                            .map(String::from),
                    );
                } else {
                    match data {
                        DONE => fetch_args.done = true,
                        THIN_PACK => fetch_args.thin_pack = true,
                        NO_PROGRESS => fetch_args.no_progress = true,
                        INCLUDE_TAG => fetch_args.include_tag = true,
                        OFFSET_DELTA => fetch_args.ofs_delta = true,
                        WAIT_FOR_DONE => fetch_args.wait_for_done = true,
                        SIDEBAND_ALL => fetch_args.sideband_all = true,
                        DEEPEN_RELATIVE => fetch_args.deepen_relative = true,
                        arg => bail!(
                            "Unexpected arg {} in fetch command args",
                            String::from_utf8_lossy(arg)
                        ),
                    };
                }
            } else {
                bail!("Unexpected token {token:?} in packetline during fetch command args parsing");
            };
        }
        fetch_args.wants.sort();
        fetch_args.haves.sort();
        fetch_args.validate()?;
        Ok(fetch_args)
    }

    /// Convert the fetch command args into FetchRequest instance
    pub fn into_request(
        self,
        concurrency: PackfileConcurrency,
        shallow_info: Option<ShallowInfoResponse>,
        bypass_cache: bool,
        delta_form: DeltaForm,
    ) -> FetchRequest {
        let shallow_variant = self.shallow_variant();
        FetchRequest {
            heads: self.wants,
            bases: self.haves,
            include_out_of_pack_deltas: self.thin_pack,
            include_annotated_tags: self.include_tag,
            offset_delta: self.ofs_delta,
            delta_form,
            shallow_variant,
            filter: self.filter.map(FilterArgs::into_fetch_filter),
            concurrency,
            shallow_info,
            chain_breaking_mode: ChainBreakingMode::Stochastic,
            refs_source: match bypass_cache {
                true => RefsSource::DatabaseFollower,
                false => RefsSource::WarmBookmarksCache,
            },
        }
    }

    fn shallow_variant(&self) -> ShallowVariant {
        if let Some(timestamp) = self.deepen_since.as_ref() {
            ShallowVariant::FromServerWithTime(timestamp.clone())
        } else if !self.deepen_not.is_empty() {
            ShallowVariant::FromServerExcludingRefs(self.deepen_not.clone())
        } else if let Some(depth) = self.deepen {
            if self.deepen_relative {
                ShallowVariant::FromClientWithDepth(depth)
            } else {
                ShallowVariant::FromServerWithDepth(depth)
            }
        } else {
            ShallowVariant::None
        }
    }

    /// Convert the fetch command args into ShallowInfoRequest instance
    pub fn into_shallow_request(&self) -> ShallowInfoRequest {
        ShallowInfoRequest {
            heads: self.wants.clone(),
            bases: self.haves.clone(),
            shallow: self.shallow.clone(),
            variant: self.shallow_variant(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use anyhow::Result;
    use gix_packetline::blocking_io::Writer;
    use gix_packetline::blocking_io::encode::flush_to_write;
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn fetch_request_preserves_effective_writer_and_shallow_mode() {
        for thin_pack in [false, true] {
            for delta_form in [DeltaForm::RefAndOffset, DeltaForm::OnlyOffset] {
                let args = FetchArgs {
                    thin_pack,
                    deepen: Some(10),
                    deepen_relative: true,
                    ..Default::default()
                };
                let request = args.into_request(
                    PackfileConcurrency::new(1, 1, 1, 1, None),
                    None,
                    false,
                    delta_form,
                );
                assert_eq!(request.include_out_of_pack_deltas, thin_pack);
                assert_eq!(request.delta_form, delta_form);
                assert!(matches!(
                    request.shallow_variant,
                    ShallowVariant::FromClientWithDepth(10)
                ));
            }
        }
        let args = FetchArgs {
            deepen: Some(10),
            ..Default::default()
        };
        let request = args.into_request(
            PackfileConcurrency::new(1, 1, 1, 1, None),
            None,
            false,
            DeltaForm::RefAndOffset,
        );
        assert!(matches!(
            request.shallow_variant,
            ShallowVariant::FromServerWithDepth(10)
        ));
    }

    #[mononoke::test]
    fn test_fetch_command_args_parsing() -> Result<()> {
        let inner_writer = Vec::new();
        let mut packetline_writer = Writer::new(inner_writer);
        packetline_writer.write_all(b"thin-pack\n")?;
        packetline_writer.write_all(b"ofs-delta\n")?;
        packetline_writer.write_all(b"no-progress\n")?;
        packetline_writer.write_all(b"include-tag\n")?;
        packetline_writer.write_all(b"wait-for-done\n")?;
        packetline_writer.write_all(b"sideband-all\n")?;
        packetline_writer.write_all(b"shallow 0000000000000000000000000000000000000000\n")?;
        packetline_writer.write_all(b"deepen 1\n")?;
        packetline_writer.write_all(b"want-ref refs/heads/master\n")?;
        packetline_writer.write_all(b"want-ref refs/heads/release\n")?;
        packetline_writer.write_all(b"have 0000000000000000000000000000000000000000\n")?;
        packetline_writer.write_all(b"want 0000000000000000000000000000000000000000\n")?;
        packetline_writer.write_all(b"have 1000000000000000000000000000000000000001\n")?;
        packetline_writer.write_all(b"want 1000000000000000000000000000000000000001\n")?;
        packetline_writer.write_all(b"have 2000000000000000000000000000000000000002\n")?;
        packetline_writer.write_all(b"shallow 1000000000000000000000000000000000000001\n")?;
        packetline_writer.write_all(b"filter combine:blob:none+tree:5+object:type=blob+object:type=tree+object:type=commit\n")?;
        packetline_writer.write_all(b"done\n")?;
        packetline_writer.flush()?;
        let mut inner_writer = packetline_writer.into_inner();
        flush_to_write(&mut inner_writer)?;

        let parsed_args = FetchArgs::parse_from_packetline(&inner_writer)?;
        assert!(parsed_args.thin_pack);
        assert!(parsed_args.ofs_delta);
        assert!(parsed_args.no_progress);
        assert!(parsed_args.include_tag);
        assert!(parsed_args.wait_for_done);
        assert!(parsed_args.sideband_all);
        assert!(parsed_args.done);
        assert_eq!(parsed_args.deepen, Some(1));
        assert_eq!(parsed_args.shallow.len(), 2);
        assert_eq!(parsed_args.haves.len(), 3);
        assert_eq!(parsed_args.wants.len(), 2);
        assert!(parsed_args.filter.is_some());
        Ok(())
    }

    #[mononoke::test]
    fn test_fetch_command_args_validation() -> Result<()> {
        let inner_writer = Vec::new();
        let mut packetline_writer = Writer::new(inner_writer);
        packetline_writer.write_all(b"deepen 1\n")?;
        packetline_writer.write_all(b"deepen-since 1979-02-26 18:30:00\n")?;
        let mut inner_writer = packetline_writer.into_inner();
        flush_to_write(&mut inner_writer)?;

        assert!(FetchArgs::parse_from_packetline(&inner_writer).is_err());

        let inner_writer = Vec::new();
        let mut packetline_writer = Writer::new(inner_writer);
        packetline_writer.write_all(b"deepen 1\n")?;
        packetline_writer.write_all(b"deepen-not heads/master\n")?;
        let mut inner_writer = packetline_writer.into_inner();
        flush_to_write(&mut inner_writer)?;

        assert!(FetchArgs::parse_from_packetline(&inner_writer).is_err());
        Ok(())
    }

    #[mononoke::test]
    fn test_fetch_command_args_time_parsing() -> Result<()> {
        let inner_writer = Vec::new();
        let mut packetline_writer = Writer::new(inner_writer);
        packetline_writer.write_all(b"deepen-since 1740503859\n")?;
        let mut inner_writer = packetline_writer.into_inner();
        flush_to_write(&mut inner_writer)?;

        assert!(FetchArgs::parse_from_packetline(&inner_writer).is_ok());

        let inner_writer = Vec::new();
        let mut packetline_writer = Writer::new(inner_writer);
        packetline_writer.write_all(b"deepen-since 949363260\n")?;
        let mut inner_writer = packetline_writer.into_inner();
        flush_to_write(&mut inner_writer)?;

        assert!(FetchArgs::parse_from_packetline(&inner_writer).is_ok());
        Ok(())
    }

    #[mononoke::test]
    fn test_filter_args_parsing() -> Result<()> {
        let raw_input =
            "combine:blob:none+tree:5+object:type=blob+object:type=tree+object:type=commit"
                .to_string();
        let filter_args = FilterArgs::parse_from_spec(raw_input)?;

        assert_eq!(
            filter_args.allowed_object_types,
            vec![Kind::Blob] // Since blob was the first one in the spec, rest are ignored
        );
        assert_eq!(filter_args.max_tree_depth, 5);
        assert_eq!(filter_args.max_blob_size, 0);

        let raw_input = "tree:5".to_string();
        let filter_args = FilterArgs::parse_from_spec(raw_input)?;
        assert_eq!(filter_args.max_tree_depth, 5);
        assert_eq!(filter_args.max_blob_size, u64::MAX);

        let raw_input = "object:type=commit".to_string();
        let filter_args = FilterArgs::parse_from_spec(raw_input)?;
        assert_eq!(filter_args.max_tree_depth, u64::MAX);
        assert_eq!(filter_args.max_blob_size, u64::MAX);
        assert_eq!(filter_args.allowed_object_types, vec![Kind::Commit]);

        let raw_input = "blob:limit=5m".to_string();
        let filter_args = FilterArgs::parse_from_spec(raw_input)?;
        assert_eq!(filter_args.max_tree_depth, u64::MAX);
        assert_eq!(filter_args.max_blob_size, 5 * 1024 * 1024);

        let raw_input = "blob:limit=49999".to_string();
        let filter_args = FilterArgs::parse_from_spec(raw_input)?;
        assert_eq!(filter_args.max_blob_size, 49999);
        assert_eq!(filter_args.max_tree_depth, u64::MAX);
        assert_eq!(
            filter_args.allowed_object_types,
            vec![Kind::Blob, Kind::Tree, Kind::Commit, Kind::Tag]
        );
        Ok(())
    }
}
