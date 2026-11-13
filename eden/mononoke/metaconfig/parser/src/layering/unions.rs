/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Finds the thrift unions in a value as serde serializes it. A thrift union is
//! a Rust enum whose variants hold values, which serde_json writes as an object
//! with one key, the variant's name; serde tells such a variant apart from a
//! struct while serializing, which the JSON no longer does.

use std::collections::HashSet;

use serde::Serialize;
use serde::ser;
use serde_json::Error;
use serde_json::Value;

use super::ConfigPath;
use super::PathSegment;

/// The paths of the values inside `value` that serialize as enum variants
/// holding values, such as thrift unions. Paths name map entries by the keys
/// serde_json gives them.
pub(super) fn union_paths<T: Serialize + ?Sized>(value: &T) -> Result<HashSet<ConfigPath>, Error> {
    let mut finder = Finder {
        path: Vec::new(),
        unions: HashSet::new(),
    };
    value.serialize(&mut finder)?;
    Ok(finder.unions)
}

struct Finder {
    path: Vec<PathSegment>,
    unions: HashSet<ConfigPath>,
}

impl Finder {
    fn record_union(&mut self) {
        self.unions.insert(ConfigPath(self.path.clone()));
    }

    fn visit<T: Serialize + ?Sized>(
        &mut self,
        segment: PathSegment,
        value: &T,
    ) -> Result<(), Error> {
        self.path.push(segment);
        let result = value.serialize(&mut *self);
        self.path.pop();
        result
    }
}

/// A list, map, struct or variant being serialized.
struct Compound<'a> {
    finder: &'a mut Finder,
    /// The next list index.
    index: usize,
    /// The pending map key.
    key: Option<String>,
    /// Whether the compound pushed a variant's name onto the path.
    in_variant: bool,
}

impl<'a> Compound<'a> {
    fn new(finder: &'a mut Finder, variant: Option<&'static str>) -> Self {
        if let Some(variant) = variant {
            finder.record_union();
            finder.path.push(PathSegment::Key(variant.to_owned()));
        }
        Compound {
            finder,
            index: 0,
            key: None,
            in_variant: variant.is_some(),
        }
    }

    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let index = self.index;
        self.index += 1;
        self.finder.visit(PathSegment::Index(index), value)
    }

    fn field<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) -> Result<(), Error> {
        self.finder.visit(PathSegment::Key(key.to_owned()), value)
    }

    fn finish(self) -> Result<(), Error> {
        if self.in_variant {
            self.finder.path.pop();
        }
        Ok(())
    }
}

macro_rules! ignore_scalars {
    ($($method:ident($ty:ty)),* $(,)?) => {
        $(fn $method(self, _: $ty) -> Result<(), Error> { Ok(()) })*
    };
}

impl<'a> ser::Serializer for &'a mut Finder {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Compound<'a>;
    type SerializeTuple = Compound<'a>;
    type SerializeTupleStruct = Compound<'a>;
    type SerializeTupleVariant = Compound<'a>;
    type SerializeMap = Compound<'a>;
    type SerializeStruct = Compound<'a>;
    type SerializeStructVariant = Compound<'a>;

    ignore_scalars!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8]),
        serialize_unit_struct(&'static str),
    );

    fn serialize_none(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), Error> {
        // A variant without a value is written as its name, not as an object.
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.record_union();
        self.visit(PathSegment::Key(variant.to_owned()), value)
    }

    fn serialize_seq(self, _: Option<usize>) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, None))
    }

    fn serialize_tuple(self, _: usize) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, None))
    }

    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, None))
    }

    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, Some(variant)))
    }

    fn serialize_map(self, _: Option<usize>) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, None))
    }

    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, None))
    }

    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Compound<'a>, Error> {
        Ok(Compound::new(self, Some(variant)))
    }
}

impl ser::SerializeSeq for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeTuple for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeTupleStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeTupleVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.element(value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeMap for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), Error> {
        // The key serde_json writes: a string as it is, a number or a boolean
        // as its text.
        self.key = Some(match serde_json::to_value(key)? {
            Value::String(key) => key,
            other => other.to_string(),
        });
        Ok(())
    }

    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        let key = self
            .key
            .take()
            .ok_or_else(|| ser::Error::custom("a map value without a key"))?;
        self.finder.visit(PathSegment::Key(key), value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeStruct for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.field(key, value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}

impl ser::SerializeStructVariant for Compound<'_> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        self.field(key, value)
    }

    fn end(self) -> Result<(), Error> {
        self.finish()
    }
}
