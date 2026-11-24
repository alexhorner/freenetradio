//! The About page's data: the repository, the licence, and the list of open
//! source libraries (name | author | licence per line, generated from
//! `cargo metadata`, see LICENSES.md). Shown by the settings menu and served
//! as plain text at `GET /licenses` for the web page. Kept in slow flash.

pub const REPO: &str = "https://github.com/alexhorner/freenetradio";
pub const LICENSE: &str = "MIT";

#[link_section = ".slow.licenses"]
static DATA: [u8; include_bytes!("licenses.txt").len()] = *include_bytes!("licenses.txt");

/// The whole list as text, one `name|author|licence` line per library.
pub fn text() -> &'static str {
    // licenses.txt is UTF-8 (it's a source file).
    unsafe { core::str::from_utf8_unchecked(&DATA) }
}

pub fn count() -> usize {
    text().lines().count()
}

/// Library `i` as (name, author, licence).
pub fn entry(i: usize) -> (&'static str, &'static str, &'static str) {
    let mut f = text().lines().nth(i).unwrap_or("").split('|');
    (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""))
}
