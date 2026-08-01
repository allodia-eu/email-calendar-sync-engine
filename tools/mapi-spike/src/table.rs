//! The folder/table ROP chain: `RopOpenFolder` (0x02), `RopGetHierarchyTable`
//! (0x04) / `RopGetContentsTable` (0x05), `RopSetColumns` (0x12),
//! `RopQueryRows` (0x15).
//!
//! All four go into **one** `RopBuffer`, chained through the handle table:
//! `RopOpenFolder` writes its handle into slot 1, `RopGetContentsTable` reads
//! slot 1 and writes slot 2, and both table ROPs read slot 2. The server
//! processes the list in order and updates the table in place, so this is one
//! HTTP round trip rather than three. Whether a given server honours that is a
//! live measurement, and a hard gate: without it every folder walk costs N
//! round trips.

use crate::{
    cursor::{Reader, Result, Writer},
    proptag::{self, Row},
    rop::{
        ROP_GET_CONTENTS_TABLE, ROP_GET_HIERARCHY_TABLE, ROP_OPEN_FOLDER, ROP_QUERY_ROWS,
        ROP_SET_COLUMNS,
    },
};

/// Slot assignments used by the CP4 chain.
pub const H_LOGON: u8 = 0;
pub const H_FOLDER: u8 = 1;
pub const H_TABLE: u8 = 2;

/// `RopOpenFolder` ([MS-OXCROPS] §2.2.4.1). `OpenModeFlags = 0` opens the
/// folder without creating it.
pub fn open_folder_request(input: u8, output: u8, folder_id: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(ROP_OPEN_FOLDER)
        .u8(0x00)
        .u8(input)
        .u8(output)
        .u64(folder_id)
        .u8(0x00);
    w.finish()
}

/// `RopGetContentsTable` / `RopGetHierarchyTable`. `TableFlags = 0` is a plain
/// non-associated, non-deferred table.
pub fn get_table_request(rop_id: u8, input: u8, output: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(rop_id).u8(0x00).u8(input).u8(output).u8(0x00);
    w.finish()
}

pub fn contents_table_request(input: u8, output: u8) -> Vec<u8> {
    get_table_request(ROP_GET_CONTENTS_TABLE, input, output)
}

pub fn hierarchy_table_request(input: u8, output: u8) -> Vec<u8> {
    get_table_request(ROP_GET_HIERARCHY_TABLE, input, output)
}

/// `RopSetColumns` ([MS-OXCROPS] §2.2.5.1). `SetColumnsFlags = 0` asks the
/// server to block until the column set is applied rather than answer async.
pub fn set_columns_request(input: u8, columns: &[u32]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(ROP_SET_COLUMNS).u8(0x00).u8(input).u8(0x00);
    proptag::write_columns(&mut w, columns);
    w.finish()
}

/// `RopQueryRows` ([MS-OXCROPS] §2.2.5.4). `QueryRowsFlags = 0` advances the
/// cursor; `ForwardRead = 1` reads from the current position forward.
pub fn query_rows_request(input: u8, row_count: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(ROP_QUERY_ROWS)
        .u8(0x00)
        .u8(input)
        .u8(0x00)
        .u8(0x01)
        .u16(row_count);
    w.finish()
}

#[derive(Debug)]
pub struct QueryRowsResponse {
    /// 0 = the cursor is at the beginning, 2 = at the end.
    pub origin: u8,
    pub rows: Vec<Row>,
}

impl QueryRowsResponse {
    /// How many rows arrived in each form. The server picks per row, so this is
    /// a measurement of the server, not of the request.
    pub fn form_counts(&self) -> (usize, usize) {
        let flagged = self.rows.iter().filter(|r| r.flagged).count();
        (self.rows.len() - flagged, flagged)
    }
}

/// Decode the body of a `RopQueryRows` response, after `RopId`,
/// `InputHandleIndex` and a zero `ReturnValue` have been consumed.
pub fn read_query_rows(r: &mut Reader<'_>, columns: &[u32]) -> Result<QueryRowsResponse> {
    let origin = r.u8()?;
    let row_count = r.u16()?;
    let mut rows = Vec::with_capacity(row_count as usize);
    for _ in 0..row_count {
        rows.push(proptag::read_row(r, columns)?);
    }
    Ok(QueryRowsResponse { origin, rows })
}

/// `RopGetContentsTable`/`RopGetHierarchyTable` response tail: a u32 row count.
pub fn read_table_row_count(r: &mut Reader<'_>) -> Result<u32> {
    r.u32()
}

/// `RopOpenFolder` response tail: HasRules (1) then IsGhosted (1). A ghosted
/// folder carries a server list after that, which this spike does not model —
/// it reports the flag instead of guessing at the length.
pub fn read_open_folder(r: &mut Reader<'_>) -> Result<bool> {
    let _has_rules = r.u8()?;
    let is_ghosted = r.u8()? != 0;
    Ok(is_ghosted)
}

/// `RopSetColumns` response tail: TableStatus (1).
pub fn read_set_columns(r: &mut Reader<'_>) -> Result<u8> {
    r.u8()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proptag::{HIERARCHY_COLUMNS, PropValue};

    #[test]
    fn open_folder_layout() {
        let bytes = open_folder_request(H_LOGON, H_FOLDER, 0x0D00_0000_0000_0001);
        #[rustfmt::skip]
        let expected = vec![
            0x02,                   // RopId
            0x00,                   // LogonId
            0x00,                   // InputHandleIndex
            0x01,                   // OutputHandleIndex
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0D, // FolderId (LE)
            0x00,                   // OpenModeFlags
        ];
        assert_eq!(bytes, expected);
    }

    #[test]
    fn get_table_layout() {
        assert_eq!(
            contents_table_request(H_FOLDER, H_TABLE),
            vec![0x05, 0x00, 0x01, 0x02, 0x00]
        );
        assert_eq!(
            hierarchy_table_request(H_FOLDER, H_TABLE),
            vec![0x04, 0x00, 0x01, 0x02, 0x00]
        );
    }

    #[test]
    fn set_columns_layout() {
        let bytes = set_columns_request(H_TABLE, &HIERARCHY_COLUMNS);
        assert_eq!(&bytes[0..4], &[0x12, 0x00, 0x02, 0x00]);
        assert_eq!(&bytes[4..6], &[0x04, 0x00]); // PropertyTagCount
        assert_eq!(bytes.len(), 4 + 2 + 16);
    }

    #[test]
    fn query_rows_layout() {
        assert_eq!(
            query_rows_request(H_TABLE, 25),
            vec![0x15, 0x00, 0x02, 0x00, 0x01, 0x19, 0x00]
        );
    }

    /// The chain is only one round trip if each ROP's input index is the
    /// previous ROP's output index — within the same buffer.
    #[test]
    fn the_chain_threads_handle_indices_not_handles() {
        let open = open_folder_request(H_LOGON, H_FOLDER, 1);
        let table = contents_table_request(H_FOLDER, H_TABLE);
        let cols = set_columns_request(H_TABLE, &HIERARCHY_COLUMNS);
        let rows = query_rows_request(H_TABLE, 10);

        assert_eq!(open[2], H_LOGON); // reads the logon handle
        assert_eq!(open[3], table[2]); // open's output is the table ROP's input
        assert_eq!(table[3], cols[2]); // table's output feeds SetColumns
        assert_eq!(cols[2], rows[2]); // ...and QueryRows
        // Every one of those is a 1-byte index; a 4-byte handle never appears.
        assert!(open[2] < 8 && table[3] < 8);
    }

    #[test]
    fn decodes_query_rows_with_two_rows() {
        let mut w = Writer::new();
        w.u8(0x00) // Origin = beginning
            .u16(2); // RowCount
        for (fid, name) in [(0x11u64, "Inbox"), (0x22, "Sent Items")] {
            w.u8(0x00).u64(fid);
            for u in name.encode_utf16() {
                w.u16(u);
            }
            w.u16(0);
            w.u32(3).u8(0);
        }
        let buf = w.finish();

        let mut r = Reader::new(&buf);
        let resp = read_query_rows(&mut r, &HIERARCHY_COLUMNS).unwrap();
        assert_eq!(resp.origin, 0);
        assert_eq!(resp.rows.len(), 2);
        assert_eq!(resp.rows[0][1], PropValue::Str("Inbox".into()));
        assert_eq!(resp.rows[1][1], PropValue::Str("Sent Items".into()));
        assert_eq!(resp.form_counts(), (2, 0));
        assert!(r.is_empty());
    }

    #[test]
    fn zero_rows_is_a_valid_answer_not_an_error() {
        let mut w = Writer::new();
        w.u8(0x02).u16(0);
        let mut r = Reader::new(w.finish().leak());
        let resp = read_query_rows(&mut r, &HIERARCHY_COLUMNS).unwrap();
        assert!(resp.rows.is_empty());
        assert_eq!(resp.origin, 2);
    }

    /// A RowCount that lies must error, not allocate 65535 rows of garbage.
    #[test]
    fn lying_row_count_errors_rather_than_over_reading() {
        let mut w = Writer::new();
        w.u8(0x00).u16(0xFFFF);
        let mut r = Reader::new(w.finish().leak());
        assert!(read_query_rows(&mut r, &HIERARCHY_COLUMNS).is_err());
    }

    #[test]
    fn truncated_table_responses_never_panic() {
        for buf in [&b""[..], &[0x00][..], &[0x00, 0x02][..], &[0xFF; 16][..]] {
            let mut r = Reader::new(buf);
            let _ = read_query_rows(&mut r, &HIERARCHY_COLUMNS);
            let mut r = Reader::new(buf);
            let _ = read_open_folder(&mut r);
        }
    }
}
