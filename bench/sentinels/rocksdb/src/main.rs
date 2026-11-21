fn main() {
    let path = std::env::temp_dir().join(format!("sentinel-rocksdb-{}", std::process::id()));
    let db = rocksdb::DB::open_default(&path).unwrap();
    db.put(b"key", b"value").unwrap();
    let value = db.get(b"key").unwrap().unwrap();
    println!("rocksdb read: {}", String::from_utf8(value).unwrap());
    drop(db);
    let _ = rocksdb::DB::destroy(&rocksdb::Options::default(), &path);
}
