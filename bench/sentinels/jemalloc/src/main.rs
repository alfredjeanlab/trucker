#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn main() {
    let numbers: Vec<u64> = (0..1_000_000).collect();
    println!("sum: {}", numbers.iter().sum::<u64>());
    println!("jemalloc: {}", tikv_jemalloc_ctl::version::read().unwrap());
}
