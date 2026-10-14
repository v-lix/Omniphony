//! The config check, through C: a config this build reads as written is
//! accepted with an empty report; one it would read otherwise is refused
//! with the reason; and the file is only read.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::ptr;

use orender::orender_check_config;

fn manifest(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orender-check-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Check `text`, written to `dir/config.yaml`: the return code and the report.
fn check(dir: &Path, text: &str) -> (i32, String) {
    let path = dir.join("config.yaml");
    std::fs::write(&path, text).unwrap();
    let c = CString::new(path.to_str().unwrap()).unwrap();
    let mut report = vec![b'x' as c_char; 512];
    let code =
        unsafe { orender_check_config(c.as_ptr(), report.as_mut_ptr(), report.len() as u32) };
    let line = unsafe { CStr::from_ptr(report.as_ptr()) }
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        text,
        "the file is only read"
    );
    (code, line)
}

#[test]
fn a_config_read_as_written_is_accepted() {
    let dir = scratch("accepted");
    let sofa = manifest("../renderer/tests/sofa/tester.sofa");
    let text = format!(
        "render:\n  bridge_path: \"/usr/lib/omniphony/libharletty_dolby_bridge.so\"\n  \
         master_gain: -12.5\n  binaural:\n    output_mode: binaural\n    hrir_source: sofa\n    \
         hrtf_sofa_path: \"{}\"\n    unit_scale_m: 2.0\n",
        sofa.display()
    );
    assert_eq!(check(&dir, &text), (0, String::new()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_config_read_otherwise_is_refused_with_the_reason() {
    let dir = scratch("refused");
    for (text, why) in [
        ("render: [\n", "YAML"),
        ("render:\n  binaural:\n    hrir_sorce: saf\n", "hrir_sorce"),
        (
            "render:\n  binaural:\n    unit_scale_m: 900\n",
            "unit_scale_m",
        ),
        (
            "render:\n  binaural:\n    hrir_source: sofa\n",
            "hrtf_sofa_path",
        ),
        (
            "render:\n  binaural:\n    hrir_source: sofa\n    hrtf_sofa_path: /nonexistent/x.sofa\n",
            "/nonexistent/x.sofa",
        ),
    ] {
        let (code, line) = check(&dir, text);
        assert_eq!(code, -1, "{text}: {line}");
        assert!(line.contains(why), "{text}: {line}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_file_or_path_is_said_so() {
    let missing = CString::new("/nonexistent/config.yaml").unwrap();
    let mut report = vec![0 as c_char; 256];
    let code =
        unsafe { orender_check_config(missing.as_ptr(), report.as_mut_ptr(), report.len() as u32) };
    assert_eq!(code, -1);
    let line = unsafe { CStr::from_ptr(report.as_ptr()) }.to_str().unwrap();
    assert!(line.contains("/nonexistent/config.yaml"), "{line}");

    assert_eq!(
        unsafe { orender_check_config(ptr::null(), report.as_mut_ptr(), 256) },
        -3
    );
    assert_eq!(
        unsafe { orender_check_config(ptr::null(), ptr::null_mut(), 0) },
        -3
    );
    // A NULL report still answers.
    assert_eq!(
        unsafe { orender_check_config(missing.as_ptr(), ptr::null_mut(), 0) },
        -1
    );
}
