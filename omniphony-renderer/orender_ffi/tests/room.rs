//! The room-preparation entry point, through C: a real room-response SOFA
//! file is described, prepared and written atomically, or kept as the room
//! already prepared from it; anything else is refused with a return code
//! and the file's description or a reason, never a crash.
#![cfg(feature = "sofa")]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use orender::orender_brir_prepare;
use renderer::binaural::brir::{BrirLoadOptions, BrirSet, ExtractedRoom};
use renderer::binaural::source_id::SourceId;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../renderer/tests/sofa")
        .join(name)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orender-room-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Prepare `bytes` into `out`, `kept` the room kept for them: the return
/// code and the summary line.
fn prepare(bytes: &[u8], kept: Option<&Path>, out: &Path, cap: usize) -> (i32, String) {
    let path = CString::new(out.to_str().unwrap()).unwrap();
    let kept = kept.map(|k| CString::new(k.to_str().unwrap()).unwrap());
    let mut summary = vec![0x7f as c_char; cap];
    let code = unsafe {
        orender_brir_prepare(
            bytes.as_ptr(),
            bytes.len(),
            kept.as_ref().map_or(ptr::null(), |k| k.as_ptr()),
            path.as_ptr(),
            if cap == 0 {
                ptr::null_mut()
            } else {
                summary.as_mut_ptr()
            },
            cap as u32,
        )
    };
    let text = if cap == 0 {
        String::new()
    } else {
        unsafe { CStr::from_ptr(summary.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string()
    };
    (code, text)
}

#[test]
fn a_room_is_prepared_summarised_and_loads_as_its_file_does() {
    let dir = scratch("ok");
    let sofa = fixture("chunked_multispeaker_brir.sofa");
    let bytes = std::fs::read(&sofa).unwrap();
    let out = dir.join("brir.room");

    let (code, line) = prepare(&bytes, None, &out, 1024);
    assert_eq!(code, 0, "{line}");
    // What orender_sofa_describe says of the file, the room's own figures
    // before the reason, which runs to the end.
    let (described, why) = line.split_once(" reason=").unwrap();
    assert!(why.contains("HRTF"), "{line}");
    let (head, stats) = described.split_once(" seconds=").unwrap();
    assert_eq!(head, describe(&bytes).1.split_once(" reason=").unwrap().0);
    assert!(
        head.starts_with("hrtf=no room=yes prepared=no conventions=MultiSpeakerBRIR"),
        "{line}"
    );
    assert!(
        head.contains(&format!(" source={} ", SourceId::of(&bytes))),
        "{line}"
    );
    assert!(head.contains(" speakers=3 "), "{line}");
    let size = std::fs::metadata(&out).unwrap().len();
    assert!(stats.ends_with(&format!(" bytes={size}")), "{line}");
    assert!(
        !dir.join("brir.room.part").exists(),
        "the part file is renamed away"
    );

    let opts = BrirLoadOptions::default();
    let prepared = BrirSet::load(out.to_str().unwrap(), 48_000, &opts).unwrap();
    let file = BrirSet::load(sofa.to_str().unwrap(), 48_000, &opts).unwrap();
    assert_eq!(prepared.emitters(), file.emitters());
    for e in 0..3 {
        assert_eq!(prepared.pair(e, 0), file.pair(e, 0));
    }

    // A summary that does not fit is cut, still terminated.
    let other = dir.join("other.room");
    let (code, cut) = prepare(&bytes, None, &other, 12);
    assert_eq!(code, 0);
    assert_eq!(cut, "hrtf=no roo");
    // No summary buffer at all is fine.
    assert_eq!(prepare(&bytes, None, &other, 0).0, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_room_kept_for_the_same_bytes_is_kept_unwritten() {
    let dir = scratch("kept");
    let bytes = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let kept = dir.join("brir.room");
    assert_eq!(prepare(&bytes, None, &kept, 0).0, 0);
    let image = std::fs::read(&kept).unwrap();

    // The same bytes: 1, the description, and nothing prepared or written.
    let next = dir.join("brir.room.next");
    let (code, line) = prepare(&bytes, Some(&kept), &next, 1024);
    assert_eq!(code, 1, "{line}");
    assert_eq!(line, describe(&bytes).1);
    assert!(!next.exists(), "nothing written");
    assert!(!dir.join("brir.room.next.part").exists());
    assert_eq!(std::fs::read(&kept).unwrap(), image);

    // Another room's bytes, a kept file that is no room, or none: prepared.
    let other = std::fs::read(fixture("rows_multispeaker_brir.sofa")).unwrap();
    assert_eq!(prepare(&other, Some(&kept), &next, 0).0, 0);
    assert!(next.exists());
    std::fs::remove_file(&next).unwrap();
    let garbage = dir.join("garbage.room");
    std::fs::write(&garbage, b"not a room").unwrap();
    assert_eq!(prepare(&bytes, Some(&garbage), &next, 0).0, 0);
    std::fs::remove_file(&next).unwrap();
    let missing = dir.join("missing.room");
    assert_eq!(prepare(&bytes, Some(&missing), &next, 0).0, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_is_not_a_room_is_refused_and_leaves_the_previous_file() {
    let dir = scratch("refused");
    let out = dir.join("brir.room");
    std::fs::write(&out, b"previous").unwrap();

    let (code, why) = prepare(b"not a SOFA file at all", None, &out, 256);
    assert_eq!(code, -1);
    assert!(!why.is_empty());
    // An HRTF set: 2, and the description saying so.
    let pulse = std::fs::read(fixture("Pulse.sofa")).unwrap();
    let (code, line) = prepare(&pulse, None, &out, 1024);
    assert_eq!(code, 2, "{line}");
    assert_eq!(line, describe(&pulse).1);
    assert!(line.starts_with("hrtf=yes room=no "), "{line}");
    assert_eq!(std::fs::read(&out).unwrap(), b"previous");

    // A prepared room is no file to prepare: 2, described as one.
    let bytes = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let room = dir.join("made.room");
    assert_eq!(prepare(&bytes, None, &room, 0).0, 0);
    let (code, line) = prepare(&std::fs::read(&room).unwrap(), None, &out, 1024);
    assert_eq!(code, 2, "{line}");
    assert!(line.contains(" prepared=yes "), "{line}");
    assert_eq!(std::fs::read(&out).unwrap(), b"previous");

    // Unwritable destination: -2, nothing left behind.
    let missing = dir.join("no-such-dir").join("brir.room");
    let (code, why) = prepare(&bytes, None, &missing, 256);
    assert_eq!(code, -2, "{why}");

    // NULL arguments: -3.
    unsafe {
        assert_eq!(
            orender_brir_prepare(ptr::null(), 0, ptr::null(), ptr::null(), ptr::null_mut(), 0),
            -3
        );
        let path = CString::new(out.to_str().unwrap()).unwrap();
        assert_eq!(
            orender_brir_prepare(
                ptr::null(),
                4,
                ptr::null(),
                path.as_ptr(),
                ptr::null_mut(),
                0
            ),
            -3
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Describe `bytes`: the return code and the line.
fn describe(bytes: &[u8]) -> (i32, String) {
    let mut line = vec![0x7f as c_char; 1024];
    let code = unsafe {
        orender::orender_sofa_describe(bytes.as_ptr(), bytes.len(), line.as_mut_ptr(), 1024)
    };
    let text = unsafe { CStr::from_ptr(line.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    (code, text)
}

/// What a file holds, through C, before anything is copied or prepared: a
/// multi-speaker room is a room and not an HRTF set, a free-field set the
/// other way round, a prepared room says so, and what is neither is refused.
#[test]
fn a_file_is_described_for_the_stage_that_takes_it() {
    let room = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let (code, line) = describe(&room);
    assert_eq!(code, 2, "{line}");
    assert!(
        line.starts_with(
            "hrtf=no room=yes prepared=no conventions=MultiSpeakerBRIR measurements=6 \
             receivers=2 emitters=3 samples=1000 rate=48000 source=106867-1c0e5590b1e71c23 \
             orientations=1 speakers=3 names="
        ),
        "{line}"
    );
    assert!(
        line.ends_with(
            " reason=3 loudspeakers in every measurement: the HRTF stage takes one \
                        direction per measurement"
        ),
        "{line}"
    );

    let hrtf = std::fs::read(fixture("Pulse.sofa")).unwrap();
    let (code, line) = describe(&hrtf);
    assert_eq!(code, 1, "{line}");
    assert!(
        line.starts_with("hrtf=yes room=no prepared=no conventions=SimpleFreeFieldHRIR"),
        "{line}"
    );
    assert!(!line.contains(" speakers="), "{line}");
    assert!(line.contains(" reason=1250 measured directions"), "{line}");

    let dir = scratch("describe");
    let out = dir.join("brir.room");
    assert_eq!(prepare(&room, None, &out, 0).0, 0);
    let (code, line) = describe(&std::fs::read(&out).unwrap());
    assert_eq!(code, 2, "{line}");
    assert!(
        line.starts_with("hrtf=no room=yes prepared=yes conventions=MultiSpeakerBRIR"),
        "{line}"
    );
    assert!(line.contains(" orientations=1 speakers=3 names="), "{line}");
    std::fs::remove_dir_all(&dir).unwrap();

    let (code, line) = describe(b"not a sofa file at all");
    assert_eq!(code, -1);
    assert!(line.starts_with("reason="), "{line}");
    let code = unsafe { orender::orender_sofa_describe(ptr::null(), 0, ptr::null_mut(), 0) };
    assert_eq!(code, -3);
}

/// The prepared room carries the identity of the bytes it was made from,
/// which the summary, the room and the description of either all report:
/// what a host matches a file against the room it keeps by.
#[test]
fn a_prepared_room_carries_its_files_identity() {
    let dir = scratch("source");
    let bytes = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let id = SourceId::of(&bytes);
    let out = dir.join("chunked.room");
    let (code, summary) = prepare(&bytes, None, &out, 1024);
    assert_eq!(code, 0, "{summary}");
    assert!(summary.contains(&format!(" source={id} ")), "{summary}");
    let (_, source) = ExtractedRoom::from_prepared(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(source, id);
    for described in [bytes.clone(), std::fs::read(&out).unwrap()] {
        let (_, line) = describe(&described);
        assert!(line.contains(&format!(" source={id} ")), "{line}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
