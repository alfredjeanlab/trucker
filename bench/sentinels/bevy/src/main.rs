use bevy::app::{App, Update};

fn hello() {
    println!("bevy system ran");
}

fn main() {
    App::new().add_systems(Update, hello).run();
}
