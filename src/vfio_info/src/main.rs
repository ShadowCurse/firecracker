use vmm::vfio::do_vfio_magic;

fn main() {
    if let Some(path) = std::env::args().skip(1).next() {
        if let Err(e) = do_vfio_magic(&path) {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    } else {
        println!("Need a path arg");
    }
}
