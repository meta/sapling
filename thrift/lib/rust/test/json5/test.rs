/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Runs the JSON5 cases that C++ and Python share, from
//! `thrift/test/json5/json5_test.thrift` and `json5_negative_test.thrift`.

use std::collections::BTreeSet;

use fbthrift::json5_protocol;
use fbthrift::json5_protocol::Json5WriterOptions;
use json5_negative_test::consts::enumValidationNegativeCases;
use json5_negative_test::consts::formatValidationNegativeCases;
use json5_negative_test::consts::invalidUtf8NegativeCases;
use json5_negative_test::consts::overflowValidationNegativeCases;
use json5_negative_test::consts::typeMismatchNegativeCases;
use json5_negative_test::consts::typeValidationNegativeCases;
use json5_test::Example;
use json5_test::consts::testCases;

/// Cases of `testCases` that Rust does not pass yet.
const KNOWN_FAILURES: &[&str] = &[
    // Rejects nonzero trailing bits in base64.
    "BinaryBareBase64OneByte",
    // Lays out floating-point values as serde_json does.
    "DoubleFormatting",
    "FloatAsKeyFromInteger",
    "FloatAsKeyFromString",
    "FloatFormatting",
    "FloatingPointKeys",
    "MoreDoubleFormatting",
    "MoreFloatFormatting",
    "NegativeLargestExactIntegers",
    // Reads an integer `-0` as `0.0`.
    "NegativeZero",
    "NegativeZeroKey",
    "SignsOfZeroAndNaNInDoubleList",
];

/// Negative cases that Rust does not reject yet.
const KNOWN_ACCEPTED: &[&str] = &[
    // Reads a union field after an unknown one.
    "EmptyUnionMultipleFields",
    "UnionUnknownThenKnownField",
    // Reads `"."` as `0.0`.
    "FloatTrailingDotOnlyQuoted",
    // Skips a leading byte order mark as whitespace.
    "Utf8Bom",
];

fn write(value: &Example, options: Json5WriterOptions) -> String {
    let options = options.with_indent_width(2).expect("supported indentation");
    let encoded = json5_protocol::serialize_with_options(value, options);
    String::from_utf8(encoded.to_vec()).expect("the writer emits UTF-8")
}

/// Checks that exactly the `known` cases fail, given each failure as a case
/// name and a message.
fn assert_failures_are_known(failures: &[(&str, String)], known: &[&str], list: &str) {
    let failing: BTreeSet<&str> = failures.iter().map(|(name, _)| *name).collect();
    let unexpected: Vec<&str> = failures
        .iter()
        .filter(|(name, _)| !known.contains(name))
        .map(|(_, message)| message.as_str())
        .collect();
    let passing: Vec<&str> = known
        .iter()
        .copied()
        .filter(|name| !failing.contains(name))
        .collect();
    assert!(
        unexpected.is_empty() && passing.is_empty(),
        "failing cases not in `{list}`:\n{}\npassing cases still in `{list}`: {passing:?}",
        unexpected.join("\n"),
    );
}

#[test]
fn decodes_and_writes_shared_cases() {
    let failures: Vec<(&str, String)> = testCases
        .iter()
        .flat_map(|case| {
            [&case.json, &case.json5]
                .into_iter()
                .chain(&case.additionalInputs)
                .filter_map(move |input| {
                    let decoded = match json5_protocol::deserialize::<Example, _, _>(input.as_str())
                    {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            return Some((
                                case.name.as_str(),
                                format!("{}: {input:?} fails: {error:#}", case.name),
                            ));
                        }
                    };
                    let json = write(&decoded, Json5WriterOptions::basic_json());
                    let json5 = write(&decoded, Json5WriterOptions::json5());
                    let mismatches = [
                        (json != case.json).then(|| format!("JSON {json:?}")),
                        (json5 != case.json5).then(|| format!("JSON5 {json5:?}")),
                        case.example
                            .as_ref()
                            .filter(|example| **example != decoded)
                            .map(|_| "a value other than `example`".to_owned()),
                    ];
                    let mismatches: Vec<String> = mismatches.into_iter().flatten().collect();
                    (!mismatches.is_empty()).then(|| {
                        (
                            case.name.as_str(),
                            format!(
                                "{}: {input:?} gives {}",
                                case.name,
                                mismatches.join(" and ")
                            ),
                        )
                    })
                })
        })
        .collect();

    assert_failures_are_known(&failures, KNOWN_FAILURES, "KNOWN_FAILURES");
}

#[test]
fn rejects_shared_negative_cases() {
    let accepted: Vec<(&str, String)> = [
        &*enumValidationNegativeCases,
        &*typeValidationNegativeCases,
        &*typeMismatchNegativeCases,
        &*formatValidationNegativeCases,
        &*overflowValidationNegativeCases,
    ]
    .into_iter()
    .flatten()
    .filter(|case| json5_protocol::deserialize::<Example, _, _>(case.json.as_str()).is_ok())
    .map(|case| {
        (
            case.name.as_str(),
            format!("{}: {:?}", case.name, case.json),
        )
    })
    .chain(
        invalidUtf8NegativeCases
            .iter()
            .filter(|case| json5_protocol::deserialize_slice::<Example>(&case.json).is_ok())
            .map(|case| {
                (
                    case.name.as_str(),
                    format!("{}: {:?}", case.name, case.json),
                )
            }),
    )
    .collect();

    assert_failures_are_known(&accepted, KNOWN_ACCEPTED, "KNOWN_ACCEPTED");
}
