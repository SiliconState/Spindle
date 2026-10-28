//! Bounded framing check for Dext worker JPEG output. This is not a pixel decoder,
//! proof of trusted sanitization, artifact identity check or disclosure grant.

pub const LONGEST_SIDE_CAP: u32 = 1_568;
pub const ENCODED_BYTE_CAP: usize = 1_500 * 1024;

pub fn sanitized_jpeg_matches_dimensions(bytes: &[u8], width: u32, height: u32) -> bool {
    if width == 0
        || height == 0
        || width.max(height) > LONGEST_SIDE_CAP
        || bytes.len() > ENCODED_BYTE_CAP
        || !bytes.starts_with(&[0xff, 0xd8])
    {
        return false;
    }
    let mut offset = 2usize;
    let mut jfif = false;
    let mut frame = false;
    let mut quantization = false;
    let mut huffman = false;
    while bytes.get(offset) == Some(&0xff) {
        let Some(&marker) = bytes.get(offset + 1) else {
            return false;
        };
        let Some(length_bytes) = bytes.get(offset + 2..offset + 4) else {
            return false;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return false;
        }
        let Some(end) = offset
            .checked_add(2 + length)
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        let data = &bytes[offset + 4..end];
        match marker {
            0xe0 if !jfif
                && data.len() == 14
                && data.starts_with(b"JFIF\0")
                && data[12..] == [0, 0] =>
            {
                jfif = true
            }
            0xc0 if !frame && data.len() == 15 && data[0] == 8 && data[5] == 3 => {
                if u32::from(u16::from_be_bytes([data[1], data[2]])) != height
                    || u32::from(u16::from_be_bytes([data[3], data[4]])) != width
                {
                    return false;
                }
                frame = true;
            }
            0xdb if !data.is_empty() => quantization = true,
            0xc4 if !data.is_empty() => huffman = true,
            0xda if jfif
                && frame
                && quantization
                && huffman
                && data.len() == 10
                && data[0] == 3
                && data[7..] == [0, 63, 0] =>
            {
                // Validate framing without invoking any image decoder in the parent.
                let mut scan = end;
                while scan < bytes.len() {
                    if bytes[scan] != 0xff {
                        scan += 1;
                        continue;
                    }
                    match bytes.get(scan + 1) {
                        Some(0x00) => scan += 2,
                        Some(0xd9) => return scan > end && scan + 2 == bytes.len(),
                        _ => return false,
                    }
                }
                return false;
            }
            _ => return false,
        }
        offset = end;
    }
    false
}
