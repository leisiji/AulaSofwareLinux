//! The AULA / BYCOMBO4 vendor HID protocol.
//!
//! Recovered from `OemDrv.exe` (the driver shipped with every AULA keyboard
//! package; internally `CDevG5KB`). Everything travels over HID feature
//! reports on the vendor usage page:
//!
//! ```text
//! byte 0      HID report id      6 when the firmware id is 0x18, else 9
//! byte 1      command            high bit set = read
//! byte 2      parameter          layer index / sub-selector, command specific
//! byte 3      0
//! byte 4      total packages     ceil(len / 512)
//! byte 5      package index      0-based
//! byte 6..7   payload length     little endian, <= 512
//! byte 8..    payload            padded with zeroes
//! ```
//!
//! The whole report is always transferred at its declared size (520 bytes for
//! this family), regardless of how much payload a package carries. Transfers
//! larger than 512 bytes are split into consecutively numbered packages.
//!
//! A read is a write of the request header followed by a `GET_FEATURE` on the
//! same report id; the reply repeats the header and carries the data from
//! byte 8 onwards.

use std::io;
use std::time::Duration;

use crate::hid::HidDevice;

/// Payload bytes per package.
pub const PAGE: usize = 512;
/// Header bytes in front of the payload, including the report id.
pub const HEADER: usize = 8;
/// Full feature report size for this device family.
pub const REPORT_LEN: usize = HEADER + PAGE;

/// Delay the vendor driver inserts around every transfer.
pub const DEFAULT_DELAY: Duration = Duration::from_millis(20);
/// Delay before retrying a failed transfer.
const RETRY_DELAY: Duration = Duration::from_millis(70);
const RETRIES: u32 = 3;

/// Commands, as issued by `CDevG5KB`.
///
/// Writes and reads are separate opcodes; for the bulk data commands the read
/// opcode is the write opcode with bit 7 set, but the identity and battery
/// reads have their own numbers, so they are all listed explicitly.
pub mod cmd {
    /// Write the key matrix for one layer (parameter = layer index).
    pub const SET_MATRIX: u8 = 0x03;
    /// Read the key matrix for one layer (parameter = layer index).
    pub const GET_MATRIX: u8 = 0x83;
    /// Write the lighting / configuration page.
    pub const SET_LED: u8 = 0x04;
    /// Read the lighting / configuration page.
    pub const GET_LED: u8 = 0x84;
    /// Write macro storage.
    pub const SET_MACRO: u8 = 0x05;
    /// Read macro storage.
    pub const GET_MACRO: u8 = 0x85;
    /// Write the "game mode" / locked-key table.
    pub const SET_GAME: u8 = 0x06;
    /// Read it back.
    pub const GET_GAME: u8 = 0x86;
    /// Write the per-key RGB table (3 bytes per LED).
    pub const SET_RGB_TABLE: u8 = 0x0a;
    /// Read the per-key RGB table.
    pub const GET_RGB_TABLE: u8 = 0x8a;
    /// Screen / display parameters (devices with a TFT).
    pub const SET_SCREEN: u8 = 0x0b;
    /// Reset the device to its factory configuration.
    pub const RESET: u8 = 0x11;

    /// Identity read; parameter 1 returns the 6-byte model password.
    pub const GET_INFO: u8 = 0x82;
    /// Parameter for [`GET_INFO`] that returns the model password.
    pub const INFO_PASSWORD: u8 = 0x01;
    /// Battery read; used with parameter 2.
    pub const GET_POWER: u8 = 0x87;
    /// Parameter for [`GET_POWER`].
    pub const POWER_PARAM: u8 = 0x02;
}

/// Builds one package header.
///
/// `payload` is copied in and the rest of the report is zero padded, which is
/// what the firmware expects — short reports are rejected.
pub fn frame(
    report_id: u8,
    command: u8,
    parameter: u8,
    packages: u8,
    index: u8,
    payload: &[u8],
) -> Vec<u8> {
    assert!(payload.len() <= PAGE, "payload must fit one package");
    let mut buf = vec![0u8; REPORT_LEN];
    let len = payload.len() as u16;
    buf[0] = report_id;
    buf[1] = command;
    buf[2] = parameter;
    buf[3] = 0;
    buf[4] = packages;
    buf[5] = index;
    buf[6] = len as u8;
    buf[7] = (len >> 8) as u8;
    buf[HEADER..HEADER + payload.len()].copy_from_slice(payload);
    buf
}

/// Number of packages a transfer of `len` bytes is split into.
pub fn package_count(len: usize) -> u8 {
    if len == 0 {
        0
    } else {
        len.div_ceil(PAGE) as u8
    }
}

/// A keyboard speaking this protocol.
pub struct Link {
    dev: HidDevice,
    report_id: u8,
    delay: Duration,
}

impl Link {
    /// `report_id` is normally 6; firmwares that do not identify as 0x18 use 9.
    /// [`report_id_for_firmware`] and the HID descriptor both tell you which.
    pub fn new(dev: HidDevice, report_id: u8) -> Link {
        Link {
            dev,
            report_id,
            delay: DEFAULT_DELAY,
        }
    }

    pub fn report_id(&self) -> u8 {
        self.report_id
    }

    /// `/dev/hidrawN` this link is bound to, for status messages.
    pub fn node_path(&self) -> String {
        self.dev.node.dev_path.display().to_string()
    }

    pub fn set_delay(&mut self, delay: Duration) {
        self.delay = delay;
    }

    /// Send `data`, split into packages, with the given command/parameter.
    pub fn write(&self, command: u8, parameter: u8, data: &[u8]) -> io::Result<()> {
        let packages = package_count(data.len());
        for (index, chunk) in data.chunks(PAGE).enumerate() {
            let buf = frame(
                self.report_id,
                command,
                parameter,
                packages,
                index as u8,
                chunk,
            );
            std::thread::sleep(self.delay);
            self.set_feature_retrying(&buf)?;
        }
        Ok(())
    }

    /// Request `len` bytes and return them.
    pub fn read(&self, command: u8, parameter: u8, len: usize) -> io::Result<Vec<u8>> {
        let packages = package_count(len);
        let mut out = Vec::with_capacity(len);
        for index in 0..packages {
            let chunk = (len - out.len()).min(PAGE);
            let request = frame(self.report_id, command, parameter, packages, index, &[]);
            // The length field describes the reply, so patch it in directly.
            let mut request = request;
            request[6] = chunk as u8;
            request[7] = (chunk >> 8) as u8;

            std::thread::sleep(self.delay);
            self.set_feature_retrying(&request)?;

            std::thread::sleep(self.delay);
            let mut reply = vec![0u8; REPORT_LEN];
            reply[0] = self.report_id;
            self.dev.get_feature(&mut reply)?;
            if reply[1] != command {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "device answered command 0x{:02x}, expected 0x{command:02x}",
                        reply[1]
                    ),
                ));
            }
            out.extend_from_slice(&reply[HEADER..HEADER + chunk]);
        }
        Ok(out)
    }

    fn set_feature_retrying(&self, buf: &[u8]) -> io::Result<()> {
        let mut last = None;
        for _ in 0..RETRIES {
            match self.dev.set_feature(buf) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("set_feature failed")))
    }

    /// The 6 identity bytes. Every model in this family shares USB
    /// 258a:010c, so this is what says *which* keyboard is attached; it is
    /// matched against `Psd=` in the device profile.
    pub fn password(&self) -> io::Result<[u8; 6]> {
        let v = self.read(cmd::GET_INFO, cmd::INFO_PASSWORD, 6)?;
        let mut out = [0u8; 6];
        out.copy_from_slice(&v[..6]);
        Ok(out)
    }

    /// Battery state, for the wireless models. The vendor driver logs two
    /// values here; the first is the charge percentage.
    pub fn power(&self) -> io::Result<Vec<u8>> {
        self.read(cmd::GET_POWER, cmd::POWER_PARAM, 2)
    }

    /// Read the lighting / configuration page.
    pub fn read_config(&self, len: usize) -> io::Result<Vec<u8>> {
        self.read(cmd::GET_LED, 0, len)
    }

    /// Write the lighting / configuration page.
    pub fn write_config(&self, page: &[u8]) -> io::Result<()> {
        self.write(cmd::SET_LED, 0, page)
    }

    /// Per-key colours, three bytes (R, G, B) per LED index.
    pub fn write_rgb_table(&self, rgb: &[u8]) -> io::Result<()> {
        self.write(cmd::SET_RGB_TABLE, 0, rgb)
    }

    pub fn read_rgb_table(&self, leds: usize) -> io::Result<Vec<u8>> {
        self.read(cmd::GET_RGB_TABLE, 0, leds * 3)
    }

    /// Key matrix for one Fn layer; 4 bytes per key slot.
    pub fn write_matrix(&self, layer: u8, matrix: &[u8]) -> io::Result<()> {
        self.write(cmd::SET_MATRIX, layer, matrix)
    }

    pub fn read_matrix(&self, layer: u8, slots: usize) -> io::Result<Vec<u8>> {
        self.read(cmd::GET_MATRIX, layer, slots * 4)
    }

    /// Restore the factory configuration.
    pub fn reset(&self) -> io::Result<()> {
        self.write(cmd::RESET, 0, &[1])
    }
}

/// The report id this firmware revision uses, per `CDevG5KB`.
///
/// `Fw=` in the device profile carries the same number (24 == 0x18 for every
/// currently shipped AULA keyboard).
pub fn report_id_for_firmware(fw: i64) -> u8 {
    if fw == 0x18 {
        6
    } else {
        9
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_matches_the_vendor_layout() {
        let f = frame(6, cmd::SET_LED, 0, 1, 0, &[0xaa, 0xbb]);
        assert_eq!(f.len(), 520);
        assert_eq!(&f[..8], &[6, 0x04, 0, 0, 1, 0, 2, 0]);
        assert_eq!(&f[8..10], &[0xaa, 0xbb]);
        assert!(f[10..].iter().all(|b| *b == 0), "tail must be zero padded");
    }

    #[test]
    fn password_request_matches_the_disassembly() {
        // OemDrv.exe builds exactly this: 06 82 01 00 01 00 06 00
        let f = frame(6, cmd::GET_INFO, cmd::INFO_PASSWORD, 1, 0, &[]);
        let mut f = f;
        f[6] = 6;
        assert_eq!(&f[..8], &[0x06, 0x82, 0x01, 0x00, 0x01, 0x00, 0x06, 0x00]);
    }

    #[test]
    fn splits_into_512_byte_packages() {
        assert_eq!(package_count(0), 0);
        assert_eq!(package_count(1), 1);
        assert_eq!(package_count(512), 1);
        assert_eq!(package_count(513), 2);
        assert_eq!(package_count(2048), 4);
    }

    #[test]
    fn last_package_carries_the_remainder() {
        let data: Vec<u8> = (0..600).map(|i| i as u8).collect();
        let packages = package_count(data.len());
        let chunks: Vec<&[u8]> = data.chunks(PAGE).collect();
        assert_eq!(packages, 2);
        let second = frame(6, cmd::SET_MATRIX, 1, packages, 1, chunks[1]);
        assert_eq!(second[4], 2, "total packages");
        assert_eq!(second[5], 1, "package index");
        assert_eq!(u16::from_le_bytes([second[6], second[7]]), 88);
    }

    #[test]
    fn report_id_follows_the_firmware_id() {
        assert_eq!(report_id_for_firmware(24), 6);
        assert_eq!(report_id_for_firmware(0x18), 6);
        assert_eq!(report_id_for_firmware(0x1a), 9);
    }
}
