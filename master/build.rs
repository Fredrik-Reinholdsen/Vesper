//! This build script copies the `memory.x` file from the crate root into
//! a directory where the linker can always find it at build time.
//! For many projects this is optional, as the linker always searches the
//! project root directory -- wherever `Cargo.toml` is. However, if you
//! are using a workspace or have a more complicated build setup, this
//! build script becomes required. Additionally, by requesting that
//! Cargo re-run the build script whenever `memory.x` is changed,
//! updating `memory.x` ensures a rebuild of the application with the
//! new memory settings.

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Put `memory.x` in our output directory and ensure it's
    // on the linker search path.
    let out = &PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let target_dir = &PathBuf::from("../target");
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());

    // By default, Cargo will re-run a build script whenever
    // any file in the project changes. By specifying `memory.x`
    // here, we ensure the build script is only re-run when
    // `memory.x` is changed.
    println!("cargo:rerun-if-changed=memory.x");

    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");

    let out_dir = target_dir.join("thumbv8m.main-none-eabihf").join("release");
    let slave_elf = out_dir.join("slave");

    if !slave_elf.exists() {
        panic!("slave ELF not found. Build it first with: cargo build -p slave --release");
    }

    let bin_path = out_dir.join("slave.bin");

    let status = Command::new("rust-objcopy")
        .args([
            "-O",
            "binary",
            slave_elf.to_str().unwrap(),
            bin_path.to_str().unwrap(),
        ])
        .status()
        .expect("failed to run rust-objcopy");

    assert!(status.success(), "rust-objcopy failed");

    println!("cargo:rerun-if-changed={}", slave_elf.display());
}
