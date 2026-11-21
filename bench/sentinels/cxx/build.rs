fn main() {
    cxx_build::bridge("src/main.rs").file("src/blob.cc").std("c++17").compile("sentinel-cxx");
    println!("cargo:rerun-if-changed=src/main.rs");
    println!("cargo:rerun-if-changed=src/blob.cc");
    println!("cargo:rerun-if-changed=include/blob.h");
}
