use vmm::vfio::do_vfio_magic;

fn main() {
    if let Some(path) = std::env::args().skip(1).next() {
        do_vfio_magic(&path);
    } else {
        println!("Need a path arg");
    }
}
