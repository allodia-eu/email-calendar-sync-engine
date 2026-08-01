//! Property tags and row decoding ([MS-OXCDATA] §2.9, §2.11, §2.8.1).
//!
//! **`PropertyRow` is not self-describing.** A row carries values only — the
//! types come entirely from the column set the client last sent in
//! `RopSetColumns`. So the ROP layer is *stateful*, and the state that matters
//! is `table handle -> ordered column list`. Lose it and the bytes are
//! undecodable. That is why `read_row` takes the columns as an argument rather
//! than discovering them.
//!
//! A `PropertyTag` ([MS-OXCDATA] §2.9) is PropertyType (u16) then PropertyId
//! (u16) — which is exactly the conventional `0xIIIITTTT` constant written as a
//! little-endian u32. `PidTagSubject = 0x0037001F` goes out as `1F 00 37 00`,
//! so the constants below can stay in the notation the docs use.

use crate::cursor::{Reader, Result, Writer};

pub const PT_INTEGER32: u16 = 0x0003;
pub const PT_ERROR: u16 = 0x000A;
pub const PT_BOOLEAN: u16 = 0x000B;
pub const PT_INTEGER64: u16 = 0x0014;
pub const PT_STRING: u16 = 0x001F;
pub const PT_TIME: u16 = 0x0040;

// Contents-table columns: Int64 / String / Time / Int32.
pub const PID_TAG_MID: u32 = 0x674A_0014;
pub const PID_TAG_SUBJECT: u32 = 0x0037_001F;
pub const PID_TAG_MESSAGE_DELIVERY_TIME: u32 = 0x0E06_0040;
pub const PID_TAG_MESSAGE_FLAGS: u32 = 0x0E07_0003;

// Hierarchy-table columns: Int64 / String / Int32 / Boolean.
pub const PID_TAG_FOLDER_ID: u32 = 0x6748_0014;
pub const PID_TAG_DISPLAY_NAME: u32 = 0x3001_001F;
pub const PID_TAG_CONTENT_COUNT: u32 = 0x3602_0003;
pub const PID_TAG_SUBFOLDERS: u32 = 0x360A_000B;

pub const CONTENTS_COLUMNS: [u32; 4] = [
    PID_TAG_MID,
    PID_TAG_SUBJECT,
    PID_TAG_MESSAGE_DELIVERY_TIME,
    PID_TAG_MESSAGE_FLAGS,
];
pub const HIERARCHY_COLUMNS: [u32; 4] = [
    PID_TAG_FOLDER_ID,
    PID_TAG_DISPLAY_NAME,
    PID_TAG_CONTENT_COUNT,
    PID_TAG_SUBFOLDERS,
];

pub fn prop_type(tag: u32) -> u16 {
    (tag & 0xFFFF) as u16
}

pub fn tag_name(tag: u32) -> &'static str {
    match tag {
        PID_TAG_MID => "PidTagMid",
        PID_TAG_SUBJECT => "PidTagSubject",
        PID_TAG_MESSAGE_DELIVERY_TIME => "PidTagMessageDeliveryTime",
        PID_TAG_MESSAGE_FLAGS => "PidTagMessageFlags",
        PID_TAG_FOLDER_ID => "PidTagFolderId",
        PID_TAG_DISPLAY_NAME => "PidTagDisplayName",
        PID_TAG_CONTENT_COUNT => "PidTagContentCount",
        PID_TAG_SUBFOLDERS => "PidTagSubfolders",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    I32(u32),
    I64(u64),
    Bool(bool),
    Str(String),
    /// A FILETIME: 100-nanosecond intervals since 1601-01-01 UTC.
    Time(u64),
    /// The column came back as an error rather than a value (flag 0xA). Routine
    /// for strings, because table values are length-limited.
    Error(u32),
    /// Flag 0x1: not present, and *no bytes were consumed*.
    Absent,
}

impl std::fmt::Display for PropValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::I32(v) => write!(f, "{v}"),
            Self::I64(v) => write!(f, "0x{v:016X}"),
            Self::Bool(v) => write!(f, "{v}"),
            Self::Str(v) => write!(f, "{v:?}"),
            Self::Time(v) => write!(f, "FILETIME({v})"),
            Self::Error(v) => write!(f, "<error 0x{v:08X}>"),
            Self::Absent => write!(f, "<absent>"),
        }
    }
}

/// Read one value of the type implied by its column.
pub fn read_value(r: &mut Reader<'_>, ptype: u16) -> Result<PropValue> {
    Ok(match ptype {
        PT_INTEGER32 => PropValue::I32(r.u32()?),
        PT_ERROR => PropValue::Error(r.u32()?),
        PT_BOOLEAN => PropValue::Bool(r.u8()? != 0),
        PT_INTEGER64 => PropValue::I64(r.u64()?),
        PT_STRING => PropValue::Str(r.utf16_z()?),
        PT_TIME => PropValue::Time(r.u64()?),
        // An unmodelled type cannot be skipped safely — its length is unknown,
        // so stopping is the only honest option.
        _ => {
            return Err(crate::cursor::Error::Truncated {
                at: r.pos(),
                need: 0,
                have: 0,
            });
        }
    })
}

/// Read one `PropertyRow` ([MS-OXCDATA] §2.8.1) against `columns`.
///
/// The leading byte selects the form: `0x00` is a StandardPropertyRow (values
/// in column order, no per-value flag); `0x01` is a FlaggedPropertyRow, whose
/// per-value flag is `0x0` (value follows), `0x1` (absent — consume nothing) or
/// `0xA` (a u32 error code follows). A client MUST handle both; which one the
/// server picks is its choice, not the client's.
pub fn read_row(r: &mut Reader<'_>, columns: &[u32]) -> Result<Vec<PropValue>> {
    let flag = r.u8()?;
    let mut out = Vec::with_capacity(columns.len());
    for &tag in columns {
        if flag == 0x00 {
            out.push(read_value(r, prop_type(tag))?);
            continue;
        }
        match r.u8()? {
            0x00 => out.push(read_value(r, prop_type(tag))?),
            0x01 => out.push(PropValue::Absent),
            0x0A => out.push(PropValue::Error(r.u32()?)),
            _ => {
                return Err(crate::cursor::Error::Truncated {
                    at: r.pos(),
                    need: 0,
                    have: 0,
                });
            }
        }
    }
    Ok(out)
}

/// `PropertyTagArray` as `RopSetColumns` carries it: a u16 count then the tags.
pub fn write_columns(w: &mut Writer, columns: &[u32]) {
    w.u16(columns.len() as u16);
    for &tag in columns {
        w.u32(tag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical `0xIIIITTTT` constant, little-endian, *is* the wire form.
    #[test]
    fn property_tag_le_u32_is_type_then_id() {
        let mut w = Writer::new();
        w.u32(PID_TAG_SUBJECT);
        assert_eq!(w.finish(), vec![0x1F, 0x00, 0x37, 0x00]);
        assert_eq!(prop_type(PID_TAG_SUBJECT), PT_STRING);
        assert_eq!(prop_type(PID_TAG_SUBFOLDERS), PT_BOOLEAN);
        assert_eq!(prop_type(PID_TAG_MID), PT_INTEGER64);
    }

    #[test]
    fn set_columns_array_is_count_then_tags() {
        let mut w = Writer::new();
        write_columns(&mut w, &HIERARCHY_COLUMNS);
        let bytes = w.finish();
        assert_eq!(&bytes[0..2], &[0x04, 0x00]);
        assert_eq!(bytes.len(), 2 + 4 * 4);
    }

    fn hierarchy_row_standard() -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(0x00); // StandardPropertyRow
        w.u64(0x0D00_0000_0000_0001); // PidTagFolderId
        for u in "Inbox".encode_utf16() {
            w.u16(u);
        }
        w.u16(0);
        w.u32(7); // PidTagContentCount
        w.u8(1); // PidTagSubfolders
        w.finish()
    }

    #[test]
    fn decodes_a_standard_property_row() {
        let buf = hierarchy_row_standard();
        let mut r = Reader::new(&buf);
        let row = read_row(&mut r, &HIERARCHY_COLUMNS).unwrap();
        assert_eq!(row[0], PropValue::I64(0x0D00_0000_0000_0001));
        assert_eq!(row[1], PropValue::Str("Inbox".into()));
        assert_eq!(row[2], PropValue::I32(7));
        assert_eq!(row[3], PropValue::Bool(true));
        assert!(r.is_empty());
    }

    /// Flag 0x1 means the value is **absent and consumes no bytes** — the
    /// mistake that desynchronises every later column in the row.
    #[test]
    fn flagged_row_handles_present_absent_and_error() {
        let mut w = Writer::new();
        w.u8(0x01); // FlaggedPropertyRow
        w.u8(0x00).u64(0x1234); // present
        w.u8(0x0A).u32(0x8004_0301); // error (string too long for a table)
        w.u8(0x01); // absent — no value bytes at all
        w.u8(0x00).u8(0); // present
        let buf = w.finish();

        let mut r = Reader::new(&buf);
        let row = read_row(&mut r, &HIERARCHY_COLUMNS).unwrap();
        assert_eq!(row[0], PropValue::I64(0x1234));
        assert_eq!(row[1], PropValue::Error(0x8004_0301));
        assert_eq!(row[2], PropValue::Absent);
        assert_eq!(row[3], PropValue::Bool(false));
        assert!(r.is_empty(), "absent must consume nothing");
    }

    /// The same bytes decode differently under a different column set — the
    /// clearest demonstration that rows are not self-describing.
    #[test]
    fn identical_bytes_decode_differently_per_column_set() {
        let mut w = Writer::new();
        w.u8(0x00).u32(1).u32(2);
        let buf = w.finish();

        let two_i32 = [PID_TAG_MESSAGE_FLAGS, PID_TAG_CONTENT_COUNT];
        let row = read_row(&mut Reader::new(&buf), &two_i32).unwrap();
        assert_eq!(row, vec![PropValue::I32(1), PropValue::I32(2)]);

        let one_i64 = [PID_TAG_MID];
        let row = read_row(&mut Reader::new(&buf), &one_i64).unwrap();
        assert_eq!(row, vec![PropValue::I64(0x0000_0002_0000_0001)]);
    }

    #[test]
    fn unmodelled_type_stops_rather_than_guessing_a_length() {
        // PtypBinary — deliberately out of scope, because its COUNT is 2 bytes
        // in a ROP buffer but 4 in a FastTransfer stream.
        assert!(read_value(&mut Reader::new(&[0u8; 8]), 0x0102).is_err());
    }

    #[test]
    fn truncated_rows_never_panic() {
        for buf in [
            &b""[..],
            &[0x00][..],
            &[0x00, 0x01, 0x02][..],
            &[0x01, 0x0A][..],
            &[0xFF; 12][..],
        ] {
            let mut r = Reader::new(buf);
            let _ = read_row(&mut r, &HIERARCHY_COLUMNS);
        }
    }
}
