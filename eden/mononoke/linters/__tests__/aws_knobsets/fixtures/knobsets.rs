/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

const NEW_SERVICE_KNOB: &str = "scm/new_service:through_const";

fn shipped() -> Result<bool, anyhow::Error> {
    justknobs::eval(
        "scm/mononoke:pushrebase_enable_merge_resolution",
        None,
        None,
    )
}

fn shipped_new_mononoke_knobset() -> Result<bool, anyhow::Error> {
    justknobs::eval("scm/mononoke_new_feature:enabled", None, None)
}

fn shipped_by_name() -> Result<bool, anyhow::Error> {
    justknobs::eval("scm/artillery:enable_artillery", None, None)
}

fn not_shipped() -> Result<bool, anyhow::Error> {
    justknobs::eval("scm/new_service:enabled", None, None)
}

fn not_shipped_nested_knobset() -> Result<bool, anyhow::Error> {
    justknobs::eval("scm/mononoke/nested:enabled", None, None)
}

fn not_shipped_name_from_match(multi: bool) -> Result<bool, anyhow::Error> {
    let knob = match multi {
        true => "scm/new_service:multi",
        false => "scm/new_service:single",
    };
    justknobs::eval(knob, None, None)
}

async fn not_shipped_in_macro(rx: tokio::sync::mpsc::Receiver<()>) -> i64 {
    tokio::select! {
        _ = rx.recv() => justknobs::get("scm/new_service:in_macro", None).unwrap_or(0),
    }
}

fn not_on_aws() -> Result<bool, anyhow::Error> {
    justknobs::eval("scm/metagit:mononoke_git_lfs", None, None)
}

fn comments_and_non_knob_strings() -> &'static str {
    // justknobs::eval("scm/new_service:commented_out", None, None)
    /* justknobs::eval("scm/new_service:block_comment", None, None) */
    let _unused = '"'; // justknobs::eval("scm/new_service:trailing", None, None)
    let _raw = r#"not a knob: "scm/new_service:inside_raw" "#;
    "see scm/new_service:in_prose for details"
}

fn suppressed() -> Result<bool, anyhow::Error> {
    // @lint-ignore MONONOKEAWSJK
    justknobs::eval("scm/new_service:suppressed", None, None)
}

#[cfg(test)]
mod tests {
    #[mononoke::test]
    fn reads_in_tests_are_not_reported() {
        let _ = justknobs::eval("scm/new_service:in_test", None, None);
    }
}

#[cfg(test)]
#[allow(unused)]
mod more_tests {
    fn also_not_reported() {
        let _ = justknobs::eval("scm/new_service:in_attributed_test", None, None);
    }
}
