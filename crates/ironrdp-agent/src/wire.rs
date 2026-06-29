//! Binary wire primitives shared by the IPC message codecs.
//!
//! Everything is little-endian and cursor-based so it composes directly with [`ironrdp_core`]'s
//! `Encode`/`DecodeOwned` traits. Strings (and string-shaped payloads) are length-delimited with a
//! `u32` byte-count prefix.

pub(crate) mod propertyset;

use ironrdp_core::{DecodeResult, EncodeResult, ReadCursor, WriteCursor, cast_length, ensure_size};

/// Size on the wire of a length-prefixed UTF-8 string.
pub(crate) fn string_size(value: &str) -> usize {
    4 /* length prefix */ + value.len() /* UTF-8 bytes */
}

/// Size on the wire of an optional length-prefixed UTF-8 string.
pub(crate) fn opt_string_size(value: Option<&str>) -> usize {
    1 /* presence flag */ + value.map_or(0, string_size)
}

pub(crate) fn write_string(dst: &mut WriteCursor<'_>, value: &str) -> EncodeResult<()> {
    ensure_size!(in: dst, size: string_size(value));
    let len: u32 = cast_length!("string length", value.len())?;
    dst.write_u32(len);
    dst.write_slice(value.as_bytes());
    Ok(())
}

pub(crate) fn read_string(src: &mut ReadCursor<'_>) -> DecodeResult<String> {
    ensure_size!(in: src, size: 4);
    let len = src.read_u32();
    let len = usize::try_from(len).map_err(|_| ironrdp_core::other_err!("string", "length does not fit in usize"))?;
    ensure_size!(in: src, size: len);
    let bytes = src.read_slice(len);
    String::from_utf8(bytes.to_vec()).map_err(|_| ironrdp_core::invalid_field_err!("string", "not valid UTF-8"))
}

pub(crate) fn write_opt_string(dst: &mut WriteCursor<'_>, value: Option<&str>) -> EncodeResult<()> {
    ensure_size!(in: dst, size: 1);
    match value {
        Some(value) => {
            dst.write_u8(1);
            write_string(dst, value)
        }
        None => {
            dst.write_u8(0);
            Ok(())
        }
    }
}

pub(crate) fn read_opt_string(src: &mut ReadCursor<'_>) -> DecodeResult<Option<String>> {
    ensure_size!(in: src, size: 1);
    match src.read_u8() {
        0 => Ok(None),
        1 => Ok(Some(read_string(src)?)),
        _ => Err(ironrdp_core::invalid_field_err!(
            "optional string",
            "invalid presence flag"
        )),
    }
}

pub(crate) fn write_bool(dst: &mut WriteCursor<'_>, value: bool) -> EncodeResult<()> {
    ensure_size!(in: dst, size: 1);
    dst.write_u8(u8::from(value));
    Ok(())
}

pub(crate) fn read_bool(src: &mut ReadCursor<'_>) -> DecodeResult<bool> {
    ensure_size!(in: src, size: 1);
    Ok(src.read_u8() != 0)
}

pub(crate) fn write_char(dst: &mut WriteCursor<'_>, value: char) -> EncodeResult<()> {
    ensure_size!(in: dst, size: 4);
    dst.write_u32(u32::from(value));
    Ok(())
}

pub(crate) fn read_char(src: &mut ReadCursor<'_>) -> DecodeResult<char> {
    ensure_size!(in: src, size: 4);
    let code = src.read_u32();
    char::from_u32(code).ok_or_else(|| ironrdp_core::invalid_field_err!("char", "not a valid Unicode scalar value"))
}
