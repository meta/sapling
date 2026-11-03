/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Mint CATs signed by an in-process test keychain.
//!
//! For use against a Mononoke server started with
//! `--dangerously-skipping-cat-verification-for-tests`, which switches the
//! server's cryptocat into the matching test mode. Tokens minted here
//! don't verify against any production tier.

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use clap::Parser;
use mononoke_app::MononokeApp;

/// Mint one or more CATs for tests using cryptocat's in-process test keychain.
///
/// Reads whitespace-separated rows from stdin, one per token to mint. Each row
/// has three columns:
///
///   <signer>  <verifier>  <token_timeout_seconds>
///
/// where `<signer>` and `<verifier>` are identities in `TYPE:DATA` form and
/// `<token_timeout_seconds>` is a positive integer. Blank lines and lines
/// starting with `#` are skipped.
///
/// Prints the URL-safe base64-encoded `CryptoAuthTokenList` (containing one
/// token per input row, in input order) to stdout — the value that goes into
/// the `x-auth-cats` request header.
#[derive(Parser)]
pub struct CommandArgs {}

#[cfg_attr(
    not(fbcode_build),
    expect(dead_code, reason = "Only the internal build can mint tokens")
)]
#[derive(Clone)]
struct ParsedIdentity {
    id_type: String,
    id_data: String,
}

#[cfg_attr(
    not(fbcode_build),
    expect(dead_code, reason = "Only the internal build can mint tokens")
)]
fn parse_identity(s: &str) -> Result<ParsedIdentity> {
    let (id_type, id_data) = s
        .split_once(':')
        .ok_or_else(|| anyhow!("expected `TYPE:DATA`, got `{s}`"))?;
    if id_type.is_empty() || id_data.is_empty() {
        return Err(anyhow!(
            "identity TYPE and DATA must be non-empty, got `{s}`"
        ));
    }
    Ok(ParsedIdentity {
        id_type: id_type.to_owned(),
        id_data: id_data.to_owned(),
    })
}

#[cfg_attr(
    not(fbcode_build),
    expect(dead_code, reason = "Only the internal build can mint tokens")
)]
struct Row {
    signer: ParsedIdentity,
    verifier: ParsedIdentity,
    token_timeout_seconds: u64,
}

#[cfg_attr(
    not(fbcode_build),
    expect(dead_code, reason = "Only the internal build can mint tokens")
)]
fn parse_rows(input: &str) -> Result<Vec<Row>> {
    let rows = input
        .lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                None
            } else {
                Some((i + 1, line))
            }
        })
        .map(|(lineno, line)| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() != 3 {
                return Err(anyhow!(
                    "line {lineno}: expected 3 whitespace-separated columns \
                     (signer verifier token_timeout_seconds), got {}: `{line}`",
                    cols.len(),
                ));
            }
            let signer =
                parse_identity(cols[0]).with_context(|| format!("line {lineno}: signer"))?;
            let verifier =
                parse_identity(cols[1]).with_context(|| format!("line {lineno}: verifier"))?;
            let token_timeout_seconds = cols[2]
                .parse::<u64>()
                .with_context(|| format!("line {lineno}: token_timeout_seconds `{}`", cols[2]))?;
            Ok(Row {
                signer,
                verifier,
                token_timeout_seconds,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Err(anyhow!(
            "no rows on stdin (expected at least one row of: \
             signer verifier token_timeout_seconds)"
        ));
    }
    Ok(rows)
}

#[cfg(fbcode_build)]
pub async fn run(app: MononokeApp, _args: CommandArgs) -> Result<()> {
    use std::io::Read;
    use std::time::Duration;

    use cryptocat::CATOptionsBuilder;
    use cryptocat::CryptoAuthTokenList;

    cryptocat::enable_test_mode();

    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading rows from stdin")?;
    let rows = parse_rows(&input)?;

    let tokens = rows
        .into_iter()
        .map(|row| -> Result<_> {
            let signer = cryptocat::Identity {
                id_type: row.signer.id_type,
                id_data: row.signer.id_data,
                ..Default::default()
            };
            let verifier = cryptocat::Identity {
                id_type: row.verifier.id_type,
                id_data: row.verifier.id_data,
                ..Default::default()
            };
            let mut builder = CATOptionsBuilder::default();
            builder.token_timeout(Duration::from_secs(row.token_timeout_seconds));
            let options = builder
                .build()
                .map_err(|e| anyhow!("failed to build CAT options: {e}"))?;
            let token = cryptocat::get_crypto_auth_token(app.fb, &signer, &verifier, options)
                .context("failed to mint CAT")?;
            Ok(token)
        })
        .collect::<Result<Vec<_>>>()?;

    let list = CryptoAuthTokenList {
        tokens,
        ..Default::default()
    };
    let encoded = cryptocat::serialize_crypto_auth_tokens(&list)?;
    println!("{encoded}");
    Ok(())
}

#[cfg(not(fbcode_build))]
pub async fn run(_app: MononokeApp, _args: CommandArgs) -> Result<()> {
    anyhow::bail!("cat-mint is only available in fbcode builds (cryptocat is fbcode-only)");
}
