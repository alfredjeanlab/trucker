#[cxx::bridge]
mod ffi {
    unsafe extern "C++" {
        include!("sentinel-cxx/include/blob.h");
        fn checksum(bytes: &[u8]) -> u64;
    }
}

fn main() {
    println!("cxx checksum: {:x}", ffi::checksum(b"sentinel"));
}
