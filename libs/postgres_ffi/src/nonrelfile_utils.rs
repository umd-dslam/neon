//!
//! Common utilities for dealing with PostgreSQL non-relation files.
//!
use crate::pg_constants;
use crate::transaction_id_precedes;
use byteorder::{ByteOrder, LittleEndian};
use bytes::BytesMut;
use log::*;

use super::bindings::{MultiXactId, XidCSN};

pub fn transaction_id_set_status(xid: u32, status: u8, page: &mut BytesMut) {
    trace!(
        "handle_apply_request for RM_XACT_ID-{} (1-commit, 2-abort, 3-sub_commit)",
        status
    );

    let byteno: usize =
        ((xid % pg_constants::CLOG_XACTS_PER_PAGE) / pg_constants::CLOG_XACTS_PER_BYTE) as usize;

    let bshift: u8 =
        ((xid % pg_constants::CLOG_XACTS_PER_BYTE) * pg_constants::CLOG_BITS_PER_XACT as u32) as u8;

    page[byteno] =
        (page[byteno] & !(pg_constants::CLOG_XACT_BITMASK << bshift)) | (status << bshift);
}

pub fn transaction_id_get_status(xid: u32, page: &[u8]) -> u8 {
    let byteno: usize =
        ((xid % pg_constants::CLOG_XACTS_PER_PAGE) / pg_constants::CLOG_XACTS_PER_BYTE) as usize;

    let bshift: u8 =
        ((xid % pg_constants::CLOG_XACTS_PER_BYTE) * pg_constants::CLOG_BITS_PER_XACT as u32) as u8;

    (page[byteno] >> bshift) & pg_constants::CLOG_XACT_BITMASK
}

pub fn transaction_id_set_csn(xid: u32, csn: XidCSN, page: &mut BytesMut) {
    trace!("handle_apply_csn_request for RM_XACT_ID-{}", csn);

    let entryno: usize = xid as usize % pg_constants::CSN_LOG_XACTS_PER_PAGE as usize;
    let bytebegin: usize = entryno * pg_constants::CSN_SIZE as usize;
    let byteend: usize = bytebegin + pg_constants::CSN_SIZE as usize;

    LittleEndian::write_u64(&mut page[bytebegin..byteend], csn);
}

// See CLOGPagePrecedes in clog.c
pub const fn clogpage_precedes(page1: u32, page2: u32) -> bool {
    let mut xid1 = page1 * pg_constants::CLOG_XACTS_PER_PAGE;
    xid1 += pg_constants::FIRST_NORMAL_TRANSACTION_ID + 1;
    let mut xid2 = page2 * pg_constants::CLOG_XACTS_PER_PAGE;
    xid2 += pg_constants::FIRST_NORMAL_TRANSACTION_ID + 1;

    transaction_id_precedes(xid1, xid2)
        && transaction_id_precedes(xid1, xid2 + pg_constants::CLOG_XACTS_PER_PAGE - 1)
}

// See SlruMayDeleteSegment() in slru.c
pub fn slru_may_delete_segment(segpage: u32, cutoff_page: u32) -> bool {
    let seg_last_page = segpage + pg_constants::SLRU_PAGES_PER_SEGMENT - 1;

    assert_eq!(segpage % pg_constants::SLRU_PAGES_PER_SEGMENT, 0);

    clogpage_precedes(segpage, cutoff_page) && clogpage_precedes(seg_last_page, cutoff_page)
}

// Multixact utils

pub fn mx_offset_to_flags_offset(xid: MultiXactId) -> usize {
    ((xid / pg_constants::MULTIXACT_MEMBERS_PER_MEMBERGROUP as u32)
        % pg_constants::MULTIXACT_MEMBERGROUPS_PER_PAGE as u32
        * pg_constants::MULTIXACT_MEMBERGROUP_SIZE as u32) as usize
}

pub fn mx_offset_to_flags_bitshift(xid: MultiXactId) -> u16 {
    (xid as u16) % pg_constants::MULTIXACT_MEMBERS_PER_MEMBERGROUP
        * pg_constants::MXACT_MEMBER_BITS_PER_XACT
}

/* Location (byte offset within page) of TransactionId of given member */
pub fn mx_offset_to_member_offset(xid: MultiXactId) -> usize {
    mx_offset_to_flags_offset(xid)
        + (pg_constants::MULTIXACT_FLAGBYTES_PER_GROUP
            + (xid as u16 % pg_constants::MULTIXACT_MEMBERS_PER_MEMBERGROUP) * 4) as usize
}

fn mx_offset_to_member_page(xid: u32, region: u32) -> u32 {
    ((xid / pg_constants::MULTIXACT_MEMBERS_PER_PAGE as u32) * pg_constants::MAX_REGIONS) + region
}

pub fn mx_offset_to_member_segment(xid: u32, region: u32) -> i32 {
    (mx_offset_to_member_page(xid, region) / pg_constants::SLRU_PAGES_PER_SEGMENT) as i32
}

// See CSNLogPagePrecedes in csn_log.c
pub const fn csnlogpage_precedes(page1: u32, page2: u32) -> bool {
    if (page1 % pg_constants::MAX_REGIONS) != (page2 % pg_constants::MAX_REGIONS) {
        // The two pages don't belong to the same region.
        return false;
    }
    let mut xid1: u32 = (page1 / pg_constants::MAX_REGIONS) * pg_constants::CSN_LOG_XACTS_PER_PAGE;
    xid1 += pg_constants::FIRST_NORMAL_TRANSACTION_ID + 1;
    let mut xid2: u32 = (page2 / pg_constants::MAX_REGIONS) * pg_constants::CSN_LOG_XACTS_PER_PAGE;
    xid2 += pg_constants::FIRST_NORMAL_TRANSACTION_ID + 1;

    transaction_id_precedes(xid1, xid2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multixid_calc() {
        // Check that the mx_offset_* functions produce the same values as the
        // corresponding PostgreSQL C macros (MXOffsetTo*). These test values
        // were generated by calling the PostgreSQL macros with a little C
        // program.
        assert_eq!(mx_offset_to_member_segment(0, 0), 0);
        assert_eq!(mx_offset_to_member_page(0, 0), 0);
        assert_eq!(mx_offset_to_flags_offset(0), 0);
        assert_eq!(mx_offset_to_flags_bitshift(0), 0);
        assert_eq!(mx_offset_to_member_offset(0), 4);
        assert_eq!(mx_offset_to_member_segment(1, 0), 0);
        assert_eq!(mx_offset_to_member_page(1, 0), 0);
        assert_eq!(mx_offset_to_flags_offset(1), 0);
        assert_eq!(mx_offset_to_flags_bitshift(1), 8);
        assert_eq!(mx_offset_to_member_offset(1), 8);
        assert_eq!(mx_offset_to_member_segment(123456789, 0), 150924);
        assert_eq!(mx_offset_to_member_page(123456789, 0), 4829568);
        assert_eq!(mx_offset_to_flags_offset(123456789), 4780);
        assert_eq!(mx_offset_to_flags_bitshift(123456789), 8);
        assert_eq!(mx_offset_to_member_offset(123456789), 4788);
        assert_eq!(mx_offset_to_member_segment(u32::MAX - 1, 0), 5250570);
        assert_eq!(mx_offset_to_member_page(u32::MAX - 1, 0), 168018240);
        assert_eq!(mx_offset_to_flags_offset(u32::MAX - 1), 5160);
        assert_eq!(mx_offset_to_flags_bitshift(u32::MAX - 1), 16);
        assert_eq!(mx_offset_to_member_offset(u32::MAX - 1), 5172);
        assert_eq!(mx_offset_to_member_segment(u32::MAX, 0), 5250570);
        assert_eq!(mx_offset_to_member_page(u32::MAX, 0), 168018240);
        assert_eq!(mx_offset_to_flags_offset(u32::MAX), 5160);
        assert_eq!(mx_offset_to_flags_bitshift(u32::MAX), 24);
        assert_eq!(mx_offset_to_member_offset(u32::MAX), 5176);
    }
}
