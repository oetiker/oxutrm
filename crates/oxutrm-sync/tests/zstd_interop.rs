//! The wire format is zstd, and it stays zstd whoever implements it.
//!
//! `FLAG_ZSTD` is bit zero of a frame's flags and the contract names it. That
//! makes the compressor an **interoperability** concern rather than an
//! implementation detail: an oxutrm built today has to read frames from one
//! built last month, and the two halves of a session are separate binaries on
//! separate machines that were installed at different times.
//!
//! So the compressor is pinned against a second, independent implementation —
//! Facebook's C `zstd`, here as a dev-dependency and nowhere else — in both
//! directions. Testing our encoder against our own decoder would pass just as
//! happily on a private format that no other build can read, which is exactly
//! the failure worth catching.
//!
//! Dev-dependencies are deliberately out of scope for `no_io.rs`'s allowlist,
//! and this is why that exemption is worth having: the witness has to be a
//! different implementation, and a different implementation of zstd is C.

use oxutrm_proto::{Frame, ScreenState};
use oxutrm_sync::{Receiver, Sender};

/// A screen with enough repetition that compressing it actually wins, so the
/// frame under test really does carry `FLAG_ZSTD` rather than a plain payload.
fn a_compressible_screen(fill: char) -> ScreenState {
    let mut s = ScreenState::blank(40, 100).expect("a blank screen");
    for cell in &mut s.cells {
        cell.text = fill.to_string().into();
    }
    s
}

/// A frame our encoder produced, which really is compressed.
fn a_compressed_frame() -> Frame {
    let mut sender = Sender::new(a_compressible_screen(' '));
    sender.update(a_compressible_screen('x'));
    let frame = sender
        .make_frame(0)
        .expect("making a frame")
        .expect("a moved state owes a frame");
    assert_eq!(
        frame.flags & oxutrm_proto::FLAG_ZSTD,
        oxutrm_proto::FLAG_ZSTD,
        "this fixture needs a payload that actually compressed; it did not"
    );
    frame
}

/// Direction one: what we write, the reference implementation reads.
///
/// A host that upgraded talking to a client that did not.
#[test]
fn a_frame_we_compressed_is_readable_by_the_reference_zstd() {
    let frame = a_compressed_frame();

    let plain = zstd::stream::decode_all(frame.payload.as_slice())
        .expect("the C zstd library must be able to read what we produced");
    assert!(
        !plain.is_empty(),
        "decompressed to nothing, so the payload was not what it claimed"
    );
}

/// Direction two: what the reference implementation writes, we read.
///
/// A client that upgraded talking to a host that did not. Stronger than it
/// looks: the payload is decompressed and *recompressed* by the C library, so
/// nothing of our encoder's output survives into the bytes being applied.
#[test]
fn a_frame_the_reference_zstd_compressed_is_applied_by_us() {
    let frame = a_compressed_frame();

    let plain = zstd::stream::decode_all(frame.payload.as_slice()).expect("decode with C zstd");
    let theirs = zstd::stream::encode_all(plain.as_slice(), 1).expect("encode with C zstd");

    let mut receiver = Receiver::new(a_compressible_screen(' '));
    let applied = receiver
        .on_frame(&Frame {
            payload: theirs,
            ..frame
        })
        .expect("a frame compressed by the reference implementation must apply");

    assert!(applied, "the frame was accepted but changed nothing");
    assert_eq!(
        receiver.state().cells[0].text.as_str(),
        "x",
        "the applied screen is not the one that was sent"
    );
}
