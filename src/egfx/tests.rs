use super::factory::capability_avc_support;
use super::*;
use ironrdp_core::{encode_vec, Decode, Encode, ReadCursor};
use ironrdp_dvc::DvcProcessor as _;
use ironrdp_egfx::pdu::{
    Avc420BitmapStream, Avc420Region, Avc444BitmapStream, Codec1Type, Encoding, GfxPdu,
    PixelFormat, QuantQuality, WireToSurface1Pdu,
};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_server::GfxServerHandle;
use std::sync::Arc;
use tokio::sync::mpsc;

const TEST_CHANNEL_ID: u32 = 1007;

fn ready_avc444_handle(width: u16, height: u16) -> (GfxServerHandle, u16) {
    let shared = Arc::new(EgfxShared::new(DEFAULT_MAX_FRAMES_IN_FLIGHT));
    shared.set_surface_size(width, height);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    bridge.start(TEST_CHANNEL_ID).expect("channel starts");

    let caps = GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
        ironrdp_egfx::pdu::CapabilitySet::V10_7 {
            flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
        },
    ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
    assert!(shared.is_ready());
    assert!(shared.is_avc444_enabled());

    let surface_id =
        EgfxShared::init_surface(&handle, &event_tx, width, height).expect("surface init");
    (handle, surface_id)
}

fn decode_gfx_output(message: &ironrdp_dvc::DvcMessage) -> GfxPdu {
    let wrapped = encode_vec(&**message).expect("DVC message encodes");
    assert_eq!(&wrapped[0..2], &[0xe0, 0x04]);
    let mut cursor = ReadCursor::new(&wrapped[2..]);
    GfxPdu::decode(&mut cursor).expect("GFX PDU decodes")
}

fn decode_avc444_wire_to_surface(message: &ironrdp_dvc::DvcMessage) -> WireToSurface1Pdu {
    match decode_gfx_output(message) {
        GfxPdu::WireToSurface1(pdu) => pdu,
        other => panic!("expected WireToSurface1, got {other:?}"),
    }
}

#[test]
fn full_frame_region_uses_rdpegfx_exclusive_bounds() {
    let region = rdpegfx_full_frame_region(1280, 720, 23);
    assert_eq!(region.left, 0);
    assert_eq!(region.top, 0);
    assert_eq!(region.right, 1280);
    assert_eq!(region.bottom, 720);
    assert_eq!(region.quantization_parameter, 23);
    assert_eq!(region.quality, 77);
}

#[test]
fn avc444_stream_info_encodes_lc_and_stream1_size() {
    let rectangle = InclusiveRectangle {
        left: 0,
        top: 0,
        right: 15,
        bottom: 15,
    };
    let quant = QuantQuality {
        quantization_parameter: 20,
        progressive: false,
        quality: 80,
    };
    let stream1 = Avc420BitmapStream {
        rectangles: vec![rectangle.clone()],
        quant_qual_vals: vec![quant.clone()],
        data: &[1, 2, 3, 4],
    };
    let stream2 = Avc420BitmapStream {
        rectangles: vec![rectangle],
        quant_qual_vals: vec![quant],
        data: &[5, 6, 7],
    };
    let stream1_size = stream1.size();
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA_AND_CHROMA,
        stream1,
        stream2: Some(stream2),
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let stream_info = u32::from_le_bytes(encoded[0..4].try_into().unwrap());

    assert_eq!(stream_info & 0x3fff_ffff, stream1_size as u32);
    assert_eq!(
        stream_info >> 30,
        u32::from(Encoding::LUMA_AND_CHROMA.bits())
    );
}

#[test]
fn avc444_luma_only_decodes_stream1_without_stream2() {
    let rectangle = InclusiveRectangle {
        left: 2,
        top: 4,
        right: 18,
        bottom: 20,
    };
    let quant = QuantQuality {
        quantization_parameter: 18,
        progressive: false,
        quality: 82,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[1, 3, 5, 7],
        },
        stream2: None,
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

    assert_eq!(decoded.encoding, Encoding::LUMA);
    assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
    assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
    assert_eq!(decoded.stream1.data, &[1, 3, 5, 7]);
    assert!(decoded.stream2.is_none());
}

#[test]
fn avc444_chroma_only_decodes_stream1_without_stream2() {
    let rectangle = InclusiveRectangle {
        left: 4,
        top: 2,
        right: 11,
        bottom: 7,
    };
    let quant = QuantQuality {
        quantization_parameter: 23,
        progressive: false,
        quality: 77,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::CHROMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[9, 8, 7, 6],
        },
        stream2: None,
    };

    let encoded = encode_vec(&avc444).expect("AVC444 stream encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = Avc444BitmapStream::decode(&mut cursor).expect("AVC444 stream decodes");

    assert_eq!(decoded.encoding, Encoding::CHROMA);
    assert_eq!(decoded.stream1.rectangles, vec![rectangle]);
    assert_eq!(decoded.stream1.quant_qual_vals, vec![quant]);
    assert_eq!(decoded.stream1.data, &[9, 8, 7, 6]);
    assert!(decoded.stream2.is_none());
}

#[test]
fn wire_to_surface1_roundtrips_avc444v2_bitmap_payload() {
    let rectangle = InclusiveRectangle {
        left: 0,
        top: 0,
        right: 32,
        bottom: 24,
    };
    let quant = QuantQuality {
        quantization_parameter: 21,
        progressive: false,
        quality: 79,
    };
    let avc444 = Avc444BitmapStream {
        encoding: Encoding::LUMA_AND_CHROMA,
        stream1: Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[0xaa, 0xbb, 0xcc],
        },
        stream2: Some(Avc420BitmapStream {
            rectangles: vec![rectangle.clone()],
            quant_qual_vals: vec![quant.clone()],
            data: &[0x11, 0x22],
        }),
    };
    let pdu = WireToSurface1Pdu {
        surface_id: 7,
        codec_id: Codec1Type::Avc444v2,
        pixel_format: PixelFormat::ARgb,
        destination_rectangle: rectangle.clone(),
        bitmap_data: encode_vec(&avc444).expect("AVC444 stream encodes"),
    };

    let encoded = encode_vec(&pdu).expect("WireToSurface1 encodes");
    let mut cursor = ReadCursor::new(&encoded);
    let decoded = WireToSurface1Pdu::decode(&mut cursor).expect("WireToSurface1 decodes");
    let mut bitmap_cursor = ReadCursor::new(&decoded.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");

    assert_eq!(decoded.surface_id, 7);
    assert_eq!(decoded.codec_id, Codec1Type::Avc444v2);
    assert_eq!(decoded.pixel_format, PixelFormat::ARgb);
    assert_eq!(decoded.destination_rectangle, rectangle.clone());
    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert_eq!(bitmap.stream1.rectangles, vec![rectangle.clone()]);
    assert_eq!(bitmap.stream1.quant_qual_vals, vec![quant.clone()]);
    assert_eq!(bitmap.stream1.data, &[0xaa, 0xbb, 0xcc]);
    let stream2 = bitmap.stream2.expect("LC=0 carries stream2");
    assert_eq!(stream2.rectangles, vec![rectangle]);
    assert_eq!(stream2.quant_qual_vals, vec![quant]);
    assert_eq!(stream2.data, &[0x11, 0x22]);
}

#[test]
fn avc444_send_wrapper_maps_luma_and_chroma_to_wire_payload() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    let (_frame_id, dvc_messages, _channel_id) = EgfxShared::queue_avc444_frame_with_regions(
        &handle,
        surface_id,
        encoder::Avc444FrameEncoding::LumaAndChroma,
        &[1, 2, 3],
        &stream1_regions,
        Some(&[4, 5]),
        Some(&stream2_regions),
        123,
    )
    .expect("LC=0 AVC444v2 frame queues");

    assert_eq!(dvc_messages.len(), 3);
    assert!(matches!(
        decode_gfx_output(&dvc_messages[0]),
        GfxPdu::StartFrame(_)
    ));
    assert!(matches!(
        decode_gfx_output(&dvc_messages[2]),
        GfxPdu::EndFrame(_)
    ));
    let wire = decode_avc444_wire_to_surface(&dvc_messages[1]);
    let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
    let bitmap = Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
    let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

    assert_eq!(wire.surface_id, surface_id);
    assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
    assert_eq!(
        stream_info >> 30,
        u32::from(Encoding::LUMA_AND_CHROMA.bits())
    );
    assert_eq!(bitmap.encoding, Encoding::LUMA_AND_CHROMA);
    assert_eq!(bitmap.stream1.data, &[1, 2, 3]);
    assert_eq!(bitmap.stream1.rectangles[0].left, 0);
    assert_eq!(bitmap.stream1.rectangles[0].right, 32);
    let stream2 = bitmap.stream2.expect("LC=0 has stream2");
    assert_eq!(stream2.data, &[4, 5]);
    assert_eq!(stream2.rectangles[0].left, 16);
    assert_eq!(stream2.rectangles[0].right, 64);
    assert_eq!(wire.destination_rectangle.left, 0);
    assert_eq!(wire.destination_rectangle.top, 0);
    assert_eq!(wire.destination_rectangle.right, 64);
    assert_eq!(wire.destination_rectangle.bottom, 48);
}

#[test]
fn avc444_send_wrapper_maps_luma_only_and_chroma_only_to_stream1() {
    for (local_encoding, wire_encoding, payload) in [
        (
            encoder::Avc444FrameEncoding::Luma,
            Encoding::LUMA,
            &[0x10, 0x11][..],
        ),
        (
            encoder::Avc444FrameEncoding::Chroma,
            Encoding::CHROMA,
            &[0x20, 0x21, 0x22][..],
        ),
    ] {
        let (handle, surface_id) = ready_avc444_handle(64, 64);
        let regions = [Avc420Region::new(4, 6, 20, 22, 18, 82)];
        let (_frame_id, dvc_messages, _channel_id) = EgfxShared::queue_avc444_frame_with_regions(
            &handle,
            surface_id,
            local_encoding,
            payload,
            &regions,
            None,
            None,
            123,
        )
        .expect("single-stream AVC444v2 frame queues");

        assert_eq!(dvc_messages.len(), 3);
        let wire = decode_avc444_wire_to_surface(&dvc_messages[1]);
        let mut bitmap_cursor = ReadCursor::new(&wire.bitmap_data);
        let bitmap =
            Avc444BitmapStream::decode(&mut bitmap_cursor).expect("AVC444 payload decodes");
        let stream_info = u32::from_le_bytes(wire.bitmap_data[0..4].try_into().unwrap());

        assert_eq!(wire.surface_id, surface_id);
        assert_eq!(wire.codec_id, Codec1Type::Avc444v2);
        assert_eq!(stream_info >> 30, u32::from(wire_encoding.bits()));
        assert_eq!(bitmap.encoding, wire_encoding);
        assert_eq!(bitmap.stream1.data, payload);
        assert_eq!(bitmap.stream1.rectangles[0].left, 4);
        assert_eq!(bitmap.stream1.rectangles[0].right, 20);
        assert!(bitmap.stream2.is_none());
    }
}

#[test]
fn avc444_send_wrapper_rejects_stream2_for_single_stream_lc() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];
    let stream2_regions = [Avc420Region::new(16, 8, 64, 48, 22, 78)];

    for encoding in [
        encoder::Avc444FrameEncoding::Luma,
        encoder::Avc444FrameEncoding::Chroma,
    ] {
        assert!(EgfxShared::queue_avc444_frame_with_regions(
            &handle,
            surface_id,
            encoding,
            &[1, 2, 3],
            &stream1_regions,
            Some(&[4, 5]),
            Some(&stream2_regions),
            123,
        )
        .is_none());
    }
}

#[test]
fn avc444_send_wrapper_rejects_lc0_without_stream2() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let stream1_regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

    assert!(EgfxShared::queue_avc444_frame_with_regions(
        &handle,
        surface_id,
        encoder::Avc444FrameEncoding::LumaAndChroma,
        &[1, 2, 3],
        &stream1_regions,
        None,
        None,
        123,
    )
    .is_none());
}

#[test]
fn avc444_send_with_closed_event_channel_does_not_queue_frame() {
    let (handle, surface_id) = ready_avc444_handle(64, 64);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    drop(event_rx);
    let regions = [Avc420Region::new(0, 0, 32, 32, 21, 79)];

    assert!(!EgfxShared::send_avc444_frame_with_regions(
        &handle,
        &event_tx,
        surface_id,
        encoder::Avc444FrameEncoding::Luma,
        &[1, 2, 3],
        &regions,
        None,
        None,
        123,
    ));
    let server = handle.lock().expect("server lock");
    assert_eq!(server.frames_in_flight(), 0);
}

#[test]
fn resize_does_not_bump_generation_when_reset_cannot_be_sent() {
    let shared = Arc::new(EgfxShared::new(DEFAULT_MAX_FRAMES_IN_FLIGHT));
    shared.set_surface_size(64, 64);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let mut factory = HyprGfxFactory::new(Arc::clone(&shared));
    ironrdp_server::ServerEventSender::set_sender(&mut factory, event_tx.clone());
    let (mut bridge, handle) = ironrdp_server::GfxServerFactory::build_server_with_handle(&factory)
        .expect("EGFX server builds");
    bridge.start(TEST_CHANNEL_ID).expect("channel starts");

    let caps = GfxPdu::CapabilitiesAdvertise(ironrdp_egfx::pdu::CapabilitiesAdvertisePdu(vec![
        ironrdp_egfx::pdu::CapabilitySet::V10_7 {
            flags: ironrdp_egfx::pdu::CapabilitiesV107Flags::empty(),
        },
    ]));
    let caps = encode_vec(&caps).expect("capabilities encode");
    let _ = bridge
        .process(TEST_CHANNEL_ID, &caps)
        .expect("capabilities process");
    let _ = EgfxShared::init_surface(&handle, &event_tx, 64, 64).expect("surface init");
    let generation = shared.generation();

    drop(event_rx);
    shared.prepare_for_resize(64, 64);

    assert_eq!(shared.generation(), generation);
}

#[test]
fn capability_support_respects_avc_disabled_flags_and_avc444_env_switch() {
    use ironrdp_egfx::pdu::*;

    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V8_1 {
                flags: CapabilitiesV81Flags::AVC420_ENABLED,
            },
            false,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            false,
        ),
        (true, true)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::empty(),
            },
            true,
        ),
        (true, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10_7 {
                flags: CapabilitiesV107Flags::AVC_DISABLED,
            },
            false,
        ),
        (false, false)
    );
    assert_eq!(
        capability_avc_support(
            &CapabilitySet::V10 {
                flags: CapabilitiesV10Flags::AVC_DISABLED,
            },
            false,
        ),
        (false, false)
    );
}

#[test]
fn avc420_region_preserves_rect16_bounds_and_quant_quality() {
    let region = Avc420Region::new(4, 6, 20, 22, 19, 81);
    let rectangle = region.to_rectangle();
    let quant = region.to_quant_quality();

    assert_eq!(rectangle.left, 4);
    assert_eq!(rectangle.top, 6);
    assert_eq!(rectangle.right, 20);
    assert_eq!(rectangle.bottom, 22);
    assert_eq!(quant.quantization_parameter, 19);
    assert!(!quant.progressive);
    assert_eq!(quant.quality, 81);
}
