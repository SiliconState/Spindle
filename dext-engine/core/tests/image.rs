use dext_core::image::{ENCODED_BYTE_CAP, LONGEST_SIDE_CAP, sanitized_jpeg_matches_dimensions};

fn framing_fixture(scan: &[u8]) -> Vec<u8> {
    fn segment(out: &mut Vec<u8>, marker: u8, data: &[u8]) {
        out.extend([0xff, marker]);
        out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(data);
    }
    let mut bytes = vec![0xff, 0xd8];
    segment(&mut bytes, 0xe0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0");
    segment(&mut bytes, 0xdb, &[1]);
    segment(
        &mut bytes,
        0xc0,
        &[8, 0, 1, 0, 2, 3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0],
    );
    segment(&mut bytes, 0xc4, &[1]);
    segment(&mut bytes, 0xda, &[3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
    bytes.extend_from_slice(scan);
    bytes.extend([0xff, 0xd9]);
    bytes
}

#[test]
fn profile_checks_bounded_framing_without_claiming_pixel_validity() {
    // Deliberately not decodable: table/entropy semantics are the isolated worker's
    // responsibility. Acceptance here must never be promoted to sanitizer proof.
    let bytes = framing_fixture(&[1, 0xff, 0, 2]);
    assert!(sanitized_jpeg_matches_dimensions(&bytes, 2, 1));
    for dimensions in [(0, 1), (2, 0), (1, 2), (3, 1), (LONGEST_SIDE_CAP + 1, 1)] {
        assert!(!sanitized_jpeg_matches_dimensions(
            &bytes,
            dimensions.0,
            dimensions.1
        ));
    }
    for end in 0..bytes.len() {
        assert!(!sanitized_jpeg_matches_dimensions(&bytes[..end], 2, 1));
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(!sanitized_jpeg_matches_dimensions(&trailing, 2, 1));
    let mut metadata = bytes;
    metadata.splice(2..2, [0xff, 0xe1, 0, 2]);
    assert!(!sanitized_jpeg_matches_dimensions(&metadata, 2, 1));
    assert!(!sanitized_jpeg_matches_dimensions(
        &framing_fixture(&[]),
        2,
        1
    ));
    let overhead = framing_fixture(&[]).len();
    let exact = framing_fixture(&vec![1; ENCODED_BYTE_CAP - overhead]);
    assert_eq!(exact.len(), ENCODED_BYTE_CAP);
    assert!(sanitized_jpeg_matches_dimensions(&exact, 2, 1));
    assert!(!sanitized_jpeg_matches_dimensions(
        &framing_fixture(&vec![1; ENCODED_BYTE_CAP + 1 - overhead]),
        2,
        1
    ));
}

#[test]
fn malformed_marker_lengths_and_deterministic_mutations_never_panic() {
    for bad in [
        vec![],
        vec![0xff],
        vec![0xff, 0xd8],
        vec![0xff, 0xd8, 0xff],
        vec![0xff, 0xd8, 0xff, 0xe0, 0, 0],
        vec![0xff, 0xd8, 0xff, 0xe0, 0xff, 0xff],
    ] {
        assert!(!sanitized_jpeg_matches_dimensions(&bad, 2, 1));
    }
    let fixture = framing_fixture(&[1, 0xff, 0, 2]);
    for index in 0..fixture.len() {
        for replacement in 0..=u8::MAX {
            let mut mutated = fixture.clone();
            mutated[index] = replacement;
            let _ = sanitized_jpeg_matches_dimensions(&mutated, 2, 1);
        }
    }
}
