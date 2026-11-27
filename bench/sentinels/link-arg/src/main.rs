extern "C" {
    fn sentinel_value() -> i32;
}

fn main() {
    println!("value of the linked object: {}", unsafe { sentinel_value() });
}
