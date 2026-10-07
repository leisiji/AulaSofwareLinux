//! The AULA Hall-effect keyboard protocol (vendor usage page `0xFFA0`).
//!
//! A different generation from the mechanical BYCOMBO4 family in
//! [`crate::proto`]: it talks over 64-byte HID **output** reports with no
//! report id, and answers on **input** reports, not feature reports.
//!
//! Recovered from AULA's own web driver (`win.aulacn.com`, mirrored as
//! `caioalonso/win-68-he-tool`) and confirmed on an AULA WIN 60 HE PRO
//! (USB `1ca2:1902`, firmware `App V1.1.6`, protocol `1.0.9`).
//!
//! ```text
//! byte 0      0x5C, the frame header
//! byte 1      payload length, counting bytes 4 onwards
//! byte 2      command
//! byte 3      checksum = 0x35 + byte0 + byte1 + byte2 + last payload byte
//! byte 4..    payload
//! ```
//!
//! A reply sets bit 7 on the command byte. For the main command (`0x00`), the
//! requested order is repeated at byte 5 and data starts at byte 6.

use std::io;
use std::time::Duration;

use crate::hid::{HidDevice, HidNode};

/// Vendor usage page of the Hall-effect family.
pub const USAGE_PAGE: u16 = 0xFFA0;
/// Usage of the vendor collection inside that page.
pub const USAGE: u16 = 0x01;
/// Report size in bytes, without a report id.
pub const REPORT_LEN: usize = 64;
/// First byte of every frame.
pub const HEAD: u8 = 0x5C;
/// OR'd into the command byte of a reply.
pub const REPLY: u8 = 0x80;
/// Base of the checksum.
const CHECKSUM_SEED: u8 = 0x35;
/// How long to wait for a reply before giving up.
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

/// Command bytes (`ProtocolCMD` in the vendor driver).
pub mod cmd {
    /// The main command; the real opcode is the first payload byte.
    pub const KB2_CMD: u8 = 0x00;
    /// Handshake sent once after opening the device.
    pub const KB2_CMD_SYNC: u8 = 0x01;
    /// Keyboard lighting page.
    pub const PRGB: u8 = 0x18;
    /// Per-key custom colours.
    pub const KRGB: u8 = 0x2A;
    /// Base key matrix (which HID usage each physical key sends).
    pub const DEFKEY: u8 = 0x2B;
}

/// Opcodes carried at byte 4 of a [`cmd::KB2_CMD`] frame (`CMDOrder`).
pub mod order {
    /// Protocol version, e.g. `1.0.9`.
    pub const PROTOCOL_VERSION: u8 = 1;
    /// Hall-effect precision and travel limits.
    pub const QUERY_PRECISION: u8 = 37;
    /// The model name, e.g. `WIN 60 HE PRO`.
    pub const QUERY_KEYBOARD_NAME: u8 = 38;
    /// Polling rate.
    pub const RATE_OF_RETURN: u8 = 80;
    /// Active onboard configuration (profile).
    pub const CONFIG_ID: u8 = 112;
}

/// Checksum over a complete frame.
pub fn checksum(frame: &[u8]) -> u8 {
    let len = frame[1] as usize;
    let last = if len > 0 && len + 3 < frame.len() {
        frame[len + 3]
    } else {
        0
    };
    CHECKSUM_SEED
        .wrapping_add(frame[0])
        .wrapping_add(frame[1])
        .wrapping_add(frame[2])
        .wrapping_add(last)
}

/// `CMDPack(order, arg)` — a main-command frame.
pub fn command(order: u8, arg: Option<u8>) -> Vec<u8> {
    let mut f = vec![0u8; REPORT_LEN];
    f[0] = HEAD;
    f[2] = cmd::KB2_CMD;
    let mut n = 4;
    f[n] = order;
    n += 1;
    if let Some(a) = arg {
        f[n] = a;
        n += 1;
    }
    f[n] = 0xFF;
    n += 1;
    f[n] = 0xFF;
    n += 1;
    f[1] = (n - 4) as u8;
    f[3] = checksum(&f);
    f
}

/// `SYNCPack()` — the handshake the vendor driver sends on connect.
pub fn sync() -> Vec<u8> {
    let mut f = vec![0u8; REPORT_LEN];
    f[0] = HEAD;
    f[2] = cmd::KB2_CMD_SYNC;
    let mut n = 4;
    for b in [1u8, 2, 3, 4, 0xFF, 0xFF] {
        f[n] = b;
        n += 1;
    }
    f[1] = (n - 4) as u8;
    f[3] = checksum(&f);
    f
}

/// Does this node carry the Hall-effect vendor channel?
///
/// Recognised by the shape of the collection rather than the USB id, so a
/// rebrand on the same silicon is still found.
pub fn is_he(node: &HidNode) -> bool {
    node.collections.iter().any(|c| {
        c.usage_page == USAGE_PAGE
            && c.usage == USAGE
            && c.inputs.iter().any(|(_, len)| *len == REPORT_LEN)
            && c.outputs.iter().any(|(_, len)| *len == REPORT_LEN)
    })
}

/// The first attached Hall-effect keyboard, if any.
pub fn find() -> Option<HidNode> {
    crate::hid::enumerate().into_iter().find(is_he)
}

/// An open Hall-effect keyboard.
pub struct Link {
    dev: HidDevice,
}

impl Link {
    pub fn open(node: &HidNode) -> anyhow::Result<Link> {
        let dev = HidDevice::open(node)
            .map_err(|e| anyhow::anyhow!("{}", crate::hid::permission_hint(&node.dev_path, &e)))?;
        Ok(Link { dev })
    }

    pub fn node_path(&self) -> String {
        self.dev.node.dev_path.display().to_string()
    }

    /// Send one 64-byte frame. Writes nothing back, so callers must only pass
    /// frames they have a reason to send.
    pub fn send(&self, frame: &[u8]) -> io::Result<()> {
        debug_assert_eq!(frame.len(), REPORT_LEN);
        self.dev.write_output(frame).map(|_| ())
    }

    /// Send a request and wait for the matching reply.
    ///
    /// Replies that do not answer this request (asynchronous reports are
    /// possible) are skipped until the timeout runs out.
    pub fn request(&self, frame: &[u8]) -> io::Result<Vec<u8>> {
        self.send(frame)?;
        let deadline = std::time::Instant::now() + REPLY_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply"));
            }
            let mut buf = vec![0u8; REPORT_LEN];
            match self.dev.read_report(&mut buf, left)? {
                Some(n) => {
                    buf.truncate(n);
                    if answers(&buf, frame) {
                        return Ok(buf);
                    }
                }
                None => return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply")),
            }
        }
    }

    /// Read the active onboard configuration (profile) id, `0..=3`.
    ///
    /// The keyboard re-emits its whole state after a switch, so anything still
    /// buffered is discarded first; otherwise a stale reply reads as the old id.
    pub fn config_id(&self) -> anyhow::Result<u8> {
        self.drain();
        let reply = self.request(&command(order::CONFIG_ID, None))?;
        parse_config_id(&reply)
            .ok_or_else(|| anyhow::anyhow!("the device did not answer the config query"))
    }

    /// Select an onboard configuration (profile) and wait for the keyboard to
    /// confirm it. Returns the id the keyboard reports back.
    pub fn set_config_id(&self, id: u8) -> anyhow::Result<u8> {
        if id > 3 {
            anyhow::bail!("config id must be 0-3, got {id}");
        }
        self.drain();
        self.send(&command(order::CONFIG_ID, Some(id)))?;
        self.wait_config(id)
    }

    /// Wait for the config acknowledgement carrying `want`.
    fn wait_config(&self, want: u8) -> anyhow::Result<u8> {
        use std::time::Instant;
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                anyhow::bail!("the keyboard did not confirm the config switch to {want}");
            }
            let mut buf = vec![0u8; REPORT_LEN];
            match self.dev.read_report(&mut buf, left)? {
                Some(n) => {
                    buf.truncate(n);
                    if parse_config_id(&buf) == Some(want) {
                        return Ok(want);
                    }
                }
                None => anyhow::bail!("no reply to the config switch"),
            }
        }
    }

    /// Read and discard any reports the keyboard has queued up.
    pub fn drain(&self) {
        use std::time::Instant;
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut buf = vec![0u8; REPORT_LEN];
        while Instant::now() < deadline {
            match self.dev.read_report(&mut buf, Duration::from_millis(20)) {
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
    }

    /// Read the base key matrix: which HID usage each physical key sends.
    ///
    /// The matrix is 6 rows of 21 columns; the vendor driver reads it two rows
    /// at a time. Only populated slots are returned.
    pub fn keymap(&self) -> anyhow::Result<Vec<KeySlot>> {
        let mut slots = Vec::new();
        for (a, b) in [(0u8, 1u8), (2, 3), (4, 5)] {
            let reply = self.request(&defkey(a, b))?;
            if reply.len() < 49 {
                anyhow::bail!("short key matrix reply ({} bytes)", reply.len());
            }
            for off in [5usize, 27] {
                let row = reply[off];
                for col in 0..21u8 {
                    let value = reply[off + 1 + col as usize];
                    if value != 0 {
                        slots.push(KeySlot { row, col, value });
                    }
                }
            }
        }
        Ok(slots)
    }

    /// Read the per-key custom colours for `keys` (HID usage codes).
    pub fn key_colors(&self, keys: &[u8]) -> anyhow::Result<Vec<(u8, [u8; 3])>> {
        let mut out = Vec::new();
        for chunk in keys.chunks(KRGB_BATCH) {
            let entries: Vec<(u8, [u8; 3])> = chunk.iter().map(|k| (*k, [0, 0, 0])).collect();
            let reply = self.request(&krgb(0, &entries))?;
            for j in 0..KRGB_BATCH {
                let base = 5 + j * 4;
                if base + 3 >= reply.len() {
                    break;
                }
                let key = reply[base];
                if key != 0 && chunk.contains(&key) {
                    out.push((key, [reply[base + 1], reply[base + 2], reply[base + 3]]));
                }
            }
        }
        Ok(out)
    }

    /// Write per-key custom colours. Only visible when the lighting mode is
    /// the custom (per-key) one, but the values are stored either way.
    pub fn set_key_colors(&self, entries: &[(u8, [u8; 3])]) -> anyhow::Result<()> {
        for chunk in entries.chunks(KRGB_BATCH) {
            self.send(&krgb(1, chunk))?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Read the identity, limits and capabilities the driver shows on connect.
    ///
    /// Read-only: sends only `SYNC` and query orders.
    pub fn info(&self) -> anyhow::Result<Info> {
        let sync = self.request(&sync())?;
        let version = self.request(&command(order::PROTOCOL_VERSION, None))?;
        let name = self.request(&command(order::QUERY_KEYBOARD_NAME, None))?;
        let precision = self.request(&command(order::QUERY_PRECISION, None))?;
        let rate = self.request(&command(order::RATE_OF_RETURN, None))?;

        let sync = parse_sync(&sync).unwrap_or_default();
        Ok(Info {
            name: parse_name(&name).unwrap_or_default(),
            protocol: parse_version(&version).unwrap_or_default(),
            precision_mm: parse_precision(&precision).map(|p| p.0).unwrap_or(0.0),
            min_travel_mm: parse_precision(&precision).map(|p| p.1).unwrap_or(0.0),
            max_travel_mm: parse_precision(&precision).map(|p| p.2).unwrap_or(0.0),
            polling: parse_polling(&rate).unwrap_or(0),
            ..sync
        })
    }

    /// Read the keyboard lighting page.
    pub fn lighting(&self) -> anyhow::Result<Lighting> {
        let reply = self.request(&prgb(0, &Lighting::default()))?;
        parse_lighting(&reply)
            .ok_or_else(|| anyhow::anyhow!("the device did not answer the lighting query"))
    }

    /// Write the lighting page, then read it back and compare.
    ///
    /// The current page is kept first; if the write does not stick it is put
    /// back and an error is returned, so a bad write leaves no surprise
    /// behind.
    pub fn set_lighting(&self, want: &Lighting) -> anyhow::Result<Lighting> {
        let before = self.lighting()?;
        self.send(&prgb(1, want))?;
        std::thread::sleep(Duration::from_millis(50));
        let after = self.lighting()?;
        if after != *want {
            // Put the old page back, best effort, before reporting.
            let _ = self.send(&prgb(1, &before));
            anyhow::bail!(
                "lighting write did not stick; read back {after:?}, wanted {want:?} \
                 (previous page restored)"
            );
        }
        Ok(after)
    }
}

/// How many keys a single `KRGB` frame carries.
pub const KRGB_BATCH: usize = 14;

/// One populated slot of the base key matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySlot {
    pub row: u8,
    pub col: u8,
    /// The HID usage code the key sends.
    pub value: u8,
}

/// `DEFKEYPack(row_a, row_b)` — reads two rows of the base key matrix.
pub fn defkey(row_a: u8, row_b: u8) -> Vec<u8> {
    let mut f = vec![0u8; REPORT_LEN];
    f[0] = HEAD;
    f[2] = cmd::DEFKEY;
    f[4] = 0;
    f[5] = row_a;
    f[6] = row_b;
    f[1] = 3;
    f[3] = checksum(&f);
    f
}

/// `KRGBPack(e, …)`. `e` is `0` to read the given keys, `1` to write them.
///
/// Each entry is `(HID usage, [R, G, B])`; a read ignores the colour bytes.
pub fn krgb(e: u8, entries: &[(u8, [u8; 3])]) -> Vec<u8> {
    let mut f = vec![0xFFu8; REPORT_LEN];
    f[0] = HEAD;
    f[2] = cmd::KRGB;
    let mut n = 4;
    f[n] = e;
    n += 1;
    for (key, c) in entries {
        f[n] = *key;
        f[n + 1] = c[0];
        f[n + 2] = c[1];
        f[n + 3] = c[2];
        n += 4;
    }
    f[1] = (n - 4) as u8;
    f[3] = checksum(&f);
    f
}

/// The keyboard lighting page (`PRGB`, command `0x18`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lighting {
    pub on: bool,
    /// Direction flag for the effects that animate.
    pub direction: bool,
    /// Super-response flag.
    pub super_response: bool,
    /// Raw firmware effect id.
    pub mode: u8,
    pub brightness: u8,
    pub speed: u8,
    /// Sleep timeout in raw units; `0` means never.
    pub sleep: u8,
    /// Sub-mode the static effect uses.
    pub static_mode: u8,
    /// The seven colours the effect cycles through, RGB order.
    pub colors: [[u8; 3]; 7],
}

impl Default for Lighting {
    /// The seven colours the vendor driver ships, so a write that only
    /// changes the mode does not blank the palette.
    fn default() -> Self {
        Self {
            on: false,
            direction: false,
            super_response: false,
            mode: 0,
            brightness: 4,
            speed: 0,
            sleep: 0,
            static_mode: 0,
            colors: [
                [0xff, 0x00, 0x00],
                [0x00, 0xff, 0x00],
                [0x7f, 0x7f, 0x00],
                [0x00, 0x00, 0xff],
                [0x7f, 0x00, 0x7f],
                [0x00, 0x7f, 0x7f],
                [0x7f, 0x7f, 0x7f],
            ],
        }
    }
}

/// `PRGBPack()`. `e` is `0` to read the page, `1` to write it.
pub fn prgb(e: u8, l: &Lighting) -> Vec<u8> {
    let mut f = vec![0u8; REPORT_LEN];
    f[0] = HEAD;
    f[2] = cmd::PRGB;
    let mut n = 4;
    f[n] = e;
    n += 1;
    n += 4; // reserved
    for c in &l.colors {
        // The device stores blue, green, red, then a 0xFF separator.
        f[n] = c[2];
        f[n + 1] = c[1];
        f[n + 2] = c[0];
        f[n + 3] = 0xFF;
        n += 4;
    }
    n += 4; // reserved
    f[n] = lighting_bitmap(l);
    n += 1;
    f[n] = l.brightness;
    n += 1;
    f[n] = l.mode;
    n += 1;
    f[n] = l.speed;
    n += 1;
    f[n] = l.sleep;
    n += 1;
    f[n] = l.static_mode;
    n += 1;
    f[1] = (n - 4) as u8;
    f[3] = checksum(&f);
    f
}

/// Bits the vendor driver sets in the lighting flag byte.
fn lighting_bitmap(l: &Lighting) -> u8 {
    (l.on as u8) | ((l.direction as u8) << 1) | ((l.super_response as u8) << 4)
}

fn parse_lighting(frame: &[u8]) -> Option<Lighting> {
    if frame.len() < 47 || frame[0] != HEAD || frame[2] != cmd::PRGB | REPLY {
        return None;
    }
    let mut colors = [[0u8; 3]; 7];
    let mut n = 9; // first colour byte
    for c in colors.iter_mut() {
        // Wire order is blue, green, red.
        let (b, g, r) = (frame[n], frame[n + 1], frame[n + 2]);
        *c = [r, g, b];
        n += 4;
    }
    let bitmap = frame[41];
    Some(Lighting {
        on: bitmap & 0x01 != 0,
        direction: bitmap & 0x02 != 0,
        super_response: bitmap & 0x10 != 0,
        brightness: frame[42],
        mode: frame[43],
        speed: frame[44],
        sleep: frame[45],
        static_mode: frame[46],
        colors,
    })
}

/// Everything `aula he-info` prints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Info {
    pub name: String,
    pub protocol: String,
    pub firmware: String,
    pub serial: String,
    pub board_id: u32,
    pub key_type: u8,
    pub layout: u8,
    pub run_mode: u8,
    /// Reported precision, travel limits in millimetres.
    pub precision_mm: f32,
    pub min_travel_mm: f32,
    pub max_travel_mm: f32,
    /// Raw polling-rate value from the device.
    pub polling: u8,
}

/// Does `frame` answer `request`?
fn answers(frame: &[u8], request: &[u8]) -> bool {
    if frame.len() < 6 || frame[0] != HEAD {
        return false;
    }
    if frame[2] != request[2] | REPLY {
        return false;
    }
    // The main command echoes the order; SYNC does not.
    request[2] != cmd::KB2_CMD || frame[5] == request[4]
}

/// If `frame` is a main-command reply, return its order and data (byte 6 on).
fn reply(frame: &[u8]) -> Option<(u8, &[u8])> {
    if frame.len() < 6 || frame[0] != HEAD || frame[2] != cmd::KB2_CMD | REPLY {
        return None;
    }
    Some((frame[5], &frame[6..]))
}

fn parse_name(frame: &[u8]) -> Option<String> {
    let (order, data) = reply(frame)?;
    if order != order::QUERY_KEYBOARD_NAME {
        return None;
    }
    Some(ascii(
        data.iter().take_while(|b| **b != 0).copied().collect(),
    ))
}

fn parse_version(frame: &[u8]) -> Option<String> {
    let (order, data) = reply(frame)?;
    if order != order::PROTOCOL_VERSION || data.len() < 2 {
        return None;
    }
    // The driver builds it as patch.minor.major from the two bytes.
    Some(format!(
        "{}.{}.{}",
        data[1] & 0x0F,
        data[0] >> 4,
        data[0] & 0x0F
    ))
}

fn parse_precision(frame: &[u8]) -> Option<(f32, f32, f32)> {
    let (order, data) = reply(frame)?;
    if order != order::QUERY_PRECISION || data.len() < 5 {
        return None;
    }
    let mm = |v: u16| v as f32 / 1000.0;
    let precision = mm(data[0] as u16);
    let min = mm(u16::from(data[1]) | u16::from(data[2]) << 8);
    let max = mm(u16::from(data[3]) | u16::from(data[4]) << 8);
    Some((precision, min, max))
}

fn parse_polling(frame: &[u8]) -> Option<u8> {
    let (order, data) = reply(frame)?;
    if order != order::RATE_OF_RETURN {
        return None;
    }
    data.first().copied()
}

fn parse_config_id(frame: &[u8]) -> Option<u8> {
    let (order, data) = reply(frame)?;
    if order != order::CONFIG_ID {
        return None;
    }
    data.first().copied()
}

fn parse_sync(frame: &[u8]) -> Option<Info> {
    if frame.len() < 40 || frame[0] != HEAD || frame[2] != cmd::KB2_CMD_SYNC | REPLY {
        return None;
    }
    let firmware = if &frame[30..34] == b"Boot" {
        ascii(frame[30..41].to_vec())
    } else {
        ascii(frame[30..40].to_vec())
    };
    Some(Info {
        firmware,
        serial: ascii(frame[13..29].to_vec()),
        board_id: u32::from_le_bytes([frame[5], frame[6], frame[7], frame[8]]),
        key_type: frame[7],
        layout: frame[8],
        run_mode: frame[11],
        ..Info::default()
    })
}

/// Decode a fixed-width ASCII field, dropping trailing NULs and whitespace.
fn ascii(bytes: Vec<u8>) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_frame_matches_the_driver() {
        let f = command(order::QUERY_KEYBOARD_NAME, None);
        assert_eq!(f.len(), 64);
        assert_eq!(
            &f[..8],
            &[0x5C, 0x03, 0x00, 0x93, 0x26, 0xFF, 0xFF, 0x00],
            "captured request: 5c 03 00 93 26 ff ff"
        );
    }

    #[test]
    fn command_with_argument_grows_the_length() {
        let f = command(0x01, Some(0xAB));
        assert_eq!(f[1], 4);
        assert_eq!(&f[4..8], &[0x01, 0xAB, 0xFF, 0xFF]);
    }

    #[test]
    fn sync_frame_matches_the_driver() {
        let f = sync();
        assert_eq!(
            &f[..10],
            &[0x5C, 0x06, 0x01, 0x97, 0x01, 0x02, 0x03, 0x04, 0xFF, 0xFF],
            "captured sync: 5c 06 01 97 01 02 03 04 ff ff"
        );
    }

    /// The reply an AULA WIN 60 HE PRO gave to `QUERY_KEYBOARD_NAME`.
    fn name_reply() -> Vec<u8> {
        let mut f = vec![0u8; 64];
        f[0] = HEAD;
        f[1] = 0x22;
        f[2] = cmd::KB2_CMD | REPLY;
        f[3] = 0x33;
        f[4] = 0x00;
        f[5] = order::QUERY_KEYBOARD_NAME;
        f[6..19].copy_from_slice(b"WIN 60 HE PRO");
        f
    }

    #[test]
    fn reads_the_model_name() {
        assert_eq!(parse_name(&name_reply()).as_deref(), Some("WIN 60 HE PRO"));
    }

    #[test]
    fn reads_protocol_version() {
        // captured: 5c 05 80 15 00 01 09 01 -> 1.0.9
        let mut f = vec![0u8; 64];
        f[0] = HEAD;
        f[1] = 0x05;
        f[2] = cmd::KB2_CMD | REPLY;
        f[5] = order::PROTOCOL_VERSION;
        f[6] = 0x09;
        f[7] = 0x01;
        assert_eq!(parse_version(&f).as_deref(), Some("1.0.9"));
    }

    #[test]
    fn reads_precision_and_travel() {
        // captured reply: 5c 07 80 25 00 25 14 14 00 48 0d
        let mut f = vec![0u8; 64];
        f[0] = HEAD;
        f[1] = 0x07;
        f[2] = cmd::KB2_CMD | REPLY;
        f[5] = order::QUERY_PRECISION;
        f[6] = 0x14; // 0.02 mm
        f[7] = 0x14; // min low
        f[8] = 0x00; // min high -> 0.02 mm
        f[9] = 0x48; // max low
        f[10] = 0x0D; // max high -> 3400 -> 3.40 mm
        let (p, min, max) = parse_precision(&f).unwrap();
        assert!((p - 0.02).abs() < f32::EPSILON);
        assert!((min - 0.02).abs() < f32::EPSILON);
        assert!((max - 3.40).abs() < 1e-6);
    }

    #[test]
    fn reads_the_sync_reply() {
        // captured SYNC reply from the WIN 60 HE PRO.
        let mut f = vec![0u8; 64];
        f[0] = HEAD;
        f[1] = 0x3C;
        f[2] = cmd::KB2_CMD_SYNC | REPLY;
        f[3] = 0x4D;
        f[4] = 0x00;
        f[5..9].copy_from_slice(&[0x02, 0x19, 0x02, 0x0a]);
        f[9] = 0xc0;
        f[10] = 0x01;
        f[13..29].copy_from_slice(b"8148144509091415");
        f[30..40].copy_from_slice(b"App V1.1.6");
        let s = parse_sync(&f).unwrap();
        assert_eq!(s.board_id, 0x0a021902);
        assert_eq!(s.layout, 0x0a);
        assert_eq!(s.key_type, 0x02);
        assert_eq!(s.serial, "8148144509091415");
        assert_eq!(s.firmware, "App V1.1.6");
    }

    #[test]
    fn ignores_replies_to_other_orders() {
        let f = name_reply();
        assert!(parse_version(&f).is_none());
        assert!(parse_name(&f).is_some());
    }

    #[test]
    fn reads_the_config_id() {
        // captured reply: 5c 04 80 14 00 70 00 ff
        let mut f = vec![0u8; 64];
        f[0] = HEAD;
        f[1] = 0x04;
        f[2] = cmd::KB2_CMD | REPLY;
        f[5] = order::CONFIG_ID;
        f[6] = 0x02;
        f[7] = 0xff;
        assert_eq!(parse_config_id(&f), Some(2));
        // A read reply for a different order must not be mistaken for it.
        assert_eq!(parse_config_id(&name_reply()), None);
    }

    #[test]
    fn defkey_frame_matches_the_driver() {
        let f = defkey(0, 1);
        assert_eq!(f[2], cmd::DEFKEY);
        assert_eq!(&f[4..7], &[0x00, 0x00, 0x01]);
        assert_eq!(f[1], 3);
        assert_eq!(f[3], checksum(&f));
        assert_eq!(f[3], 0xC0);
    }

    #[test]
    fn krgb_frame_matches_the_driver() {
        let f = krgb(1, &[(0x04, [0x11, 0x22, 0x33])]);
        assert_eq!(f[2], cmd::KRGB);
        assert_eq!(f[4], 1, "write sub-command");
        assert_eq!(&f[5..9], &[0x04, 0x11, 0x22, 0x33]);
        assert_eq!(f[1], 5);
        assert_eq!(f[3], checksum(&f));
    }

    #[test]
    fn checksum_covers_the_last_payload_byte() {
        let f = command(order::QUERY_KEYBOARD_NAME, None);
        assert_eq!(f[3], checksum(&f));
        assert_eq!(f[3], 0x93);
    }

    /// Captured `PRGB` reply from the WIN 60 HE PRO: super-response on,
    /// mode 0, brightness 3, sleep 2, static 7, the vendor default palette.
    const PRGB_REPLY: &[u8] = &[
        0x5c, 0x2d, 0x98, 0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0x00, 0xff,
        0x00, 0xff, 0x00, 0x7f, 0x7f, 0xff, 0xff, 0x00, 0x00, 0xff, 0x7f, 0x00, 0x7f, 0xff, 0x7f,
        0x7f, 0x00, 0xff, 0x7f, 0x7f, 0x7f, 0xff, 0x00, 0x00, 0x00, 0x00, 0xd0, 0x03, 0x00, 0x00,
        0x02, 0x07, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn reads_the_lighting_reply() {
        let l = parse_lighting(PRGB_REPLY).expect("the captured reply should parse");
        assert!(!l.on);
        assert!(!l.direction);
        assert!(l.super_response, "bit 4 of 0xd0");
        assert_eq!(l.mode, 0);
        assert_eq!(l.brightness, 3);
        assert_eq!(l.speed, 0);
        assert_eq!(l.sleep, 2);
        assert_eq!(l.static_mode, 7);
        assert_eq!(l.colors[0], [0xff, 0x00, 0x00]);
        assert_eq!(l.colors[1], [0x00, 0xff, 0x00]);
        assert_eq!(l.colors[6], [0x7f, 0x7f, 0x7f]);
    }

    #[test]
    fn builds_a_lighting_write_frame() {
        let l = Lighting {
            on: true,
            super_response: true,
            mode: 5,
            brightness: 2,
            speed: 3,
            sleep: 1,
            static_mode: 7,
            ..Lighting::default()
        };
        let f = prgb(1, &l);
        assert_eq!(f[2], cmd::PRGB);
        assert_eq!(f[4], 1, "write sub-command");
        assert_eq!(&f[5..9], &[0, 0, 0, 0]);
        // Colours go on the wire blue, green, red, 0xFF.
        assert_eq!(&f[9..13], &[0x00, 0x00, 0xff, 0xff]);
        assert_eq!(f[41], 0x11, "on | super-response");
        assert_eq!(f[42], 2);
        assert_eq!(f[43], 5);
        assert_eq!(f[44], 3);
        assert_eq!(f[45], 1);
        assert_eq!(f[46], 7);
        assert_eq!(f[1], 43);
        assert_eq!(f[3], checksum(&f));
    }

    #[test]
    fn lighting_write_round_trips_through_the_parser() {
        let l = Lighting {
            on: true,
            direction: true,
            super_response: false,
            mode: 6,
            brightness: 4,
            speed: 1,
            sleep: 0,
            static_mode: 3,
            ..Lighting::default()
        };
        let mut reply = prgb(1, &l);
        reply[2] |= REPLY;
        reply[3] = checksum(&reply);
        assert_eq!(parse_lighting(&reply), Some(l));
    }
}
