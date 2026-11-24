# FreeNetRadio: technical notes

This is the engineering documentation, and is produced entirely by AI. For an overview, the parts list and screenshots, see the [README](README.md).

An internet radio for the **WCH CH32V307V-EVT-R1** board, written in Rust.

It plays MP3 streams over **http:// and https://** (Icecast/Shoutcast, plus `.m3u` and `.pls` playlists) through a **PCM5102A** I2S DAC, and shows what's playing on a **256×64 SSD1322 OLED**. You control it from a **web page** served over the chip's built-in Ethernet, or with a cheap **NEC infrared remote**. Up to 99 station presets are kept in flash.

Everything runs on one 144 MHz RISC-V microcontroller with 128 KB of RAM. There is no operating system, no heap and no external memory. The MP3 decoder, TCP/IP stack, TLS 1.3 client, web server, display driver and IR decoder all share that 128 KB.

---

## Contents

- [Features](#features)
- [Hardware](#hardware)
- [Building and flashing](#building-and-flashing)
- [Using it](#using-it)
- [How it works](#how-it-works)
- [Libraries used](#libraries-used) (licenses: [LICENSES.md](LICENSES.md))
- [Memory and flash budget](#memory-and-flash-budget)
- [Source files](#source-files)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)
- [Lessons learned / gotchas](#lessons-learned--gotchas)

---

## Features

- **Streams:**
  - MP3 at any bitrate or sample rate the decoder supports (MPEG-1/2 Layer III), mono or stereo.
  - Over http:// or https:// (TLS 1.3).
  - Follows HTTP redirects (up to 6 hops) and `.m3u` / `.pls` playlists.
  - Shows the station name (`icy-name`) and the current track title (ICY `StreamTitle` metadata).
  - Clear error messages for AAC, Ogg/Opus/FLAC, web pages, HTTP errors, DNS failures and TLS problems.
  - Reconnects automatically when a stream drops or a server goes quiet, carrying on from the buffered audio.
  - If the network link goes down, shows "No network" with a hint to check the cable, and picks the stream up again by itself once the link and an address are back (also at boot, with no cable).
- **Audio:**
  - 16-bit stereo I2S into a PCM5102A, at the stream's own sample rate.
  - Software fade-in and fade-out on start, stop and station change, so there are no clicks.
  - Perceptual volume curve from 0 to 100; 0 means muted.
  - A three-band equaliser (bass, mid, treble) with presets, set from the settings menu.
  - About 1.5 s of network buffering, with automatic re-buffering after an underrun.
- **Display:**
  - Station name, bitrate and sample rate, state, a scrolling description and title, and gradient L/R VU meters. A station name too long for the status bar scrolls in the space left of the format text.
  - Either VU meters or a spectrum analyser (or both in turn), chosen in the settings menu.
  - A clock in the status bar, set from NTP (pool.ntp.org, re-synced hourly), with a time zone offset and EU/UK daylight saving.
  - On-screen messages for remote presses, and a flashing MUTED badge.
  - When stopped: a FreeNetRadio logo with a reflection. After 10 s idle the logo slides to the centre and the panel dims.
  - At power-on: the logo fades in alone in the centre, then slides down into place as the rest of the screen fades in (the sleep animation in reverse, ~1.9 s).
- **Web UI:**
  - Stream URL entry, play/stop, volume, and live status.
  - A preset editor for slots 1–99.
  - A virtual copy of the IR remote.
  - Served gzip-compressed from flash.
- **IR remote:**
  - The common 17-key NEC remote from VS1838B receiver kits.
  - Volume in steps of 1 (with auto-repeat), mute, previous/next preset, play/stop, one- or two-digit preset numbers, and an info screen.
- **Presets:**
  - 99 slots in on-chip flash, one 256-byte page each, checksummed.
  - They survive power loss and reflashing.
  - Ten built-in SomaFM defaults until you save your own.
- **Remembers where you left it:**
  - The last station, volume and mute come back at power-on, and it resumes playing if it was playing.
  - Saved to flash only once settings have been unchanged for 10 s, rotating over 16 pages, so flicking through stations or holding volume-up doesn't wear the flash.
- **Diagnostics:**
  - `/stats` shows stack headroom, decoder load, DAC buffer low-water mark, audio/display timing, Ethernet overflows, IR counters and TLS timings.
  - `/screen` dumps the framebuffer.

---

## Hardware

### Board

| | |
|---|---|
| Board | WCH **CH32V307V-EVT-R1** |
| MCU | **CH32V307VCT6**: QingKe V4F RISC-V core (RV32IMAFC + WCH extensions), 144 MHz, hardware FPU |
| Memory | 480 KB flash and 128 KB SRAM after the option-byte change described below |
| Network | Built-in 10BASE-T Ethernet MAC + PHY with an RJ45 jack on the board |
| Debug probe | On-board **WCH-LinkE** (USB `1a86:8010`), used in RISC-V mode |
| Clock | 8 MHz HSE crystal → PLL → 144 MHz SYSCLK |

### Add-on modules

| Module | Part | Notes |
|---|---|---|
| DAC | PCM5102A I2S breakout | Uses its internal PLL (SCK tied to GND). Many modules have no XSMT pin; it's tied high on the board, which is why fades are done in software. |
| Display | 256×64 SSD1322 OLED (4-bit greyscale), 4-wire SPI | Driven at 9 MHz SCLK with DMA |
| IR receiver | VS1838B (38 kHz) from a "17-key NEC remote + receiver" kit | Output idles high and goes low during bursts |

### Wiring

```
PCM5102A                 SSD1322 OLED (4-wire SPI)      IR receiver (VS1838B)
  BCK   -> PB13            SCLK (D0) -> PA5               S (signal) -> PB10
  LCK   -> PB12            SDIN (D1) -> PA7               +          -> 3V3
  DIN   -> PB15            CS        -> PA4               -          -> GND
  SCK   -> GND             D/C       -> PA3
  FMT   -> GND (I2S)       RES       -> PA2               RJ45 jack LEDs (on board)
  XSMT  -> high (unmute)   VCC/GND   -> 3V3/GND             ELED1 (link)     PC0
  VIN   -> 3V3 or 5V                                         ELED2 (activity) PC1
  GND   -> GND
```

### Peripherals used

| Peripheral | Use |
|---|---|
| SPI1 + DMA1 channel 3 | OLED frame transfers (8 KB per frame, async) |
| SPI2 in I2S mode + DMA1 channel 5 | Audio output from a circular 16 KB ring (register-level driver) |
| ETH MAC + internal 10M PHY | Networking: 2 TX + 7 RX DMA descriptors |
| EXTI line 10 (PB10) | IR edge timestamps, with its own `EXTI15_10` handler |
| RNG | Hardware random numbers for the TLS key exchange |
| Flash controller | Preset storage (256-byte page erase and program) |
| SysTick | Embassy time base |

---

## Building and flashing

### One-time setup

1. **Install the tools.** You need `rustup` and [`wlink`](https://github.com/ch32-rs/wlink) (`cargo install wlink`). The toolchain itself is pinned in `rust-toolchain.toml`: nightly `2026-11-12` with `rust-src`. It's installed automatically on the first build.
2. **Set the chip's flash/RAM split.** The CH32V307 shares its SRAM between "zero-wait flash" (a RAM copy of the first part of flash) and real RAM. The factory setting is 288 KB + 32 KB, and FreeNetRadio needs **192 KB + 128 KB**. The tool in [`tools/ram-split`](tools/ram-split) rewrites the option bytes and leaves read protection off:
   ```sh
   cd tools/ram-split   # TARGET_MODE = 0b00 in src/main.rs
   cargo run --release  # flashes the tool, which reprograms the option bytes and resets
   cd ../..
   ```
   Option bytes survive reflashing, so this is a one-off.

### Build and flash

```sh
cd freenetradio
cargo build --release
wlink flash target/riscv32imfc-unknown-none-elf/release/freenetradio
# or simply: cargo run --release   (the runner is `wlink flash`)
```

A flash takes about 25 s, because the image is ~275 KB across fast and slow flash. `wlink flash` doesn't chip-erase unless you pass `-e`, so **presets survive reflashing**.

### Why the build is unusual

- **Custom target `riscv32imfc-unknown-none-elf.json`:** this is RV32IMFC *without* the A (atomics) extension, plus `+forced-atomics` and WCH's `+xwchc` compressed-instruction extension. QingKe V4's atomic instructions are suspected broken: with them, this firmware occasionally lost task wake-ups and one task stopped forever. Without them, every atomic becomes a short critical section (through `portable-atomic`'s `critical-section` feature). The JSON also turns on `function-sections`, which the linker script's code placement depends on.
- **`-Zbuild-std=core`** (in `.cargo/config.toml`), because a custom target has no prebuilt `core`.
- **Custom linker script `freenetradio.x`:** a copy of `qingke-rt`'s `link.x` with an added `.slow` output section. See [Flash layout](#flash-layout).
- **`build.rs`:** copies `memory.x` and `freenetradio.x` to the linker search path, and gzips `src/index.html` with `flate2` (~24 KB → ~7 KB).
- **Per-crate optimisation:** the release profile is `opt-level = "s"`, but `nanomp3-core` and the P-256 crates (`p256`, `primeorder`, `elliptic-curve`, `crypto-bigint`) are built at `opt-level = 3`. The decoder is the hot path. P-256 blocks the executor during the TLS handshake, so faster maths means a shorter freeze (see [TLS](#tls-https)).

---

## Using it

1. Plug the board into a **10/100/1000** switch port. The PHY is 10 Mbit only; multi-gig and 10G ports usually refuse 10BASE-T. The board gets an address by DHCP, and it's shown on the OLED when stopped. The MAC address is derived from the chip's unique ID, so the DHCP lease stays stable.
2. Open `http://<board-ip>/`.

### Web page

- **Station:** enter a stream or playlist URL, then Play or Stop. Volume slider.
- **Now playing:** state, station, title, format, buffer fill, underruns.
- **Presets:** edit a name and URL for any slot from 1 to 99, save, play, or reset to defaults.
- **Remote:** an on-screen copy of the IR remote. Holding a volume button repeats like the real remote.

### IR remote

| Button | Action |
|---|---|
| ▲ / ▼ | Volume ±1. Holding repeats after 0.4 s (about 9 steps a second). Down to 0 mutes. |
| `*` | Mute/unmute. Unmuting at volume 0 goes to 2. |
| ◀ / ▶ | Previous/next non-empty preset |
| OK | Play/stop. With nothing selected, plays preset 1. |
| `0`–`9` | Preset number. Type one or two digits; a single digit plays after 1.2 s. A lone `0` means preset 10. |
| `#` | Settings menu: time zone, daylight saving, status bar (clock/codec/both/off), clock on the dimmed screen, visualiser (meters/spectrum/cycle), Equaliser submenu (presets, bass/mid/treble), address (up/down pick, left/right change, OK or `#` close), About (repo, license and a scrollable list of open source libraries) |

Any other code is shown on screen as `IR code 0xNN`, so you can map a different remote in `src/remote.rs`. Every button press wakes a dimmed screen.

### HTTP API

| Method & path | Body | Does |
|---|---|---|
| `GET /` | | The web page (gzip) |
| `GET /radio` | | Status JSON: state, message, url, station, title, sample_rate, bitrate, channels, buffer, underruns, volume, muted |
| `POST /radio/play` | `text/plain` URL, or form `url=` | Play a stream |
| `POST /radio/stop` | | Stop |
| `POST /radio/volume` | `0`–`100`, or form `volume=` | Set volume (0 mutes) |
| `GET /presets` | | All presets as JSON (streamed) |
| `POST /presets/<n>` | form `name=`, `url=` | Save slot `n` (1–99). An empty URL clears it. |
| `POST /presets/<n>/play` | | Play slot `n` |
| `POST /presets/reset` | | Back to the built-in list |
| `POST /remote/<key>[?repeat]` | | Press a virtual button: `0`–`9`, `star`, `hash`, `up`, `down`, `left`, `right`, `ok` |
| `GET /stats` | | Plain-text diagnostics (see below) |
| `GET /licenses` | | Plain text, one `name\|author\|license` line per open source library (the web page's About dialog reads it) |
| `GET /screen` | | Raw framebuffer: 256×64, 4 bpp, 8192 bytes |

POSTs reply with the status JSON. A plain HTML form submit (`Accept: text/html`) gets a redirect back to `/` instead.

```sh
curl -X POST -H 'Content-Type: text/plain' \
     --data 'https://ice1.somafm.com/groovesalad-128-mp3' http://<board-ip>/radio/play
```

### `/stats` (diag build only; the numbers below are an example)

```
stack: 3476 of 13992 bytes never used          <- headroom; keep it above ~2 KB
uptime: 166 s
frame gap: 12-26 ms                            <- display frame pacing (min-max since last read)
decode per frame: avg 12089 us, max 12165 us   <- MP3 decoder cost per 26 ms frame
underruns: 0
rx ring overflows: 0                           <- Ethernet RX descriptors ran out
stream bytes in: 927960
audio loops: 8255
display frames: 1168 (stage 3), flush timeouts: 0, overlay draw max: 3100 us
DAC queue low-water: 2169 frames, audio loop max gap: 27574 us   <- the stutter indicator
ir: last code addr 0x00 cmd 0x1C, frames 2, errors 0
tls handshake: 286 ms, longest read 4703 us, last error 0, stage 6, at error: tcp rx 0 tx 0, tls decoded 0 pending 0
reconnects: 0, network outages: 0
settings saves: 2, failed 0 ()
last underrun (ms, kind, buffered, queued): 1953 2 2508 380
spectrum dB (bands 0 10 20 30 39): 34 31 27 22 14
i2s underruns: 0, dma errors: 0
decode errors (noframe, reservoir, corrupt, other): 0 0 0 0
```

The `tls` line shows the last handshake's duration and the longest single TLS read (most of it is decrypting one record). It also gives the last TLS error code (see `tls_error_code` in `radio.rs`) and how far the last connection got: 1 DNS, 2 TCP connect, 3 TLS handshake, 4 request sent, 5 headers received, 6 streaming. At the last read error it records the TCP queues and the TLS reader's buffer state. `reconnects` counts automatic reconnects since power-on. `last underrun` gives four numbers (in the order shown): ms after playback started, the cause (1 = the DMA overtook the decoder, 2 = ran out of stream data), the compressed bytes buffered, and the frames queued for the DAC.

The **DAC queue low-water** is the key health number. The ring holds 4096 frames (93 ms). Smooth playback sits around 2000–2200 frames, and anything near 0 is audible stutter. Most values reset each time they're read.

**Diagnostics are a build option.** `/stats` and every counter behind it are only compiled in with `cargo build --release --features diag` (about 8 KB larger, and it changes timing slightly). The normal build has no `/stats`, so `GET /stats` returns 404. `GET /screen/stream?n=<frames>` (diag build only) records the display for the README's GIFs: the display task hands over each finished frame and waits while it is copied out, so every frame is whole (no tearing) and the menu pointer's bob is driven by the frame count rather than the clock. The capture runs at about 23 frames/s, so playback at the panel's native 50 fps (a 20 ms GIF delay) is the real animation; the panel itself draws at 50 fps, which is why the GIFs are not 60 fps. The diag build also adds `i2s underruns` / `dma errors` (the I2S transmitter missing a sample slot, and DMA transfer errors) and `decode errors` (frames the MP3 decoder dropped: no frame, reservoir, corrupt, other). Build with `diag` whenever you need to check stack headroom after a change.

---

## How it works

### Task layout

Everything runs as cooperative tasks on a single **Embassy** executor (`platform-spin`). Interrupts are used only where timing can't wait: Ethernet, IR edge capture, and the RNG.

```
                         ┌──────────────┐
  Ethernet ── net_task ──┤ embassy-net  │── http_task ×2 ── web page / API
  (ETH IRQ)              │  (smoltcp)   │
                         └──────┬───────┘
                                │ TCP (optionally TLS 1.3)
                          stream_task  ── HTTP/ICY client, redirects, playlists
                                │ MP3 bytes
                          STREAM pipe (10 KB)
                                │
                          audio_task   ── nanomp3 decode → volume/fade → meter
                                │ PCM
                          I2S ring (4096 frames) ── DMA1 ch5 ── SPI2/I2S ── PCM5102A

  display_task ── reads status, draws with embedded-graphics, DMA to SSD1322
  ir_task      ── decodes NEC from edge timestamps (EXTI15_10 IRQ) → remote.rs
  digit_task   ── commits multi-digit preset numbers after a pause
  jack_leds    ── RJ45 link/activity LEDs
```

### Audio path

- **Decoder:** `audio_task` decodes one MP3 frame (1152 samples) at a time with `nanomp3-core`. A frame takes about 12 ms of CPU for every 26 ms of audio, so the task **yields between frames** to let the network and display run.
- **Output:** samples go into a circular DMA ring. `I2sOut` keeps **running totals** of frames written and frames played, not positions modulo the ring size. Positions modulo the ring were the cause of an earlier, hard-to-find stutter: after a long writer stall, a full ring looked empty.
- **Fades:** start and stop are faded over 1024 frames (~23 ms). Played audio is zeroed behind the DMA, so a stall produces silence rather than a repeating loop.
- **Sample rate:** changes reprogram the I2S prescaler, clearing `TXDMAEN` first. The reported rate is the real one the divider gives, e.g. 44117 Hz for 44.1 kHz.
- **Buffering:** about 1.5 s at 128 kbit/s in total:

  - The TCP receive buffer (10 KB, or 7 KB over TLS).
  - The `STREAM` pipe (10 KB).
  - The decoder's 4 KB input window.

  When the pipe is full, the stream task stops reading, which back-pressures TCP. Playback starts once the pipe is (nearly) full and has *stayed* full for 0.4 s, so the TCP buffer behind it has filled too.

  That matters at high bitrates. The buffers are sized in bytes, so at 320 kbit/s they hold only ~0.5 s, and some servers send in ~16 KB lumps with pauses of up to ~0.5 s between them. Starting at 75% of the pipe (~0.27 s at 320k) underran in the first 2 s about half the time. Start-up now takes ~1 s, plus the TLS handshake for https.

### Networking

- **Driver and stack:** `ch32-hal`'s Ethernet driver feeds `embassy-net`, which wraps `smoltcp`. The stack runs DHCPv4, a DNS client and ICMP echo (ping), with sockets for the stream and the two web server tasks.
- **RX buffers:** there are 7 RX DMA descriptors. Each MP3 decode blocks the network task for ~12 ms, and 4 descriptors overflowed during bursts. The 7th was added for 320 kbit/s https, where a record decrypt can follow a decode.
- **RX overflows are expensive:** a dropped frame costs TCP a retransmission. At start-up that sometimes turned into multi-second stalls, or a server that went silent until the 15 s socket timeout. Keeping every executor block short (see TLS) is what keeps the ring from overflowing; `rx ring overflows` in `/stats` should stay at or near 0.
- **Custom `ETH` interrupt handler:** it clears flags the HAL leaves set (RBUS/AIS, and DMASR bit 31 "PHY link change"). Without that, there was an interrupt storm.
- **TX buffers:** the TX descriptor count must stay **even**. The HAL packs 1514-byte TX buffers in front of the RX buffers, so an odd count misaligns RX and the DMA corrupts RAM.

### TLS (https)

TLS uses `embedded-tls`, a no_std, allocation-free TLS 1.3 client, with these choices:

- **Cipher suite:** `TLS_AES_128_GCM_SHA256`, with P-256 (secp256r1) ECDHE key exchange.
- **Signature schemes:** ECDSA and RSA are advertised (`enable_rsa_signatures()`). The servers tried all use RSA-PSS certificates. Nothing is actually verified, so no RSA code is linked.
- **No certificate verification.** There's no room for a root-CA store. The stream is encrypted on the wire, but the server isn't authenticated.
- **Small records:** the client sends the **max_fragment_length** extension (RFC 6066) asking for **2 KB** records. A TLS client must normally buffer a whole 16 KB+256 record, which is more RAM than there is to spare. SomaFM, Radio Paradise, Radio France and Simulator Radio all honour it (they run OpenSSL), so the read buffer is 2560 bytes: 2048 + 256 + 5 for the largest record. Small records also keep each decrypt short. A record is decrypted in one go with no await inside, ~4.7 ms for 2 KB. The earlier 4 KB records took ~9 ms, and on top of an MP3 decode that let the Ethernet RX ring overflow at 320 kbit/s.
- **Vendored, patched `embedded-tls`** (`vendor/embedded-tls`, search for "FreeNetRadio patch"). Upstream can't handle a handshake message split across several records, and with 2 KB fragments the server's 3–5 KB certificate message always is. Without a verifier the certificate is never inspected, so the patch feeds the pieces into the handshake transcript hash and skips them. Reassembling them would need a certificate-sized buffer.
- **Shared buffer:** the stream task's 10 KB network buffer is split at run time. Plain http uses all 10 KB as the TCP receive buffer. https uses 7 KB for TCP, 2.5 KB for the TLS read buffer and 0.5 KB for the TLS write buffer. TLS adds only its key-schedule state, about 1 KB. A second patch adds two diagnostic accessors used by `/stats`.
- **Randomness:** keys come from the CH32V307's hardware **RNG** through `ch32-hal::rng`, which implements `rand_core` 0.6 `CryptoRng`.
- **Timing:**
  - The handshake takes ~0.3 s to a nearby server, ~0.65 s to one 140 ms away.
  - The two P-256 scalar multiplications have no await points, and each blocks the executor for ~90 ms. Their inner loop (field add/sub/mul, point add/double, ~6 KB) lives in zero-wait flash (`.fastecc` in `freenetradio.x`). From slow flash each one blocked for ~600 ms, which overflowed the Ethernet RX ring and made https start-ups stall or fail about one time in four.
  - The stream task yields after each decrypted record.
  - In steady state, decryption costs ~4.7 ms per 2 KB record, about 10% CPU at 320 kbit/s.
- **Reconnecting:** if the response headers don't arrive within 5 s, or an established stream drops or goes quiet for 5 s, the client reconnects. After a network blip, the old connection can sit idle for many seconds (the server's TCP retries back off), so this is much quicker than waiting for the 15 s socket timeout. A stream that won't start gets 3 attempts. A stream that was playing gets a fresh 3 each time it drops, and carries on from the buffered audio meanwhile. Bad URLs, HTTP errors and unsupported formats still stop at once with a message.

### Display

- **Framebuffer:** an 8 KB, 4 bpp framebuffer in RAM. `embedded-graphics` draws into it through a `DrawTarget`, and it's sent to the SSD1322 by SPI DMA. The panel is initialised with tuned contrast and pre-charge settings (`Tuning::DEFAULT` in `ssd1322.rs`).
- **Frame pacing:** while streaming, frames are drawn on a **fixed 20 ms grid** (50 fps) and scrolling text moves exactly 1 px per frame. Steps by elapsed time looked juddery when a decode delayed a frame. When idle it redraws a few times a second.
- **Idle dimming:** 10 s after the last activity while stopped:
  - The UI fades out (the framebuffer levels are scaled).
  - The logo slides to the centre over 1.5 s with smoothstep easing.
  - The panel dims to 60% of the way towards minimum. Master contrast (0xC7), segment current (0xC1), pre-charge voltage (0xBB) and second pre-charge period (0xB6) are lowered together.

  Lowering the current alone only flattens the gradient, because the pre-charge stage still lights every pixel. A button press or web action wakes it instantly.

### IR remote

The VS1838B output goes to PB10. An `EXTI15_10` handler timestamps every edge (`embassy_time::Instant`, µs) and queues period lengths into a 128-entry channel. `ir_task` decodes NEC at leisure:
- a 9 ms leader;
- a 4.5 ms gap for a new code, or 2.25 ms for a repeat;
- 32 bits, LSB first.

The timing is captured in the interrupt, not in a task, because MP3 decoding blocks the executor for ~12 ms, far too long for sub-millisecond pulses. `ch32-hal` is built with `default-features = false` to drop its `exti` feature, whose handler would claim the interrupt and only wake a task.

### Remembered settings

`settings.rs` saves the station URL, volume, mute and whether it was playing. At startup they're restored before the stream task starts. If it was playing, it starts playing once the network is up; otherwise the station is just selected, ready for OK.

Flash wear is what shapes this. A page is good for roughly 10,000 erases, and someone flicking through stations or holding volume-up changes the settings many times a second.
- **Settle first:** the save task checks once a second, and writes only after the settings have been unchanged for 10 s *and* differ from what's saved. Ten station changes in a row cost one write, and a change that's undone before it settles costs nothing.
- **Rotate, in two rings:** the settings (volume, mute, playing, clock, display and EQ choices) go round 12 pages at `0x0807_0000`–`0x0807_0BFF`, and the station URL (up to 240 characters) round 4 pages at `0x0807_0C00`–`0x0807_0FFF`, just below preset 99. They're separate so changing the volume doesn't rewrite the URL and changing station doesn't rewrite the settings. Each record carries a sequence number, and the newest valid one wins at boot. The previous record stays intact until the new one is written, so a power cut mid-save loses nothing. (The chip's flash is programmed a whole 256-byte page at a time, so a page per save is as small as it gets.)
- **Small RAM footprint:** between checks the task keeps only checksums of the pending and saved settings, not copies, which saves ~500 bytes.

Writes go through `presets::write_page_at`, which stays in fast flash; the save task itself lives in `.slow`. A save blocks for ~25 ms, well within the 93 ms audio ring.

### Presets

Presets live in the top of flash, above the firmware. Each slot is one 256-byte page: a magic number, an FNV-1a checksum, the name (≤32 bytes) and the URL (≤212 bytes).
- **Layout:** slots 1–10 are at `0x0807_7000`, and a marker page follows them. Slots 11–99 grow downwards from there.
- **Defaults:** without the marker page, the built-in defaults are used.
- **First save:** writes slots 1–10, then the marker, so a power cut mid-save leaves the defaults rather than a half-written set.
- **Audio during saves:** writing a page blocks for ~25 ms, so multi-page saves pause 50 ms between pages to keep the audio buffer topped up.
- **Placement:** `write_page` is `#[inline(never)]` and kept in zero-wait flash, so the code never runs from the flash bank being written.

---

## Libraries used

### Runtime and HAL

| Crate | Version | Role |
|---|---|---|
| [`ch32-hal`](https://github.com/ch32-rs/ch32-hal) | git `a0863eb` | HAL for WCH CH32 chips: clocks, GPIO, SPI + DMA, Ethernet MAC/PHY, flash, RNG, Embassy time driver. Features: `ch32v307vct6`, `rt`, `embassy`, with default features off (no `exti`). |
| `ch32-metapac` | 0.3.0 | Register definitions (pulled in by ch32-hal) |
| [`qingke`](https://github.com/ch32-rs/qingke) / `qingke-rt` | 0.8.4 | QingKe core support (PFIC interrupt controller) and the runtime / startup code. FreeNetRadio's linker script is a modified copy of `qingke-rt`'s `link.x`. |
| [`embassy-executor`](https://embassy.dev) | 0.10.0 | Async task executor (`platform-spin`, `executor-thread`); tasks are statically allocated |
| `embassy-time` | 0.5.1 | Timers, timeouts and `Instant` (SysTick based) |
| `embassy-sync` | 0.8.0 | `Pipe` (stream → decoder), `Signal`, `Channel`, mutexes |
| `embassy-futures` | 0.1.2 | `select`, `yield_now` |
| `critical-section` | 1.2.0 | Interrupt-safe critical sections, used for all shared state |
| `portable-atomic` | 1.15.0 | Atomics emulated with critical sections (no A extension) |
| `static_cell` | 2.1.1 | `ConstStaticCell` for large static buffers. Built in place, so nothing big goes on the stack. |

### Networking

| Crate | Version | Role |
|---|---|---|
| [`embassy-net`](https://embassy.dev) | 0.9.1 | Async socket API over smoltcp. Features: `tcp`, `dns`, `dhcpv4`, `medium-ethernet`, `proto-ipv4`, `auto-icmp-echo-reply`. |
| [`smoltcp`](https://github.com/smoltcp-rs/smoltcp) | 0.13.1 | TCP/IP stack: TCP, DHCP client, DNS, ICMP |
| `embedded-io-async` | 0.7.0 | `Read`/`Write` traits shared by the TCP socket and the TLS connection |

The HTTP server (both the web UI and the API) and the HTTP/ICY stream client are hand-written in `main.rs` and `radio.rs`. There are no HTTP crates.

### TLS and crypto

| Crate | Version | Role |
|---|---|---|
| [`embedded-tls`](https://github.com/drogue-iot/embedded-tls) | 0.19.0, **vendored + patched** | TLS 1.3 client, no_std, no allocator. Default features off. |
| [`p256`](https://github.com/RustCrypto/elliptic-curves) | 0.13.2 | P-256 ECDHE key exchange (via `elliptic-curve`, `primeorder`, `crypto-bigint`) |
| [`aes-gcm`](https://github.com/RustCrypto/AEADs) / `aes` / `ghash` | 0.10.3 / 0.8.4 / 0.5.1 | Record encryption: AES-128-GCM, constant-time software ("fixslice") AES |
| `sha2`, `hmac`, `hkdf` | 0.10.9 / 0.12.1 / 0.12.4 | Handshake transcript hash and the TLS 1.3 key schedule |
| `rand_core` | 0.6.4 | RNG traits linking `ch32-hal`'s hardware RNG to the TLS stack |
| `heapless` | 0.9.3 | Fixed-capacity collections inside embedded-tls |

### Audio, graphics and build

| Crate | Version | Role |
|---|---|---|
| `nanomp3-core` | 0.2.0 | no_std, allocation-free MPEG audio decoder (a port of minimp3). One 22 KB `Decoder` lives in a static. |
| [`embedded-graphics`](https://github.com/embedded-graphics/embedded-graphics) | 0.8.2 | Text (mono fonts), shapes and the `DrawTarget` used for the whole UI. The logo, meters and fades are drawn on top of it. |
| `flate2` (build-dependency) | 1.1.11 | Gzips the web page at build time |

### Hand-written drivers (no crate)

- **SSD1322 driver** (`ssd1322.rs`): initialisation, tuning, DMA flush, greyscale level scaling, dimming.
- **I2S output** (`i2s.rs`): ch32-hal has no I2S support, so SPI2 I2S mode and DMA1 channel 5 are programmed through raw registers.
- **NEC IR decoder** (`ir.rs`): raw GPIO, AFIO and EXTI registers, plus a state machine.
- **Internal PHY** (`InternalPhy` in `main.rs`): reset, initialisation and link polling for the on-chip 10BASE-T PHY.

### Licenses

FreeNetRadio is MIT licensed ([LICENSE](LICENSE)). The full list of third-party libraries, their authors and licenses, and the compatibility check, is in [LICENSES.md](LICENSES.md). The patched copy in `vendor/embedded-tls` keeps its `LICENSE` file, and the changes are marked "FreeNetRadio patch".

---

## Memory and flash budget

### RAM (128 KB)

| What | Size |
|---|---|
| MP3 decoder state (`nanomp3_core::Decoder`) | 22.3 KB |
| Stream task: 10 KB network buffer (TCP rx, or TCP + TLS), TCP tx, header/audio buffer, ICY parser, TLS state | ~18 KB |
| I2S DMA ring (4096 stereo frames) | 16 KB |
| Ethernet DMA buffers (2 TX + 7 RX) | 13.6 KB |
| Stream → decoder pipe | 10 KB |
| HTTP server tasks (2 × sockets + 1024-byte request buffer) and the shared 1 KB status buffer | ~7.5 KB |
| OLED framebuffer | 8 KB |
| Decoder PCM output + input window | 8.6 KB |
| Network stack resources, display task, IR queue, status, misc. | ~10 KB |
| **Stack** (whatever is left) | ~14 KB, peak use ~11 KB |

Static data uses about 112 KB, and the stack gets the rest. Check `stack: … never used` in `/stats` (build with `--features diag`) after any change; it should stay above ~2 KB. Large buffers must be statics (`ConstStaticCell`) or task-local, because `StaticCell::init(value)` builds the value on the stack first and overflows it.

### Flash layout

The chip has 480 KB of flash, but only the first part is mirrored into zero-wait SRAM at boot. The rest is read directly with wait states and is **much** slower: AES ran ~10× slower from it.

| Region | Address (alias) | Size | Contents |
|---|---|---|---|
| `FLASH` (zero-wait) | `0x0000_0000` | 192 KB (~160 KB used) | Vector table, `.fastecc` (P-256 inner loop), hot code (decoder, audio, display, TCP/IP core, interrupt handlers, AES-GCM) and all `.rodata` |
| `SLOW` (wait states) | `0x0003_0000` | 256 KB (~114 KB used) | `.slow` section: web page, HTTP server, stream client, startup code, remote handler, DHCP/DNS, the rest of TLS (embedded-tls, P-256 setup and encoding, SHA-2/HMAC/HKDF) |
| Presets + settings | `0x0807_0000`… | 32 KB | Settings ring (12 pages from `0x0807_0000`) and URL ring (4 pages after it) and preset pages (see above), not touched by flashing |

Code is assigned to `.slow` by **v0-mangled symbol name patterns** in `freenetradio.x`. For example, `*(.text.*12embedded_tls*)` matches any function from the `embedded_tls` crate, including generic instantiations. This only works because `function-sections` puts every function in its own `.text.<mangled name>` section. Data opts in with `#[link_section = ".slow.*"]`; the gzipped web page does this. Rules of thumb:

- **Cold or network-paced code goes in `.slow`.** Anything run per byte or per audio frame stays in fast flash.
- **Keep an eye on fast flash** (~32 KB free at the time of writing). New hot code needs room, so move something cold out if it fills.
- **Never put `presets::write_page` (or anything it calls) in `.slow`.** The code must not run from the flash it's erasing.
- An lld `INSERT` script doesn't work here: the main script's `*(.text .text.*)` claims everything first. That's why the whole linker script is copied.

---

## Source files

| File | What's in it |
|---|---|
| `src/main.rs` | Startup and peripheral setup, task spawning, the Ethernet interrupt handler, the internal PHY driver, the HTTP server and all API routes, `/stats`, the panic handler, stack painting, jack LEDs |
| `src/about.rs`, `src/licenses.txt` | The About page's data: repo, license, and the open source list (kept in slow flash, shown on the OLED and served at `/licenses`). `licenses.txt` is generated from `cargo metadata`; regenerate it (and [LICENSES.md](LICENSES.md)) when dependencies change |
| `src/radio.rs` | Player state, the stream client (DNS, TCP/TLS, HTTP, redirects, playlists, ICY metadata), the shared network buffer, the decode/volume/fade/meter audio task |
| `src/clock.rs` | NTP clock (pool.ntp.org, hourly), time zone and EU/UK daylight saving |
| `src/menu.rs` | The settings menu's state and key handling (main page, Equaliser, About) |
| `src/eq.rs` | Three-band equaliser and its presets |
| `src/spectrum.rs` | Spectrum analyser bands from the decoder's frequency lines |
| `src/i2s.rs` | Register-level I2S2 + DMA ring driver, sample-rate setting, fades, underrun detection |
| `src/display.rs` | Screen layout, scrolling marquees, VU meters, on-screen messages, logo, idle dim and slide |
| `src/ssd1322.rs` | SSD1322 panel driver and `DrawTarget` |
| `src/ir.rs` | EXTI edge capture and NEC decoding |
| `src/remote.rs` | Button → action mapping, multi-digit preset entry, volume repeat |
| `src/presets.rs` | Flash-backed preset storage, and the page read/write helpers |
| `src/settings.rs` | Remembered station, volume and mute (debounced, wear-levelled saves) |
| `src/text.rs` | `Text<N>`: a fixed-capacity string (truncating `fmt::Write`, sanitising pushes) |
| `src/index.html` | The web UI (vanilla HTML/CSS/JS, gzipped into the firmware) |
| `memory.x` | Memory regions (192 K fast flash, 256 K slow flash, 128 K RAM) |
| `freenetradio.x` | Linker script with the `.slow` placement rules |
| `riscv32imfc-unknown-none-elf.json` | Custom target without the A extension |
| `build.rs` | Linker script setup and web page compression |
| `vendor/embedded-tls/` | Patched TLS 1.3 client |
| `tools/ram-split/` | One-off tool to set the 192 K / 128 K flash/RAM split (its own small Cargo project) |

---

## Limitations

- **MP3 only.** AAC/HE-AAC, Ogg Vorbis/Opus and FLAC are detected from `Content-Type` and refused with a message. AAC decoding wouldn't fit alongside TLS in the remaining RAM.
- **TLS 1.3 only, P-256 only, AES-128-GCM only.** These are the mandatory-to-implement TLS 1.3 options, so most servers are fine, but TLS 1.2-only servers fail with "the server refused the secure connection".
- **No certificate verification** (see [TLS](#tls-https)).
- **Records over 2.5 KB:** servers that ignore max_fragment_length (e.g. BoringSSL, so Cloudflare-fronted streams) fail as soon as they send a bigger record: "TLS records are too big … try http://".
- **Handshake stutter:** the display pauses for ~0.1 s, twice, while an https station connects.
- **10 Mbit Ethernet only**, IPv4 only, DHCP only.
- **No authentication on the web interface or API.** Anyone on the same network, or a web page open in a browser on it, can change stations and presets. Keep the radio on a trusted network.
- **Two web connections at a time.** More simultaneous connections are refused (RST) rather than queued.
- **URLs:** up to 256 bytes for playing, and 212 for presets.

---

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| No network link, no IP on screen | Use a 10/100/1000 switch port (not multi-gig). Check the RJ45 link LED. |
| Network dies after using the debugger | Any `wlink` attach (`status`, `regs`, `dump`) reconfigures the clocks and stops the PHY's clock. Reset the board. Use `/stats` (diag build) for health checks instead of the probe. |
| Stutter | In a `--features diag` build, look at `DAC queue low-water` and `audio loop max gap` in `/stats`. Gaps over ~60 ms mean something is blocking the executor. |
| "HTTP status 4xx/5xx", "web page, not an audio stream" | The URL isn't a direct stream. Try the station's `.pls`/`.m3u` link. |
| https station fails | See the message on the web page. In a diag build, `/stats` shows `tls … last error <n>`; the codes are listed in `tls_error_code` in `radio.rs`. Try the station's http:// URL. |
| Random crashes after a change | Check stack headroom in `/stats`. The panic message is captured and printed over the debug channel. |
| Presets or remembered station gone after reflashing | Only happens with `wlink flash -e` (chip erase). |

---

## Lessons learned / gotchas

These notes cover CH32V307 quirks that cost time and aren't obvious from the datasheet:

1. **QingKe V4 atomics:** avoid the A extension, and use `portable-atomic` with `critical-section`.
2. **Flash erases to `0xE339`, not `0xFF`:** after erasing option bytes, every option halfword must be rewritten (see `ram-split`).
3. **Slow flash is slow:** hot loops (crypto, DSP) must live in the zero-wait region. AES-GCM from slow flash took ~25 µs per byte and stalled audio for 100 ms per record. P-256 was ~6× slower there, enough to overflow the Ethernet RX ring during the handshake.
4. **Never use `hal::delay::Delay` alongside Embassy:** it reconfigures SysTick, which is Embassy's time base.
5. **ch32-hal Ethernet:** clear RBUS/AIS and DMASR bit 31 in your ETH interrupt handler, use more RX descriptors than you think, and keep the TX count even.
6. **SSD1322 dimming:** the pre-charge voltage and period set a brightness floor. Lowering the current alone doesn't make it darker.
7. **Ring buffers shared with DMA:** track monotonic totals, not wrapped indices.
8. **Long executor blocks lose packets:** while nothing polls the network stack, the Ethernet RX ring fills and frames are dropped. TCP recovers, but sometimes only after seconds. Keep every blocking computation well under ~100 ms.
9. **`embedded-tls`** needs a 16 KB+ read buffer unless the server honours max_fragment_length. It also can't reassemble handshake messages split across records (patched here for `Certificate`).
