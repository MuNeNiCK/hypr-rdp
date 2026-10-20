pub(super) const MAX_CLIPBOARD_SIZE: usize = 100 * 1024 * 1024;

pub(super) const TEXT_MIME: &str = "text/plain;charset=utf-8";
pub(super) const UTF8_MIME: &str = "UTF8_STRING";
pub(super) const TEXT_PLAIN_MIME: &str = "text/plain";
/// ICCCM `STRING` (ISO-8859-1), also seen in offers from legacy X11 applications.
pub(super) const LATIN1_STRING_MIME: &str = "STRING";
pub(super) const IMAGE_PNG_MIME: &str = "image/png";
pub(super) const FILE_URI_LIST_MIME: &str = "text/uri-list";
pub(super) const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";

/// The kind of content carried by a clipboard selection.
///
/// A selection may advertise more than one representation, but every
/// representation belongs to one of these kinds. Keep this list exhaustive so
/// adding a new selection kind cannot accidentally fall through text/image
/// handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionKind {
    Text,
    Image,
    Files,
}

impl SelectionKind {
    pub(super) const ALL: [Self; 3] = [Self::Text, Self::Image, Self::Files];
    /// Order used when choosing one of several remote clipboard formats.
    pub(super) const REMOTE_PREFERENCE: [Self; 3] = [Self::Text, Self::Image, Self::Files];

    pub(super) fn accepts_wayland_mime(self, mime: &str) -> bool {
        match self {
            Self::Text => mime == TEXT_MIME || mime == UTF8_MIME || mime == TEXT_PLAIN_MIME,
            Self::Image => mime == IMAGE_PNG_MIME,
            Self::Files => mime == FILE_URI_LIST_MIME || mime == GNOME_COPIED_FILES_MIME,
        }
    }

    pub(super) fn offered_wayland_mime(self, mimes: &[String]) -> Option<String> {
        mimes
            .iter()
            .find(|mime| self.accepts_wayland_mime(mime))
            .or_else(|| {
                // Text only offered as Latin-1 STRING (e.g. Motif apps under Xwayland):
                // read it as a fallback when no UTF-8 text target is offered.
                (self == Self::Text)
                    .then(|| mimes.iter().find(|mime| *mime == LATIN1_STRING_MIME))
                    .flatten()
            })
            .cloned()
    }
}

/// Normalize CR, LF, and CRLF line endings to Wayland's LF form.
pub(super) fn normalize_lf(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let (mut start, mut i) = (0, 0);

    while i < bytes.len() {
        if bytes[i] != b'\r' {
            i += 1;
            continue;
        }

        out.push_str(&text[start..i]);
        out.push('\n');
        i += usize::from(bytes.get(i + 1) == Some(&b'\n')) + 1;
        start = i;
    }

    out.push_str(&text[start..]);
    out
}

/// Normalize CR, LF, and CRLF line endings to the CRLF form required by
/// `CF_UNICODETEXT`.
///
/// [Standard Clipboard Formats]: https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats
pub(super) fn to_crlf(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 32 + 2);
    let (mut start, mut i) = (0, 0);

    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                out.push_str(&text[start..i]);
                out.push_str("\r\n");
                i += usize::from(bytes.get(i + 1) == Some(&b'\n')) + 1;
                start = i;
            }
            b'\n' => {
                out.push_str(&text[start..i]);
                out.push_str("\r\n");
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[start..]);
    out
}

/// Data pending write to Wayland clipboard (from RDP client).
pub(super) enum PendingWrite {
    Text(Vec<u8>),
    Image(Vec<u8>), // PNG bytes
    Files {
        uri_list: Vec<u8>,
        gnome_copied_files: Vec<u8>,
    },
}

impl PendingWrite {
    pub(super) fn kind(&self) -> SelectionKind {
        match self {
            Self::Text(_) => SelectionKind::Text,
            Self::Image(_) => SelectionKind::Image,
            Self::Files { .. } => SelectionKind::Files,
        }
    }

    pub(super) fn data_for_mime(&self, mime: &str) -> Option<&[u8]> {
        match self {
            Self::Text(data) if SelectionKind::Text.accepts_wayland_mime(mime) => Some(data),
            Self::Image(data) if SelectionKind::Image.accepts_wayland_mime(mime) => Some(data),
            Self::Files { uri_list, .. } if mime == FILE_URI_LIST_MIME => Some(uri_list),
            Self::Files {
                gnome_copied_files, ..
            } if mime == GNOME_COPIED_FILES_MIME => Some(gnome_copied_files),
            _ => None,
        }
    }
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();

    let end = u16s.iter().position(|&c| c == 0).unwrap_or(u16s.len());
    String::from_utf16_lossy(&u16s[..end])
}

/// Convert a STRING text offer to UTF-8, preserving existing UTF-8 MIME bytes.
/// Check the output bound before allocation; Latin-1 can double in size.
pub(super) fn decode_wayland_text_offer(
    mime: &str,
    bytes: Vec<u8>,
    limit: usize,
) -> Option<Vec<u8>> {
    if bytes.len() > limit {
        return None;
    }
    match mime {
        TEXT_MIME | UTF8_MIME | TEXT_PLAIN_MIME => Some(bytes),
        LATIN1_STRING_MIME => {
            let high_bytes = bytes.iter().filter(|&&byte| byte >= 0x80).count();
            let output_len = bytes.len().checked_add(high_bytes)?;
            if output_len > limit {
                return None;
            }
            let mut text = String::with_capacity(output_len);
            for byte in bytes {
                text.push(char::from(byte));
            }
            Some(text.into_bytes())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn latin1_string_is_a_text_fallback_only() {
        let only_string = vec!["TARGETS".to_string(), "STRING".to_string()];
        assert_eq!(
            SelectionKind::Text
                .offered_wayland_mime(&only_string)
                .as_deref(),
            Some(LATIN1_STRING_MIME)
        );
        let both = vec!["STRING".to_string(), "UTF8_STRING".to_string()];
        assert_eq!(
            SelectionKind::Text.offered_wayland_mime(&both).as_deref(),
            Some(UTF8_MIME)
        );
        assert_eq!(
            SelectionKind::Image.offered_wayland_mime(&only_string),
            None
        );
        assert!(!SelectionKind::Text.accepts_wayland_mime(LATIN1_STRING_MIME));
    }

    #[test]
    fn latin1_converts_to_utf8() {
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, b"Gr\xfc\xdfe \xe4".to_vec(), 32)
                .unwrap(),
            "Grüße ä".as_bytes()
        );
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, b"plain".to_vec(), 5).unwrap(),
            b"plain"
        );
    }

    #[test]
    fn string_offer_is_a_read_only_text_fallback() {
        for utf8 in [TEXT_MIME, UTF8_MIME, TEXT_PLAIN_MIME] {
            for offers in [[LATIN1_STRING_MIME, utf8], [utf8, LATIN1_STRING_MIME]] {
                let offers: Vec<_> = offers.into_iter().map(str::to_owned).collect();
                assert_eq!(
                    SelectionKind::Text.offered_wayland_mime(&offers).as_deref(),
                    Some(utf8)
                );
            }
        }
        let offers = vec!["COMPOUND_TEXT".into(), LATIN1_STRING_MIME.into()];
        assert_eq!(
            SelectionKind::Text.offered_wayland_mime(&offers).as_deref(),
            Some(LATIN1_STRING_MIME)
        );
        assert!(SelectionKind::Image.offered_wayland_mime(&offers).is_none());
        assert!(SelectionKind::Files.offered_wayland_mime(&offers).is_none());
        assert!(PendingWrite::Text("é".as_bytes().to_vec())
            .data_for_mime(LATIN1_STRING_MIME)
            .is_none());
        assert!(SelectionKind::Text
            .offered_wayland_mime(&["TEXT".into(), "COMPOUND_TEXT".into()])
            .is_none());
    }

    #[test]
    fn string_text_decoding_enforces_expanded_limit() {
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, vec![0xe9], 1),
            None
        );
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, vec![0xe9], 2),
            Some(vec![0xc3, 0xa9])
        );
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, vec![0xe9, 0xe9], 3),
            None
        );
        assert_eq!(
            decode_wayland_text_offer(LATIN1_STRING_MIME, vec![], 0),
            Some(vec![])
        );
        assert_eq!(
            decode_wayland_text_offer(UTF8_MIME, "四".as_bytes().to_vec(), 2),
            None
        );
        assert_eq!(
            decode_wayland_text_offer(UTF8_MIME, "四".as_bytes().to_vec(), 3).unwrap(),
            "四".as_bytes()
        );
        assert_eq!(
            decode_wayland_text_offer("COMPOUND_TEXT", vec![b'a'], 10),
            None
        );
        assert_eq!(
            String::from_utf8(
                decode_wayland_text_offer(LATIN1_STRING_MIME, vec![0x80, 0xa3, 0xc3, 0xa9], 8)
                    .unwrap()
            )
            .unwrap(),
            "\u{80}£Ã©"
        );
    }

    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn generated_string_text_decoding_preserves_codepoints_and_bounds(
            bytes in prop::collection::vec(any::<u8>(), 0..1024),
            limit in 0usize..2048,
        ) {
            let expected_len: usize = bytes.iter().map(|&b| if b < 0x80 { 1 } else { 2 }).sum();
            let decoded = decode_wayland_text_offer(LATIN1_STRING_MIME, bytes.clone(), limit);
            prop_assert_eq!(decoded.is_some(), expected_len <= limit);
            if let Some(decoded) = decoded {
                prop_assert_eq!(decoded.len(), expected_len);
                let text = String::from_utf8(decoded).unwrap();
                prop_assert_eq!(text.chars().map(u32::from).collect::<Vec<_>>(), bytes.iter().map(|&b| u32::from(b)).collect::<Vec<_>>());
            }
        }
    }

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
    fn line_endings_are_normalized_at_both_boundaries() {
        let text = "\nA\rB\r\nЖ\n";

        assert_eq!(normalize_lf(text), "\nA\nB\nЖ\n");
        assert_eq!(to_crlf(text), "\r\nA\r\nB\r\nЖ\r\n");
    }

    fn text_with_line_endings() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            proptest::sample::select(vec!["a", "b", "\u{444}", " ", "\r\n", "\n", "\r", ""]),
            0..64,
        )
        .prop_map(|parts| parts.concat())
    }

    proptest! {
        #[test]
        fn generated_line_endings_round_trip(text in text_with_line_endings()) {
            let wire = to_crlf(&text);
            prop_assert_eq!(normalize_lf(&wire), normalize_lf(&text));

            let mut i = 0;
            while i < wire.len() {
                match wire.as_bytes()[i] {
                    b'\r' => {
                        prop_assert_eq!(wire.as_bytes().get(i + 1), Some(&b'\n'));
                        i += 2;
                    }
                    b'\n' => prop_assert!(false, "bare LF in CF_UNICODETEXT input"),
                    _ => i += 1,
                }
            }
        }

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
