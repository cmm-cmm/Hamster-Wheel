//! The shipped executable has to identify itself.
//!
//! Before this existed, `icon/hamster-wheel.rc` carried only an icon, so a
//! deployed `hamster-wheel.exe` showed an empty `FileVersion` in its Windows
//! file properties and nothing in the artifact named the tree it came from.
//! `doc/PACKAGING_UPGRADE_ROLLBACK.md` called that a release blocker. These
//! tests read the resource back out of the built binary, so losing it again
//! fails the suite rather than a deployment.

#![cfg(windows)]

use std::{os::windows::ffi::OsStrExt, process::Command};

#[link(name = "version")]
unsafe extern "system" {
    fn GetFileVersionInfoSizeW(file: *const u16, handle: *mut u32) -> u32;
    fn GetFileVersionInfoW(file: *const u16, handle: u32, length: u32, data: *mut u8) -> i32;
    fn VerQueryValueW(
        block: *const u8,
        sub_block: *const u16,
        buffer: *mut *mut u8,
        length: *mut u32,
    ) -> i32;
}

/// The fixed part of a `VERSIONINFO` resource, as Windows defines it.
#[repr(C)]
struct FixedFileInfo {
    signature: u32,
    struct_version: u32,
    file_version_ms: u32,
    file_version_ls: u32,
    product_version_ms: u32,
    product_version_ls: u32,
    flags_mask: u32,
    flags: u32,
    os: u32,
    kind: u32,
    subtype: u32,
    date_ms: u32,
    date_ls: u32,
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The whole version resource of `path`, or `None` when it has none.
fn version_resource(path: &str) -> Option<Vec<u8>> {
    let file = wide(path);
    let mut handle = 0_u32;
    let size = unsafe { GetFileVersionInfoSizeW(file.as_ptr(), &raw mut handle) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0_u8; size as usize];
    let read = unsafe { GetFileVersionInfoW(file.as_ptr(), handle, size, data.as_mut_ptr()) };
    (read != 0).then_some(data)
}

/// One `StringFileInfo` value from the resource.
fn string_value(resource: &[u8], name: &str) -> Option<String> {
    // 040904b0: US English, Unicode, which is what build.rs writes.
    let sub_block = wide(&format!("\\StringFileInfo\\040904b0\\{name}"));
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut length = 0_u32;
    let found = unsafe {
        VerQueryValueW(
            resource.as_ptr(),
            sub_block.as_ptr(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    if found == 0 || buffer.is_null() || length == 0 {
        return None;
    }
    // `length` counts characters including the terminator.
    let characters =
        unsafe { std::slice::from_raw_parts(buffer.cast::<u16>(), (length - 1) as usize) };
    Some(String::from_utf16_lossy(characters))
}

#[test]
fn the_executable_reports_its_version_and_the_tree_it_was_built_from() {
    let path = env!("CARGO_BIN_EXE_hamster-wheel");
    let resource = version_resource(path).expect("the executable should carry a version resource");

    // The numeric fields, which Explorer and most inventory tools read.
    let mut buffer: *mut u8 = std::ptr::null_mut();
    let mut length = 0_u32;
    let root = wide("\\");
    let found = unsafe {
        VerQueryValueW(
            resource.as_ptr(),
            root.as_ptr(),
            &raw mut buffer,
            &raw mut length,
        )
    };
    assert_ne!(found, 0, "the resource has no fixed file information");
    let fixed = unsafe { &*buffer.cast::<FixedFileInfo>() };
    let numbers = [
        fixed.file_version_ms >> 16,
        fixed.file_version_ms & 0xffff,
        fixed.file_version_ls >> 16,
    ];
    let expected: Vec<u32> = env!("CARGO_PKG_VERSION")
        .split('-')
        .next()
        .unwrap()
        .split('.')
        .map(|part| part.parse().unwrap())
        .collect();
    assert_eq!(
        numbers.to_vec(),
        expected,
        "FILEVERSION does not match the package version"
    );

    // And the strings, including the stamp that identifies the artifact.
    assert_eq!(
        string_value(&resource, "FileVersion").as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(
        string_value(&resource, "OriginalFilename").as_deref(),
        Some("hamster-wheel.exe")
    );
    assert_eq!(
        string_value(&resource, "ProductName").as_deref(),
        Some("Hamster Wheel")
    );
    let product_version =
        string_value(&resource, "ProductVersion").expect("ProductVersion is written");
    assert!(
        product_version.starts_with(env!("CARGO_PKG_VERSION")),
        "{product_version}"
    );
    assert!(
        product_version.contains('(') && product_version.contains(')'),
        "the build stamp is missing from {product_version}"
    );
    let description =
        string_value(&resource, "FileDescription").expect("FileDescription is written");
    assert!(!description.is_empty(), "the description is empty");
}

#[test]
fn validate_reports_the_same_version_and_build_as_the_resource() {
    // The two have to agree, or one of them is identifying something else.
    let output = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"))
        .args(["--validate"])
        .output()
        .expect("the binary should run");
    assert!(output.status.success());
    let report = String::from_utf8_lossy(&output.stdout);
    let line = report
        .lines()
        .find(|line| line.starts_with("version="))
        .expect("the report names the build");
    let resource = version_resource(env!("CARGO_BIN_EXE_hamster-wheel")).expect("a resource");
    let product_version = string_value(&resource, "ProductVersion").expect("ProductVersion");

    let version = line
        .split_whitespace()
        .next()
        .and_then(|field| field.strip_prefix("version="))
        .expect("a version field");
    let build = line
        .split_whitespace()
        .nth(1)
        .and_then(|field| field.strip_prefix("build="))
        .expect("a build field");
    assert_eq!(version, env!("CARGO_PKG_VERSION"));
    assert!(!build.is_empty());
    assert!(
        product_version.contains(build),
        "the report says {build}, the resource says {product_version}"
    );
}
