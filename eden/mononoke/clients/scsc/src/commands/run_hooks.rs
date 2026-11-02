/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::io::Write;

#[cfg(all(target_os = "linux", fbcode_build))]
use anyhow::Context;
use anyhow::Result;
use commit_id_types::CommitIdArgs;
#[cfg(all(target_os = "linux", fbcode_build))]
use percent_encoding::percent_decode;
#[cfg(all(target_os = "linux", fbcode_build))]
use permission_checker::MononokeIdentity;
#[cfg(all(target_os = "linux", fbcode_build))]
use permission_checker::MononokeIdentitySet;
use scs_client_raw::thrift;
use serde::Serialize;

use crate::ScscApp;
use crate::args::commit_id::resolve_commit_id;
use crate::args::pushvars::PushvarArgs;
use crate::args::repo::RepoArgs;
use crate::errors::SelectionErrorExt;
use crate::render::Render;

#[derive(clap::Parser)]
/// Run hooks on a commit without pushing it
///
/// Provide a commit and the bookmark you plan to push to.
/// The hooks that would run when you push this commit to bookmark will run now
/// and their outcomes will be reported. A success does NOT guarantee
/// the commit will successfully land (e.g. conflicts may prevent landing).
pub(crate) struct CommandArgs {
    #[clap(flatten)]
    repo_args: RepoArgs,
    #[clap(flatten)]
    commit_id_args: CommitIdArgs,
    #[clap(flatten)]
    pushvar_args: PushvarArgs,
    #[clap(long)]
    /// Name of the bookmark you would push to if pushing for real
    to: String,
    // `--run-as` / `--run-as-encoded` serialize an `AuthenticatedIdentity`
    // envelope with `MononokeIdentity::serialize_thrift_compact_bytes`, which
    // exists only in Linux fbcode builds. Impersonation would be meaningless in
    // OSS regardless: that server rejects caller-supplied identities and runs
    // every request with access control bypassed.
    #[cfg(all(target_os = "linux", fbcode_build))]
    #[clap(long = "run-as", value_name = "TYPE:DATA")]
    /// Run the hooks as if the push was performed by these identities instead
    /// of your own (format: TYPE:data, e.g. USER:alice).
    run_as: Vec<String>,
    #[cfg(all(target_os = "linux", fbcode_build))]
    #[clap(
        long = "run-as-encoded",
        value_name = "ENCODED",
        conflicts_with = "run_as"
    )]
    /// Like --run-as, but takes a percent-encoded JSON identity envelope (the
    /// wire form of the `x-fb-validated-client-encoded-identity` header), e.g.
    /// `%7b%22authn%22%3a%5b%22mid%3a%2f%2fPROD%2fUSER%2falice%22%5d%7d`.
    /// Unlike --run-as this preserves identity attributes, so hooks that
    /// inspect them (e.g. agent taints) see the real thing.
    run_as_encoded: Option<String>,
    #[clap(long, value_name = "MESSAGE")]
    /// Run the hooks as if the commit carried this message instead of its
    /// stored one (e.g. the message it will carry once regenerated at land
    /// time). Only affects this dry run's reported verdicts.
    override_commit_message: Option<String>,
    #[clap(long)]
    /// Include rejections from hooks configured as log-only instead of
    /// showing them as accepted (previews what they would do once
    /// enforcing).
    include_log_only_rejections: bool,
}

/// Build the `run_as` payload from the `--run-as` / `--run-as-encoded` flags,
/// which clap keeps mutually exclusive. Both forms are sent as a
/// compact-encoded `AuthenticatedIdentity` list so identity attributes survive
/// the wire.
#[cfg(all(target_os = "linux", fbcode_build))]
fn run_as_identities(
    run_as: &[String],
    run_as_encoded: Option<&str>,
) -> Result<Option<thrift::RunAsIdentities>> {
    let identities = if let Some(encoded) = run_as_encoded {
        let json = percent_decode(encoded.as_bytes())
            .decode_utf8()
            .context("percent-decoding --run-as-encoded failed")?;
        let identities = MononokeIdentity::try_from_json_encoded(&json)
            .context("parsing the --run-as-encoded identity envelope failed")?;
        if identities.is_empty() {
            anyhow::bail!("--run-as-encoded resolved to no identities");
        }
        identities
    } else if !run_as.is_empty() {
        run_as
            .iter()
            .map(|id| match id.split_once(':') {
                Some((id_type, id_data)) if !id_type.is_empty() && !id_data.is_empty() => {
                    Ok(MononokeIdentity::from_legacy_type_data(id_type, id_data))
                }
                _ => Err(anyhow::anyhow!(
                    "invalid --run-as value '{id}', expected TYPE:data with non-empty TYPE and data"
                )),
            })
            .collect::<Result<MononokeIdentitySet>>()?
    } else {
        return Ok(None);
    };
    Ok(Some(
        thrift::RunAsIdentities::encoded_authenticated_identities(
            MononokeIdentity::serialize_thrift_compact_bytes(&identities),
        ),
    ))
}

#[derive(Serialize)]
#[serde(tag = "status")]
enum HookOutcome {
    Accepted,
    Rejected { reason: String },
}

#[derive(Serialize)]
struct RunHooksOutput {
    commit: String,
    bookmark: String,
    outcomes: BTreeMap<String, HookOutcome>,
}

impl Render for RunHooksOutput {
    type Args = CommandArgs;

    fn render(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        write!(
            w,
            "Hook outcomes when dry-run landing {} to bookmark {}:\n\n",
            self.commit, self.bookmark
        )?;
        for (hook_name, outcome) in &self.outcomes {
            write!(w, "{hook_name} => ")?;
            match outcome {
                HookOutcome::Accepted => write!(w, "ACCEPTED\n")?,
                HookOutcome::Rejected { reason } => write!(w, "REJECTED: {reason}\n")?,
            };
        }
        Ok(())
    }

    fn render_json(&self, _args: &Self::Args, w: &mut dyn Write) -> Result<()> {
        Ok(serde_json::to_writer(w, self)?)
    }
}

pub(crate) async fn run(app: ScscApp, args: CommandArgs) -> Result<()> {
    let repo = args.repo_args.clone().into_repo_specifier();
    let original_commit_id = args.commit_id_args.clone().into_commit_id();
    let conn = app.get_connection(Some(&repo.name)).await?;
    let commit_id = resolve_commit_id(&conn, &repo, &original_commit_id).await?;
    let commit_specifier = thrift::CommitSpecifier {
        id: commit_id,
        repo,
        ..Default::default()
    };
    let bookmark: String = args.to.clone();
    let pushvars = args.pushvar_args.clone().into_pushvars();
    #[cfg(all(target_os = "linux", fbcode_build))]
    let run_as = run_as_identities(&args.run_as, args.run_as_encoded.as_deref())?;
    #[cfg(not(all(target_os = "linux", fbcode_build)))]
    let run_as = None;

    let params = thrift::CommitRunHooksParams {
        bookmark: bookmark.clone(),
        pushvars,
        run_as,
        override_commit_message: args.override_commit_message.clone(),
        include_log_only_rejections: args.include_log_only_rejections.then_some(true),
        ..Default::default()
    };
    let response = conn
        .commit_run_hooks(&commit_specifier, &params)
        .await
        .map_err(|e| e.handle_selection_error(&commit_specifier.repo))?;
    let outcomes = response
        .outcomes
        .into_iter()
        .map(|(name, outcome)| {
            Ok((
                name,
                match outcome {
                    thrift::HookOutcome::accepted(_) => HookOutcome::Accepted,
                    thrift::HookOutcome::rejections(rejs) => HookOutcome::Rejected {
                        reason: rejs
                            .into_iter()
                            .map(|rej| rej.long_description)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    },
                    thrift::HookOutcome::UnknownField(_) => anyhow::bail!("Unknown hook outcome"),
                },
            ))
        })
        .collect::<Result<_>>()?;
    let output = RunHooksOutput {
        commit: original_commit_id.to_string(),
        bookmark,
        outcomes,
    };
    app.target.render_one(&args, output).await
}

#[cfg(all(test, target_os = "linux", fbcode_build))]
mod tests {
    use mononoke_macros::mononoke;
    use permission_checker::MononokeIdentitySetExt;

    use super::*;

    /// Decode what `run_as_identities` put on the wire, so the assertions are
    /// about what the server will actually see.
    fn decode(run_as: Option<thrift::RunAsIdentities>) -> MononokeIdentitySet {
        match run_as.expect("run_as should be set") {
            thrift::RunAsIdentities::encoded_authenticated_identities(bytes) => {
                MononokeIdentity::try_from_thrift_compact_bytes(&bytes).expect("decodes")
            }
            other => panic!("unexpected run_as variant: {other:?}"),
        }
    }

    /// What it tests: neither flag set produces no `run_as`, so the server keeps
    /// using the caller's own identities.
    #[mononoke::test]
    fn no_flags_means_no_run_as() {
        assert!(
            run_as_identities(&[], None)
                .expect("no flags is valid")
                .is_none()
        );
    }

    /// What it tests: `--run-as TYPE:data` still round-trips to the same
    /// identities after the `--run-as-encoded` refactor.
    #[mononoke::test]
    fn typed_run_as_round_trips() {
        let run_as = vec!["USER:alice".to_string(), "MACHINE_TIER:od".to_string()];
        let identities = decode(run_as_identities(&run_as, None).expect("valid"));
        assert_eq!(identities.len(), 2);
        assert_eq!(identities.username(), Some("alice"));
        assert_eq!(identities.hostprefix(), Some("od"));
    }

    /// What it tests: a malformed `--run-as` value is rejected client-side
    /// rather than sent as a half-formed identity.
    #[mononoke::test]
    fn malformed_typed_run_as_is_rejected() {
        assert!(run_as_identities(&["notvalid".to_string()], None).is_err());
        assert!(run_as_identities(&["USER:".to_string()], None).is_err());
        assert!(run_as_identities(&[":alice".to_string()], None).is_err());
    }

    /// What it tests: a percent-encoded JSON envelope is decoded into the full
    /// identity set, including the attributes that `--run-as` cannot express.
    /// Expected: the `agent.id` attribute survives to the compact-encoded wire
    /// form, so a hook inspecting agent taints sees it.
    #[mononoke::test]
    fn encoded_run_as_preserves_attributes() {
        // {"authn":["mid://PROD/USER/alice?agent.id=AGENT%3aclaude_code",
        //           "mid://PROD/MACHINE_TIER/twshared"]}
        let encoded = "%7b%22authn%22%3a%5b%22mid%3a%2f%2fPROD%2fUSER%2falice%3fagent.id%3dAGENT%253aclaude_code%22%2c%22mid%3a%2f%2fPROD%2fMACHINE_TIER%2ftwshared%22%5d%7d";
        let identities = decode(run_as_identities(&[], Some(encoded)).expect("valid"));

        assert_eq!(identities.len(), 2);
        assert_eq!(identities.username(), Some("alice"));
        assert_eq!(identities.hostprefix(), Some("twshared"));
        assert!(
            identities.likely_an_agent(),
            "the agent attribute should survive the encode/decode round trip"
        );
    }

    /// What it tests: an envelope carrying no identities is rejected
    /// client-side rather than running the hooks as nobody.
    #[mononoke::test]
    fn envelope_with_no_identities_is_rejected() {
        // {"authn":[]}
        assert!(run_as_identities(&[], Some("%7b%22authn%22%3a%5b%5d%7d")).is_err());
    }

    /// What it tests: input that is not a valid identity envelope fails with an
    /// error instead of silently running the hooks as nobody.
    #[mononoke::test]
    fn garbage_encoded_run_as_is_rejected() {
        assert!(run_as_identities(&[], Some("not-an-envelope")).is_err());
        // Valid JSON, but not an identity envelope.
        assert!(run_as_identities(&[], Some("%7b%22foo%22%3a1%7d")).is_err());
    }
}
