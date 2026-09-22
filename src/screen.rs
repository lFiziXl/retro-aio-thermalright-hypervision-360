//! USB driver for the Thermalright AIO panel (ChiZhu Tech `87ad:70db`),
//! implementing the "USBLCDNew" raw-bulk protocol.
//!
//! Protocol adapted from the `thermalright-trcc-linux` reference
//! (`src/trcc/adapters/device/bulk_lcd.py`, `transport.py`, and the
//! `USBLCDNEW.exe` decompile in `doc/PROTOCOL_USBLCDNEW.md`):
//!
//! * **Transport** — USB bulk transfers, configuration 1, interface 0
//!   (vendor-specific class). The firmware exposes several interfaces
//!   (vendor + HID + mass-storage), so kernel drivers on interfaces 0..3
//!   are detached before claiming. The first bulk OUT endpoint (EP01 OUT)
//!   carries writes; the first bulk IN endpoint (EP01 IN) carries reads.
//!
//! * **Handshake** — write a 64-byte request:
//!   `[0..4]` magic `0x12345678` (LE), `[56]` command `0x01` (device-info
//!   query), all else zero. Read a 1024-byte response and validate
//!   `resp[24] != 0`. `resp[24]` is the PM byte, `resp[36]` the SUB byte.
//!
//! * **Frame** — a 64-byte header followed by the image payload:
//!   `[0..4]`  magic `0x12345678` (LE)
//!   `[4..8]`  command (LE u32): `2` = JPEG, `3` = raw RGB565
//!   `[8..12]` width (LE u32)
//!   `[12..16]` height (LE u32)
//!   `[56..60]` constant `2` (LE u32)
//!   `[60..64]` payload length in bytes (LE u32)
//!   The firmware uses the header's width/height to interpret the buffer,
//!   so it must match the payload exactly.
//!
//! * **Encoding** — this panel (`PM=4`/`SUB=1`, Hyper Vision 360) is a JPEG
//!   panel: the reference driver sends JPEG (`cmd=2`) for every bulk PM
//!   except `32`, which is the only raw-RGB565 (`cmd=3`) model. A raw
//!   RGB565 payload was accepted by the firmware (bulk write completed) but
//!   never painted — the firmware de-blocks the JPEG using the header
//!   geometry, so the payload must be a genuine JPEG of the declared size.
//!
//! * **Write cadence** — the frame is written in 16 KiB bulk chunks with a
//!   short-write check, and a zero-length packet terminates the transfer
//!   when the total frame length is an exact multiple of 512 bytes
//!   (USB bulk framing requirement).

use std::fmt;
use std::time::Duration;

use rusb::{Context, DeviceHandle, Error, TransferType, UsbContext};

/// ChiZhu Tech "USBDISPLAY" panel as seen by `lsusb` (`87ad:70db`).
pub const AIO_VID: u16 = 0x87AD;
pub const AIO_PID: u16 = 0x70DB;

/// Native geometry of the AIO panel (FBL 72 in the reference FBL table —
/// the hardcoded bulk-protocol base resolution; `PM=4` is not in the
/// reference's known-bulk-PM set, so it keeps this 480x480 base).
pub const WIDTH: u16 = 480;
pub const HEIGHT: u16 = 480;

/// Protocol magic `0x12345678`, wire order (little-endian).
const MAGIC: [u8; 4] = [0x12, 0x34, 0x56, 0x78];
/// Handshake command (offset 56): device-info query.
const CMD_DEV_INFO: u8 = 0x01;
/// Frame command (offset 4): JPEG payload. In the USBLCDNew protocol every
/// bulk PM is JPEG except `32` (raw RGB565); our `PM=4` panel is JPEG.
const CMD_JPEG: u32 = 2;

const HANDSHAKE_SIZE: usize = 64;
const HANDSHAKE_READ_SIZE: usize = 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(1_000);
const WRITE_TIMEOUT: Duration = Duration::from_millis(5_000);
/// Bulk writes are chunked at 16 KiB, mirroring the reference driver.
const WRITE_CHUNK_SIZE: usize = 16 * 1024;
/// ZLP delimiter threshold for USB bulk framing.
const ZLP_ALIGN: usize = 512;

const USB_CONFIGURATION: u8 = 1;
const USB_INTERFACE: u8 = 0;
/// The firmware can expose up to 4 interfaces; the kernel may hold a
/// driver on any of them, so detach all of 0..4 before claiming.
const DETACH_INTERFACES: u8 = 4;
/// Fallback endpoint addresses when auto-detection finds nothing
/// (the protocol reference names EP01 OUT / EP01 IN).
const EP_OUT_DEFAULT: u8 = 0x01;
const EP_IN_DEFAULT: u8 = 0x81;

/// Errors from finding, opening, or talking to the panel.
#[derive(Debug)]
pub enum AioError {
    /// `87ad:70db` is not present on the USB bus.
    DeviceNotFound { vid: u16, pid: u16 },
    /// The panel answered the handshake but the response failed validation.
    HandshakeFailed { len: usize, pm: Option<u8> },
    /// A bulk write transferred fewer bytes than requested.
    ShortWrite { offset: usize, wrote: usize, expected: usize },
    /// The payload is not a JPEG stream (missing `FF D8` start marker).
    NotJpeg { len: usize },
    /// A USB transfer failed.
    Usb(Error),
}

impl fmt::Display for AioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceNotFound { vid, pid } => write!(
                f,
                "USB device {vid:04x}:{pid:04x} not found — check the cable, \
                 power and that no other process holds the panel"
            ),
            Self::HandshakeFailed { len, pm } => write!(
                f,
                "panel handshake failed (response {len} bytes, PM={pm:?}) — \
                 the device answered but did not pass validation"
            ),
            Self::ShortWrite { offset, wrote, expected } => write!(
                f,
                "short bulk write at offset {offset}: {wrote}/{expected} bytes transferred"
            ),
            Self::NotJpeg { len } => write!(
                f,
                "payload is {len} bytes but is not a JPEG stream (no FF D8 \
                 start-of-image marker) — this PM=4 panel requires JPEG (cmd=2)"
            ),
            Self::Usb(e) => write!(f, "USB transfer failed: {e}"),
        }
    }
}

impl std::error::Error for AioError {}

impl From<Error> for AioError {
    fn from(e: Error) -> Self {
        Self::Usb(e)
    }
}

/// Read a JPEG's `(width, height)` from its SOF segment, or `None`.
///
/// Header-only: walks the segment markers rather than decoding pixels, so it
/// is cheap enough to run on every frame. Mirrors the reference driver's
/// `jpeg_dimensions()` — the firmware de-blocks with the header's declared
/// geometry, so a differently sized JPEG paints only the overlap, and this
/// lets us say so before sending instead of after a blank screen.
///
/// JPEG start-of-frame markers carry the image's real dimensions; C4/C8/CC
/// are Huffman/extension segments, not frames, and are excluded.
fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 9 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if (0xC0..0xD0).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let height = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            let width = u16::from_be_bytes([data[i + 7], data[i + 8]]);
            return Some((width, height));
        }
        let len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

/// An opened, handshaked AIO panel ready to receive frames.
#[derive(Debug)]
pub struct AioScreen {
    handle: DeviceHandle<Context>,
    ep_out: u8,
    /// PM byte from the handshake (response offset 24).
    pm: u8,
    /// SUB byte from the handshake (response offset 36).
    sub: u8,
}

impl AioScreen {
    /// Find the panel by `vid`/`pid`, open it, detach kernel drivers,
    /// claim the vendor interface, detect the bulk endpoints and run the
    /// protocol handshake.
    pub fn new(vid: u16, pid: u16) -> Result<Self, AioError> {
        let ctx = Context::new()?;
        let device = ctx
            .devices()?
            .iter()
            .find_map(|d| match d.device_descriptor() {
                Ok(desc) if desc.vendor_id() == vid && desc.product_id() == pid => Some(d),
                _ => None,
            })
            .ok_or(AioError::DeviceNotFound { vid, pid })?;
        let handle = device.open()?;

        // The cooler firmware presents multiple interfaces (vendor + HID +
        // mass-storage); a kernel driver held on any sibling interface can
        // prevent claiming the bulk endpoints, so detach 0..4 first.
        for i in 0..DETACH_INTERFACES {
            if handle.kernel_driver_active(i)? {
                handle.detach_kernel_driver(i)?;
            }
        }

        handle.set_active_configuration(USB_CONFIGURATION)?;
        handle.claim_interface(USB_INTERFACE)?;

        let (ep_out, ep_in) = Self::detect_endpoints(&handle, vid, pid);
        let (pm, sub) = Self::handshake(&handle, ep_out, ep_in)?;

        Ok(Self {
            handle,
            ep_out,
            pm,
            sub,
        })
    }

    /// PM byte from the handshake (device fingerprint, response offset 24).
    pub fn pm(&self) -> u8 {
        self.pm
    }

    /// SUB byte from the handshake (response offset 36).
    pub fn sub(&self) -> u8 {
        self.sub
    }

    /// Send a JPEG-encoded frame to the panel.
    ///
    /// The `PM=4`/`SUB=1` fingerprint is a JPEG panel (the reference driver
    /// sends JPEG for every bulk PM except `32`), so the payload must be a
    /// genuine JPEG byte stream — `FF D8` start-of-image marker and all.
    /// The 64-byte header declares the panel's native 480x480 geometry and
    /// the actual payload length; the firmware de-blocks the JPEG using the
    /// header dimensions, so a differently sized JPEG paints only the
    /// overlap (warned here, mirroring the reference driver).
    pub fn send_frame(&self, jpeg: &[u8]) -> Result<(), AioError> {
        if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
            return Err(AioError::NotJpeg { len: jpeg.len() });
        }
        if let Some((w, h)) = jpeg_dimensions(jpeg) {
            if (w, h) != (WIDTH, HEIGHT) {
                eprintln!(
                    "warn: JPEG is {w}x{h} but the header declares {WIDTH}x{HEIGHT} \
                     — the panel will paint only the overlap"
                );
            }
        }

        // 64-byte USBLCDNew header — the firmware uses the declared geometry
        // to de-block the JPEG, so it must match the payload exactly.
        let mut header = [0u8; HANDSHAKE_SIZE];
        header[0..4].copy_from_slice(&MAGIC);
        header[4..8].copy_from_slice(&CMD_JPEG.to_le_bytes());
        header[8..12].copy_from_slice(&(WIDTH as u32).to_le_bytes());
        header[12..16].copy_from_slice(&(HEIGHT as u32).to_le_bytes());
        header[56..60].copy_from_slice(&2u32.to_le_bytes());
        header[60..64].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());

        let mut frame = Vec::with_capacity(HANDSHAKE_SIZE + jpeg.len());
        frame.extend_from_slice(&header);
        frame.extend_from_slice(jpeg);

        // 16 KiB bulk writes with a short-write check: a partial chunk leaves
        // the panel with a torn frame, which looks identical to a dead screen.
        for (offset, chunk) in frame.chunks(WRITE_CHUNK_SIZE).enumerate() {
            let wrote = self.handle.write_bulk(self.ep_out, chunk, WRITE_TIMEOUT)?;
            if wrote < chunk.len() {
                return Err(AioError::ShortWrite {
                    offset: offset * WRITE_CHUNK_SIZE,
                    wrote,
                    expected: chunk.len(),
                });
            }
        }

        // Zero-length packet when the total transfer is an exact multiple of
        // 512 bytes (USB bulk framing delimiter).
        if frame.len() % ZLP_ALIGN == 0 {
            self.handle.write_bulk(self.ep_out, &[], WRITE_TIMEOUT)?;
        }
        Ok(())
    }

    /// Protocol handshake: 64-byte device-info request, 1024-byte response.
    fn handshake(
        handle: &DeviceHandle<Context>,
        ep_out: u8,
        ep_in: u8,
    ) -> Result<(u8, u8), AioError> {
        let mut req = [0u8; HANDSHAKE_SIZE];
        req[0..4].copy_from_slice(&MAGIC);
        req[56] = CMD_DEV_INFO;
        handle.write_bulk(ep_out, &req, HANDSHAKE_TIMEOUT)?;

        let mut resp = [0u8; HANDSHAKE_READ_SIZE];
        let len = handle.read_bulk(ep_in, &mut resp, HANDSHAKE_TIMEOUT)?;
        let pm = resp.get(24).copied().filter(|v| *v != 0);
        if len < 41 || pm.is_none() {
            return Err(AioError::HandshakeFailed { len, pm });
        }
        Ok((pm.unwrap(), resp[36]))
    }

    /// Take the first bulk OUT and first bulk IN endpoint of interface 0's
    /// default alternate setting, falling back to the reference
    /// EP01 OUT / EP01 IN addresses.
    fn detect_endpoints(handle: &DeviceHandle<Context>, vid: u16, pid: u16) -> (u8, u8) {
        let (mut ep_out, mut ep_in) = (0u8, 0u8);
        let cfg = handle.device().active_config_descriptor().ok();
        if let Some(cfg) = cfg {
            for intf in cfg.interfaces() {
                if intf.number() != USB_INTERFACE {
                    continue;
                }
                for idesc in intf.descriptors() {
                    if idesc.setting_number() != 0 {
                        continue;
                    }
                    for ep in idesc.endpoint_descriptors() {
                        if ep.transfer_type() != TransferType::Bulk {
                            continue;
                        }
                        let addr = ep.address();
                        if addr & 0x80 == 0 {
                            if ep_out == 0 {
                                ep_out = addr;
                            }
                        } else if ep_in == 0 {
                            ep_in = addr;
                        }
                    }
                }
            }
        }
        if ep_out == 0 || ep_in == 0 {
            eprintln!(
                "warn: {vid:04x}:{pid:04x} — endpoint auto-detection \
                 incomplete (OUT=0x{ep_out:02x} IN=0x{ep_in:02x}); using defaults"
            );
        }
        (
            if ep_out == 0 { EP_OUT_DEFAULT } else { ep_out },
            if ep_in == 0 { EP_IN_DEFAULT } else { ep_in },
        )
    }
}
