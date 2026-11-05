/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::Result;
use mononoke_macros::mononoke;
use sql::mysql_async::Value;
use sql::mysql_async::prelude::FromValue;

use super::P4_CHANGELIST_EXTRA;
use super::P4_SOURCE_EXTRA;
use super::P4_SOURCE_VALUE;
use crate::BonsaiChangeset;
use crate::BonsaiChangesetMut;
use crate::DateTime;
use crate::P4ChangelistId;

fn create_bonsai(hg_extra: impl IntoIterator<Item = (String, Vec<u8>)>) -> Result<BonsaiChangeset> {
    BonsaiChangesetMut {
        author: "author".into(),
        author_date: DateTime::from_timestamp(0, 0)?,
        message: "message".into(),
        hg_extra: hg_extra.into_iter().collect(),
        ..Default::default()
    }
    .freeze()
}

fn extra(key: &str, value: &str) -> (String, Vec<u8>) {
    (key.to_string(), value.into())
}

#[mononoke::test]
fn test_sql_value_round_trip() {
    let id = P4ChangelistId::new(41002);
    let value = Value::from(id);
    assert_eq!(value, Value::UInt(41002));
    assert_eq!(P4ChangelistId::from_value(value), id);
}

#[mononoke::test]
fn test_from_bcs_reads_back_to_hg_extras() -> Result<()> {
    let id = P4ChangelistId::new(41002);
    let bcs = create_bonsai(id.to_hg_extras())?;
    assert_eq!(P4ChangelistId::from_bcs(&bcs)?, id);
    Ok(())
}

#[mononoke::test]
fn test_from_bcs_rejects_svn_commit() -> Result<()> {
    let bcs = create_bonsai([extra(P4_CHANGELIST_EXTRA, "svn:uuid/path@41002")])?;
    assert!(P4ChangelistId::from_bcs(&bcs).is_err());
    Ok(())
}

#[mononoke::test]
fn test_from_bcs_rejects_missing_source_or_changelist() -> Result<()> {
    let no_source = create_bonsai([extra(P4_CHANGELIST_EXTRA, "41002")])?;
    assert!(P4ChangelistId::from_bcs(&no_source).is_err());
    let wrong_source = create_bonsai([
        extra(P4_SOURCE_EXTRA, "git"),
        extra(P4_CHANGELIST_EXTRA, "41002"),
    ])?;
    assert!(P4ChangelistId::from_bcs(&wrong_source).is_err());
    let no_changelist = create_bonsai([extra(P4_SOURCE_EXTRA, P4_SOURCE_VALUE)])?;
    assert!(P4ChangelistId::from_bcs(&no_changelist).is_err());
    Ok(())
}

#[mononoke::test]
fn test_from_bcs_rejects_malformed_changelist() -> Result<()> {
    for value in [
        "",
        "0",
        "abc",
        "-1",
        "9223372036854775808",
        "p4:depot/path@41002",
    ] {
        let bcs = create_bonsai([
            extra(P4_SOURCE_EXTRA, P4_SOURCE_VALUE),
            extra(P4_CHANGELIST_EXTRA, value),
        ])?;
        assert!(
            P4ChangelistId::from_bcs(&bcs).is_err(),
            "accepted {value:?}"
        );
    }
    Ok(())
}

#[mononoke::test]
fn test_from_hg_extras_reads_the_same_extras() -> Result<()> {
    let id = P4ChangelistId::new(41002);
    let extras = id.to_hg_extras();
    let borrowed = extras
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_slice()));
    assert_eq!(P4ChangelistId::from_hg_extras(borrowed)?, id);
    let error = P4ChangelistId::from_bcs(&create_bonsai([extra(P4_SOURCE_EXTRA, "git")])?)
        .expect_err("not a Perforce import");
    assert!(format!("{error:#}").contains("Bonsai cs"), "{error:#}");
    Ok(())
}
