use std::process::Command;
use std::{env, path::PathBuf};

// The object is named only by its path in a link argument, and OUT_DIR is not a link search directory.
fn main() {
    let object = PathBuf::from(env::var("OUT_DIR").unwrap()).join("value.o");
    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(cc).args(["-c", "src/value.c", "-o"]).arg(&object).status().unwrap();
    assert!(status.success());
    println!("cargo::rerun-if-changed=src/value.c");
    println!("cargo::rustc-link-arg={}", object.display());
}
