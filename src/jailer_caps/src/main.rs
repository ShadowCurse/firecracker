fn main() {
    use caps::CapSet;

    let cur = caps::read(None, CapSet::Permitted).unwrap();
    eprintln!("Current permitted caps: {:?}.", cur);

    // Retrieve effective set.
    let cur = caps::read(None, CapSet::Effective).unwrap();
    eprintln!("Current effective caps: {:?}.", cur);
}
