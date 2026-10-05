// The few protobuf primitives the bootstrap and the AV channels need.

pub fn varint(value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = value;
    while v > 0x7F {
        out.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
    out.push(v as u8 & 0x7F);
    out
}

pub fn pb_string(field: u32, s: &str) -> Vec<u8> {
    let mut out = varint(((field << 3) | 2) as u64);
    out.extend(varint(s.len() as u64));
    out.extend_from_slice(s.as_bytes());
    out
}

// Wire type 0 (varint) is zero, so the tag is just the shifted field number.
pub fn pb_varint(field: u32, value: u64) -> Vec<u8> {
    let mut out = varint((field << 3) as u64);
    out.extend(varint(value));
    out
}

pub fn pb_bytes(field: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = varint(((field << 3) | 2) as u64);
    out.extend(varint(bytes.len() as u64));
    out.extend_from_slice(bytes);
    out
}

/// A field's value. Length-delimited ones come back as a slice, which is how
/// nested messages and packed repeated fields arrive.
#[derive(Debug, PartialEq, Eq)]
pub enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// The next field of a message and where the walk ended up.
///
/// Walk a nested message by calling this again on the returned slice: the
/// depth is the caller's business, which keeps this file free of message
/// definitions. Fixed-width fields are stepped over rather than returned —
/// nothing here decodes a double or a float — so a field we do want, sitting
/// behind one, stays reachable. A group (wire type 3/4) ends the walk rather
/// than being guessed at.
pub fn next_field<'a>(buf: &'a [u8], i: &mut usize) -> Option<(u32, Field<'a>)> {
    loop {
        let tag = read_varint(buf, i)?;
        match tag & 7 {
            0 => return Some(((tag >> 3) as u32, Field::Varint(read_varint(buf, i)?))),
            2 => {
                let len = read_varint(buf, i)? as usize;
                let end = i.checked_add(len)?;
                let bytes = buf.get(*i..end)?;
                *i = end;
                return Some(((tag >> 3) as u32, Field::Bytes(bytes)));
            }
            1 => *i = i.checked_add(8).filter(|end| *end <= buf.len())?,
            5 => *i = i.checked_add(4).filter(|end| *end <= buf.len())?,
            _ => return None,
        }
    }
}

/// Every top-level varint with this field number, in wire order. Repeated
/// fields usually come back as one entry per element, but a sender is allowed
/// to repeat the key instead of packing, so both are collected.
pub fn varint_fields(buf: &[u8], field: u32) -> Vec<u64> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let Some((n, value)) = next_field(buf, &mut i) else {
            break;
        };
        if n != field {
            continue;
        }
        match value {
            Field::Varint(v) => out.push(v),
            // packed repeated varints
            Field::Bytes(b) => {
                let mut j = 0usize;
                while j < b.len() {
                    let Some(v) = read_varint(b, &mut j) else {
                        break;
                    };
                    out.push(v);
                }
            }
        }
    }
    out
}

/// The first top-level submessage with this field number.
pub fn bytes_field(buf: &[u8], field: u32) -> Option<&[u8]> {
    bytes_fields(buf, field).into_iter().next()
}

/// Every top-level submessage with this field number.
pub fn bytes_fields(buf: &[u8], field: u32) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let Some((n, value)) = next_field(buf, &mut i) else {
            break;
        };
        if n == field {
            if let Field::Bytes(b) = value {
                out.push(b);
            }
        }
    }
    out
}

pub fn read_varint(buf: &[u8], i: &mut usize) -> Option<u64> {
    let mut val = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *buf.get(*i)?;
        *i += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// The first top-level varint field with this number, skipping the others.
pub fn varint_field(buf: &[u8], field: u32) -> Option<u64> {
    let mut i = 0usize;
    while i < buf.len() {
        let (n, value) = next_field(buf, &mut i)?;
        if n == field {
            // A length-delimited field wearing the same number is not a varint;
            // whoever wants one of those reaches for `bytes_field`.
            return match value {
                Field::Varint(v) => Some(v),
                Field::Bytes(_) => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trips() {
        for v in [0u64, 1, 127, 128, 300, 1 << 40] {
            let mut i = 0;
            assert_eq!(read_varint(&varint(v), &mut i), Some(v));
        }
    }

    #[test]
    fn field_is_found_behind_other_fields() {
        let mut body = pb_string(3, "abc");
        body.extend(pb_varint(1, 7));
        body.extend(pb_varint(2, 9));
        assert_eq!(varint_field(&body, 2), Some(9));
        assert_eq!(varint_field(&body, 1), Some(7));
        assert_eq!(varint_field(&body, 4), None);
    }

    #[test]
    fn garbage_yields_nothing() {
        assert_eq!(varint_field(&[0xFF, 0xFF], 1), None);
    }
}
