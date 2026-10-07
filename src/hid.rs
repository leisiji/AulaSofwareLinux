//! Linux `hidraw` transport.
//!
//! The vendor driver talks to the keyboard with HID *feature* reports on a
//! vendor-defined usage page (0xFF00). On Linux that is `/dev/hidrawN` plus the
//! `HIDIOCGFEATURE` / `HIDIOCSFEATURE` ioctls — no libusb, no kernel module.
//!
//! A keyboard exposes several hidraw nodes (the boot keyboard, the consumer
//! controls, the vendor collection). Only one of them carries the feature
//! reports we need, so the descriptor of each candidate is parsed and the node
//! that actually declares them is selected.

use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

const HIDRAW_CLASS: &str = "/sys/class/hidraw";

// include/uapi/asm-generic/ioctl.h
const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;
const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS;
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS;
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS;
const IOC_READ_WRITE: u32 = 3; // _IOC_WRITE | _IOC_READ

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> libc::c_ulong {
    ((dir << IOC_DIRSHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT) | (size << IOC_SIZESHIFT))
        as libc::c_ulong
}

fn hidioc_set_feature(len: usize) -> libc::c_ulong {
    ioc(IOC_READ_WRITE, b'H' as u32, 0x06, len as u32)
}

fn hidioc_get_feature(len: usize) -> libc::c_ulong {
    ioc(IOC_READ_WRITE, b'H' as u32, 0x07, len as u32)
}

/// One `/dev/hidrawN` node together with what its descriptor declares.
#[derive(Debug, Clone)]
pub struct HidNode {
    pub dev_path: PathBuf,
    pub sys_path: PathBuf,
    pub vid: u16,
    pub pid: u16,
    pub name: String,
    /// `(report id, feature payload size in bytes)`, excluding the id byte.
    pub feature_reports: Vec<(u8, usize)>,
    /// Every usage collection the descriptor declares, with the report shapes
    /// inside it. The BYCOMBO4 family is found through `feature_reports`; the
    /// Hall-effect family is found through `collections`.
    pub collections: Vec<UsageCollection>,
}

/// A usage collection from a report descriptor, with the reports it declares.
///
/// The vendor channel of the Hall-effect generation is a plain application
/// collection (`Usage Page 0xFFA0`, `Usage 0x01`) carrying a 64-byte input and
/// a 64-byte output report, so it can be recognised by shape alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageCollection {
    pub usage_page: u16,
    pub usage: u16,
    /// `(report id, payload size in bytes)` for each report kind.
    pub inputs: Vec<(u8, usize)>,
    pub outputs: Vec<(u8, usize)>,
    pub features: Vec<(u8, usize)>,
}

impl HidNode {
    pub fn feature_len(&self, report_id: u8) -> Option<usize> {
        self.feature_reports
            .iter()
            .find(|(id, _)| *id == report_id)
            .map(|(_, len)| *len)
    }
}

/// Enumerate every hidraw node on the system.
pub fn enumerate() -> Vec<HidNode> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(HIDRAW_CLASS) else {
        return out;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for sys in entries {
        let Some(node) = read_node(&sys) else {
            continue;
        };
        out.push(node);
    }
    out
}

fn read_node(sys: &Path) -> Option<HidNode> {
    let name = sys.file_name()?.to_string_lossy().into_owned();
    let uevent = std::fs::read_to_string(sys.join("device/uevent")).ok()?;
    let mut vid = 0u16;
    let mut pid = 0u16;
    let mut hid_name = String::new();
    for line in uevent.lines() {
        if let Some(v) = line.strip_prefix("HID_ID=") {
            // bus:vendor:product, each zero-padded hex
            let mut it = v.split(':');
            it.next();
            vid = it
                .next()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .unwrap_or(0) as u16;
            pid = it
                .next()
                .and_then(|s| u32::from_str_radix(s, 16).ok())
                .unwrap_or(0) as u16;
        } else if let Some(v) = line.strip_prefix("HID_NAME=") {
            hid_name = v.to_string();
        }
    }
    let desc = std::fs::read(sys.join("device/report_descriptor")).unwrap_or_default();
    Some(HidNode {
        dev_path: PathBuf::from("/dev").join(&name),
        sys_path: sys.to_path_buf(),
        vid,
        pid,
        name: hid_name,
        feature_reports: feature_reports(&desc),
        collections: usage_collections(&desc),
    })
}

/// Walk a HID report descriptor and return the declared feature report sizes.
///
/// Only the items that matter here are tracked: report id, report size (bits
/// per field), report count (number of fields) and the Feature main item.
pub fn feature_reports(desc: &[u8]) -> Vec<(u8, usize)> {
    let mut out: Vec<(u8, usize)> = Vec::new();
    let mut report_id: u8 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut bits: Vec<(u8, u64)> = Vec::new();

    let mut i = 0usize;
    while i < desc.len() {
        let head = desc[i];
        i += 1;
        if head == 0xfe {
            // Long item: [len][tag][data...]
            let len = *desc.get(i).unwrap_or(&0) as usize;
            i += 2 + len;
            continue;
        }
        let size = match head & 0x03 {
            0 => 0usize,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if i + size > desc.len() {
            break;
        }
        let mut data: u32 = 0;
        for (n, b) in desc[i..i + size].iter().enumerate() {
            data |= (*b as u32) << (8 * n);
        }
        i += size;

        let ty = (head >> 2) & 0x03;
        let tag = head >> 4;
        match (ty, tag) {
            // Global items
            (1, 0x8) => report_id = data as u8,
            (1, 0x7) => report_size = data,
            (1, 0x9) => report_count = data,
            // Main items: 0x8 Input, 0x9 Output, 0xB Feature
            (0, 0xb) => {
                let add = (report_size as u64) * (report_count as u64);
                match bits.iter_mut().find(|(id, _)| *id == report_id) {
                    Some((_, total)) => *total += add,
                    None => bits.push((report_id, add)),
                }
            }
            // Collection / End collection reset nothing we care about.
            _ => {}
        }
    }

    for (id, total_bits) in bits {
        out.push((id, total_bits.div_ceil(8) as usize));
    }
    out.sort_by_key(|(id, _)| *id);
    out
}

/// Walk a report descriptor and return every usage collection together with
/// the input, output and feature reports declared inside it.
///
/// Collections nest; fields are attributed to the enclosing application
/// collection, which is what WebHID matches a `filters` entry against.
pub fn usage_collections(desc: &[u8]) -> Vec<UsageCollection> {
    #[derive(Default)]
    struct Bits {
        inputs: Vec<(u8, u64)>,
        outputs: Vec<(u8, u64)>,
        features: Vec<(u8, u64)>,
    }
    fn add(list: &mut Vec<(u8, u64)>, id: u8, bits: u64) {
        match list.iter_mut().find(|(i, _)| *i == id) {
            Some((_, total)) => *total += bits,
            None => list.push((id, bits)),
        }
    }
    fn finish(page: u16, usage: u16, bits: Bits) -> UsageCollection {
        let conv = |v: Vec<(u8, u64)>| {
            let mut v: Vec<(u8, usize)> = v
                .into_iter()
                .map(|(id, bits)| (id, bits.div_ceil(8) as usize))
                .collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        UsageCollection {
            usage_page: page,
            usage,
            inputs: conv(bits.inputs),
            outputs: conv(bits.outputs),
            features: conv(bits.features),
        }
    }

    let mut out = Vec::new();
    // The open application collection, still collecting its report sizes.
    let mut current: Option<(u16, u16, Bits)> = None;
    // One flag per open collection: is it an application collection?
    let mut is_app: Vec<bool> = Vec::new();

    let mut usage_page: u16 = 0;
    let mut report_id: u8 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut local_usage: u16 = 0;

    let mut i = 0usize;
    while i < desc.len() {
        let head = desc[i];
        i += 1;
        if head == 0xfe {
            let len = *desc.get(i).unwrap_or(&0) as usize;
            i += 2 + len;
            continue;
        }
        let size = match head & 0x03 {
            0 => 0usize,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        if i + size > desc.len() {
            break;
        }
        let mut data: u32 = 0;
        for (n, b) in desc[i..i + size].iter().enumerate() {
            data |= (*b as u32) << (8 * n);
        }
        i += size;

        let ty = (head >> 2) & 0x03;
        let tag = head >> 4;
        match (ty, tag) {
            (1, 0x0) => usage_page = data as u16,
            (1, 0x8) => report_id = data as u8,
            (1, 0x7) => report_size = data,
            (1, 0x9) => report_count = data,
            (2, 0x0) => local_usage = data as u16,
            // Main item: Collection. `data` is the collection type (1 = app).
            (0, 0xa) => {
                let app = data == 0x01;
                is_app.push(app);
                if app {
                    if let Some((p, u, bits)) = current.take() {
                        out.push(finish(p, u, bits));
                    }
                    current = Some((usage_page, local_usage, Bits::default()));
                }
                local_usage = 0;
            }
            // Main item: End Collection.
            (0, 0xc) => {
                if is_app.pop().unwrap_or(false) {
                    if let Some((p, u, bits)) = current.take() {
                        out.push(finish(p, u, bits));
                    }
                }
            }
            // Main items: Input, Output, Feature.
            (0, 0x8) | (0, 0x9) | (0, 0xb) => {
                let bits = (report_size as u64) * (report_count as u64);
                if let Some((_, _, buf)) = current.as_mut() {
                    match tag {
                        0x8 => add(&mut buf.inputs, report_id, bits),
                        0x9 => add(&mut buf.outputs, report_id, bits),
                        _ => add(&mut buf.features, report_id, bits),
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((p, u, bits)) = current.take() {
        out.push(finish(p, u, bits));
    }
    out
}

/// An opened hidraw node.
pub struct HidDevice {
    file: File,
    pub node: HidNode,
}

impl HidDevice {
    pub fn open(node: &HidNode) -> io::Result<HidDevice> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node.dev_path)?;
        Ok(HidDevice {
            file,
            node: node.clone(),
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// `HIDIOCSFEATURE`. `buf[0]` must be the report id.
    pub fn set_feature(&self, buf: &[u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::ioctl(
                self.fd(),
                hidioc_set_feature(buf.len()),
                buf.as_ptr() as *mut libc::c_void,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// `HIDIOCGFEATURE`. `buf[0]` must be set to the report id on entry and is
    /// overwritten with the reply.
    pub fn get_feature(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe {
            libc::ioctl(
                self.fd(),
                hidioc_get_feature(buf.len()),
                buf.as_mut_ptr() as *mut libc::c_void,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    /// Send one output report by writing to the node.
    ///
    /// For a device without numbered reports the buffer *is* the report; for a
    /// numbered one `buf[0]` must be the report id. The Hall-effect family has
    /// no report id, so a 64-byte frame is written as-is.
    pub fn write_output(&self, buf: &[u8]) -> io::Result<usize> {
        use std::io::Write;
        (&self.file).write_all(buf)?;
        Ok(buf.len())
    }

    /// Wait up to `timeout` for one input report and read it into `buf`.
    ///
    /// Returns `None` when the device stayed quiet, which is normal for a
    /// request that has no answer.
    pub fn read_report(&self, buf: &mut [u8], timeout: Duration) -> io::Result<Option<usize>> {
        use std::io::Read;
        let mut pfd = libc::pollfd {
            fd: self.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
        let ret = unsafe { libc::poll(&mut pfd, 1, ms) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        if ret == 0 {
            return Ok(None);
        }
        let n = (&self.file).read(buf)?;
        Ok(Some(n))
    }
}

/// Human-readable hint for the most common failure: no permission on the node.
pub fn permission_hint(path: &Path, err: &io::Error) -> String {
    if err.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "{}: permission denied.\n\
             Install the udev rule and replug the keyboard:\n\
             \x20 sudo cp udev/60-aula.rules /etc/udev/rules.d/ && sudo udevadm control --reload",
            path.display()
        )
    } else {
        format!("{}: {err}", path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real descriptor read from an AULA F75 (258a:010c, second interface).
    const F75_IFACE1: &[u8] = &[
        0x06, 0x01, 0x00, 0x09, 0x80, 0xa1, 0x01, 0x85, 0x01, 0x19, 0x81, 0x29, 0x83, 0x15, 0x00,
        0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01, 0xc0,
        0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x02, 0x19, 0x00, 0x2a, 0xff, 0x02, 0x15, 0x00,
        0x26, 0xff, 0x7f, 0x95, 0x01, 0x75, 0x10, 0x81, 0x00, 0xc0, 0x06, 0x00, 0xff, 0x09, 0x01,
        0xa1, 0x01, 0x85, 0x03, 0x15, 0x00, 0x26, 0xff, 0x00, 0x09, 0x2f, 0x75, 0x08, 0x95, 0x03,
        0x81, 0x02, 0xc0, 0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x04, 0x05, 0x07, 0x19, 0x04,
        0x29, 0x70, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x78, 0x81, 0x02, 0xc0, 0x06, 0x00,
        0xff, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x05, 0x15, 0x00, 0x26, 0xff, 0x00, 0x19, 0x01, 0x29,
        0x02, 0x75, 0x08, 0x95, 0x05, 0xb1, 0x02, 0xc0, 0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01,
        0x85, 0x06, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95, 0x07, 0x09, 0x02, 0x81, 0x00,
        0x15, 0x00, 0x26, 0xff, 0x00, 0x19, 0x01, 0x29, 0x02, 0x75, 0x08, 0x96, 0x07, 0x02, 0xb1,
        0x02, 0xc0,
    ];

    #[test]
    fn finds_the_vendor_feature_reports() {
        let f = feature_reports(F75_IFACE1);
        assert_eq!(f, vec![(5, 5), (6, 519)]);
    }

    #[test]
    fn boot_keyboard_declares_no_feature_reports() {
        let boot = &[
            0x05u8, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
            0x25, 0x01, 0x95, 0x08, 0x75, 0x01, 0x81, 0x02, 0xc0,
        ];
        assert!(feature_reports(boot).is_empty());
    }

    #[test]
    fn ioctl_numbers_match_the_kernel_macros() {
        // HIDIOCSFEATURE(len) = _IOC(_IOC_WRITE|_IOC_READ, 'H', 0x06, len)
        assert_eq!(hidioc_set_feature(520), 0xC2084806);
        assert_eq!(hidioc_get_feature(520), 0xC2084807);
    }

    /// Real descriptor of the vendor interface of an AULA WIN 60 HE PRO
    /// (1ca2:1902, hidraw4): Usage Page 0xFFA0, Usage 0x01, one 64-byte input
    /// and one 64-byte output report, no report id.
    const WIN60HE_IFACE2: &[u8] = &[
        0x06, 0xa0, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x02, 0xa1, 0x00, 0x06, 0xa1, 0xff, 0x09,
        0x03, 0x09, 0x04, 0x15, 0x00, 0x26, 0xff, 0x00, 0x35, 0x00, 0x46, 0xff, 0x00, 0x75, 0x08,
        0x95, 0x40, 0x81, 0x02, 0x09, 0x05, 0x09, 0x06, 0x15, 0x00, 0x26, 0xff, 0x00, 0x35, 0x00,
        0x46, 0xff, 0x00, 0x75, 0x08, 0x95, 0x40, 0x91, 0x02, 0xc0, 0xc0,
    ];

    #[test]
    fn finds_the_hall_effect_vendor_collection() {
        let c = usage_collections(WIN60HE_IFACE2);
        let vendor = c
            .iter()
            .find(|c| c.usage_page == 0xFFA0 && c.usage == 0x01)
            .expect("the vendor collection should be found");
        assert_eq!(vendor.inputs, vec![(0, 64)]);
        assert_eq!(vendor.outputs, vec![(0, 64)]);
        assert!(vendor.features.is_empty());
    }

    #[test]
    fn attributes_reports_to_their_application_collection() {
        let c = usage_collections(F75_IFACE1);
        // The family declares several 0xFF00/0x01 collections; the 519-byte
        // feature report lives in one of them.
        assert!(
            c.iter().any(|c| c.features.contains(&(6, 519))),
            "expected a collection with feature report 6:519, got {c:?}"
        );
        assert!(c.iter().any(|c| c.features.contains(&(5, 5))));
    }
}
