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

//! JSON5 data serialization for generated Thrift values.
//!
//! The reader accepts JSON and JSON5. The writer emits deterministic basic JSON
//! by default, or JSON5 when requested. RPC message envelopes are deliberately
//! not implemented; this module is currently for value serialization only.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::fmt;
use std::io::Cursor;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use base64::Engine;
use base64::alphabet::STANDARD;
use base64::alphabet::URL_SAFE;
use base64::engine::DecodePaddingMode;
use base64::engine::GeneralPurpose;
use base64::engine::general_purpose::NO_PAD;
use bytes::Buf;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use serde::de;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use thiserror::Error;

use crate::ThriftEnum;
use crate::binary_type::CopyFromBuf;
use crate::bufext::BufExt;
use crate::bufext::BufMutExt;
use crate::bufext::DeserializeSource;
use crate::deserialize::Deserialize;
use crate::errors::ProtocolError;
use crate::protocol::Field;
use crate::protocol::ProtocolReader;
use crate::protocol::ProtocolWriter;
use crate::serialize::Serialize;
use crate::thrift_protocol::MessageType;
use crate::ttype::MapKeyType;
use crate::ttype::TType;

const MAXIMUM_NESTING_DEPTH: usize = 64;
const MAXIMUM_INDENT_WIDTH: usize = 16;
const STANDARD_NO_PAD_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &STANDARD,
    NO_PAD.with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const URL_SAFE_NO_PAD_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    NO_PAD.with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Clone, Debug, Error)]
#[error("{0}")]
struct Json5ParseError(String);

/// Controls whether serialization uses strict JSON or JSON5 syntax.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Json5Mode {
    /// Emit strict JSON. This is the stable default and remains valid JSON5.
    #[default]
    BasicJson,
    /// Emit JSON5 conveniences such as unquoted keys and trailing commas.
    Json5,
}

/// Options controlling JSON5 serialization output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Json5WriterOptions {
    mode: Json5Mode,
    indent_width: usize,
    binary_as_base64_string: bool,
}

impl Default for Json5WriterOptions {
    fn default() -> Self {
        Self {
            mode: Json5Mode::BasicJson,
            indent_width: 0,
            binary_as_base64_string: false,
        }
    }
}

impl Json5WriterOptions {
    pub const fn basic_json() -> Self {
        Self {
            mode: Json5Mode::BasicJson,
            indent_width: 0,
            binary_as_base64_string: false,
        }
    }

    pub const fn json5() -> Self {
        Self {
            mode: Json5Mode::Json5,
            indent_width: 0,
            binary_as_base64_string: false,
        }
    }

    pub fn with_indent_width(mut self, indent_width: usize) -> Result<Self> {
        ensure_err!(
            indent_width <= MAXIMUM_INDENT_WIDTH,
            ProtocolError::InvalidValue
        );
        self.indent_width = indent_width;
        Ok(self)
    }

    /// Encode binary values as standard unpadded base64 strings instead of tagged objects.
    pub const fn with_binary_as_base64_string(mut self, enabled: bool) -> Self {
        self.binary_as_base64_string = enabled;
        self
    }
}

#[derive(Clone, Debug)]
enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Float32(f32),
    Float64(f64),
    ParsedFloat {
        value: f64,
        token: String,
    },
    Enum {
        name: Option<&'static str>,
        value: i32,
    },
    String(String),
    Binary(Vec<u8>),
    Array(Vec<Value>),
    Struct(Vec<(i16, String, Value)>),
    Object(Vec<(String, Value)>),
    Map {
        key_type: MapKeyType,
        entries: Vec<(Value, Value)>,
    },
}

impl Value {
    fn kind(&self) -> TType {
        match self {
            Self::Null => TType::Void,
            Self::Bool(_) => TType::Bool,
            Self::Integer(_) | Self::Enum { .. } => TType::I64,
            Self::Float32(_) => TType::Float,
            Self::Float64(_) | Self::ParsedFloat { .. } => TType::Double,
            Self::String(_) => TType::UTF8,
            Self::Binary(_) => TType::String,
            Self::Array(_) => TType::List,
            Self::Struct(_) | Self::Object(_) => TType::Struct,
            Self::Map { .. } => TType::Map,
        }
    }

    fn validate_depth(&self, remaining: usize) -> Result<()> {
        match self {
            Self::Array(values) if remaining != 0 => values
                .iter()
                .try_for_each(|value| value.validate_depth(remaining - 1)),
            Self::Object(fields) if remaining != 0 => fields
                .iter()
                .try_for_each(|(_, value)| value.validate_depth(remaining - 1)),
            Self::Struct(fields) if remaining != 0 => fields
                .iter()
                .try_for_each(|(_, _, value)| value.validate_depth(remaining - 1)),
            Self::Map { entries, .. } if remaining != 0 => {
                entries.iter().try_for_each(|(key, value)| {
                    key.validate_depth(remaining - 1)?;
                    value.validate_depth(remaining - 1)
                })
            }
            Self::Array(_) | Self::Object(_) | Self::Struct(_) | Self::Map { .. } => {
                bail_err!(ProtocolError::SkipDepthExceeded)
            }
            _ => Ok(()),
        }
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON5 value")
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(Value::Integer(value))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E>
    where
        E: de::Error,
    {
        i64::try_from(value)
            .map(Value::Integer)
            .map_err(|_| E::custom("integer is outside the Thrift i64 range"))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E> {
        Ok(Value::Float64(value))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::new();
        while let Some((name, value)) = map.next_entry()? {
            fields.push((name, value));
        }
        Ok(Value::Object(fields))
    }
}

impl<'de> de::Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        deserializer.deserialize_any(ValueVisitor)
    }
}

#[derive(Debug)]
enum WriteFrame {
    Struct {
        fields: Vec<(i16, String, Value)>,
        pending: Option<(i16, String)>,
    },
    List(Vec<Value>),
    Set(Vec<Value>),
    Map {
        key_type: MapKeyType,
        entries: Vec<(Value, Value)>,
        pending_key: Option<Value>,
    },
}

/// A buffering writer used by generated Thrift `Serialize` implementations.
pub struct Json5ProtocolSerializer<B: BufMutExt> {
    buffer: B,
    options: Json5WriterOptions,
    stack: Vec<WriteFrame>,
    root: Option<Value>,
}

impl<B: BufMutExt> Json5ProtocolSerializer<B> {
    fn new(buffer: B, options: Json5WriterOptions) -> Self {
        Self {
            buffer,
            options,
            stack: Vec::new(),
            root: None,
        }
    }

    fn emit(&mut self, value: Value) {
        let Some(frame) = self.stack.last_mut() else {
            assert!(
                self.root.replace(value).is_none(),
                "multiple root JSON5 values"
            );
            return;
        };

        match frame {
            WriteFrame::Struct { fields, pending } => {
                let (id, name) = pending.take().expect("field value without field metadata");
                fields.push((id, name, value));
            }
            WriteFrame::List(values) | WriteFrame::Set(values) => values.push(value),
            WriteFrame::Map {
                entries,
                pending_key,
                ..
            } => {
                if let Some(key) = pending_key.take() {
                    entries.push((key, value));
                } else {
                    *pending_key = Some(value);
                }
            }
        }
    }

    fn finish_frame(&mut self, value: Value) {
        self.emit(value);
    }
}

impl<B: BufMutExt> ProtocolWriter for Json5ProtocolSerializer<B> {
    type Final = B::Final;

    fn write_message_begin(&mut self, _name: &str, _type_id: MessageType, _seqid: u32) {
        panic!("JSON5 RPC message envelopes are not supported")
    }

    fn write_message_end(&mut self) {
        panic!("JSON5 RPC message envelopes are not supported")
    }

    fn write_struct_begin(&mut self, _name: &str) {
        self.stack.push(WriteFrame::Struct {
            fields: Vec::new(),
            pending: None,
        });
    }

    fn write_struct_end(&mut self) {
        let Some(WriteFrame::Struct {
            mut fields,
            pending: None,
        }) = self.stack.pop()
        else {
            panic!("unbalanced JSON5 struct callbacks");
        };
        fields.sort_by_key(|(id, _, _)| *id);
        self.finish_frame(Value::Struct(fields));
    }

    fn write_field_begin(&mut self, name: &str, _type_id: TType, id: i16) {
        let Some(WriteFrame::Struct { pending, .. }) = self.stack.last_mut() else {
            panic!("JSON5 field outside a struct");
        };
        assert!(
            pending.replace((id, name.to_owned())).is_none(),
            "nested JSON5 fields"
        );
    }

    fn write_field_end(&mut self) {}

    fn write_field_stop(&mut self) {}

    fn write_map_begin(&mut self, key_type: TType, _value_type: TType, size: usize) {
        let map_key_type = if key_type == TType::String {
            MapKeyType::String
        } else {
            MapKeyType::Other
        };
        self.write_map_begin_with_key_type(key_type, _value_type, size, map_key_type);
    }

    fn write_map_begin_with_key_type(
        &mut self,
        _key_type: TType,
        _value_type: TType,
        size: usize,
        key_type: MapKeyType,
    ) {
        self.stack.push(WriteFrame::Map {
            key_type,
            entries: Vec::with_capacity(size),
            pending_key: None,
        });
    }

    fn write_map_key_begin(&mut self) {}

    fn write_map_value_begin(&mut self) {
        let Some(WriteFrame::Map { pending_key, .. }) = self.stack.last() else {
            panic!("JSON5 map value outside a map");
        };
        assert!(pending_key.is_some(), "JSON5 map value without a key");
    }

    fn write_map_end(&mut self) {
        let Some(WriteFrame::Map {
            key_type,
            mut entries,
            pending_key: None,
        }) = self.stack.pop()
        else {
            panic!("unbalanced JSON5 map callbacks");
        };
        entries.sort_by(|(left, _), (right, _)| compare_values(left, right));
        self.finish_frame(Value::Map { key_type, entries });
    }

    fn write_list_begin(&mut self, _elem_type: TType, size: usize) {
        self.stack.push(WriteFrame::List(Vec::with_capacity(size)));
    }

    fn write_list_value_begin(&mut self) {}

    fn write_list_end(&mut self) {
        let Some(WriteFrame::List(values)) = self.stack.pop() else {
            panic!("unbalanced JSON5 list callbacks");
        };
        self.finish_frame(Value::Array(values));
    }

    fn write_set_begin(&mut self, _elem_type: TType, size: usize) {
        self.stack.push(WriteFrame::Set(Vec::with_capacity(size)));
    }

    fn write_set_value_begin(&mut self) {}

    fn write_set_end(&mut self) {
        let Some(WriteFrame::Set(mut values)) = self.stack.pop() else {
            panic!("unbalanced JSON5 set callbacks");
        };
        values.sort_by(compare_values);
        self.finish_frame(Value::Array(values));
    }

    fn write_bool(&mut self, value: bool) {
        self.emit(Value::Bool(value));
    }

    fn write_byte(&mut self, value: i8) {
        self.emit(Value::Integer(value.into()));
    }

    fn write_i16(&mut self, value: i16) {
        self.emit(Value::Integer(value.into()));
    }

    fn write_i32(&mut self, value: i32) {
        self.emit(Value::Integer(value.into()));
    }

    fn write_enum<E>(&mut self, value: &E)
    where
        E: ThriftEnum + 'static,
    {
        self.emit(Value::Enum {
            name: value.variant_name(),
            value: value.inner_value(),
        });
    }

    fn write_i64(&mut self, value: i64) {
        self.emit(Value::Integer(value));
    }

    fn write_double(&mut self, value: f64) {
        self.emit(Value::Float64(value));
    }

    fn write_float(&mut self, value: f32) {
        self.emit(Value::Float32(value));
    }

    fn write_string(&mut self, value: &str) {
        self.emit(Value::String(value.to_owned()));
    }

    fn write_binary(&mut self, value: &[u8]) {
        self.emit(Value::Binary(value.to_vec()));
    }

    fn finish(mut self) -> Self::Final {
        assert!(
            self.stack.is_empty(),
            "unbalanced JSON5 serialization callbacks"
        );
        let root = self.root.take().expect("missing root JSON5 value");
        let mut output = Vec::new();
        write_value(&mut output, &root, self.options, 0, false)
            .expect("writing JSON5 into memory cannot fail");
        self.buffer.put_slice(&output);
        self.buffer.finalize()
    }
}

fn compare_values(left: &Value, right: &Value) -> Ordering {
    fn rank(value: &Value) -> u8 {
        match value {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Integer(_) => 2,
            Value::Float32(_) => 3,
            Value::Float64(_) => 4,
            Value::ParsedFloat { .. } => 5,
            Value::Enum { .. } => 6,
            Value::String(_) => 7,
            Value::Binary(_) => 8,
            Value::Array(_) => 9,
            Value::Struct(_) => 10,
            Value::Object(_) => 11,
            Value::Map { .. } => 12,
        }
    }

    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Float32(left), Value::Float32(right)) => left.total_cmp(right),
        (Value::Float64(left), Value::Float64(right)) => left.total_cmp(right),
        (Value::ParsedFloat { value: left, .. }, Value::ParsedFloat { value: right, .. }) => {
            left.total_cmp(right)
        }
        (Value::Enum { value: left, .. }, Value::Enum { value: right, .. }) => left.cmp(right),
        (Value::String(left), Value::String(right)) => left.cmp(right),
        (Value::Binary(left), Value::Binary(right)) => left.cmp(right),
        (Value::Array(left), Value::Array(right)) => compare_slices(left, right),
        (Value::Struct(left), Value::Struct(right)) => compare_slices_by(
            left,
            right,
            |(left_id, _, left_value), (right_id, _, right_value)| {
                // A struct missing the lower field id sorts before one that has it.
                right_id
                    .cmp(left_id)
                    .then_with(|| compare_values(left_value, right_value))
            },
        ),
        (Value::Object(left), Value::Object(right)) => compare_slices_by(
            left,
            right,
            |(left_name, left_value), (right_name, right_value)| {
                left_name
                    .cmp(right_name)
                    .then_with(|| compare_values(left_value, right_value))
            },
        ),
        (Value::Map { entries: left, .. }, Value::Map { entries: right, .. }) => {
            compare_slices_by(left, right, |left, right| {
                compare_values(&left.0, &right.0).then_with(|| compare_values(&left.1, &right.1))
            })
        }
        _ => rank(left).cmp(&rank(right)),
    }
}

fn compare_slices(left: &[Value], right: &[Value]) -> Ordering {
    compare_slices_by(left, right, compare_values)
}

fn compare_slices_by<T>(left: &[T], right: &[T], compare: impl Fn(&T, &T) -> Ordering) -> Ordering {
    left.iter()
        .zip(right)
        .map(|(left, right)| compare(left, right))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn write_value(
    output: &mut Vec<u8>,
    value: &Value,
    options: Json5WriterOptions,
    depth: usize,
    object_name: bool,
) -> std::io::Result<()> {
    match value {
        Value::Null => output.put_slice(b"null"),
        Value::Bool(value) => output.put_slice(if *value { b"true" } else { b"false" }),
        Value::Integer(value) => output.put_slice(value.to_string().as_bytes()),
        Value::Enum { name, value } => {
            let encoded = match name {
                Some(name) => format!("{name} ({value})"),
                None => format!("({value})"),
            };
            write_string(output, &encoded, options, object_name);
        }
        Value::Float32(value) if value.is_nan() => {
            write_special_float(output, signed_nan(*value), options.mode)
        }
        Value::Float32(value) if *value == f32::INFINITY => {
            write_special_float(output, "Infinity", options.mode)
        }
        Value::Float32(value) if *value == f32::NEG_INFINITY => {
            write_special_float(output, "-Infinity", options.mode)
        }
        Value::Float32(value) => output.put_slice(
            serde_json::to_string(value)
                .expect("finite float is representable in JSON")
                .as_bytes(),
        ),
        Value::Float64(value) if value.is_nan() => {
            write_special_float(output, signed_nan(*value), options.mode)
        }
        Value::Float64(value) if *value == f64::INFINITY => {
            write_special_float(output, "Infinity", options.mode)
        }
        Value::Float64(value) if *value == f64::NEG_INFINITY => {
            write_special_float(output, "-Infinity", options.mode)
        }
        Value::Float64(value) => output.put_slice(
            serde_json::to_string(value)
                .expect("finite float is representable in JSON")
                .as_bytes(),
        ),
        Value::ParsedFloat { value, .. } => output.put_slice(
            serde_json::to_string(value)
                .expect("finite float is representable in JSON")
                .as_bytes(),
        ),
        Value::String(value) => write_string(output, value, options, object_name),
        Value::Binary(value) if options.binary_as_base64_string => {
            write_string(
                output,
                &STANDARD_NO_PAD_INDIFFERENT.encode(value),
                options,
                object_name,
            );
        }
        Value::Binary(value) => {
            let (encoding, encoded) = match printable_utf8(value) {
                Some(text) => ("utf-8", text.to_owned()),
                None => ("base64url", URL_SAFE_NO_PAD_INDIFFERENT.encode(value)),
            };
            write_object(
                output,
                &[(encoding.to_owned(), Value::String(encoded))],
                options,
                depth,
            )?;
        }
        Value::Array(values) => write_array(output, values, options, depth)?,
        Value::Struct(fields) => write_struct(output, fields, options, depth)?,
        Value::Object(fields) => write_object(output, fields, options, depth)?,
        Value::Map { key_type, entries } => {
            if matches!(key_type, MapKeyType::String | MapKeyType::Enum)
                && entries
                    .iter()
                    .all(|(key, _)| matches!(key, Value::String(_) | Value::Enum { .. }))
            {
                let fields = entries
                    .iter()
                    .map(|(key, value)| {
                        let name = match key {
                            Value::String(name) => name.clone(),
                            Value::Enum { name, value } => match name {
                                Some(name) => format!("{name} ({value})"),
                                None => format!("({value})"),
                            },
                            _ => unreachable!(),
                        };
                        (name, value.clone())
                    })
                    .collect::<Vec<_>>();
                write_object(output, &fields, options, depth)?;
            } else {
                let values = entries
                    .iter()
                    .map(|(key, value)| {
                        Value::Object(vec![
                            ("key".to_owned(), key.clone()),
                            ("value".to_owned(), value.clone()),
                        ])
                    })
                    .collect::<Vec<_>>();
                write_array(output, &values, options, depth)?;
            }
        }
    }
    Ok(())
}

fn write_special_float(output: &mut Vec<u8>, value: &str, mode: Json5Mode) {
    if mode == Json5Mode::Json5 {
        output.put_slice(value.as_bytes());
    } else {
        serde_json::to_writer(output, value).expect("writing JSON into memory cannot fail");
    }
}

fn signed_nan<T: num_traits::Float>(value: T) -> &'static str {
    if value.is_sign_negative() {
        "-NaN"
    } else {
        "NaN"
    }
}

fn printable_utf8(value: &[u8]) -> Option<&str> {
    std::str::from_utf8(value).ok().filter(|text| {
        text.chars().all(|ch| {
            matches!(ch, '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r')
                || (!ch.is_control() && ch != '\u{007f}')
        })
    })
}

fn write_string(output: &mut Vec<u8>, value: &str, options: Json5WriterOptions, object_name: bool) {
    if object_name && options.mode == Json5Mode::Json5 && is_identifier(value) {
        output.put_slice(value.as_bytes());
    } else {
        serde_json::to_writer(output, value).expect("writing JSON into memory cannot fail");
    }
}

fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('_' | '$' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|ch| matches!(ch, '_' | '$' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

fn write_array(
    output: &mut Vec<u8>,
    values: &[Value],
    options: Json5WriterOptions,
    depth: usize,
) -> std::io::Result<()> {
    output.put_u8(b'[');
    for (index, value) in values.iter().enumerate() {
        write_separator(output, index, options, depth + 1);
        write_value(output, value, options, depth + 1, false)?;
    }
    write_container_end(output, values.is_empty(), options, depth, b']');
    Ok(())
}

fn write_object(
    output: &mut Vec<u8>,
    fields: &[(String, Value)],
    options: Json5WriterOptions,
    depth: usize,
) -> std::io::Result<()> {
    output.put_u8(b'{');
    for (index, (name, value)) in fields.iter().enumerate() {
        write_separator(output, index, options, depth + 1);
        write_string(output, name, options, true);
        output.put_u8(b':');
        if options.indent_width != 0 {
            output.put_u8(b' ');
        }
        write_value(output, value, options, depth + 1, false)?;
    }
    write_container_end(output, fields.is_empty(), options, depth, b'}');
    Ok(())
}

fn write_struct(
    output: &mut Vec<u8>,
    fields: &[(i16, String, Value)],
    options: Json5WriterOptions,
    depth: usize,
) -> std::io::Result<()> {
    output.put_u8(b'{');
    for (index, (_, name, value)) in fields.iter().enumerate() {
        write_separator(output, index, options, depth + 1);
        write_string(output, name, options, true);
        output.put_u8(b':');
        if options.indent_width != 0 {
            output.put_u8(b' ');
        }
        write_value(output, value, options, depth + 1, false)?;
    }
    write_container_end(output, fields.is_empty(), options, depth, b'}');
    Ok(())
}

fn write_separator(output: &mut Vec<u8>, index: usize, options: Json5WriterOptions, depth: usize) {
    if index != 0 {
        output.put_u8(b',');
    }
    if options.indent_width != 0 {
        output.put_u8(b'\n');
        output.resize(output.len() + depth * options.indent_width, b' ');
    }
}

fn write_container_end(
    output: &mut Vec<u8>,
    empty: bool,
    options: Json5WriterOptions,
    depth: usize,
    closing: u8,
) {
    if !empty && options.mode == Json5Mode::Json5 {
        output.put_u8(b',');
    }
    if !empty && options.indent_width != 0 {
        output.put_u8(b'\n');
        output.resize(output.len() + depth * options.indent_width, b' ');
    }
    output.put_u8(closing);
}

enum ReadFrame {
    Struct(VecDeque<(String, Value)>),
    List(VecDeque<Value>),
    Map {
        entries: VecDeque<(Value, Value)>,
        pending_value: Option<Value>,
    },
}

/// A JSON5 reader used by generated Thrift `Deserialize` implementations.
pub struct Json5ProtocolDeserializer {
    root: std::result::Result<Option<Value>, Json5ParseError>,
    stack: Vec<ReadFrame>,
}

impl Json5ProtocolDeserializer {
    pub fn new<B: Buf>(mut buffer: B) -> Self {
        let bytes = buffer.copy_to_bytes(buffer.remaining());
        let root = std::str::from_utf8(&bytes)
            .map_err(|error| Json5ParseError(error.to_string()))
            .and_then(|input| {
                validate_input_nesting(input)?;
                let (normalized, mut tokens) = normalize_json5_numbers(input)?;
                let mut value = json5::from_str::<Value>(&normalized)
                    .map_err(|error| Json5ParseError(error.to_string()))?;
                attach_float_tokens(&mut value, &mut tokens)?;
                if !tokens.is_empty() {
                    return Err(Json5ParseError(
                        "unmatched JSON5 floating-point token".to_owned(),
                    ));
                }
                Ok(value)
            })
            .and_then(|value| {
                value
                    .validate_depth(MAXIMUM_NESTING_DEPTH)
                    .map(|()| Some(value))
                    .map_err(|error| Json5ParseError(error.to_string()))
            });
        Self {
            root,
            stack: Vec::new(),
        }
    }

    fn take_value(&mut self) -> Result<Value> {
        self.root
            .as_mut()
            .map_err(|error| anyhow!(error.clone()))?
            .take()
            .ok_or_else(|| anyhow!("missing JSON5 value"))
    }

    fn ensure_value_consumed(&self) -> Result<()> {
        match &self.root {
            Ok(None) => Ok(()),
            Ok(Some(_)) => bail!("JSON5 value was not consumed"),
            Err(error) => bail!(error.clone()),
        }
    }

    fn set_value(&mut self, value: Value) -> Result<()> {
        self.ensure_value_consumed()?;
        self.root = Ok(Some(value));
        Ok(())
    }

    fn take_integer(&mut self) -> Result<i64> {
        match self.take_value()? {
            Value::Integer(value) => Ok(value),
            Value::String(value) => parse_integer(&value).context("invalid JSON5 integer"),
            _ => bail!("expected a JSON5 integer"),
        }
    }

    fn take_number(&mut self) -> Result<Number> {
        match self.take_value()? {
            Value::Integer(value) => Ok(Number::Integer(value)),
            Value::Float32(value) => Ok(Number::Float {
                value: value.into(),
                token: None,
            }),
            Value::Float64(value) => Ok(Number::Float { value, token: None }),
            Value::ParsedFloat { value, token } => Ok(Number::Float {
                value,
                token: Some(token),
            }),
            Value::String(value) => match value.parse::<i64>() {
                Ok(value) => Ok(Number::Integer(value)),
                Err(_) => Ok(Number::Float {
                    value: parse_f64(&value)?,
                    token: Some(value),
                }),
            },
            _ => bail!("expected a JSON5 floating-point value"),
        }
    }
}

impl ProtocolReader for Json5ProtocolDeserializer {
    fn read_message_begin<F, T>(&mut self, _method: F) -> Result<(T, MessageType, u32)>
    where
        F: FnOnce(&[u8]) -> T,
    {
        bail!("JSON5 RPC message envelopes are not supported")
    }

    fn read_message_end(&mut self) -> Result<()> {
        bail!("JSON5 RPC message envelopes are not supported")
    }

    fn read_struct_begin<F, T>(&mut self, strukt: F) -> Result<T>
    where
        F: FnOnce(&[u8]) -> T,
    {
        let Value::Object(fields) = self.take_value()? else {
            bail!("expected a JSON5 object for a Thrift struct");
        };
        self.stack.push(ReadFrame::Struct(fields.into()));
        Ok(strukt(&[]))
    }

    fn read_struct_end(&mut self) -> Result<()> {
        let Some(ReadFrame::Struct(fields)) = self.stack.pop() else {
            bail!("unbalanced JSON5 struct callbacks");
        };
        ensure_err!(fields.is_empty(), ProtocolError::TrailingData);
        Ok(())
    }

    fn read_field_begin<F, T>(&mut self, field: F, fields: &[Field]) -> Result<(T, TType, i16)>
    where
        F: FnOnce(&[u8]) -> T,
    {
        self.ensure_value_consumed()?;
        let Some(ReadFrame::Struct(values)) = self.stack.last_mut() else {
            bail!("JSON5 field outside a struct");
        };
        let Some((name, value)) = values.pop_front() else {
            return Ok((field(&[]), TType::Stop, -1));
        };
        let value_type = value.kind();
        let (_, provided_id) = parse_identifier(&name)?;
        provided_id
            .map(i16::try_from)
            .transpose()
            .context("JSON5 field ID is outside the i16 range")?;
        self.set_value(value)?;
        if value_type == TType::Void {
            return Ok((field(name.as_bytes()), value_type, -1));
        }

        let known = resolve_field(&name, fields)?;
        match known {
            Some(known) => Ok((field(known.name.as_bytes()), known.ttype, known.id)),
            _ => Ok((field(name.as_bytes()), value_type, -1)),
        }
    }

    fn read_field_end(&mut self) -> Result<()> {
        self.ensure_value_consumed()
    }

    fn read_map_begin_unchecked(&mut self) -> Result<(TType, TType, Option<usize>)> {
        let entries = match self.take_value()? {
            Value::Object(fields) => fields
                .into_iter()
                .map(|(key, value)| (Value::String(key), value))
                .collect(),
            Value::Array(values) => values
                .into_iter()
                .map(parse_map_entry)
                .collect::<Result<VecDeque<_>>>()?,
            _ => bail!("expected a JSON5 object or key-value array for a Thrift map"),
        };
        let len = entries.len();
        if len == 0 {
            return Ok((TType::Stop, TType::Stop, Some(0)));
        }
        self.stack.push(ReadFrame::Map {
            entries,
            pending_value: None,
        });
        Ok((TType::Stop, TType::Stop, Some(len)))
    }

    fn read_map_key_begin(&mut self) -> Result<bool> {
        self.ensure_value_consumed()?;
        let Some(ReadFrame::Map {
            entries,
            pending_value,
        }) = self.stack.last_mut()
        else {
            bail!("JSON5 map key outside a map");
        };
        let Some((key, value)) = entries.pop_front() else {
            return Ok(false);
        };
        *pending_value = Some(value);
        self.set_value(key)?;
        Ok(true)
    }

    fn read_map_value_begin(&mut self) -> Result<()> {
        self.ensure_value_consumed()?;
        let Some(ReadFrame::Map { pending_value, .. }) = self.stack.last_mut() else {
            bail!("JSON5 map value outside a map");
        };
        let value = pending_value
            .take()
            .ok_or_else(|| anyhow!("JSON5 map value without a key"))?;
        self.set_value(value)
    }

    fn read_map_value_end(&mut self) -> Result<()> {
        self.ensure_value_consumed()
    }

    fn read_map_end(&mut self) -> Result<()> {
        let Some(ReadFrame::Map {
            entries,
            pending_value: None,
        }) = self.stack.pop()
        else {
            bail!("unbalanced JSON5 map callbacks");
        };
        ensure_err!(entries.is_empty(), ProtocolError::TrailingData);
        Ok(())
    }

    fn read_list_begin_unchecked(&mut self) -> Result<(TType, Option<usize>)> {
        let Value::Array(values) = self.take_value()? else {
            bail!("expected a JSON5 array for a Thrift list");
        };
        let len = values.len();
        if len == 0 {
            return Ok((TType::Stop, Some(0)));
        }
        self.stack.push(ReadFrame::List(values.into()));
        Ok((TType::Stop, Some(len)))
    }

    fn read_list_value_begin(&mut self) -> Result<bool> {
        self.ensure_value_consumed()?;
        let Some(ReadFrame::List(values)) = self.stack.last_mut() else {
            bail!("JSON5 list value outside a list");
        };
        let Some(value) = values.pop_front() else {
            return Ok(false);
        };
        self.set_value(value)?;
        Ok(true)
    }

    fn read_list_value_end(&mut self) -> Result<()> {
        self.ensure_value_consumed()
    }

    fn read_list_end(&mut self) -> Result<()> {
        let Some(ReadFrame::List(values)) = self.stack.pop() else {
            bail!("unbalanced JSON5 list callbacks");
        };
        ensure_err!(values.is_empty(), ProtocolError::TrailingData);
        Ok(())
    }

    fn read_set_begin_unchecked(&mut self) -> Result<(TType, Option<usize>)> {
        self.read_list_begin_unchecked()
    }

    fn read_set_value_begin(&mut self) -> Result<bool> {
        self.read_list_value_begin()
    }

    fn read_set_value_end(&mut self) -> Result<()> {
        self.read_list_value_end()
    }

    fn read_set_end(&mut self) -> Result<()> {
        self.read_list_end()
    }

    fn read_bool(&mut self) -> Result<bool> {
        match self.take_value()? {
            Value::Bool(value) => Ok(value),
            Value::String(value) => value.parse().context("invalid JSON5 boolean"),
            _ => bail!("expected a JSON5 boolean"),
        }
    }

    fn read_byte(&mut self) -> Result<i8> {
        self.take_integer()?
            .try_into()
            .context("JSON5 integer is outside the Thrift byte range")
    }

    fn read_i16(&mut self) -> Result<i16> {
        self.take_integer()?
            .try_into()
            .context("JSON5 integer is outside the Thrift i16 range")
    }

    fn read_i32(&mut self) -> Result<i32> {
        self.take_integer()?
            .try_into()
            .context("JSON5 integer is outside the Thrift i32 range")
    }

    fn read_enum<E>(&mut self) -> Result<E>
    where
        E: ThriftEnum + 'static,
    {
        let (provided_name, provided_value) = match self.take_value()? {
            Value::Integer(value) => (
                None,
                Some(
                    value
                        .try_into()
                        .context("JSON5 enum value is outside the i32 range")?,
                ),
            ),
            Value::String(value) => {
                parse_identifier(&value).map(|(name, value)| (name.map(str::to_owned), value))?
            }
            _ => bail!("expected a JSON5 enum name or value"),
        };

        let schema_value = provided_name.as_deref().and_then(|name| {
            E::enumerate()
                .iter()
                .find(|(_, candidate)| *candidate == name)
                .map(|(variant, _)| variant.inner_value())
        });
        let schema_name = provided_value.and_then(|value| {
            E::enumerate()
                .iter()
                .find(|(variant, _)| variant.inner_value() == value)
                .map(|(_, name)| *name)
        });

        match (provided_name.as_deref(), provided_value) {
            (None, Some(value)) => E::from_inner_value(value),
            (Some(_), None) => {
                E::from_inner_value(schema_value.ok_or_else(|| anyhow!("unknown JSON5 enum name"))?)
            }
            (Some(name), Some(value)) => {
                if let Some(expected_name) = schema_name {
                    ensure_err!(expected_name == name, ProtocolError::InvalidValue);
                }
                if let Some(expected_value) = schema_value {
                    ensure_err!(expected_value == value, ProtocolError::InvalidValue);
                }
                E::from_inner_value(value)
            }
            (None, None) => bail!("JSON5 enum has neither a name nor a value"),
        }
    }

    fn read_i64(&mut self) -> Result<i64> {
        self.take_integer()
    }

    fn read_double(&mut self) -> Result<f64> {
        match self.take_number()? {
            Number::Float { value, token } => match token {
                Some(token) => parse_f64(&token),
                None => Ok(value),
            },
            Number::Integer(value) => {
                let converted = value as f64;
                ensure_err!(
                    converted as i128 == value as i128,
                    ProtocolError::InvalidValue
                );
                Ok(converted)
            }
        }
    }

    fn read_float(&mut self) -> Result<f32> {
        match self.take_number()? {
            Number::Float { value, token } => {
                let converted = match token {
                    Some(token) => parse_f32(&token)?,
                    None => value as f32,
                };
                ensure_err!(
                    !value.is_finite() || converted.is_finite(),
                    ProtocolError::InvalidValue
                );
                Ok(converted)
            }
            Number::Integer(value) => {
                let converted = value as f32;
                ensure_err!(
                    converted as i128 == value as i128,
                    ProtocolError::InvalidValue
                );
                Ok(converted)
            }
        }
    }

    fn read_string(&mut self) -> Result<String> {
        match self.take_value()? {
            Value::String(value) => Ok(value),
            _ => bail!("expected a JSON5 string"),
        }
    }

    fn read_binary<V: CopyFromBuf>(&mut self) -> Result<V> {
        let bytes = match self.take_value()? {
            Value::String(value) => decode_base64_compatible(&value)?,
            Value::Object(mut fields) if fields.len() == 1 => {
                let (encoding, Value::String(value)) = fields.pop().expect("length checked") else {
                    bail!("JSON5 binary payload must be a string");
                };
                match encoding.as_str() {
                    "utf-8" => value.into_bytes(),
                    "base64url" => decode_base64(&URL_SAFE_NO_PAD_INDIFFERENT, &value)
                        .context("invalid base64url binary value")?,
                    "base64" => decode_base64(&STANDARD_NO_PAD_INDIFFERENT, &value)
                        .context("invalid base64 binary value")?,
                    _ => bail!("unknown JSON5 binary encoding `{encoding}`"),
                }
            }
            _ => bail!("expected a JSON5 binary object"),
        };
        Ok(V::from_vec(bytes))
    }

    fn skip(&mut self, _field_type: TType) -> Result<()> {
        self.take_value().map(|_| ())
    }
}

enum Number {
    Integer(i64),
    Float { value: f64, token: Option<String> },
}

fn parse_f32(token: &str) -> Result<f32> {
    let normalized = match parse_special_float(token) {
        Some(value) => return Ok(value as f32),
        None => normalize_float_token(token),
    };
    let value = normalized
        .parse::<f32>()
        .context("invalid JSON5 float value")?;
    ensure_err!(value.is_finite(), ProtocolError::InvalidValue);
    Ok(value)
}

fn parse_f64(token: &str) -> Result<f64> {
    let normalized = match parse_special_float(token) {
        Some(value) => return Ok(value),
        None => normalize_float_token(token),
    };
    let value = normalized
        .parse::<f64>()
        .context("invalid JSON5 double value")?;
    ensure_err!(value.is_finite(), ProtocolError::InvalidValue);
    Ok(value)
}

fn parse_special_float(token: &str) -> Option<f64> {
    match token {
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        "NaN" | "+NaN" => Some(f64::NAN),
        "-NaN" => Some(-f64::NAN),
        _ => None,
    }
}

fn normalize_float_token(token: &str) -> String {
    match token {
        token if token.starts_with("+.") => format!("+0{}", &token[1..]),
        token if token.starts_with("-.") => format!("-0{}", &token[1..]),
        token if token.starts_with('.') => format!("0{token}"),
        token if token.ends_with('.') => format!("{token}0"),
        token => token.to_owned(),
    }
}

fn attach_float_tokens(
    value: &mut Value,
    tokens: &mut VecDeque<String>,
) -> std::result::Result<(), Json5ParseError> {
    match value {
        Value::Float64(parsed) => {
            let token = tokens
                .pop_front()
                .ok_or_else(|| Json5ParseError("missing JSON5 floating-point token".to_owned()))?;
            *value = Value::ParsedFloat {
                value: *parsed,
                token,
            };
        }
        Value::Array(values) => {
            values
                .iter_mut()
                .try_for_each(|value| attach_float_tokens(value, tokens))?;
        }
        Value::Object(fields) => {
            fields
                .iter_mut()
                .try_for_each(|(_, value)| attach_float_tokens(value, tokens))?;
        }
        _ => {}
    }
    Ok(())
}

fn normalize_json5_numbers(
    input: &str,
) -> std::result::Result<(String, VecDeque<String>), Json5ParseError> {
    #[derive(Clone, Copy)]
    enum ScanState {
        Normal,
        String { quote: u8, escaped: bool },
        LineComment,
        BlockComment,
    }

    let bytes = input.as_bytes();
    let mut state = ScanState::Normal;
    let mut tokens = VecDeque::new();
    let mut normalized = String::with_capacity(input.len());
    let mut unchanged_start = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        match state {
            ScanState::Normal if matches!(bytes[index], b'\'' | b'"') => {
                state = ScanState::String {
                    quote: bytes[index],
                    escaped: false,
                };
            }
            ScanState::Normal if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'/') => {
                state = ScanState::LineComment;
                index += 1;
            }
            ScanState::Normal if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') => {
                state = ScanState::BlockComment;
                index += 1;
            }
            ScanState::Normal => {
                let Some(end) = number_token_end(bytes, index) else {
                    index += 1;
                    continue;
                };
                let token = &input[index..end];
                let is_object_key = next_significant_byte(input, end) == Some(b':');
                if !is_object_key {
                    if is_float_token(token) {
                        tokens.push_back(token.to_owned());
                    }
                    let replacement = normalize_number_token(token)?;
                    if replacement != token {
                        normalized.push_str(&input[unchanged_start..index]);
                        normalized.push_str(&replacement);
                        unchanged_start = end;
                    }
                }
                index = end;
                continue;
            }
            ScanState::String { quote, escaped } if escaped => {
                state = ScanState::String {
                    quote,
                    escaped: false,
                };
            }
            ScanState::String { quote, .. } if bytes[index] == b'\\' => {
                state = ScanState::String {
                    quote,
                    escaped: true,
                };
            }
            ScanState::String { quote, .. } if bytes[index] == quote => {
                state = ScanState::Normal;
            }
            ScanState::LineComment if json5_line_terminator_width(bytes, index) != 0 => {
                index += json5_line_terminator_width(bytes, index) - 1;
                state = ScanState::Normal;
            }
            ScanState::BlockComment
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') =>
            {
                state = ScanState::Normal;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    normalized.push_str(&input[unchanged_start..]);
    Ok((normalized, tokens))
}

fn number_token_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start != 0 && is_identifier_continue(bytes[start - 1]) {
        return None;
    }

    let mut index = start;
    if matches!(bytes[index], b'+' | b'-') {
        index += 1;
    }
    let first = *bytes.get(index)?;
    if matches!(first, b'I' | b'N') {
        let suffix: &[u8] = if first == b'I' { b"Infinity" } else { b"NaN" };
        let end = index.checked_add(suffix.len())?;
        return (bytes.get(index..end) == Some(suffix)
            && bytes
                .get(end)
                .is_none_or(|byte| !is_identifier_continue(*byte)))
        .then_some(end);
    }
    if !first.is_ascii_digit() && first != b'.' {
        return None;
    }

    index += 1;
    while index < bytes.len() && is_number_part(bytes[index]) {
        index += 1;
    }
    Some(index)
}

fn is_number_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

fn is_float_token(token: &str) -> bool {
    let unsigned = token.strip_prefix(['+', '-']).unwrap_or(token);
    if unsigned.starts_with("0x") || unsigned.starts_with("0X") {
        return false;
    }
    token.contains(['.', 'e', 'E']) || token.contains("Infinity") || token.contains("NaN")
}

fn normalize_number_token(token: &str) -> std::result::Result<String, Json5ParseError> {
    if token == "+Infinity" || token == "+NaN" {
        return Ok(token[1..].to_owned());
    }

    let (negative, unsigned) = match token.as_bytes().first() {
        Some(b'-') => (true, &token[1..]),
        Some(b'+') => (false, &token[1..]),
        _ => (false, token),
    };
    let Some(hex) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    else {
        return Ok(token.to_owned());
    };
    let magnitude = u64::from_str_radix(hex, 16).map_err(|_| {
        Json5ParseError("hex integer is outside the JSON5 integer range".to_owned())
    })?;
    if !negative {
        return Ok(magnitude.to_string());
    }
    if magnitude == (i64::MAX as u64) + 1 {
        return Ok(i64::MIN.to_string());
    }
    let magnitude = i64::try_from(magnitude)
        .map_err(|_| Json5ParseError("hex integer is outside the Thrift i64 range".to_owned()))?;
    Ok((-magnitude).to_string())
}

fn next_significant_byte(input: &str, mut index: usize) -> Option<u8> {
    let bytes = input.as_bytes();
    loop {
        while index < bytes.len() {
            let ch = input[index..].chars().next()?;
            if !ch.is_whitespace() && ch != '\u{feff}' {
                break;
            }
            index += ch.len_utf8();
        }
        if bytes.get(index..index + 2) == Some(b"//") {
            index += 2;
            while index < bytes.len() {
                let width = json5_line_terminator_width(bytes, index);
                if width != 0 {
                    index += width;
                    break;
                }
                index += 1;
            }
            continue;
        }
        if bytes.get(index..index + 2) == Some(b"/*") {
            index += 2;
            while index < bytes.len() && bytes.get(index..index + 2) != Some(b"*/") {
                index += 1;
            }
            index = index.saturating_add(2).min(bytes.len());
            continue;
        }
        return bytes.get(index).copied();
    }
}

fn json5_line_terminator_width(bytes: &[u8], index: usize) -> usize {
    match bytes[index..] {
        [b'\n' | b'\r', ..] => 1,
        [0xe2, 0x80, 0xa8 | 0xa9, ..] => 3,
        _ => 0,
    }
}

fn validate_input_nesting(input: &str) -> std::result::Result<(), Json5ParseError> {
    enum ScanState {
        Normal,
        String { quote: u8, escaped: bool },
        LineComment,
        BlockComment,
    }

    let bytes = input.as_bytes();
    let mut state = ScanState::Normal;
    let mut depth = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        match &mut state {
            ScanState::Normal => match bytes[index] {
                b'\'' | b'"' => {
                    state = ScanState::String {
                        quote: bytes[index],
                        escaped: false,
                    };
                }
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    state = ScanState::LineComment;
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = ScanState::BlockComment;
                    index += 1;
                }
                b'{' | b'[' => {
                    depth += 1;
                    if depth > MAXIMUM_NESTING_DEPTH {
                        return Err(Json5ParseError(
                            ProtocolError::SkipDepthExceeded.to_string(),
                        ));
                    }
                }
                b'}' | b']' => depth = depth.saturating_sub(1),
                _ => {}
            },
            ScanState::String { quote, escaped } => {
                if *escaped {
                    *escaped = false;
                } else if bytes[index] == b'\\' {
                    *escaped = true;
                } else if bytes[index] == *quote {
                    state = ScanState::Normal;
                }
            }
            ScanState::LineComment => {
                let terminator_width = json5_line_terminator_width(bytes, index);
                if terminator_width != 0 {
                    state = ScanState::Normal;
                    index += terminator_width - 1;
                }
            }
            ScanState::BlockComment
                if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') =>
            {
                state = ScanState::Normal;
                index += 1;
            }
            ScanState::BlockComment => {}
        }
        index += 1;
    }
    Ok(())
}

fn decode_base64_compatible(value: &str) -> Result<Vec<u8>> {
    decode_base64(&URL_SAFE_NO_PAD_INDIFFERENT, value)
        .or_else(|_| decode_base64(&STANDARD_NO_PAD_INDIFFERENT, value))
        .context("invalid base64 binary value")
}

/// Padding is optional, but must be complete when present.
fn decode_base64(engine: &GeneralPurpose, value: &str) -> Result<Vec<u8>> {
    let decoded = engine.decode(value)?;
    ensure!(
        !value.ends_with('=') || value.len().is_multiple_of(4),
        "incomplete base64 padding"
    );
    Ok(decoded)
}

fn parse_map_entry(value: Value) -> Result<(Value, Value)> {
    let Value::Object(fields) = value else {
        bail!("JSON5 map array entries must be objects");
    };
    let mut key = None;
    let mut value = None;
    for (name, field_value) in fields {
        match name.as_str() {
            "key" if key.is_none() => key = Some(field_value),
            "value" if value.is_none() => value = Some(field_value),
            _ => bail!("JSON5 map entry must contain exactly `key` and `value`"),
        }
    }
    Ok((
        key.ok_or_else(|| anyhow!("JSON5 map entry is missing `key`"))?,
        value.ok_or_else(|| anyhow!("JSON5 map entry is missing `value`"))?,
    ))
}

fn parse_identifier(identifier: &str) -> Result<(Option<&str>, Option<i32>)> {
    ensure_err!(!identifier.is_empty(), ProtocolError::InvalidValue);
    if let Some(value) = parse_identifier_number(identifier) {
        return Ok((None, Some(value)));
    }
    if is_thrift_identifier(identifier) {
        return Ok((Some(identifier), None));
    }

    let (name, value) = identifier
        .strip_suffix(')')
        .and_then(|identifier| identifier.split_once('('))
        .ok_or_else(|| anyhow!("invalid JSON5 identifier `{identifier}`"))?;
    let name = name.trim_end();
    ensure_err!(
        name.is_empty() || is_thrift_identifier(name),
        ProtocolError::InvalidValue
    );
    Ok((
        (!name.is_empty()).then_some(name),
        Some(parse_identifier_number(value).ok_or_else(|| anyhow!("invalid identifier value"))?),
    ))
}

fn parse_identifier_number(value: &str) -> Option<i32> {
    parse_integer(value).ok()?.try_into().ok()
}

fn parse_integer(text: &str) -> Result<i64> {
    if has_leading_zero(text) {
        bail!("leading zeros are not permitted");
    }
    Ok(text.parse()?)
}

fn has_leading_zero(number: &str) -> bool {
    let unsigned = number.strip_prefix(['+', '-']).unwrap_or(number);
    unsigned
        .strip_prefix('0')
        .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()))
}

fn is_thrift_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|ch| matches!(ch, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

fn resolve_field<'a>(identifier: &str, fields: &'a [Field]) -> Result<Option<&'a Field>> {
    if let Ok(index) = fields.binary_search_by_key(&identifier, |field| field.name) {
        return Ok(Some(&fields[index]));
    }

    let (provided_name, provided_id) = parse_identifier(identifier)?;
    let provided_id = provided_id
        .map(i16::try_from)
        .transpose()
        .context("JSON5 field ID is outside the i16 range")?;
    let by_name = provided_name.and_then(|name| fields.iter().find(|field| field.name == name));
    let by_id = provided_id.and_then(|id| fields.iter().find(|field| field.id == id));

    match (provided_name, provided_id, by_name, by_id) {
        (Some(_), Some(_), Some(by_name), Some(by_id))
            if by_name.id == by_id.id && by_name.name == by_id.name =>
        {
            Ok(Some(by_name))
        }
        (Some(_), Some(_), None, None) => Ok(None),
        (Some(_), Some(_), _, _) => bail!("JSON5 field name and ID do not match the schema"),
        (Some(_), None, field, _) | (None, Some(_), _, field) => Ok(field),
        (None, None, _, _) => bail!("invalid empty JSON5 field identifier"),
    }
}

pub trait SerializeRef: Serialize<Json5ProtocolSerializer<BytesMut>>
where
    for<'a> &'a Self: Serialize<Json5ProtocolSerializer<BytesMut>>,
{
}

impl<T> SerializeRef for T
where
    T: Serialize<Json5ProtocolSerializer<BytesMut>>,
    for<'a> &'a T: Serialize<Json5ProtocolSerializer<BytesMut>>,
{
}

pub trait Serializable: Serialize<Json5ProtocolSerializer<BytesMut>> {}

impl<T> Serializable for T where T: Serialize<Json5ProtocolSerializer<BytesMut>> {}

pub trait DeserializeSlice: Deserialize<Json5ProtocolDeserializer> {}

impl<T> DeserializeSlice for T where T: Deserialize<Json5ProtocolDeserializer> {}

/// Serializes a Thrift value as compact, deterministic basic JSON.
pub fn serialize<T>(value: T) -> Bytes
where
    T: Serializable,
{
    serialize_with_options(value, Json5WriterOptions::default())
}

/// Serializes a Thrift value with the requested JSON5 writer options.
pub fn serialize_with_options<T>(value: T, options: Json5WriterOptions) -> Bytes
where
    T: Serializable,
{
    let mut serializer = Json5ProtocolSerializer::new(BytesMut::new(), options);
    value.rs_thrift_write(&mut serializer);
    serializer.finish()
}

/// Serializes a borrowed Thrift value as compact, deterministic basic JSON.
pub fn serialize_ref<T>(value: &T) -> Bytes
where
    T: SerializeRef,
{
    let options = Json5WriterOptions::default();
    let mut serializer = Json5ProtocolSerializer::new(BytesMut::new(), options);
    value.rs_thrift_write(&mut serializer);
    serializer.finish()
}

/// Deserializes a JSON or JSON5 document into a Thrift value.
pub fn deserialize<T, B, C>(input: B) -> Result<T>
where
    B: Into<DeserializeSource<C>>,
    C: BufExt,
    T: Deserialize<Json5ProtocolDeserializer>,
{
    let source: DeserializeSource<C> = input.into();
    let mut deserializer = Json5ProtocolDeserializer::new(source.0);
    let value = T::rs_thrift_read(&mut deserializer)?;
    deserializer.ensure_value_consumed()?;
    ensure_err!(deserializer.stack.is_empty(), ProtocolError::TrailingData);
    Ok(value)
}

/// Deserializes JSON5 directly from a byte slice.
pub fn deserialize_slice<T>(input: &[u8]) -> Result<T>
where
    T: Deserialize<Json5ProtocolDeserializer>,
{
    deserialize::<T, _, Cursor<&[u8]>>(input)
}
