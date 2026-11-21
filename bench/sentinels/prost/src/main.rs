pub mod hello {
    include!(concat!(env!("OUT_DIR"), "/hello.rs"));
}

fn main() {
    let message = hello::Hello { name: "sentinel".into(), count: 3, ..Default::default() };
    println!("{message:?}");
}
