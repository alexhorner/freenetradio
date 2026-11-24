# Third-party licenses

FreeNetRadio itself is released under the [MIT License](LICENSE). It is built from open source libraries, listed below with their authors and licenses. The same list is shown on the radio (Settings → About) and in the web interface (**About & licenses**).

The list is generated from `cargo metadata` for the firmware's target and covers the 95 libraries linked into the firmware. `cargo tree` shows how they fit together.

## Licensing at a glance

| License | Libraries | Obligation when you distribute a firmware image |
|---|---|---|
| MIT OR Apache-2.0 (dual) | 85 | Pick either one. Keep the copyright notice. |
| MIT | 4 | Keep the copyright notice. |
| 0BSD | 2 (`smoltcp`, `managed`) | None. |
| Apache-2.0 | 1 (`embedded-tls`) | Keep the notices and the license; mark changes (our changes are marked "FreeNetRadio patch"). |
| BSD-3-Clause | 1 (`subtle`) | Keep the copyright notice. |
| Unlicense OR MIT | 1 (`byteorder`) | None, or keep the notice if you pick MIT. |
| MPL-2.0 | 1 (`bitmaps`, pulled in by `ch32-hal`) | If you modify `bitmaps`' own files, publish those modifications. We use it unmodified. |

## Compatibility with our MIT license

All of the above can be combined with MIT-licensed code, and a firmware image built from them can be distributed under MIT terms for our own code:

- **MIT, BSD-3-Clause, 0BSD, Unlicense:** permissive, no conflict.
- **Apache-2.0:** compatible with MIT in this direction. Our code stays MIT. A firmware image that includes Apache-2.0 code must be accompanied by Apache-2.0's notices, which this file and `vendor/embedded-tls/LICENSE` provide. Nearly every library here is dual-licensed so we can take the MIT option; `embedded-tls` is Apache-2.0 only.
- **MPL-2.0** (`bitmaps`): weak, per-file copyleft. It only covers `bitmaps`' own source files, not the code that uses it, so it does not affect our MIT license. Its source is unmodified and public at <https://github.com/bodil/bitmaps>, which satisfies MPL's "make the source available" condition.
- **No GPL, LGPL or AGPL code** is used anywhere, so nothing forces the project under a copyleft license.

**Notices for binary releases.** MIT, BSD-3-Clause and Apache-2.0 all ask that the copyright notice and license text travel with copies you distribute, including compiled ones. This file names every library, author and license, and `LICENSE` files for the vendored code are kept in the repository, but it does not reproduce each library's own copyright line. If you publish firmware images, ship a full notices file with them (for example generated with `cargo about` or `cargo bundle-licenses`).

This is our reading of the licenses, not legal advice. Re-check this file when you add or update a dependency.

## Vendored and derived code

| Component | Origin | License |
|---|---|---|
| `vendor/embedded-tls` | [drogue-iot/embedded-tls](https://github.com/drogue-iot/embedded-tls) 0.19.0, patched (changes are marked "FreeNetRadio patch" in the source; the crate's tests and dev-dependencies were also removed from its `Cargo.toml`) | Apache-2.0 (`vendor/embedded-tls/LICENSE`) |
| `vendor/nanomp3-core` | [robbie01/nanomp3](https://github.com/robbie01/nanomp3) 0.2.0, patched (spectrum bands) | MIT OR Apache-2.0 (`vendor/nanomp3-core/LICENSE-MIT`, `LICENSE-APACHE`) |
| MP3 decoding algorithm | `nanomp3-core` is a Rust port of [minimp3](https://github.com/lieff/minimp3) by Lieff | CC0 (public domain) |
| Bitmap fonts | The `FONT_*` fonts shipped inside `embedded-graphics`: the X.org "misc-fixed" fonts (the `ucs-fonts` family by Markus Kuhn). Their source files carry the notice "Public domain font. Share and enjoy." | Public domain |
| Rust `core` and `compiler_builtins` | Compiled into the firmware (`-Zbuild-std=core`) | `core`: MIT OR Apache-2.0. `compiler_builtins` also contains code derived from LLVM's compiler-rt (see its own license files). |

## The flash/RAM split tool

`tools/ram-split` is part of this repository (MIT, like the rest). It is a separate Cargo project that links `ch32-hal` and `panic-halt` (MIT OR Apache-2.0, by Emil Fresk and the Rust Embedded contributors), neither of which goes into the radio firmware. It isn't covered by the library table below.

## Build-time only

These are used to compile the firmware but are not part of the firmware image: `flate2` (compresses the web page), the procedural macro crates (`syn`, `quote`, `proc-macro2`, `darling`, `proc-macro-error`, `embassy-executor-macros`, `qingke-rt-macros`, `riscv-macros`, `futures-macro` and friends) and their helpers (`miniz_oxide`, `crc32fast`, `zlib-rs`, `adler2`, `simd-adler32`, `unicode-ident`, `strsim`, `ident_case`, `litrs`, `pastey`, `fnv`, `version_check`, `autocfg`, `document-features`). All are MIT, Apache-2.0, Zlib, 0BSD or a dual combination of them (`unicode-ident` also includes data under the Unicode license).

## Libraries in the firmware

| Library | Version | Author | License |
|---|---|---|---|
| [aead](https://github.com/RustCrypto/traits) | 0.5.2 | RustCrypto Developers | MIT/Apache-2.0 |
| [aes](https://github.com/RustCrypto/block-ciphers) | 0.8.4 | RustCrypto Developers | MIT/Apache-2.0 |
| [aes-gcm](https://github.com/RustCrypto/AEADs) | 0.10.3 | RustCrypto Developers | MIT/Apache-2.0 |
| [az](https://gitlab.com/tspiteri/az) | 1.2.1 | Trevor Spiteri | MIT/Apache-2.0 |
| [base16ct](https://github.com/RustCrypto/formats/tree/master/base16ct) | 0.2.0 | RustCrypto Developers | MIT/Apache-2.0 |
| [bit_field](https://github.com/phil-opp/rust-bit-field) | 0.10.4 | Philipp Oppermann | MIT/Apache-2.0 |
| [bitflags](https://github.com/bitflags/bitflags) | 1.3.2 | The Rust Project Developers | MIT/Apache-2.0 |
| [bitmaps](https://github.com/bodil/bitmaps) | 3.2.1 | Bodil Stokke | MPL-2.0 (or later) |
| [block-buffer](https://github.com/RustCrypto/utils) | 0.10.4 | RustCrypto Developers | MIT/Apache-2.0 |
| [byteorder](https://github.com/BurntSushi/byteorder) | 1.5.0 | Andrew Gallant | Unlicense/MIT |
| [cfg-if](https://github.com/rust-lang/cfg-if) | 1.0.5 | Alex Crichton | MIT/Apache-2.0 |
| [ch32-hal](https://github.com/ch32-rs/ch32-hal) | 0.1.0 | Andelf | MIT/Apache-2.0 |
| [ch32-metapac](https://github.com/ch32-rs/ch32-data) | 0.3.0 | Andelf | MIT/Apache-2.0 |
| [cipher](https://github.com/RustCrypto/traits) | 0.4.4 | RustCrypto Developers | MIT/Apache-2.0 |
| [const-oid](https://github.com/RustCrypto/formats/tree/master/const-oid) | 0.9.6 | RustCrypto Developers | MIT/Apache-2.0 |
| [cordyceps](https://github.com/hawkw/mycelium) | 0.3.5 | Eliza Weisman | MIT |
| [critical-section](https://github.com/rust-embedded/critical-section) | 1.2.0 | Rust Embedded contributors | MIT/Apache-2.0 |
| [crypto-bigint](https://github.com/RustCrypto/crypto-bigint) | 0.5.5 | RustCrypto Developers | MIT/Apache-2.0 |
| [crypto-common](https://github.com/RustCrypto/traits) | 0.1.7 | RustCrypto Developers | MIT/Apache-2.0 |
| [ctr](https://github.com/RustCrypto/block-modes) | 0.9.2 | RustCrypto Developers | MIT/Apache-2.0 |
| [der](https://github.com/RustCrypto/formats/tree/master/der) | 0.7.10 | RustCrypto Developers | MIT/Apache-2.0 |
| [digest](https://github.com/RustCrypto/traits) | 0.10.7 | RustCrypto Developers | MIT/Apache-2.0 |
| [ecdsa](https://github.com/RustCrypto/signatures/tree/master/ecdsa) | 0.16.9 | RustCrypto Developers | MIT/Apache-2.0 |
| [elliptic-curve](https://github.com/RustCrypto/traits/tree/master/elliptic-curve) | 0.13.8 | RustCrypto Developers | MIT/Apache-2.0 |
| [embassy-executor](https://github.com/embassy-rs/embassy) | 0.10.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-executor-timer-queue](https://github.com/embassy-rs/embassy) | 0.1.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-futures](https://github.com/embassy-rs/embassy) | 0.1.2 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-hal-internal](https://github.com/embassy-rs/embassy) | 0.5.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-net](https://github.com/embassy-rs/embassy) | 0.9.1 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-net-driver](https://github.com/embassy-rs/embassy) | 0.2.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-net-driver-channel](https://github.com/embassy-rs/embassy) | 0.4.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-sync](https://github.com/embassy-rs/embassy) | 0.8.0 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-time](https://github.com/embassy-rs/embassy) | 0.5.1 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-time-driver](https://github.com/embassy-rs/embassy) | 0.2.2 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-time-queue-utils](https://github.com/embassy-rs/embassy) | 0.3.2 | Embassy contributors | MIT/Apache-2.0 |
| [embassy-usb-driver](https://github.com/embassy-rs/embassy) | 0.2.2 | Embassy contributors | MIT/Apache-2.0 |
| [embedded-can](https://github.com/rust-embedded/embedded-hal) | 0.4.1 | Rust Embedded contributors | MIT/Apache-2.0 |
| [embedded-graphics](https://github.com/embedded-graphics/embedded-graphics) | 0.8.2 | James Waples | MIT/Apache-2.0 |
| [embedded-graphics-core](https://github.com/embedded-graphics/embedded-graphics) | 0.4.1 | James Waples | MIT/Apache-2.0 |
| [embedded-hal](https://github.com/rust-embedded/embedded-hal) | 0.2.7 | The Embedded HAL Team | MIT/Apache-2.0 |
| [embedded-hal-async](https://github.com/rust-embedded/embedded-hal) | 1.0.0 | The Embedded HAL Team and Contributors | MIT/Apache-2.0 |
| [embedded-hal-nb](https://github.com/rust-embedded/embedded-hal) | 1.0.0 | Rust Embedded contributors | MIT/Apache-2.0 |
| [embedded-io](https://github.com/rust-embedded/embedded-hal) | 0.6.1 | Rust Embedded contributors | MIT/Apache-2.0 |
| [embedded-io-async](https://github.com/rust-embedded/embedded-hal) | 0.7.0 | Rust Embedded contributors | MIT/Apache-2.0 |
| [embedded-nal](https://github.com/rust-embedded-community/embedded-nal) | 0.9.0 | Jonathan 'theJPster' Pallant | MIT/Apache-2.0 |
| [embedded-nal-async](https://github.com/rust-embedded-community/embedded-nal) | 0.9.0 | Rust Embedded Community | MIT/Apache-2.0 |
| [embedded-storage](https://github.com/rust-embedded-community/embedded-storage) | 0.3.2 | Mathias Koch | MIT/Apache-2.0 |
| [embedded-tls](https://github.com/drogue-iot/embedded-tls) | 0.19.0 | Drogue IoT contributors | Apache-2.0 |
| [ff](https://github.com/zkcrypto/ff) | 0.13.1 | Sean Bowe | MIT/Apache-2.0 |
| [float-cmp](https://github.com/mikedilger/float-cmp) | 0.9.0 | Mike Dilger | MIT |
| [futures](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-channel](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-core](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-io](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-sink](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-task](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [futures-util](https://github.com/rust-lang/futures-rs) | 0.3.34 | Rust Project contributors | MIT/Apache-2.0 |
| [generic-array](https://github.com/fizyk20/generic-array.git) | 0.14.7 | Bartłomiej Kamiński | MIT |
| [ghash](https://github.com/RustCrypto/universal-hashes) | 0.5.1 | RustCrypto Developers | MIT/Apache-2.0 |
| [group](https://github.com/zkcrypto/group) | 0.13.0 | Sean Bowe | MIT/Apache-2.0 |
| [hash32](https://github.com/japaric/hash32) | 0.3.1 | Jorge Aparicio | MIT/Apache-2.0 |
| [heapless](https://github.com/rust-embedded/heapless) | 0.9.3 | Jorge Aparicio | MIT/Apache-2.0 |
| [hkdf](https://github.com/RustCrypto/KDFs/) | 0.12.4 | RustCrypto Developers | MIT/Apache-2.0 |
| [hmac](https://github.com/RustCrypto/MACs) | 0.12.1 | RustCrypto Developers | MIT/Apache-2.0 |
| [inout](https://github.com/RustCrypto/utils) | 0.1.4 | RustCrypto Developers | MIT/Apache-2.0 |
| [managed](https://github.com/m-labs/rust-managed.git) | 0.8.0 | whitequark | 0BSD |
| [micromath](https://github.com/tarcieri/micromath) | 2.1.0 | Tony Arcieri | MIT/Apache-2.0 |
| [nanomp3-core](https://github.com/robbie01/nanomp3) | 0.2.0 | Robert B. Langer | MIT/Apache-2.0 |
| [nb](https://github.com/rust-embedded/nb) | 0.1.3 | Jorge Aparicio | MIT/Apache-2.0 |
| [num-traits](https://github.com/rust-num/num-traits) | 0.2.19 | The Rust Project Developers | MIT/Apache-2.0 |
| [opaque-debug](https://github.com/RustCrypto/utils) | 0.3.1 | RustCrypto Developers | MIT/Apache-2.0 |
| [p256](https://github.com/RustCrypto/elliptic-curves/tree/master/p256) | 0.13.2 | RustCrypto Developers | MIT/Apache-2.0 |
| [pin-project-lite](https://github.com/taiki-e/pin-project-lite) | 0.2.17 | Taiki Endo | MIT/Apache-2.0 |
| [polyval](https://github.com/RustCrypto/universal-hashes) | 0.6.2 | RustCrypto Developers | MIT/Apache-2.0 |
| [portable-atomic](https://github.com/taiki-e/portable-atomic) | 1.15.0 | Taiki Endo | MIT/Apache-2.0 |
| [primeorder](https://github.com/RustCrypto/elliptic-curves/tree/master/primeorder) | 0.13.6 | RustCrypto Developers | MIT/Apache-2.0 |
| [qingke](https://github.com/ch32-rs/qingke) | 0.8.4 | Andelf | MIT/Apache-2.0 |
| [qingke-rt](https://github.com/ch32-rs/qingke) | 0.8.4 | Andelf | MIT/Apache-2.0 |
| [rand_core](https://github.com/rust-random/rand) | 0.6.4 | The Rand Project Developers | MIT/Apache-2.0 |
| [rfc6979](https://github.com/RustCrypto/signatures/tree/master/rfc6979) | 0.4.0 | RustCrypto Developers | MIT/Apache-2.0 |
| [riscv](https://github.com/rust-embedded/riscv) | 0.16.1 | The RISC-V Team | MIT/Apache-2.0 |
| [riscv-types](https://github.com/rust-embedded/riscv) | 0.1.0 | The RISC-V Team | MIT/Apache-2.0 |
| [sdio-host](https://github.com/jkristell/sdio-host) | 0.5.0 | Johan Kristell | MIT/Apache-2.0 |
| [sec1](https://github.com/RustCrypto/formats/tree/master/sec1) | 0.7.3 | RustCrypto Developers | MIT/Apache-2.0 |
| [sha2](https://github.com/RustCrypto/hashes) | 0.10.9 | RustCrypto Developers | MIT/Apache-2.0 |
| [signature](https://github.com/RustCrypto/traits/tree/master/signature) | 2.2.0 | RustCrypto Developers | MIT/Apache-2.0 |
| [smoltcp](https://github.com/smoltcp-rs/smoltcp.git) | 0.13.1 | whitequark | 0BSD |
| [stable_deref_trait](https://github.com/storyyeller/stable_deref_trait) | 1.2.1 | Robert Grosse | MIT/Apache-2.0 |
| [static_cell](https://github.com/embassy-rs/static-cell) | 2.1.1 | Embassy contributors | MIT/Apache-2.0 |
| [subtle](https://github.com/dalek-cryptography/subtle) | 2.6.1 | Isis Lovecruft | BSD-3-Clause |
| [typenum](https://github.com/paholg/typenum) | 1.20.1 | paholg | MIT/Apache-2.0 |
| [universal-hash](https://github.com/RustCrypto/traits) | 0.5.1 | RustCrypto Developers | MIT/Apache-2.0 |
| [vcell](https://github.com/japaric/vcell) | 0.1.3 | Jorge Aparicio | MIT/Apache-2.0 |
| [void](https://github.com/reem/rust-void.git) | 1.0.2 | Jonathan Reem | MIT |
| [zeroize](https://github.com/RustCrypto/utils) | 1.9.1 | RustCrypto Developers | MIT/Apache-2.0 |
