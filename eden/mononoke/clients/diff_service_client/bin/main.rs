/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::str::FromStr;

use anyhow::Result;
use clap::Args;
use clap::Parser;
use clap::ValueEnum;
use clientinfo::ClientEntryPoint;
use clientinfo::ClientInfo;
use cmdlib_logging::LoggingArgs;
use commit_id_types::CommitId;
use context::CoreContext;
use diff_service_client::DiffInput;
use diff_service_client::LfsPointer;
use diff_service_client::RepoDiffServiceClient;
use diff_service_if as thrift;
use diff_service_if::DiffContentType;
use diff_service_if::DiffFileType;
use diff_service_if::DiffGeneratedStatus;
use faster_hex::hex_decode_array;
use fbinit::FacebookInit;
use futures::StreamExt;
use mononoke_types::ContentId;
use source_control as source_control_thrift;

#[cfg(fbcode_build)]
mod facebook;
#[cfg(not(fbcode_build))]
mod oss;

#[cfg(fbcode_build)]
use facebook as platform;
#[cfg(not(fbcode_build))]
use oss as platform;

fn format_file_type(file_type: &DiffFileType) -> &'static str {
    match *file_type {
        DiffFileType::REGULAR => "REGULAR",
        DiffFileType::EXECUTABLE => "EXECUTABLE",
        DiffFileType::SYMLINK => "SYMLINK",
        DiffFileType::GIT_SUBMODULE => "GIT_SUBMODULE",
        _ => "UNKNOWN",
    }
}

fn format_content_type(content_type: &DiffContentType) -> &'static str {
    match *content_type {
        DiffContentType::TEXT => "TEXT",
        DiffContentType::NON_UTF8 => "NON_UTF8",
        DiffContentType::BINARY => "BINARY",
        DiffContentType::LFS_POINTER => "LFS_POINTER",
        _ => "UNKNOWN",
    }
}

fn format_generated_status(generated_status: &DiffGeneratedStatus) -> &'static str {
    match *generated_status {
        DiffGeneratedStatus::NON_GENERATED => "NON_GENERATED",
        DiffGeneratedStatus::PARTIALLY => "PARTIALLY",
        DiffGeneratedStatus::FULLY => "FULLY",
        _ => "UNKNOWN",
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CommitScheme {
    Bonsai,
    Hg,
    Git,
    Globalrev,
    Svnrev,
    EphemeralBonsai,
}

fn parse_commit_id(id_str: &str, scheme: CommitScheme) -> Result<CommitId> {
    match scheme {
        CommitScheme::Bonsai => {
            let id = hex_decode_array::<32>(id_str.as_bytes())?;
            Ok(CommitId::BonsaiId(id))
        }
        CommitScheme::Hg => {
            let id = hex_decode_array::<20>(id_str.as_bytes())?;
            Ok(CommitId::HgId(id))
        }
        CommitScheme::Git => {
            let id = hex_decode_array::<20>(id_str.as_bytes())?;
            Ok(CommitId::GitSha1(id))
        }
        CommitScheme::Globalrev => Ok(CommitId::Globalrev(id_str.parse()?)),
        CommitScheme::Svnrev => Ok(CommitId::Svnrev(id_str.parse()?)),
        CommitScheme::EphemeralBonsai => {
            let id = hex_decode_array::<32>(id_str.as_bytes())?;
            Ok(CommitId::EphemeralBonsai(id))
        }
    }
}

/// Convert a commit_id_types::CommitId to thrift::CommitId.
/// This follows the same pattern as scsc/src/args/commit_id.rs::resolve_commit_ids.
fn commit_id_to_thrift(commit_id: CommitId) -> Result<thrift::CommitId> {
    match commit_id {
        CommitId::BonsaiId(bonsai) => Ok(thrift::CommitId::bonsai(bonsai.to_vec())),
        CommitId::EphemeralBonsai(bonsai) => Ok(thrift::CommitId::ephemeral_bonsai(
            thrift::EphemeralBonsai {
                bonsai_id: bonsai.to_vec(),
                bubble_id: 0,
                ..Default::default()
            },
        )),
        CommitId::HgId(hg) => Ok(thrift::CommitId::hg(hg.to_vec())),
        CommitId::GitSha1(hash) => Ok(thrift::CommitId::git(hash.to_vec())),
        CommitId::Globalrev(rev) => Ok(thrift::CommitId::globalrev(rev.try_into()?)),
        CommitId::Svnrev(rev) => Ok(thrift::CommitId::svnrev(rev.try_into()?)),
        CommitId::P4Changelist(cl) => Ok(thrift::CommitId::p4_changelist(cl.try_into()?)),
        CommitId::Resolve(_) | CommitId::Bookmark(_) => {
            Err(anyhow::anyhow!("Unsupported commit id type: {commit_id}"))
        }
    }
}

/// Parse the `--diff-mode` CLI string into the thrift enum.
fn parse_diff_mode(s: &str) -> Result<thrift::DiffMode> {
    match s {
        "inline" => Ok(thrift::DiffMode::INLINE),
        "omit_content" => Ok(thrift::DiffMode::OMIT_CONTENT),
        _ => Err(anyhow::anyhow!("Invalid diff_mode: {s}")),
    }
}

/// Parse the `--file-type` CLI string into the thrift enum.
fn parse_file_type(s: &str) -> Result<thrift::DiffFileType> {
    match s {
        "regular" => Ok(thrift::DiffFileType::REGULAR),
        "executable" => Ok(thrift::DiffFileType::EXECUTABLE),
        "symlink" => Ok(thrift::DiffFileType::SYMLINK),
        "git_submodule" => Ok(thrift::DiffFileType::GIT_SUBMODULE),
        _ => Err(anyhow::anyhow!("Invalid file_type: {s}")),
    }
}

/// Parse the `--copy-info` CLI string into the thrift enum.
fn parse_copy_info(s: &str) -> Result<thrift::DiffCopyInfo> {
    match s {
        "none" => Ok(thrift::DiffCopyInfo::NONE),
        "move" => Ok(thrift::DiffCopyInfo::MOVE),
        "copy" => Ok(thrift::DiffCopyInfo::COPY),
        _ => Err(anyhow::anyhow!("Invalid copy_info: {s}")),
    }
}

/// Resolve a (`--*-lfs-sha256`, `--*-lfs-size`) pair, requiring both or neither.
fn parse_lfs_pointer(
    side: &str,
    sha256: Option<String>,
    size: Option<i64>,
) -> Result<Option<LfsPointer>> {
    match (sha256, size) {
        (Some(sha256), Some(size)) => Ok(Some(LfsPointer { sha256, size })),
        (None, None) => Ok(None),
        (Some(_), None) => Err(anyhow::anyhow!(
            "--{side}-lfs-size is required when --{side}-lfs-sha256 is provided"
        )),
        (None, Some(_)) => Err(anyhow::anyhow!(
            "--{side}-lfs-sha256 is required when --{side}-lfs-size is provided"
        )),
    }
}

/// Drain a stream of diff chunks to stdout, printing each chunk's content
/// inline and emitting a trailing newline if the last chunk did not end with
/// one. Stops on the first error and logs it to stderr.
async fn drain_streamed_chunks<S, C, F>(label: &str, mut stream: S, get_content: F)
where
    S: futures::Stream<Item = std::result::Result<C, anyhow::Error>> + Unpin,
    F: Fn(&C) -> &[u8],
{
    let mut last_char_is_newline = true;
    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                let content = String::from_utf8_lossy(get_content(&chunk));
                print!("{content}");
                last_char_is_newline = content.ends_with('\n');
            }
            Err(e) => {
                eprintln!("Error reading {label} chunk: {e}");
                break;
            }
        }
    }
    if !last_char_is_newline {
        println!();
    }
}

/// Print a unary response body to stdout, ensuring a trailing newline if the
/// content did not end with one. Mirrors `drain_streamed_chunks`'s newline
/// behaviour so streamed and unary handlers produce byte-identical output.
fn print_diff_body(content: &[u8]) {
    let s = String::from_utf8_lossy(content);
    print!("{s}");
    if !s.ends_with('\n') {
        println!();
    }
}

/// Shared CLI surface for `diff-unified` (streamed) and `diff-unified-unary`.
/// The two commands diverge only in which client method is called and how
/// the response body is read, so all input/option parsing lives here.
#[derive(Debug, Args)]
struct UnifiedDiffArgs {
    #[arg(long, short = 'R')]
    repo_name: String,
    #[arg(long)]
    base_content_id: Option<String>,
    #[arg(long)]
    other_content_id: Option<String>,
    #[arg(long, alias = "base-changeset-id")]
    base_commit_id: Option<String>,
    #[arg(long)]
    base_path: Option<String>,
    #[arg(long, alias = "other-changeset-id")]
    other_commit_id: Option<String>,
    #[arg(long)]
    other_path: Option<String>,
    #[arg(long, help = "Base string content")]
    base_string: Option<String>,
    #[arg(long, help = "Other string content")]
    other_string: Option<String>,
    #[arg(long, default_value_t = 3)]
    context_lines: i32,
    #[arg(long, default_value = "inline")]
    diff_mode: String,
    #[arg(long, default_value = "regular")]
    file_type: String,
    #[arg(long, default_value = "none")]
    copy_info: String,
    #[arg(long)]
    replacement_base_path: Option<String>,
    #[arg(long)]
    inspect_lfs_pointers: bool,
    /// Currently a no-op; preserved for CLI compatibility.
    /// `DiffUnifiedRequest` has no matching server-side option.
    #[arg(long)]
    inspect_binary_diffs: bool,
    #[arg(long)]
    base_lfs_sha256: Option<String>,
    #[arg(long)]
    base_lfs_size: Option<i64>,
    #[arg(long)]
    other_lfs_sha256: Option<String>,
    #[arg(long)]
    other_lfs_size: Option<i64>,
    #[arg(long, help = "Path for base content (optional when using content IDs)")]
    base_content_path: Option<String>,
    #[arg(
        long,
        help = "Path for other content (optional when using content IDs)"
    )]
    other_content_path: Option<String>,
    #[arg(long, help = "Ignore whitespace changes")]
    ignore_whitespace: bool,
    #[arg(long, value_enum, default_value_t = CommitScheme::Bonsai)]
    commit_scheme: CommitScheme,
}

impl UnifiedDiffArgs {
    /// Consume the args and produce the four pieces both `DiffUnified` and
    /// `DiffUnifiedUnary` handlers need: repo name, base/other inputs, options.
    fn into_request_parts(
        self,
    ) -> Result<(
        String,
        Option<DiffInput>,
        Option<DiffInput>,
        thrift::DiffUnifiedOptions,
    )> {
        let UnifiedDiffArgs {
            repo_name,
            base_content_id,
            other_content_id,
            base_commit_id,
            base_path,
            other_commit_id,
            other_path,
            base_string,
            other_string,
            context_lines,
            diff_mode,
            file_type,
            copy_info,
            replacement_base_path,
            inspect_lfs_pointers,
            inspect_binary_diffs: _,
            base_lfs_sha256,
            base_lfs_size,
            other_lfs_sha256,
            other_lfs_size,
            base_content_path,
            other_content_path,
            ignore_whitespace,
            commit_scheme,
        } = self;

        let options = thrift::DiffUnifiedOptions {
            context_lines,
            diff_mode: parse_diff_mode(&diff_mode)?,
            file_type: parse_file_type(&file_type)?,
            copy_info: parse_copy_info(&copy_info)?,
            inspect_lfs_pointers,
            ignore_whitespace,
            ..Default::default()
        };

        let base_input = if let Some(content_id_str) = base_content_id {
            let content_id = ContentId::from_str(&content_id_str)?;
            let lfs_pointer = parse_lfs_pointer("base", base_lfs_sha256, base_lfs_size)?;
            Some(DiffInput::Content {
                content_id,
                path: base_content_path,
                lfs_pointer,
            })
        } else if let (Some(commit_id_str), Some(path)) = (base_commit_id, base_path) {
            let commit_id = commit_id_to_thrift(parse_commit_id(&commit_id_str, commit_scheme)?)?;
            Some(DiffInput::CommitPath {
                commit_id,
                path,
                replacement_path: replacement_base_path,
            })
        } else if let Some(content) = base_string {
            Some(DiffInput::string(content))
        } else {
            return Err(anyhow::anyhow!(
                "Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string"
            ));
        };

        let other_input = if let Some(content_id_str) = other_content_id {
            let content_id = ContentId::from_str(&content_id_str)?;
            let lfs_pointer = parse_lfs_pointer("other", other_lfs_sha256, other_lfs_size)?;
            Some(DiffInput::Content {
                content_id,
                path: other_content_path,
                lfs_pointer,
            })
        } else if let (Some(commit_id_str), Some(path)) = (other_commit_id, other_path) {
            let commit_id = commit_id_to_thrift(parse_commit_id(&commit_id_str, commit_scheme)?)?;
            Some(DiffInput::commit_path(commit_id, path))
        } else if let Some(content) = other_string {
            Some(DiffInput::string(content))
        } else {
            return Err(anyhow::anyhow!(
                "Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string"
            ));
        };

        Ok((repo_name, base_input, other_input, options))
    }
}

/// Shared CLI surface for `diff-unified-headerless` (streamed) and
/// `diff-unified-headerless-unary`. Smaller surface than `UnifiedDiffArgs`:
/// no LFS pointer / file_type / diff_mode / replacement_path knobs.
#[derive(Debug, Args)]
struct HeaderlessDiffArgs {
    #[arg(long, short = 'R')]
    repo_name: String,
    #[arg(long)]
    base_content_id: Option<String>,
    #[arg(long)]
    other_content_id: Option<String>,
    #[arg(long, alias = "base-changeset-id")]
    base_commit_id: Option<String>,
    #[arg(long)]
    base_path: Option<String>,
    #[arg(long, alias = "other-changeset-id")]
    other_commit_id: Option<String>,
    #[arg(long)]
    other_path: Option<String>,
    #[arg(long, help = "Base string content")]
    base_string: Option<String>,
    #[arg(long, help = "Other string content")]
    other_string: Option<String>,
    #[arg(long, default_value_t = 3)]
    context_lines: i32,
    #[arg(long)]
    inspect_binary_data: bool,
    #[arg(long, help = "Ignore whitespace changes")]
    ignore_whitespace: bool,
    #[arg(
        long,
        help = "Send options: None on the request to exercise the server's default-options path"
    )]
    omit_options: bool,
    #[arg(long, value_enum, default_value_t = CommitScheme::Bonsai)]
    commit_scheme: CommitScheme,
}

impl HeaderlessDiffArgs {
    /// Consume the args and produce the four pieces both
    /// `DiffUnifiedHeaderless` and `DiffUnifiedHeaderlessUnary` handlers need.
    /// Options are returned as `Option` so `--omit-options` can send `None`.
    fn into_request_parts(
        self,
    ) -> Result<(
        String,
        Option<DiffInput>,
        Option<DiffInput>,
        Option<thrift::DiffUnifiedHeaderlessOptions>,
    )> {
        let HeaderlessDiffArgs {
            repo_name,
            base_content_id,
            other_content_id,
            base_commit_id,
            base_path,
            other_commit_id,
            other_path,
            base_string,
            other_string,
            context_lines,
            inspect_binary_data,
            ignore_whitespace,
            omit_options,
            commit_scheme,
        } = self;

        let options = if omit_options {
            None
        } else {
            Some(thrift::DiffUnifiedHeaderlessOptions {
                context_lines,
                inspect_binary_data,
                ignore_whitespace,
                ..Default::default()
            })
        };

        let base_input = if let Some(content_id_str) = base_content_id {
            let content_id = ContentId::from_str(&content_id_str)?;
            Some(DiffInput::content(content_id))
        } else if let (Some(commit_id_str), Some(path)) = (base_commit_id, base_path) {
            let commit_id = commit_id_to_thrift(parse_commit_id(&commit_id_str, commit_scheme)?)?;
            Some(DiffInput::commit_path(commit_id, path))
        } else if let Some(content) = base_string {
            Some(DiffInput::string(content))
        } else {
            return Err(anyhow::anyhow!(
                "Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string"
            ));
        };

        let other_input = if let Some(content_id_str) = other_content_id {
            let content_id = ContentId::from_str(&content_id_str)?;
            Some(DiffInput::content(content_id))
        } else if let (Some(commit_id_str), Some(path)) = (other_commit_id, other_path) {
            let commit_id = commit_id_to_thrift(parse_commit_id(&commit_id_str, commit_scheme)?)?;
            Some(DiffInput::commit_path(commit_id, path))
        } else if let Some(content) = other_string {
            Some(DiffInput::string(content))
        } else {
            return Err(anyhow::anyhow!(
                "Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string"
            ));
        };

        Ok((repo_name, base_input, other_input, options))
    }
}

#[derive(Debug, Parser)]
#[clap(about = "Thin CLI for the Diff Service (used for testing and debug)")]
struct DiffServiceClientArgs {
    #[arg(long)]
    host: Option<String>,
    #[cfg(fbcode_build)]
    #[clap(long, conflicts_with = "host")]
    tier: Option<String>,
    #[clap(flatten, next_help_heading = "LOGGING OPTIONS")]
    logging_args: LoggingArgs,
    #[clap(subcommand)]
    command: Command,
}

#[derive(Debug, Parser)]
enum Command {
    DiffHunks(DiffHunksArgs),
    DiffUnified(UnifiedDiffArgs),
    DiffUnifiedHeaderless(HeaderlessDiffArgs),
    DiffUnifiedUnary(UnifiedDiffArgs),
    DiffUnifiedHeaderlessUnary(HeaderlessDiffArgs),
    MetadataDiff {
        #[arg(long, short = 'R')]
        repo_name: String,
        #[arg(long)]
        base_content_id: Option<String>,
        #[arg(long)]
        other_content_id: Option<String>,
        #[arg(long, alias = "base-changeset-id")]
        base_commit_id: Option<String>,
        #[arg(long)]
        base_path: Option<String>,
        #[arg(long, alias = "other-changeset-id")]
        other_commit_id: Option<String>,
        #[arg(long)]
        other_path: Option<String>,
        #[arg(long, help = "Base string content")]
        base_string: Option<String>,
        #[arg(long, help = "Other string content")]
        other_string: Option<String>,
        #[arg(long, help = "Ignore whitespace changes")]
        ignore_whitespace: bool,
        #[arg(long, value_enum, default_value_t = CommitScheme::Bonsai)]
        commit_scheme: CommitScheme,
    },
    CommitCompare {
        #[arg(long, short = 'R')]
        repo_name: String,
        #[arg(long, alias = "commit-id", help = "Base commit ID")]
        commit_id: String,
        #[arg(
            long,
            alias = "other-commit-id",
            help = "Other commit ID (defaults to parent)"
        )]
        other_commit_id: Option<String>,
        #[arg(long, value_enum, default_value_t = CommitScheme::Bonsai)]
        commit_scheme: CommitScheme,
        #[arg(long, help = "Only list changed file paths")]
        paths_only: bool,
        #[arg(long, help = "Show copies and renames as plain adds/deletes")]
        skip_copies_renames: bool,
        #[arg(long, help = "Include tree (directory) changes in comparison")]
        compare_trees: bool,
    },
    Status,
}
#[derive(Debug, Parser)]
struct DiffHunksArgs {
    #[arg(long, short = 'R')]
    repo_name: String,

    #[arg(long, help = "Base content ID")]
    base_content_id: Option<String>,
    #[arg(
        long,
        alias = "base-changeset-id",
        help = "Base commit ID",
        requires = "base_path"
    )]
    base_commit_id: Option<String>,
    #[arg(long, help = "Base path", requires = "base_commit_id")]
    base_path: Option<String>,
    #[arg(long, help = "Base string content")]
    base_string: Option<String>,

    // Other input - can be either content ID or changeset+path
    #[arg(long, help = "Other content ID")]
    other_content_id: Option<String>,
    #[arg(
        long,
        alias = "other-changeset-id",
        help = "Other commit ID",
        requires = "other_path"
    )]
    other_commit_id: Option<String>,
    #[arg(long, help = "Other path", requires = "other_commit_id")]
    other_path: Option<String>,

    #[arg(long, help = "Other string content")]
    other_string: Option<String>,

    #[arg(long, help = "Ignore whitespace changes")]
    ignore_whitespace: bool,

    #[arg(long, value_enum, default_value_t = CommitScheme::Bonsai)]
    commit_scheme: CommitScheme,
}

#[fbinit::main]
async fn main(fb: FacebookInit) -> Result<()> {
    let args = DiffServiceClientArgs::parse();
    args.logging_args.setup_logging(fb)?;
    platform::setup(fb);

    let ctx = CoreContext::new_with_client_info(
        fb,
        ClientInfo::default_with_entry_point(ClientEntryPoint::DiffService),
    );

    let client = platform::connect(fb, &args)?;

    match args.command {
        Command::DiffHunks(DiffHunksArgs {
            repo_name,
            base_content_id,
            other_content_id,
            base_commit_id,
            base_path,
            base_string,
            other_commit_id,
            other_path,
            other_string,
            ignore_whitespace,
            commit_scheme,
        }) => {
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let base_input = if let Some(base_content_id) = base_content_id {
                let content_id = ContentId::from_str(&base_content_id)?;
                Some(DiffInput::content(content_id))
            } else if let (Some(base_commit_id), Some(base_path)) = (base_commit_id, base_path) {
                let commit_id =
                    commit_id_to_thrift(parse_commit_id(&base_commit_id, commit_scheme)?)?;
                Some(DiffInput::commit_path(commit_id, base_path))
            } else if let Some(base_string) = base_string {
                Some(DiffInput::string(base_string))
            } else {
                return Err(anyhow::anyhow!(
                    "Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string"
                ));
            };

            let other_input = if let Some(other_content_id) = other_content_id {
                let content_id = ContentId::from_str(&other_content_id)?;
                Some(DiffInput::content(content_id))
            } else if let (Some(other_commit_id), Some(other_path)) = (other_commit_id, other_path)
            {
                let commit_id =
                    commit_id_to_thrift(parse_commit_id(&other_commit_id, commit_scheme)?)?;
                Some(DiffInput::commit_path(commit_id, other_path))
            } else if let Some(other_string) = other_string {
                Some(DiffInput::string(other_string))
            } else {
                return Err(anyhow::anyhow!(
                    "Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string"
                ));
            };

            let (response, mut stream) = repo_client
                .diff_hunks(&ctx, base_input, other_input, ignore_whitespace)
                .await?;

            println!("Hunks output (num_hunks: {}):", response.num_hunks);
            while let Some(chunk_result) = stream.next().await {
                match chunk_result {
                    Ok(hunk_data) => {
                        println!(
                            "Hunk: add_range({}-{}), remove_range({}-{})",
                            hunk_data.add_range.start,
                            hunk_data.add_range.end,
                            hunk_data.remove_range.start,
                            hunk_data.remove_range.end
                        );
                    }
                    Err(e) => {
                        eprintln!("Error reading hunk data: {e}");
                        break;
                    }
                }
            }
            println!("=== End of hunks ===");
            Ok(())
        }
        Command::DiffUnified(args) => {
            let (repo_name, base_input, other_input, options) = args.into_request_parts()?;
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let (response, stream) = repo_client
                .diff_unified(&ctx, base_input, other_input, options)
                .await?;

            println!("Unified diff output (is_binary: {}):", response.is_binary);
            drain_streamed_chunks("unified diff", stream, |c| &c.content).await;
            println!("=== End of unified diff ===");
            Ok(())
        }
        Command::DiffUnifiedHeaderless(args) => {
            let (repo_name, base_input, other_input, options) = args.into_request_parts()?;
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let (response, stream) = repo_client
                .diff_unified_headerless(&ctx, base_input, other_input, options)
                .await?;

            println!(
                "Headerless diff output (is_binary: {}, num_chunks: {}):",
                response.is_binary, response.num_chunks
            );
            drain_streamed_chunks("headerless diff", stream, |c| &c.content).await;
            println!("=== End of headerless diff ===");
            Ok(())
        }
        Command::DiffUnifiedUnary(args) => {
            let (repo_name, base_input, other_input, options) = args.into_request_parts()?;
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let response = repo_client
                .diff_unified_unary(&ctx, base_input, other_input, options)
                .await?;

            println!(
                "Unary unified diff output (is_binary: {}):",
                response.is_binary
            );
            print_diff_body(&response.content);
            println!("=== End of unary unified diff ===");
            Ok(())
        }
        Command::DiffUnifiedHeaderlessUnary(args) => {
            let (repo_name, base_input, other_input, options) = args.into_request_parts()?;
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let response = repo_client
                .diff_unified_headerless_unary(&ctx, base_input, other_input, options)
                .await?;

            println!(
                "Unary headerless diff output (is_binary: {}):",
                response.is_binary
            );
            print_diff_body(&response.content);
            println!("=== End of unary headerless diff ===");
            Ok(())
        }
        Command::MetadataDiff {
            repo_name,
            base_content_id,
            other_content_id,
            base_commit_id,
            base_path,
            other_commit_id,
            other_path,
            base_string,
            other_string,
            ignore_whitespace,
            commit_scheme,
        } => {
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let base_input = if let Some(base_content_id) = base_content_id {
                let content_id = ContentId::from_str(&base_content_id)?;
                Some(DiffInput::content(content_id))
            } else if let (Some(base_commit_id), Some(base_path)) = (base_commit_id, base_path) {
                let commit_id =
                    commit_id_to_thrift(parse_commit_id(&base_commit_id, commit_scheme)?)?;
                Some(DiffInput::commit_path(commit_id, base_path))
            } else if let Some(base_string) = base_string {
                Some(DiffInput::string(base_string))
            } else {
                return Err(anyhow::anyhow!(
                    "Must provide either --base-content-id, (--base-commit-id and --base-path), or --base-string"
                ));
            };

            let other_input = if let Some(other_content_id) = other_content_id {
                let content_id = ContentId::from_str(&other_content_id)?;
                Some(DiffInput::content(content_id))
            } else if let (Some(other_commit_id), Some(other_path)) = (other_commit_id, other_path)
            {
                let commit_id =
                    commit_id_to_thrift(parse_commit_id(&other_commit_id, commit_scheme)?)?;
                Some(DiffInput::commit_path(commit_id, other_path))
            } else if let Some(other_string) = other_string {
                Some(DiffInput::string(other_string))
            } else {
                return Err(anyhow::anyhow!(
                    "Must provide either --other-content-id, (--other-commit-id and --other-path), or --other-string"
                ));
            };

            let response = repo_client
                .metadata_diff(&ctx, base_input, other_input, ignore_whitespace)
                .await?;

            println!("Metadata diff output:");

            // Print base file info
            println!("Base file info:");
            if let Some(file_type) = response.base_file_info.file_type {
                println!("  File type: {}", format_file_type(&file_type));
            }
            if let Some(content_type) = response.base_file_info.content_type {
                println!("  Content type: {}", format_content_type(&content_type));
            }
            if let Some(generated_status) = response.base_file_info.generated_status {
                println!(
                    "  Generated status: {}",
                    format_generated_status(&generated_status)
                );
            }

            // Print other file info
            println!("Other file info:");
            if let Some(file_type) = response.other_file_info.file_type {
                println!("  File type: {}", format_file_type(&file_type));
            }
            if let Some(content_type) = response.other_file_info.content_type {
                println!("  Content type: {}", format_content_type(&content_type));
            }
            if let Some(generated_status) = response.other_file_info.generated_status {
                println!(
                    "  Generated status: {}",
                    format_generated_status(&generated_status)
                );
            }

            // Print lines count if available
            if let Some(lines_count) = response.lines_count {
                println!("Lines count:");
                println!("  Added lines: {}", lines_count.added_lines);
                println!("  Deleted lines: {}", lines_count.deleted_lines);
                println!(
                    "  Significant added lines: {}",
                    lines_count.significant_added_lines
                );
                println!(
                    "  Significant deleted lines: {}",
                    lines_count.significant_deleted_lines
                );
                if let Some(first_added_line) = lines_count.first_added_line_number {
                    println!("  First added line number: {first_added_line}");
                }
            } else {
                println!("Lines count: Not available (non-text files)");
            }

            println!("=== End of metadata diff ===");
            Ok(())
        }
        Command::CommitCompare {
            repo_name,
            commit_id,
            other_commit_id,
            commit_scheme,
            paths_only,
            skip_copies_renames,
            compare_trees,
        } => {
            let repo_client = RepoDiffServiceClient::new(repo_name, client);

            let base_commit = commit_id_to_thrift(parse_commit_id(&commit_id, commit_scheme)?)?;

            let other_commit = other_commit_id
                .map(|id| {
                    let parsed = parse_commit_id(&id, commit_scheme)?;
                    commit_id_to_thrift(parsed)
                })
                .transpose()?;

            let mut compare_items =
                std::collections::BTreeSet::from([source_control_thrift::CommitCompareItem::FILES]);
            if compare_trees {
                compare_items.insert(source_control_thrift::CommitCompareItem::TREES);
            }

            let params = source_control_thrift::CommitCompareParams {
                other_commit_id: other_commit,
                skip_copies_renames,
                compare_items,
                ..Default::default()
            };

            let response = repo_client
                .commit_compare(&ctx, base_commit, params)
                .await?;

            if paths_only {
                for file in &response.diff_files {
                    match (file.base_file.as_ref(), file.other_file.as_ref()) {
                        (Some(base_file), None) => println!("A {}", base_file.path),
                        (None, Some(other_file)) => println!("D {}", other_file.path),
                        (Some(base_file), Some(other_file)) => match file.copy_info {
                            source_control_thrift::CopyInfo::NONE => {
                                println!("M {}", base_file.path)
                            }
                            source_control_thrift::CopyInfo::MOVE => {
                                println!("R {} -> {}", other_file.path, base_file.path)
                            }
                            source_control_thrift::CopyInfo::COPY => {
                                println!("C {} -> {}", other_file.path, base_file.path)
                            }
                            _ => {
                                return Err(anyhow::anyhow!("Unrecognized CopyInfo"));
                            }
                        },
                        (None, None) => {
                            return Err(anyhow::anyhow!("Empty file pair in response"));
                        }
                    }
                }
                for tree in &response.diff_trees {
                    match (tree.base_tree.as_ref(), tree.other_tree.as_ref()) {
                        (Some(base_tree), None) => println!("A {} (tree)", base_tree.path),
                        (None, Some(other_tree)) => println!("D {} (tree)", other_tree.path),
                        (Some(base_tree), Some(_)) => println!("M {} (tree)", base_tree.path),
                        (None, None) => {
                            return Err(anyhow::anyhow!("Empty tree pair in response"));
                        }
                    }
                }
            } else {
                println!("Changed files ({}):", response.diff_files.len());
                for file in &response.diff_files {
                    match (file.base_file.as_ref(), file.other_file.as_ref()) {
                        (Some(base_file), None) => println!("  A {}", base_file.path),
                        (None, Some(other_file)) => println!("  D {}", other_file.path),
                        (Some(base_file), Some(other_file)) => match file.copy_info {
                            source_control_thrift::CopyInfo::NONE => {
                                println!("  M {}", base_file.path)
                            }
                            source_control_thrift::CopyInfo::MOVE => {
                                println!("  R {} -> {}", other_file.path, base_file.path)
                            }
                            source_control_thrift::CopyInfo::COPY => {
                                println!("  C {} -> {}", other_file.path, base_file.path)
                            }
                            _ => {
                                return Err(anyhow::anyhow!("Unrecognized CopyInfo"));
                            }
                        },
                        (None, None) => {
                            return Err(anyhow::anyhow!("Empty file pair in response"));
                        }
                    }
                }
                if !response.diff_trees.is_empty() {
                    println!("Changed trees ({}):", response.diff_trees.len());
                    for tree in &response.diff_trees {
                        match (tree.base_tree.as_ref(), tree.other_tree.as_ref()) {
                            (Some(base_tree), None) => {
                                println!("  A {} (tree)", base_tree.path)
                            }
                            (None, Some(other_tree)) => {
                                println!("  D {} (tree)", other_tree.path)
                            }
                            (Some(base_tree), Some(_)) => {
                                println!("  M {} (tree)", base_tree.path)
                            }
                            (None, None) => {
                                return Err(anyhow::anyhow!("Empty tree pair in response"));
                            }
                        }
                    }
                }
                println!("Other commit IDs:");
                if let Some(other_ids) = &response.other_commit_ids {
                    for (scheme, id) in other_ids {
                        println!("  {scheme:?}: {id:?}");
                    }
                }
            }
            Ok(())
        }
        Command::Status => {
            let status = client.status().await?;
            println!("Service status: {status:?}");
            Ok(())
        }
    }
}
