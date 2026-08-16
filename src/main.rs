fn main() {
    if let Err(error) = sbc::run() {
        if let Some(error) = error.downcast_ref::<clap::Error>() {
            let exit_code = error.exit_code();
            let _ = error.print();
            std::process::exit(exit_code);
        }
        eprintln!("sbc: {error}");
        std::process::exit(1);
    }
}
