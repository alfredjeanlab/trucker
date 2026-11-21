// The vendored OpenSSL's version and a digest that it computes, without any path that differs between worktrees.
fn main() {
    let digest = openssl::sha::sha256(b"sentinel");
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    println!("{}", openssl::version::version());
    println!("sha256: {hex}");
}
