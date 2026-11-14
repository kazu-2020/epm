use bytes::{Buf, BufMut, Bytes, BytesMut};
use dimension::MemberIndex;
use measure::MeasureGlobalId;
use ulid::Ulid;
use workspace::{Edit, EditCheck, EditError, Edits};

use crate::Invalid;

const VERSION: u8 = 1;
const CLEAR: u8 = 0;
const SET: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Version(u8),
    Truncated,
    Tag(u8),
    NotFinite,
    TrailingBytes,
}

pub fn check(edit_check: &EditCheck, payload: &Bytes) -> Result<(), Invalid> {
    add(edit_check, payload, &mut Edits::default())
}

pub(crate) fn add(
    edit_check: &EditCheck,
    payload: &Bytes,
    edits: &mut Edits,
) -> Result<(), Invalid> {
    let edit = decode(payload.clone()).map_err(Invalid::Undecodable)?;
    edit_check.add(&edit, edits).map_err(|error| match error {
        EditError::NotAnInput(measure) => Invalid::NotAnInput(measure),
        EditError::Coordinate(error) => Invalid::Coordinate(error),
    })
}

pub fn encode(edit: &Edit) -> Bytes {
    let mut out = BytesMut::new();
    out.put_u8(VERSION);
    out.put_slice(&edit.measure.0.to_bytes());
    let count = u32::try_from(edit.coordinate.len()).expect("a coordinate has < 2^32 members");
    out.put_u32_le(count);
    for member in &edit.coordinate {
        out.put_u32_le(member.0);
    }
    match edit.value {
        None => out.put_u8(CLEAR),
        Some(value) => {
            out.put_u8(SET);
            out.put_f64_le(value);
        }
    }
    out.freeze()
}

pub fn decode(mut bytes: Bytes) -> Result<Edit, DecodeError> {
    let version = take(&mut bytes, 1)?.get_u8();
    if version != VERSION {
        return Err(DecodeError::Version(version));
    }
    let measure = MeasureGlobalId(Ulid(take(&mut bytes, 16)?.get_u128()));
    let count = take(&mut bytes, 4)?.get_u32_le();
    // Not Vec::with_capacity(count): the count is untrusted until the bytes behind it are read.
    let mut coordinate = Vec::new();
    for _ in 0..count {
        coordinate.push(MemberIndex(take(&mut bytes, 4)?.get_u32_le()));
    }
    let value = match take(&mut bytes, 1)?.get_u8() {
        CLEAR => None,
        SET => Some(take(&mut bytes, 8)?.get_f64_le()),
        tag => return Err(DecodeError::Tag(tag)),
    };
    if value.is_some_and(|value| !value.is_finite()) {
        return Err(DecodeError::NotFinite);
    }
    if bytes.has_remaining() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(Edit {
        measure,
        coordinate,
        value,
    })
}

fn take(bytes: &mut Bytes, length: usize) -> Result<Bytes, DecodeError> {
    if bytes.remaining() < length {
        return Err(DecodeError::Truncated);
    }
    Ok(bytes.split_to(length))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(value: Option<f64>) -> Edit {
        Edit {
            measure: MeasureGlobalId(Ulid(0x0102)),
            coordinate: vec![MemberIndex(3), MemberIndex(0x0405)],
            value,
        }
    }

    #[test]
    fn an_edit_reads_back_as_written() {
        for value in [Some(-12.25), Some(0.0), None] {
            assert_eq!(decode(encode(&edit(value))), Ok(edit(value)));
        }
    }

    #[test]
    fn the_layout_is_fixed() {
        let ulid = b"\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x01\x02";
        let members = b"\x02\0\0\0\x03\0\0\0\x05\x04\0\0";
        let cleared = [&b"\x01"[..], ulid, members, b"\0"].concat();
        assert_eq!(&encode(&edit(None))[..], cleared);
        let set = [&b"\x01"[..], ulid, members, b"\x01", &1.5f64.to_le_bytes()].concat();
        assert_eq!(&encode(&edit(Some(1.5)))[..], set);
    }

    #[test]
    fn every_truncation_is_an_error() {
        let bytes = encode(&edit(Some(2.0)));
        for end in 0..bytes.len() {
            assert_eq!(
                decode(bytes.slice(..end)),
                Err(DecodeError::Truncated),
                "cut at {end}"
            );
        }
    }

    #[test]
    fn bad_versions_tags_values_and_trailing_bytes_are_errors() {
        let bytes = encode(&edit(Some(2.0))).to_vec();
        let tag = bytes.len() - 9;
        let mut version = bytes.clone();
        version[0] = 2;
        assert_eq!(decode(version.into()), Err(DecodeError::Version(2)));
        let mut unknown = bytes.clone();
        unknown[tag] = 2;
        assert_eq!(decode(unknown.into()), Err(DecodeError::Tag(2)));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut not_finite = bytes[..=tag].to_vec();
            not_finite.extend_from_slice(&value.to_le_bytes());
            assert_eq!(
                decode(not_finite.into()),
                Err(DecodeError::NotFinite),
                "{value}"
            );
        }
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(decode(trailing.into()), Err(DecodeError::TrailingBytes));
    }

    #[test]
    fn a_count_past_the_end_is_an_error() {
        let mut huge = encode(&edit(None)).to_vec();
        huge[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode(huge.into()), Err(DecodeError::Truncated));
    }
}
