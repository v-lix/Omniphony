//! The HRIR grid preparation entry point, through C: a SOFA set's grid is
//! built and kept once, found kept the next time, and anything else is
//! refused with a return code and a reason, never a crash.
#![cfg(feature = "sofa")]

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use orender::orender_hrtf_prepare;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../renderer/tests/sofa")
        .join(name)
}

/// Prepare the grid of `sofa` into `grid`: the return code and the summary.
fn prepare(sofa: &Path, grid: &Path) -> (i32, String) {
    let sofa = CString::new(sofa.to_str().unwrap()).unwrap();
    let grid = CString::new(grid.to_str().unwrap()).unwrap();
    let mut summary = vec![0 as c_char; 256];
    let code = unsafe {
        orender_hrtf_prepare(
            sofa.as_ptr(),
            grid.as_ptr(),
            48_000,
            1,
            summary.as_mut_ptr(),
            summary.len() as u32,
        )
    };
    let text = unsafe { CStr::from_ptr(summary.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    (code, text)
}

#[test]
fn a_sofa_sets_grid_is_built_once_and_found_kept() {
    let dir = std::env::temp_dir().join(format!("orender-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let grid = dir.join("hrtf.grid");

    let (code, line) = prepare(&fixture("tester.sofa"), &grid);
    assert_eq!(code, 0, "{line}");
    assert!(line.starts_with("grid=built seconds="), "{line}");
    let bytes = std::fs::metadata(&grid).unwrap().len();
    assert!(line.ends_with(&format!(" bytes={bytes}")), "{line}");
    assert!(!dir.join("hrtf.grid.part").exists());

    let (code, line) = prepare(&fixture("tester.sofa"), &grid);
    assert_eq!((code, line), (1, format!("grid=kept bytes={bytes}")));

    // A room is not a set the HRTF stage loads; nothing is written for it.
    let other = dir.join("room.grid");
    let (code, line) = prepare(&fixture("rows_multispeaker_brir.sofa"), &other);
    assert_eq!(code, -1, "{line}");
    assert!(!line.is_empty());
    assert!(!other.exists());

    let (code, _) = prepare(&dir.join("missing.sofa"), &other);
    assert_eq!(code, -1);

    let (code, line) = prepare(&fixture("tester.sofa"), &dir.join("no/such/dir/hrtf.grid"));
    assert_eq!(code, -2, "{line}");

    assert_eq!(
        unsafe { orender_hrtf_prepare(ptr::null(), ptr::null(), 48_000, 1, ptr::null_mut(), 0) },
        -3
    );
    std::fs::remove_dir_all(&dir).unwrap();
}
