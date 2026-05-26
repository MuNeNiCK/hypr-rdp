use std::io::{self, Read};

pub(super) const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

pub(super) const TEXT_MIME: &str = "text/plain;charset=utf-8";
pub(super) const UTF8_MIME: &str = "UTF8_STRING";
pub(super) const TEXT_PLAIN_MIME: &str = "text/plain";
pub(super) const IMAGE_PNG_MIME: &str = "image/png";

/// Data pending write to Wayland clipboard (from RDP client).
pub(super) enum PendingWrite {
    Text(Vec<u8>),
    Image(Vec<u8>), // PNG bytes
}

pub(super) fn read_bounded_clipboard_data<R: Read>(
    reader: R,
    max_size: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    let mut bounded = reader.take((max_size as u64).saturating_add(1));
    bounded.read_to_end(&mut data)?;
    if data.len() > max_size {
        return Ok(None);
    }
    Ok(Some(data))
}

/// Fix a CF_DIB with BI_BITFIELDS compression (common on Windows for 32-bit BGRA).
///
/// BITMAPINFOHEADER (40 bytes) + 3 DWORD color masks (12 bytes) + pixel data
/// → BITMAPINFOHEADER (40 bytes, compression=BI_RGB) + pixel data
pub(super) fn fix_bitfields_dib(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 52 {
        return None;
    }
    let header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if header_size != 40 {
        return None;
    }
    let bit_count = u16::from_le_bytes([data[14], data[15]]);
    if bit_count != 32 {
        return None;
    }
    let compression = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    if compression != 3 {
        // Not BI_BITFIELDS
        return None;
    }

    // Reconstruct as BI_RGB: copy header with compression=0, skip 12 bytes of masks
    let mut fixed = Vec::with_capacity(data.len() - 12);
    fixed.extend_from_slice(&data[..16]); // header up to compression field
    fixed.extend_from_slice(&0u32.to_le_bytes()); // compression = BI_RGB (0)
    fixed.extend_from_slice(&data[20..40]); // rest of header
    fixed.extend_from_slice(&data[52..]); // pixel data (skip 12 bytes of color masks)
    Some(fixed)
}

pub(super) fn utf16le_to_utf8(data: &[u8]) -> String {
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();

    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn utf16le(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn utf16le_to_utf8_stops_at_nul_and_ignores_trailing_odd_byte() {
        let mut data = utf16le(&['h' as u16, 'i' as u16, 0, 'x' as u16]);
        data.push(0xff);

        assert_eq!(utf16le_to_utf8(&data), "hi");
    }

    #[test]
    fn utf16le_to_utf8_handles_missing_nul_and_surrogates() {
        let data = utf16le(&['A' as u16, 0xd83d, 0xde00, 'Z' as u16]);

        assert_eq!(utf16le_to_utf8(&data), "A😀Z");
    }

    #[test]
    fn utf16le_to_utf8_replaces_invalid_surrogates() {
        let data = utf16le(&['A' as u16, 0xd83d, 'B' as u16]);

        assert_eq!(utf16le_to_utf8(&data), "A�B");
    }

    #[test]
    fn fix_bitfields_dib_rewrites_32bpp_bitfields_to_bi_rgb() {
        let mut dib = vec![0; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[14..16].copy_from_slice(&32u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        dib.extend_from_slice(&0x00ff_0000u32.to_le_bytes());
        dib.extend_from_slice(&0x0000_ff00u32.to_le_bytes());
        dib.extend_from_slice(&0x0000_00ffu32.to_le_bytes());
        dib.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);

        let fixed = fix_bitfields_dib(&dib).expect("BITFIELDS DIB is fixable");

        assert_eq!(fixed.len(), dib.len() - 12);
        assert_eq!(&fixed[0..4], &40u32.to_le_bytes());
        assert_eq!(&fixed[14..16], &32u16.to_le_bytes());
        assert_eq!(&fixed[16..20], &0u32.to_le_bytes());
        assert_eq!(&fixed[40..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn fix_bitfields_dib_rejects_non_matching_headers() {
        assert_eq!(fix_bitfields_dib(&[0; 51]), None);

        let mut wrong_header_size = vec![0; 52];
        wrong_header_size[0..4].copy_from_slice(&108u32.to_le_bytes());
        wrong_header_size[14..16].copy_from_slice(&32u16.to_le_bytes());
        wrong_header_size[16..20].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(fix_bitfields_dib(&wrong_header_size), None);

        let mut wrong_bpp = vec![0; 52];
        wrong_bpp[0..4].copy_from_slice(&40u32.to_le_bytes());
        wrong_bpp[14..16].copy_from_slice(&24u16.to_le_bytes());
        wrong_bpp[16..20].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(fix_bitfields_dib(&wrong_bpp), None);

        let mut not_bitfields = vec![0; 52];
        not_bitfields[0..4].copy_from_slice(&40u32.to_le_bytes());
        not_bitfields[14..16].copy_from_slice(&32u16.to_le_bytes());
        not_bitfields[16..20].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(fix_bitfields_dib(&not_bitfields), None);
    }

    #[test]
    fn bounded_clipboard_read_accepts_payload_at_limit() {
        let data = read_bounded_clipboard_data(std::io::Cursor::new(b"abcd"), 4)
            .expect("bounded read succeeds");

        assert_eq!(data, Some(b"abcd".to_vec()));
    }

    #[test]
    fn bounded_clipboard_read_rejects_payload_over_limit() {
        let data = read_bounded_clipboard_data(std::io::Cursor::new(b"abcde"), 4)
            .expect("bounded read succeeds");

        assert_eq!(data, None);
    }

    proptest! {
        #[test]
        fn generated_utf16le_conversion_stops_at_first_nul(
            before in proptest::collection::vec(1u16..=0xd7ff, 0..32),
            after in proptest::collection::vec(any::<u16>(), 0..32),
            trailing_odd_byte in proptest::option::of(any::<u8>()),
        ) {
            let mut data = utf16le(&before);
            data.extend_from_slice(&0u16.to_le_bytes());
            data.extend_from_slice(&utf16le(&after));
            if let Some(byte) = trailing_odd_byte {
                data.push(byte);
            }

            let expected = String::from_utf16_lossy(&before);

            prop_assert_eq!(utf16le_to_utf8(&data), expected);
        }

        #[test]
        fn generated_bitfields_dib_rewrite_preserves_header_and_payload(
            width in any::<u32>(),
            height in any::<u32>(),
            planes in any::<u16>(),
            image_size in any::<u32>(),
            masks in (any::<u32>(), any::<u32>(), any::<u32>()),
            payload in proptest::collection::vec(any::<u8>(), 0..256),
        ) {
            let mut dib = vec![0; 40];
            dib[0..4].copy_from_slice(&40u32.to_le_bytes());
            dib[4..8].copy_from_slice(&width.to_le_bytes());
            dib[8..12].copy_from_slice(&height.to_le_bytes());
            dib[12..14].copy_from_slice(&planes.to_le_bytes());
            dib[14..16].copy_from_slice(&32u16.to_le_bytes());
            dib[16..20].copy_from_slice(&3u32.to_le_bytes());
            dib[20..24].copy_from_slice(&image_size.to_le_bytes());
            dib.extend_from_slice(&masks.0.to_le_bytes());
            dib.extend_from_slice(&masks.1.to_le_bytes());
            dib.extend_from_slice(&masks.2.to_le_bytes());
            dib.extend_from_slice(&payload);

            let fixed = fix_bitfields_dib(&dib).expect("generated BITFIELDS DIB is fixable");

            prop_assert_eq!(fixed.len(), dib.len() - 12);
            prop_assert_eq!(&fixed[0..16], &dib[0..16]);
            prop_assert_eq!(&fixed[16..20], &0u32.to_le_bytes());
            prop_assert_eq!(&fixed[20..40], &dib[20..40]);
            prop_assert_eq!(&fixed[40..], payload.as_slice());
        }

        #[test]
        fn generated_non_bitfields_dib_headers_are_not_rewritten(
            header_size in any::<u32>(),
            bit_count in any::<u16>(),
            compression in any::<u32>(),
            payload in proptest::collection::vec(any::<u8>(), 12..256),
        ) {
            prop_assume!(header_size != 40 || bit_count != 32 || compression != 3);

            let mut dib = vec![0; 40];
            dib[0..4].copy_from_slice(&header_size.to_le_bytes());
            dib[14..16].copy_from_slice(&bit_count.to_le_bytes());
            dib[16..20].copy_from_slice(&compression.to_le_bytes());
            dib.extend_from_slice(&payload);

            prop_assert!(fix_bitfields_dib(&dib).is_none());
        }
    }
}
