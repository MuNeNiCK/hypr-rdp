use super::test_support::{
    ack_frame, drain_gfx_pdus, negotiated_avc420_session, negotiated_no_avc_session,
    tracked_avc444_session, ClearCodecFrameOracle, GfxPduTrace, TestGfxSession,
};
use super::{Avc444FrameEncoding, DEFAULT_MAX_FRAMES_IN_FLIGHT};
use ironrdp_egfx::pdu::{Avc420Region, GfxPdu, QueueDepth};
use ironrdp_server::PixelFormat;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
enum FrameEncoding {
    ClearCodec,
    Avc420,
    Avc444,
}

fn session(codec: FrameEncoding) -> (TestGfxSession, u16) {
    let session = match codec {
        FrameEncoding::ClearCodec => negotiated_no_avc_session(16, 16),
        FrameEncoding::Avc420 => {
            let (shared, bridge, handle, event_tx, event_rx) =
                negotiated_avc420_session(16, 16, DEFAULT_MAX_FRAMES_IN_FLIGHT);
            TestGfxSession {
                shared,
                bridge,
                handle,
                event_tx,
                event_rx,
            }
        }
        FrameEncoding::Avc444 => tracked_avc444_session(16, 16, DEFAULT_MAX_FRAMES_IN_FLIGHT),
    };
    let surface = session
        .shared
        .init_or_reuse_surface(&session.handle, &session.event_tx, 16, 16)
        .unwrap();
    (session, surface)
}

fn send(session: &TestGfxSession, surface: u16, codec: FrameEncoding, value: u8) -> bool {
    let regions = [Avc420Region::new(0, 0, 16, 16, 21, 79)];
    match codec {
        FrameEncoding::ClearCodec => session
            .shared
            .send_clearcodec_damage(
                &session.handle,
                &session.event_tx,
                surface,
                &vec![value; 16 * 16 * 4],
                16,
                16,
                64,
                &[(0, 0, 16, 16)],
                value as u32,
                PixelFormat::BgrA32,
            )
            .unwrap()
            .is_some(),
        FrameEncoding::Avc420 => session.shared.send_tracked_avc420_rdpegfx_frame(
            &session.handle,
            &session.event_tx,
            surface,
            &[0, 0, 1, 0x65, value],
            &regions,
            value as u32,
        ),
        FrameEncoding::Avc444 => session.shared.send_tracked_avc444_rdpegfx_frame(
            &session.handle,
            &session.event_tx,
            surface,
            Avc444FrameEncoding::Luma,
            &[0, 0, 1, 0x65, value],
            &regions,
            None,
            None,
            value as u32,
        ),
    }
}

fn consume_one(session: &mut TestGfxSession) -> GfxPduTrace {
    let event = session.event_rx.try_recv().expect("queued frame event");
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(event).unwrap();
    drop(tx);
    drain_gfx_pdus(&mut rx)
}

fn frame_id(trace: &GfxPduTrace) -> u32 {
    let starts: Vec<_> = trace
        .as_slice()
        .iter()
        .filter_map(|pdu| match pdu {
            GfxPdu::StartFrame(start) => Some(start.frame_id),
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert!(
        matches!(trace.as_slice().last(), Some(GfxPdu::EndFrame(end)) if end.frame_id == starts[0])
    );
    starts[0]
}

#[test]
fn output_queue_suspended_ack_bounds_all_codecs_and_resumes_without_ack() {
    for codec in [
        FrameEncoding::ClearCodec,
        FrameEncoding::Avc420,
        FrameEncoding::Avc444,
    ] {
        let (mut session, surface) = session(codec);
        drain_gfx_pdus(&mut session.event_rx);
        let mut pixels = ClearCodecFrameOracle::new();
        assert!(send(&session, surface, codec, 17));
        let first = consume_one(&mut session);
        let first_id = frame_id(&first);
        if matches!(codec, FrameEncoding::ClearCodec) {
            pixels.assert_frame(
                &first,
                surface,
                &vec![17; 1024],
                16,
                16,
                64,
                &[(0, 0, 16, 16)],
            );
        }
        ack_frame(&mut session.bridge, first_id, QueueDepth::Suspend);
        for value in 18..21 {
            assert!(send(&session, surface, codec, value));
        }
        assert_eq!(session.event_rx.len(), 3, "{codec:?}");
        assert!(
            !send(&session, surface, codec, 99),
            "{codec:?}: producer must stop when the event consumer holds three frames"
        );
        assert_eq!(session.event_rx.len(), 3, "{codec:?}");
        for value in 18..21 {
            let trace = consume_one(&mut session);
            assert_eq!(frame_id(&trace), first_id + u32::from(value - 17));
            if matches!(codec, FrameEncoding::ClearCodec) {
                pixels.assert_frame(
                    &trace,
                    surface,
                    &vec![value; 1024],
                    16,
                    16,
                    64,
                    &[(0, 0, 16, 16)],
                );
            }
        }
        // Encoding/drop by the transport consumer releases capacity. No client ACK arrives.
        assert!(send(&session, surface, codec, 21));
        let trace = consume_one(&mut session);
        assert_eq!(frame_id(&trace), first_id + 4);
        if matches!(codec, FrameEncoding::ClearCodec) {
            pixels.assert_frame(
                &trace,
                surface,
                &vec![21; 1024],
                16,
                16,
                64,
                &[(0, 0, 16, 16)],
            );
        }
    }
}

#[test]
fn output_queue_closed_receiver_releases_slots() {
    for codec in [
        FrameEncoding::ClearCodec,
        FrameEncoding::Avc420,
        FrameEncoding::Avc444,
    ] {
        let (mut session, surface) = session(codec);
        drain_gfx_pdus(&mut session.event_rx);
        assert!(send(&session, surface, codec, 17));
        let first = consume_one(&mut session);
        ack_frame(&mut session.bridge, frame_id(&first), QueueDepth::Suspend);
        for value in 18..21 {
            assert!(send(&session, surface, codec, value));
        }
        assert_eq!(session.shared.output_queue.pending(), 3);
        session.event_rx.close();
        assert!(!send(&session, surface, codec, 99));
        assert_eq!(
            session.shared.output_queue.pending(),
            3,
            "close alone retains buffered messages"
        );
        let closed = std::mem::replace(&mut session.event_rx, mpsc::unbounded_channel().1);
        drop(closed);
        assert_eq!(session.shared.output_queue.pending(), 0);
        let before = session.shared.frame_flow_snapshot();
        assert!(!send(&session, surface, codec, 99));
        assert_eq!(session.shared.output_queue.pending(), 0);
        assert_eq!(session.shared.frame_flow_snapshot(), before);
    }
}

#[test]
fn output_queue_generation_change_does_not_reset_live_slots() {
    for codec in [
        FrameEncoding::ClearCodec,
        FrameEncoding::Avc420,
        FrameEncoding::Avc444,
    ] {
        let (mut session, surface) = session(codec);
        drain_gfx_pdus(&mut session.event_rx);
        assert!(send(&session, surface, codec, 17));
        let first = consume_one(&mut session);
        ack_frame(&mut session.bridge, frame_id(&first), QueueDepth::Suspend);
        for value in 18..21 {
            assert!(send(&session, surface, codec, value));
        }
        let held: Vec<_> = (0..3)
            .map(|_| session.event_rx.try_recv().unwrap())
            .collect();
        session.shared.prepare_for_resize(32, 32);
        session.shared.reset_for_new_client();
        assert_eq!(session.shared.output_queue.pending(), 3);
        assert!(matches!(
            session.shared.full_frame_refresh_readiness(),
            super::EgfxFrameReadiness::OutputBackpressure { queued: 3, max: 3 }
        ));
        drop(held);
        assert_eq!(session.shared.output_queue.pending(), 0);
        assert!(session.shared.full_frame_refresh_readiness().is_ready());
    }
}
