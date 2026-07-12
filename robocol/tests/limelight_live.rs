//! Live-hardware check: confirms a real Limelight's MJPEG stream is actually
//! reachable and delivering frames.
//!
//! Skipped by default (no external network dependency to run `cargo test`).
//! Point it at a Limelight by setting `DECK_LIMELIGHT_STREAM` to its stream
//! URL, same as a manual `curl` check. The stream is served at the root path
//! on port 5800 (annotated) or 5802 (raw).
//!
//! ```sh
//! DECK_LIMELIGHT_STREAM="http://<limelight-ip>:5800/" \
//!     cargo test -p robocol --test limelight_live -- --ignored
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use robocol::video::{run_stream, StreamConfig, VideoEvent};

#[test]
#[ignore = "needs a real Limelight on the network; set DECK_LIMELIGHT_STREAM and pass --ignored"]
fn limelight_stream_delivers_frames() {
    let Ok(url) = std::env::var("DECK_LIMELIGHT_STREAM") else {
        panic!("DECK_LIMELIGHT_STREAM not set — point it at a real Limelight's stream URL");
    };
    let cfg = StreamConfig::parse("limelight", &url)
        .unwrap_or_else(|| panic!("DECK_LIMELIGHT_STREAM is not a valid stream URL: {url}"));

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);
    let handle = std::thread::spawn(move || run_stream(cfg, tx, stop_clone));

    let event = rx.recv_timeout(Duration::from_secs(5));
    stop.store(true, Ordering::Relaxed);
    let _ = handle.join();

    match event.expect("no frame arrived from the Limelight stream within 5s") {
        VideoEvent::Frame { jpeg, .. } => assert!(!jpeg.is_empty(), "received an empty frame"),
        VideoEvent::Disconnected { .. } => {
            panic!("Limelight stream disconnected before delivering a frame")
        }
    }
}
