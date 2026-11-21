use askama::Template;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "assets/"]
struct Assets;

#[derive(Template)]
#[template(path = "hello.html")]
struct Hello<'a> {
    name: &'a str,
}

fn main() {
    let asset = Assets::get("hello.txt").unwrap();
    print!("{}", std::str::from_utf8(&asset.data).unwrap());
    println!("{}", Hello { name: "sentinel" }.render().unwrap());
}
