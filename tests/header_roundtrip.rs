//! `FileHeader` round-trip — proptest over the wire format.
//!
//! Covers the format-level invariants the user named in the v0.2 exit
//! criteria: random valid `FileHeader` values round-trip; bad magic /
//! wrong version / truncated bytes surface as the matching
//! `PersistError` variant rather than a panic or silent success.

use std::io::Cursor;

use iqdb_persist::format::{read_header, write_header};
use iqdb_persist::{CURRENT_VERSION, FileHeader, MAGIC, PersistError};
use iqdb_types::DistanceMetric;
use proptest::prelude::*;

fn metric_strategy() -> impl Strategy<Value = DistanceMetric> {
    prop_oneof![
        Just(DistanceMetric::Cosine),
        Just(DistanceMetric::DotProduct),
        Just(DistanceMetric::Euclidean),
        Just(DistanceMetric::Manhattan),
        Just(DistanceMetric::Hamming),
    ]
}

fn index_type_strategy() -> impl Strategy<Value = String> {
    // Short ASCII tags — production-realistic. Wider strategies have no
    // additional coverage value for the wire format.
    "[a-z0-9_-]{1,16}".prop_map(|s| s)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn header_roundtrip(
        index_type in index_type_strategy(),
        dim in 1usize..=16384,
        metric in metric_strategy(),
        n_vectors in 0usize..=(1usize << 20),
        crc32 in any::<u32>(),
    ) {
        let header = FileHeader {
            magic: MAGIC,
            version: CURRENT_VERSION,
            index_type,
            dim,
            metric,
            n_vectors,
            crc32,
        };
        let mut buf: Vec<u8> = Vec::new();
        write_header(&mut buf, &header).unwrap();

        let mut cur = Cursor::new(&buf[..]);
        let parsed = read_header(&mut cur).unwrap();
        prop_assert_eq!(parsed, header);
    }
}

// -- targeted error-shape tests ---------------------------------------

#[test]
fn bad_magic_surfaces_as_bad_magic() {
    let mut buf: Vec<u8> = b"NOTAFILE".to_vec();
    buf.extend_from_slice(&1u32.to_le_bytes());

    let mut cur = Cursor::new(&buf[..]);
    let err = read_header(&mut cur).unwrap_err();
    assert!(matches!(err, PersistError::BadMagic { .. }));
}

#[test]
fn wrong_version_surfaces_as_unsupported_version() {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&999u32.to_le_bytes());

    let mut cur = Cursor::new(&buf[..]);
    let err = read_header(&mut cur).unwrap_err();
    assert!(matches!(
        err,
        PersistError::UnsupportedVersion {
            found: 999,
            supported: 1,
        }
    ));
}

#[test]
fn truncated_after_magic_surfaces_as_truncated_header() {
    let buf = MAGIC.to_vec();
    let mut cur = Cursor::new(&buf[..]);
    let err = read_header(&mut cur).unwrap_err();
    assert!(matches!(err, PersistError::TruncatedHeader { .. }));
}

#[test]
fn truncated_in_index_type_surfaces_as_truncated_header() {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&CURRENT_VERSION.to_le_bytes());
    buf.extend_from_slice(&100u64.to_le_bytes());
    buf.extend_from_slice(b"hello"); // only 5 of 100 bytes
    let mut cur = Cursor::new(&buf[..]);
    let err = read_header(&mut cur).unwrap_err();
    assert!(matches!(err, PersistError::TruncatedHeader { .. }));
}

#[test]
fn invalid_metric_tag_surfaces_as_invalid_metric() {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&MAGIC);
    buf.extend_from_slice(&CURRENT_VERSION.to_le_bytes());
    let tag = b"flat";
    buf.extend_from_slice(&(tag.len() as u64).to_le_bytes());
    buf.extend_from_slice(tag);
    buf.extend_from_slice(&8u64.to_le_bytes()); // dim
    buf.push(99); // <- invalid metric tag
    buf.extend_from_slice(&3u64.to_le_bytes()); // n_vectors
    buf.extend_from_slice(&0u32.to_le_bytes()); // crc32

    let mut cur = Cursor::new(&buf[..]);
    let err = read_header(&mut cur).unwrap_err();
    assert!(matches!(err, PersistError::InvalidMetric { tag: 99 }));
}
