fn main() {
    if let Err(error) = sbc::run() {
        eprintln!("sbc: {error}");
        std::process::exit(1);
    }
}
