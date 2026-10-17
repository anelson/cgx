fn main() {
    println!("{:?}", std::env::args_os().skip(1).collect::<Vec<_>>());
}
