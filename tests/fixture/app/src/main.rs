fn main() {
    println!("{}", core_lib::greeting());
    println!("embedded-manifest-dir={}", embed::MANIFEST_DIR);
}
