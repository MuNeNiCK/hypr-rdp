use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use ironrdp_core::{Encode, EncodeResult, WriteCursor};
use ironrdp_svc::{ChannelFlags, SvcEncode, SvcMessage};

/// Application output slots, independent of the client's frame-ACK window.
pub(in crate::egfx) const MAX_QUEUED_OUTPUT_FRAMES: u32 = 3;

#[derive(Default)]
pub(in crate::egfx) struct EgfxOutputQueue {
    pending: AtomicU32,
}

impl EgfxOutputQueue {
    pub(in crate::egfx) fn pending(&self) -> u32 {
        self.pending.load(Ordering::Acquire)
    }

    pub(in crate::egfx) fn try_reserve(self: &Arc<Self>) -> Option<Arc<EgfxOutputLease>> {
        let mut pending = self.pending();
        loop {
            if pending >= MAX_QUEUED_OUTPUT_FRAMES {
                return None;
            }
            match self.pending.compare_exchange_weak(
                pending,
                pending + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => pending = current,
            }
        }
        Some(Arc::new(EgfxOutputLease {
            queue: Arc::clone(self),
        }))
    }
}

pub(in crate::egfx) struct EgfxOutputLease {
    queue: Arc<EgfxOutputQueue>,
}

impl EgfxOutputLease {
    /// Preserve IronRDP's encoded DVC payloads and SHOW_PROTOCOL flags, carrying
    /// the frame's slot until the consumer has serialized or dropped every part.
    pub(in crate::egfx) fn retain_messages(
        self: Arc<Self>,
        messages: Vec<SvcMessage>,
    ) -> EncodeResult<Vec<SvcMessage>> {
        messages
            .into_iter()
            .map(|message| {
                let bytes = message.encode_unframed_pdu()?;
                Ok(SvcMessage::from(LeasedFrameFragment {
                    bytes,
                    _lease: Arc::clone(&self),
                })
                .with_flags(ChannelFlags::SHOW_PROTOCOL))
            })
            .collect()
    }
}

impl Drop for EgfxOutputLease {
    fn drop(&mut self) {
        self.queue.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

struct LeasedFrameFragment {
    bytes: Vec<u8>,
    _lease: Arc<EgfxOutputLease>,
}

impl Encode for LeasedFrameFragment {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        ironrdp_core::ensure_size!(in: dst, size: self.bytes.len());
        dst.write_slice(&self.bytes);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "LeasedEgfxFrameFragment"
    }

    fn size(&self) -> usize {
        self.bytes.len()
    }
}

impl SvcEncode for LeasedFrameFragment {}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages() -> Vec<SvcMessage> {
        [1, 1600, 3207]
            .into_iter()
            .map(|size| SvcMessage::from(vec![0x5a; size]).with_flags(ChannelFlags::SHOW_PROTOCOL))
            .collect()
    }

    #[test]
    fn output_queue_holds_slot_until_last_fragment_drops() {
        let queue = Arc::new(EgfxOutputQueue::default());
        let mut parts = queue
            .try_reserve()
            .unwrap()
            .retain_messages(messages())
            .unwrap();
        let last = parts.pop().unwrap();
        drop(parts);
        assert_eq!(queue.pending(), 1);
        last.encode_unframed_pdu().unwrap();
        assert_eq!(
            queue.pending(),
            1,
            "borrowing bytes does not consume the message"
        );
        drop(last);
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn output_queue_preserves_framed_svc_bytes() {
        let expected = ironrdp_svc::server_encode_svc_messages(messages(), 1003, 1001).unwrap();
        let queue = Arc::new(EgfxOutputQueue::default());
        let retained = queue
            .try_reserve()
            .unwrap()
            .retain_messages(messages())
            .unwrap();
        assert_eq!(queue.pending(), 1);
        let actual = ironrdp_svc::server_encode_svc_messages(retained, 1003, 1001).unwrap();
        assert_eq!(
            actual, expected,
            "SVC flags, fragment headers and payload bytes must agree"
        );
        assert_eq!(queue.pending(), 0);
    }

    struct FailingPayload;

    impl Encode for FailingPayload {
        fn encode(&self, _: &mut WriteCursor<'_>) -> EncodeResult<()> {
            Err(ironrdp_core::other_err!(
                "FailingPayload",
                "injected encoding failure"
            ))
        }
        fn name(&self) -> &'static str {
            "FailingPayload"
        }
        fn size(&self) -> usize {
            1
        }
    }

    impl SvcEncode for FailingPayload {}

    #[test]
    fn output_queue_encoding_failure_releases_slot() {
        let queue = Arc::new(EgfxOutputQueue::default());
        let messages = vec![
            SvcMessage::from(vec![1; 1600]),
            SvcMessage::from(FailingPayload),
        ];
        assert!(queue
            .try_reserve()
            .unwrap()
            .retain_messages(messages)
            .is_err());
        assert_eq!(
            queue.pending(),
            0,
            "partially wrapped fragments must release their lease"
        );
    }

    #[test]
    fn output_queue_concurrent_producers_cannot_exceed_budget() {
        let queue = Arc::new(EgfxOutputQueue::default());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let queue = Arc::clone(&queue);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    let lease = queue.try_reserve();
                    barrier.wait();
                    assert_eq!(queue.pending(), MAX_QUEUED_OUTPUT_FRAMES);
                    barrier.wait();
                    drop(lease);
                });
            }
        });
        assert_eq!(queue.pending(), 0);
    }

    proptest::proptest! {
        #[test]
        fn output_queue_generated_lifecycle_schedules_bound_payloads(
            actions in proptest::collection::vec(0u8..8, 1..160),
            size in 0usize..18000,
        ) {
            let shared = crate::egfx::EgfxShared::with_codec_policy(3, crate::egfx::EgfxCodecPolicy::Auto);
            let mut frames: Vec<Vec<SvcMessage>> = Vec::new();
            for action in actions {
                match action {
                    0..=2 => {
                        let reservation = shared.output_queue.try_reserve();
                        proptest::prop_assert_eq!(reservation.is_some(), frames.len() < 3);
                        if let Some(lease) = reservation {
                            let payloads = (0..3).map(|_| SvcMessage::from(vec![action; size])).collect();
                            frames.push(lease.retain_messages(payloads).unwrap());
                        }
                    }
                    3 | 4 if !frames.is_empty() => {
                        let fragment = frames[0].pop().unwrap();
                        if action == 4 { fragment.encode_unframed_pdu().unwrap(); }
                        drop(fragment);
                        if frames[0].is_empty() { frames.remove(0); }
                    }
                    5 if !frames.is_empty() => { frames.remove(0); }
                    6 => shared.reset_for_new_client(),
                    7 => {
                        if let Some(lease) = shared.output_queue.try_reserve() {
                            proptest::prop_assert!(lease.retain_messages(vec![SvcMessage::from(FailingPayload)]).is_err());
                        }
                    }
                    _ => {}
                }
                proptest::prop_assert_eq!(shared.output_queue.pending() as usize, frames.len());
                proptest::prop_assert!(frames.len() <= 3);
            }
            drop(frames);
            proptest::prop_assert_eq!(shared.output_queue.pending(), 0);
        }
    }
}
