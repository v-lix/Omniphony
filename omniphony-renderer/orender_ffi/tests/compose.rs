//! The config-composition entry point, through C: a patch that sets
//! nothing changes nothing; one that sets values applies them key for key,
//! its relative paths from the patch directory; and whatever the patch, the
//! result is a return code, a report line and, when it applies, a config the
//! engine reads.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use orender::orender_compose_config;

const BASE: &str = "\
render:
  bridge_path: \"/usr/lib/omniphony/libharletty_dolby_bridge.so\"
  master_gain: -12.50
  auto_gain: false
  binaural:
    output_mode: binaural
    hrir_source: saf
    unit_scale_m: 2.00
";

fn manifest(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orender-compose-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Compose `patch` over [`BASE`] into `dir/effective.yaml`, relative paths
/// from `patch_dir`: the return code and the report line.
fn compose(dir: &Path, patch: &Path, patch_dir: Option<&Path>) -> (i32, String) {
    let base = dir.join("base.yaml");
    std::fs::write(&base, BASE).unwrap();
    let c = |p: &Path| CString::new(p.to_str().unwrap()).unwrap();
    let (base, patch, out) = (c(&base), c(patch), c(&dir.join("effective.yaml")));
    let patch_dir = patch_dir.map(c);
    let mut report = vec![0 as c_char; 512];
    let code = unsafe {
        orender_compose_config(
            base.as_ptr(),
            patch.as_ptr(),
            patch_dir.as_ref().map_or(ptr::null(), |d| d.as_ptr()),
            out.as_ptr(),
            report.as_mut_ptr(),
            report.len() as u32,
        )
    };
    let line = unsafe { CStr::from_ptr(report.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    (code, line)
}

#[test]
fn a_patch_that_sets_nothing_changes_nothing() {
    let dir = scratch("nothing");
    let patch = dir.join("config.yaml");
    std::fs::write(
        &patch,
        "render:\n  master_gain: null\n  binaural:\n    reverb: null\n",
    )
    .unwrap();
    let (code, line) = compose(&dir, &patch, None);
    assert_eq!(code, 0, "{line}");
    assert_eq!(line, "status=none keys=0 layout_set=0 decode_thread_set=0");
    assert!(!dir.join("effective.yaml").exists(), "nothing written");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// What a patch sets applies, key for key, and what it leaves out is the
/// host's; a relative path starts in the patch directory.
#[test]
fn a_patch_applies_key_for_key() {
    let dir = scratch("applies");
    let sofa_dir = manifest("../renderer/tests/sofa");
    let patch = dir.join("config.yaml");
    std::fs::write(
        &patch,
        "render:\n  master_gain: -12.0\n  decode_thread: false\n  binaural:\n    \
         hrir_source: brir\n    brir_sofa_path: chunked_multispeaker_brir.sofa\n    \
         unit_scale_m: 1.5\n",
    )
    .unwrap();
    let (code, line) = compose(&dir, &patch, Some(&sofa_dir));
    assert_eq!(code, 1, "{line}");
    assert!(line.starts_with("status=applied keys=5 "), "{line}");
    assert!(line.ends_with("layout_set=0 decode_thread_set=1"), "{line}");
    let config = renderer::config::Config::load(&dir.join("effective.yaml")).expect("reads");
    let render = config.render.expect("render");
    assert_eq!(render.master_gain, Some(-12.0), "the patch's value");
    assert!(render.bridge_path.is_some(), "the host's key kept");
    let bin = render.binaural.expect("binaural");
    assert_eq!(bin.unit_scale_m, Some(1.5));
    assert_eq!(
        bin.brir_sofa_path.as_deref(),
        Some(sofa_dir.join("chunked_multispeaker_brir.sofa").as_path()),
        "resolved from the patch directory"
    );
    assert!(!dir.join("effective.yaml.part").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_rejected_or_missing_patch_writes_nothing_and_says_why() {
    let dir = scratch("rejected");
    let patch = dir.join("config.yaml");
    std::fs::write(&patch, "render:\n  osc: true\n").unwrap();
    let (code, line) = compose(&dir, &patch, None);
    assert_eq!(code, -1);
    assert!(line.starts_with("status=rejected keys=0"), "{line}");
    assert!(line.contains("reason=render.osc"), "{line}");
    assert!(!dir.join("effective.yaml").exists());

    let (code, line) = compose(&dir, &dir.join("absent.yaml"), None);
    assert_eq!(code, 0, "{line}");
    assert!(line.starts_with("status=none"));

    // Nowhere to write the composed config.
    std::fs::write(&patch, "render:\n  auto_gain: true\n").unwrap();
    let base = CString::new(dir.join("base.yaml").to_str().unwrap()).unwrap();
    let p = CString::new(patch.to_str().unwrap()).unwrap();
    let out = CString::new(dir.join("missing/effective.yaml").to_str().unwrap()).unwrap();
    let mut report = vec![0 as c_char; 256];
    let code = unsafe {
        orender_compose_config(
            base.as_ptr(),
            p.as_ptr(),
            ptr::null(),
            out.as_ptr(),
            report.as_mut_ptr(),
            256,
        )
    };
    assert_eq!(code, -2);
    let line = unsafe { CStr::from_ptr(report.as_ptr()) }.to_str().unwrap();
    assert!(
        line.starts_with("status=rejected") && line.contains("cannot write"),
        "{line}"
    );

    unsafe {
        assert_eq!(
            orender_compose_config(
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
                0
            ),
            -3
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A composition kept between sessions is rewritten only when it changes:
/// the same base and patch leave the file as it was, a changed patch
/// replaces it.
#[test]
fn an_unchanged_composition_is_left_as_it_is() {
    use std::os::unix::fs::MetadataExt;
    let dir = scratch("unchanged");
    let patch = dir.join("config.yaml");
    let out = dir.join("effective.yaml");
    std::fs::write(&patch, "render:\n  master_gain: -6.0\n").unwrap();
    let (code, line) = compose(&dir, &patch, None);
    assert_eq!(code, 1, "{line}");
    let first = std::fs::metadata(&out).unwrap().ino();

    let (code, again) = compose(&dir, &patch, None);
    assert_eq!(code, 1, "{again}");
    assert_eq!(again, line, "the same report");
    assert_eq!(
        std::fs::metadata(&out).unwrap().ino(),
        first,
        "the same file, not a rewrite"
    );

    std::fs::write(&patch, "render:\n  master_gain: -3.0\n").unwrap();
    let (code, line) = compose(&dir, &patch, None);
    assert_eq!(code, 1, "{line}");
    assert_ne!(std::fs::metadata(&out).unwrap().ino(), first, "rewritten");
    let config = renderer::config::Config::load(&out).expect("reads");
    assert_eq!(config.render.expect("render").master_gain, Some(-3.0));
    assert!(!dir.join("effective.yaml.part").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}
