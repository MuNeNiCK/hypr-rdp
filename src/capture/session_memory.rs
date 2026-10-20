use std::thread::JoinHandle;

/// Finish the old producer before reclaiming its freed frame/output pages.
/// Run on the blocking pool, once per capture restart, never on the frame path.
pub(super) fn join_previous_capture(handle: JoinHandle<()>) {
    let _ = handle.join();
    #[cfg(target_env = "gnu")]
    // SAFETY: GNU malloc_trim is MT-safe and only releases free whole pages in
    // allocator arenas. It does not invalidate any live capture/channel buffer.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(all(test, target_env = "gnu"))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn resident_bytes() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kb: usize = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        kb * 1024
    }

    #[test]
    fn capture_restart_reclaims_freed_pages_after_join() {
        const CHILD: &str = "HYPR_RDP_CAPTURE_RECLAIM_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Allocator controls and resident accounting must be isolated from
            // the test runner's other tests and allocation history.
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "capture::session_memory::tests::capture_restart_reclaims_freed_pages_after_join",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        // Reproduce retained free arena pages deterministically in this child,
        // independently of glibc's adaptive mmap/automatic-trim thresholds.
        // SAFETY: these documented allocator controls accept integer values;
        // they affect this disposable test process only.
        unsafe {
            assert_ne!(libc::mallopt(libc::M_TRIM_THRESHOLD, -1), 0);
            assert_ne!(libc::mallopt(libc::M_MMAP_THRESHOLD, 1024 * 1024), 0);
        }
        for panic in [false, true] {
            let guards = Arc::new(Mutex::new(Vec::new()));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let baseline = resident_bytes();
            let handle = std::thread::spawn({
                let guards = Arc::clone(&guards);
                move || {
                    let mut frames = Vec::new();
                    for i in 0..2048 {
                        frames.push(vec![0x5a_u8; 32 * 1024]);
                        // Pin small live objects between freed frame fragments,
                        // preventing a single automatic top-of-heap release.
                        guards.lock().unwrap().push(vec![i as u8; 64]);
                    }
                    ready_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    assert!(!panic, "intentional producer failure");
                    drop(frames);
                }
            });
            ready_rx.recv().unwrap();
            let loaded = resident_bytes();
            assert!(loaded > baseline + 48 * 1024 * 1024);
            release_tx.send(()).unwrap();
            join_previous_capture(handle);
            let reclaimed = resident_bytes();
            assert!(
                reclaimed + 32 * 1024 * 1024 < loaded,
                "freed producer pages stayed resident: baseline={baseline}, loaded={loaded}, after_join={reclaimed}, panic={panic}"
            );
            for (i, guard) in guards.lock().unwrap().iter().enumerate() {
                assert_eq!(guard.as_slice(), &[i as u8; 64]);
            }
        }
    }
}
