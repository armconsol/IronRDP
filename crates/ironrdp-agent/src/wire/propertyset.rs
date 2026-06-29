//! Binary wire codec for [`PropertySet`].
//!
//! This mirrors the shape of [`ironrdp_rdpfile::load`]/[`ironrdp_rdpfile::write`] but is binary and
//! cursor-based so it composes with [`ironrdp_core`]'s `Encode`/`DecodeOwned` traits.
//!
//! Layout: a `u32` entry count, then for each entry a length-prefixed UTF-8 key, a 1-byte value tag
//! (`0` = `Int`, `1` = `Str`), and the value (an `i64`, or a length-prefixed UTF-8 string).
//!
//! [`ironrdp_rdpfile::load`]: https://docs.rs/ironrdp-rdpfile

use ironrdp_core::{DecodeResult, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size};
use ironrdp_propertyset::{PropertySet, Value};

use crate::wire::{read_string, string_size, write_string};

const TAG_INT: u8 = 0;
const TAG_STR: u8 = 1;

/// Size on the wire of `properties`, for use from an enclosing `Encode::size`.
pub(crate) fn size(properties: &PropertySet) -> usize {
    let mut total = 4; // Entry count.
    for (key, value) in properties.iter() {
        total += string_size(key); // Key.
        total += 1; // Value tag.
        total += match value {
            Value::Int(_) => 8,                      // i64.
            Value::Str(value) => string_size(value), // Length-prefixed string.
        };
    }
    total
}

/// Encodes `properties` into `dst`.
pub(crate) fn write(properties: &PropertySet, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
    ensure_size!(in: dst, size: size(properties));

    let count: u32 = cast_length!("property count", properties.iter().count())?;
    dst.write_u32(count);

    for (key, value) in properties.iter() {
        write_string(dst, key)?;
        match value {
            Value::Int(value) => {
                dst.write_u8(TAG_INT);
                dst.write_i64(*value);
            }
            Value::Str(value) => {
                dst.write_u8(TAG_STR);
                write_string(dst, value)?;
            }
        }
    }

    Ok(())
}

/// Decodes entries from `src`, inserting them into `properties` (layering onto any existing keys,
/// matching the contract of [`ironrdp_rdpfile::load`]).
pub(crate) fn read(properties: &mut PropertySet, src: &mut ReadCursor<'_>) -> DecodeResult<()> {
    ensure_size!(in: src, size: 4);
    let count = src.read_u32();

    for _ in 0..count {
        let key = read_string(src)?;

        ensure_size!(in: src, size: 1);
        match src.read_u8() {
            TAG_INT => {
                ensure_size!(in: src, size: 8);
                properties.insert(key, src.read_i64());
            }
            TAG_STR => {
                let value = read_string(src)?;
                properties.insert(key, value);
            }
            _ => return Err(ironrdp_core::invalid_field_err!("property value tag", "unknown tag")),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_arbitrary_property_set() {
        let mut ps = PropertySet::new();
        ps.insert("full address", "192.0.2.10:3389");
        ps.insert("username", "alice");
        ps.insert("ClearTextPassword", "hunter2");
        ps.insert("desktopwidth", 1920_i64);
        ps.insert("desktopheight", 1080_i64);
        ps.insert("ironrdp_colordepth", 32_i64);
        ps.insert("empty", "");
        ps.insert("unicode key 🔑", "value with spaces and 🚀");

        let mut buf = vec![0u8; size(&ps)];
        let mut write_cursor = WriteCursor::new(&mut buf);
        write(&ps, &mut write_cursor).expect("write");
        assert_eq!(write_cursor.pos(), buf.len(), "size() must match bytes written");

        let mut decoded = PropertySet::new();
        let mut read_cursor = ReadCursor::new(&buf);
        read(&mut decoded, &mut read_cursor).expect("read");

        assert_eq!(ps, decoded);
        assert!(read_cursor.is_empty(), "all bytes consumed");
    }
}
