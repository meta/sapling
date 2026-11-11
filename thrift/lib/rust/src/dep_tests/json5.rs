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

use std::collections::BTreeMap;

use fbthrift::GetTType;
use fbthrift::ProtocolWriter;
use fbthrift::Serialize;
use fbthrift::TType;
use fbthrift::json5_protocol;
use fbthrift::json5_protocol::Json5WriterOptions;
use fbthrift_test_if::Basic;
use fbthrift_test_if::Containers;
use fbthrift_test_if::En;
use fbthrift_test_if::MainStruct;
use fbthrift_test_if::Small;
use fbthrift_test_if::SubStruct;
use fbthrift_test_if::Un;
use fbthrift_test_if::UnOne;
struct UnionMap(Vec<(Un, i32)>);

impl GetTType for UnionMap {
    const TTYPE: TType = TType::Map;
}

impl<P: ProtocolWriter> Serialize<P> for UnionMap {
    fn rs_thrift_write(&self, protocol: &mut P) {
        protocol.write_map_begin(Un::TTYPE, i32::TTYPE, self.0.len());
        for (key, value) in &self.0 {
            protocol.write_map_key_begin();
            key.rs_thrift_write(protocol);
            protocol.write_map_value_begin();
            value.rs_thrift_write(protocol);
        }
        protocol.write_map_end();
    }
}

#[test]
fn generated_struct_round_trips_with_canonical_shapes() {
    let value = MainStruct {
        foo: "foo".to_owned(),
        m: BTreeMap::from([("b".to_owned(), 2), ("a".to_owned(), 1)]),
        bar: "bar".to_owned(),
        s: SubStruct {
            optDef: Some("optional".to_owned()),
            req_def: "required".to_owned(),
            key_map: Some(BTreeMap::from([(
                Small {
                    num: 10,
                    two: 2,
                    ..Default::default()
                },
                1,
            )])),
            bin: b"hello\0".to_vec(),
            ..Default::default()
        },
        l: vec![Small {
            num: 2,
            two: 10,
            ..Default::default()
        }],
        u: Un::un1(UnOne {
            one: 7,
            ..Default::default()
        }),
        e: En::TWO,
        int_keys: BTreeMap::from([(10, 1), (2, 2)]),
        opt: None,
        ..Default::default()
    };

    let encoded = json5_protocol::serialize_ref(&value);
    let decoded: MainStruct = json5_protocol::deserialize(&encoded).expect("valid JSON5 output");

    assert_eq!(decoded, value);
    assert_eq!(
        encoded,
        "{\"foo\":\"foo\",\"m\":{\"a\":1,\"b\":2},\"bar\":\"bar\",\"s\":{\"optDef\":\"optional\",\"req_def\":\"required\",\"key_map\":[{\"key\":{\"num\":10,\"two\":2},\"value\":1}],\"bin\":{\"base64url\":\"aGVsbG8A\"}},\"l\":[{\"num\":2,\"two\":10}],\"u\":{\"un1\":{\"one\":7}},\"e\":\"TWO (2)\",\"int_keys\":[{\"key\":2,\"value\":2},{\"key\":10,\"value\":1}]}"
    );
}

#[test]
fn generated_struct_round_trips_binary_as_base64_string() {
    let value = SubStruct {
        bin: vec![0xfb, 0xff],
        ..Default::default()
    };
    let encoded = json5_protocol::serialize_with_options(
        &value,
        Json5WriterOptions::basic_json().with_binary_as_base64_string(true),
    );
    let json: serde_json::Value =
        serde_json::from_slice(&encoded).expect("basic JSON output must be valid JSON");
    assert_eq!(json["bin"], "+/8");
    let decoded: SubStruct =
        json5_protocol::deserialize(&encoded).expect("generated binary field must round-trip");
    assert_eq!(decoded, value);
}

#[test]
fn generated_struct_round_trips_nested_empty_collections() {
    let value = Containers {
        m: BTreeMap::new(),
        l: Vec::new(),
        ..Default::default()
    };

    let encoded = json5_protocol::serialize_ref(&value);
    let decoded: Containers =
        json5_protocol::deserialize(&encoded).expect("nested empty collections must round-trip");

    assert_eq!(encoded, r#"{"m":{},"l":[]}"#);
    assert_eq!(decoded, value);
}

#[test]
fn generated_struct_skips_unknown_empty_collections() {
    let decoded: Basic =
        json5_protocol::deserialize("{unknown_list: [], unknown_map: {}, b: true}")
            .expect("unknown empty collections must be skipped");

    assert!(decoded.b);
}

#[test]
fn accepts_and_validates_field_and_enum_identifiers() {
    let by_name_and_id: Basic = json5_protocol::deserialize("{'b (1)': true, '(3)': false}")
        .expect("qualified field identifiers");
    assert!(by_name_and_id.b);
    assert!(!by_name_and_id.b2);

    let enum_by_name: En = json5_protocol::deserialize("'TWO'").expect("enum name");
    let enum_by_name_and_value: En =
        json5_protocol::deserialize("'ONE (1)'").expect("enum name and value");
    let enum_without_space: En =
        json5_protocol::deserialize("'ONE(1)'").expect("compact enum identifier");
    let enum_with_whitespace: En =
        json5_protocol::deserialize("'ONE \t (1)'").expect("whitespace enum identifier");
    assert_eq!(enum_by_name, En::TWO);
    assert_eq!(enum_by_name_and_value, En::ONE);
    assert_eq!(enum_without_space, En::ONE);
    assert_eq!(enum_with_whitespace, En::ONE);

    let enum_map = BTreeMap::from([(Box::new(En::TWO), 2), (Box::new(En::ONE), 1)]);
    assert_eq!(
        json5_protocol::serialize(enum_map),
        r#"{"ONE (1)":1,"TWO (2)":2}"#,
        "wrapped enum keys preserve object-form map encoding"
    );

    let union_map = UnionMap(vec![
        (
            Un::un1(UnOne {
                one: 1,
                ..Default::default()
            }),
            1,
        ),
        (
            Un::un2(fbthrift_test_if::UnTwo {
                two: 2,
                ..Default::default()
            }),
            2,
        ),
    ]);
    assert_eq!(
        json5_protocol::serialize(union_map),
        r#"[{"key":{"un2":{"two":2}},"value":2},{"key":{"un1":{"one":1}},"value":1}]"#,
        "stable Thrift ordering treats an absent lower-ID field as smaller"
    );

    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'b (3)': true}").is_err(),
        "a field name and ID mismatch must fail"
    );
    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'b (3)': null}").is_ok(),
        "null fields are ignored without resolving their schema identifier"
    );
    assert!(
        json5_protocol::deserialize::<En, _, _>("'ONE (2)'").is_err(),
        "an enum name and value mismatch must fail"
    );
    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'$bad': true}").is_err(),
        "Thrift identifiers do not permit dollar signs"
    );
    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'$bad': null}").is_err(),
        "null values do not bypass Thrift identifier validation"
    );
    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'(32768)': null}").is_err(),
        "null values do not bypass the field ID range check"
    );
    assert!(
        json5_protocol::deserialize::<Basic, _, _>("{'+1': true}").is_ok(),
        "quoted field IDs permit a leading plus sign"
    );
    assert!(
        json5_protocol::deserialize::<En, _, _>("'ONE (01)'").is_err(),
        "quoted enum values do not permit leading zeros"
    );
    assert!(
        json5_protocol::deserialize::<En, _, _>("'ONE (+1)'").is_ok(),
        "quoted enum values permit a leading plus sign"
    );
}
