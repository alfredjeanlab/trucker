use std::io::{Read, Write};

fn main() {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(b"sentinel built through cmake").unwrap();
    let compressed = encoder.finish().unwrap();
    let mut text = String::new();
    flate2::read::ZlibDecoder::new(&compressed[..]).read_to_string(&mut text).unwrap();
    println!("zlib-ng round trip: {text} ({} bytes)", compressed.len());
}
