use std::io::Write;
use std::{env, fs, path::PathBuf};

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // Put our board-specific memory.x on the linker search path.
    fs::copy("memory.x", out.join("memory.x")).unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    // Our own copy of qingke-rt's link.x, with a .slow section for cold code
    // and data (see freenetradio.x).
    fs::copy("freenetradio.x", out.join("freenetradio.x")).unwrap();
    println!("cargo:rerun-if-changed=freenetradio.x");
    println!("cargo:rustc-link-arg-bins=-Tfreenetradio.x");

    // The web page is stored gzip-compressed (about a third of the size;
    // flash is tight) and served with Content-Encoding: gzip.
    let html = fs::read("src/index.html").unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gz.write_all(&html).unwrap();
    fs::write(out.join("index.html.gz"), gz.finish().unwrap()).unwrap();
    println!("cargo:rerun-if-changed=src/index.html");
}
