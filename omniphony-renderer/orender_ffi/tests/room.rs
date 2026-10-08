//! The room-preparation entry point, through C: a real room-response SOFA
//! file is prepared, written atomically and described; anything else is
//! refused with a return code and a reason, never a crash.
#![cfg(feature = "sofa")]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use orender::orender_brir_prepare;
use renderer::binaural::brir::{BrirLoadOptions, BrirSet, ExtractedRoom};

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

/// The source text the tests prepare with.
const SOURCE: &str = "/storage/sofa/chunked multispeaker.sofa\n4096\n1760000000\n";

/// Prepare `bytes` into `out`: the return code and the summary line.
fn prepare(bytes: &[u8], out: &Path, cap: usize) -> (i32, String) {
    let path = CString::new(out.to_str().unwrap()).unwrap();
    let source = CString::new(SOURCE).unwrap();
    let mut summary = vec![0x7f as c_char; cap];
    let code = unsafe {
        orender_brir_prepare(
            bytes.as_ptr(),
            bytes.len(),
            path.as_ptr(),
            source.as_ptr(),
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
fn a_room_is_prepared_described_and_loads_as_its_file_does() {
    let dir = scratch("ok");
    let sofa = fixture("chunked_multispeaker_brir.sofa");
    let bytes = std::fs::read(&sofa).unwrap();
    let out = dir.join("brir.room");

    let (code, line) = prepare(&bytes, &out, 512);
    assert_eq!(code, 0, "{line}");
    let fields: Vec<&str> = line.split(' ').collect();
    assert_eq!(fields[0], "emitters=3", "{line}");
    assert_eq!(fields[1], "orientations=1", "{line}");
    assert!(fields[2].starts_with("seconds="), "{line}");
    assert!(fields[3].starts_with("rate="), "{line}");
    let size = std::fs::metadata(&out).unwrap().len();
    assert_eq!(fields[4], format!("bytes={size}"), "{line}");
    assert_eq!(
        fields[5].trim_start_matches("names=").split(',').count(),
        3,
        "{line}"
    );
    assert_eq!(fields[6], "conventions=MultiSpeakerBRIR", "{line}");
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
    let (code, cut) = prepare(&bytes, &out, 12);
    assert_eq!(code, 0);
    assert_eq!(cut, "emitters=3 ");
    // No summary buffer at all is fine.
    assert_eq!(prepare(&bytes, &out, 0).0, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_is_not_a_room_is_refused_and_leaves_the_previous_file() {
    let dir = scratch("refused");
    let out = dir.join("brir.room");
    std::fs::write(&out, b"previous").unwrap();

    let (code, why) = prepare(b"not a SOFA file at all", &out, 256);
    assert_eq!(code, -1);
    assert!(!why.is_empty());
    let pulse = std::fs::read(fixture("Pulse.sofa")).unwrap();
    let (code, why) = prepare(&pulse, &out, 256);
    assert_eq!(code, -1);
    assert!(why.contains("HRTF"), "{why}");
    assert_eq!(std::fs::read(&out).unwrap(), b"previous");

    // Unwritable destination: -2, nothing left behind.
    let bytes = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let missing = dir.join("no-such-dir").join("brir.room");
    let (code, why) = prepare(&bytes, &missing, 256);
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
                path.as_ptr(),
                ptr::null(),
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
             receivers=2 emitters=3 samples=1000 rate=48000 orientations=1 speakers=3 names="
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
    assert_eq!(prepare(&room, &out, 0).0, 0);
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

/// The host's source text rides in the prepared room and comes back from it
/// exactly; a room is not prepared without one.
#[test]
fn a_prepared_room_carries_the_hosts_source() {
    let dir = scratch("source");
    let bytes = std::fs::read(fixture("chunked_multispeaker_brir.sofa")).unwrap();
    let out = dir.join("tagged.room");
    assert_eq!(prepare(&bytes, &out, 0).0, 0);
    let room = ExtractedRoom::from_prepared(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(room.source(), SOURCE);

    let none = dir.join("none.room");
    let path = CString::new(none.to_str().unwrap()).unwrap();
    let code = unsafe {
        orender_brir_prepare(
            bytes.as_ptr(),
            bytes.len(),
            path.as_ptr(),
            ptr::null(),
            ptr::null_mut(),
            0,
        )
    };
    assert_eq!(code, -3);
    assert!(!none.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
