// SPDX-License-Identifier: AGPL-3.0-or-later
//! Verify that a typed shard survives the browser's JSON round trip without
//! conversion. Common values need no allocation; exceptional values use the
//! same JS normalization as the ordinary client, never a guessed correction.
use serde::ser::*;
use serde::Serialize;
use std::fmt;
use wfsim_engine::fight::Shard;

#[derive(Debug)]
pub struct NeedsJson;
impl fmt::Display for NeedsJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("number needs JS normalization")
    }
}
impl std::error::Error for NeedsJson {}
impl serde::ser::Error for NeedsJson {
    fn custom<T: fmt::Display>(_: T) -> Self {
        Self
    }
}

pub fn carry(shard: Shard) -> anyhow::Result<Shard> {
    if shard.serialize(Check).is_ok() {
        return Ok(shard);
    }
    Ok(serde_json::from_str(&crate::json_numbers::normalize(&serde_json::to_string(&shard)?, false))?)
}

pub fn carry_acc(shard: &mut Shard) -> anyhow::Result<()> {
    if shard.serialize(Check).is_err() {
        *shard = serde_json::from_str(&crate::json_numbers::normalize(&serde_json::to_string(shard)?, false))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Check;
macro_rules! integer {
    ($($name:ident: $ty:ty),*) => { $(
        fn $name(self, v: $ty) -> Result<(), NeedsJson> {
            if (v as i128).unsigned_abs() <= (1u128 << 53) { Ok(()) } else { Err(NeedsJson) }
        }
    )* };
}
impl Serializer for Check {
    type Ok = ();
    type Error = NeedsJson;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;
    integer!(serialize_i8:i8, serialize_i16:i16, serialize_i32:i32, serialize_i64:i64,
        serialize_u8:u8, serialize_u16:u16, serialize_u32:u32, serialize_u64:u64);
    fn serialize_f64(self, v: f64) -> Result<(), NeedsJson> {
        if v.is_finite() && !(v == 0.0 && v.is_sign_negative()) {
            Ok(())
        } else {
            Err(NeedsJson)
        }
    }
    fn serialize_f32(self, _: f32) -> Result<(), NeedsJson> {
        Err(NeedsJson)
    }
    fn serialize_bool(self, _: bool) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_char(self, _: char) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_str(self, _: &str) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_bytes(self, _: &[u8]) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_none(self) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<(), NeedsJson> {
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, _: u32, _: &'static str) -> Result<(), NeedsJson> {
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<(), NeedsJson> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, _: u32, _: &'static str, v: &T) -> Result<(), NeedsJson> {
        v.serialize(self)
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_tuple_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, NeedsJson> {
        Ok(self)
    }
    fn serialize_struct_variant(self, _: &'static str, _: u32, _: &'static str, _: usize) -> Result<Self, NeedsJson> {
        Ok(self)
    }
}
macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl $trait for Check {
            type Ok = ();
            type Error = NeedsJson;
            fn $method<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), NeedsJson> {
                v.serialize(*self)
            }
            fn end(self) -> Result<(), NeedsJson> {
                Ok(())
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);
impl SerializeMap for Check {
    type Ok = ();
    type Error = NeedsJson;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), NeedsJson> {
        v.serialize(*self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<(), NeedsJson> {
        v.serialize(*self)
    }
    fn end(self) -> Result<(), NeedsJson> {
        Ok(())
    }
}
macro_rules! structure {
    ($trait:ident) => {
        impl $trait for Check {
            type Ok = ();
            type Error = NeedsJson;
            fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> Result<(), NeedsJson> {
                v.serialize(*self)
            }
            fn end(self) -> Result<(), NeedsJson> {
                Ok(())
            }
        }
    };
}
structure!(SerializeStruct);
structure!(SerializeStructVariant);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exceptional_numbers_use_json() {
        assert!((1u64 << 53).serialize(Check).is_ok());
        assert!(((1u64 << 53) + 1).serialize(Check).is_err());
        assert!((-0.0f64).serialize(Check).is_err());
        assert!(f64::INFINITY.serialize(Check).is_err());
        assert!(0.125f64.serialize(Check).is_ok());
        assert!([0.0f64, f64::NAN].serialize(Check).is_err());
    }
}
