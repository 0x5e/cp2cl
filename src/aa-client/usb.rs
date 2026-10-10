//! Linux USB gadget: this board plays an Android phone, so a head unit will
//! talk Android Auto to it.
//!
//! The head unit is the USB host and it expects a phone. Being one takes two
//! things the kernel provides: **configfs** to describe the device, and
//! **FunctionFS** to be it — the descriptors are written to `ep0` and the two
//! bulk endpoints are plain files we read and write.
//!
//! # About AOA
//!
//! Android Open Accessory is how a host normally switches an Android device
//! into accessory mode: it asks for the protocol version, sends a handful of
//! identifying strings, then tells the device to re-enumerate. We skip all of
//! that by enumerating as an accessory from the start — vendor 0x18D1,
//! product 0x2D00, a vendor-specific interface with subclass 0x42 and protocol
//! 0x01. Head units check for exactly that pair and start talking Android Auto
//! over the bulk endpoints without negotiating.
//!
//! # Not yet run on hardware
//!
//! Written against the documented configfs and FunctionFS layout, and compiled,
//! but no board has been plugged in yet — expect to debug the UDC name and the
//! endpoint ordering before it works. `CP2CL_AA_USB=1` selects it.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::bridge::Bridge;

const CONFIGFS: &str = "/sys/kernel/config";
const GADGET_DIR: &str = "/sys/kernel/config/usb_gadget/cp2cl";
const GADGET_NAME: &str = "cp2cl";
/// Where the function filesystem is mounted. Its name has to match the
/// `functions/ffs.<name>` directory in configfs.
const FFS_DIR: &str = "/dev/ffs-aa";
const FFS_NAME: &str = "aa";

/// Android Open Accessory: Google's vendor id, and the plain-accessory product
/// id (0x2D01 would add adb, which is only a second interface to go wrong).
const AOA_VENDOR: &str = "0x18d1";
const AOA_PRODUCT: &str = "0x2d00";

/// High-speed bulk max packet size. Full speed gets 64 below.
const HS_MAX_PACKET: u16 = 512;
const FS_MAX_PACKET: u16 = 64;

/// `FUNCTIONFS_DESCRIPTORS_MAGIC`, version 1: the four-word header every
/// gadget writes first. Version 2 adds a flags word and SuperSpeed counts,
/// which a USB 2.0 accessory has no use for.
const FFS_DESCRIPTORS_MAGIC: u32 = 1;
/// `FUNCTIONFS_STRINGS_MAGIC`.
const FFS_STRINGS_MAGIC: u32 = 2;

/// Waits for the head unit and serves it, restarting whenever the cable is
/// pulled and plugged back in.
pub async fn watch(bridge: std::sync::Arc<Bridge>) -> io::Result<()> {
    let gadget = Gadget::create()?;
    tracing::info!("[aa] usb gadget {GADGET_NAME} bound to {}", gadget.udc);
    loop {
        let io = match gadget.connect() {
            Ok(io) => io,
            Err(e) => {
                tracing::warn!("[aa] usb endpoints not ready: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        tracing::info!("[aa] usb: head unit attached");
        bridge.clone().serve(io, "usb".into()).await;
    }
}

/// The configfs gadget, kept alive for as long as the program runs.
struct Gadget {
    udc: String,
}

impl Gadget {
    fn create() -> io::Result<Self> {
        mount_configfs()?;
        let g = Path::new(GADGET_DIR);
        mkdir(&g.to_path_buf())?;
        // Present as an accessory from the first enumeration, see the module
        // comment on why AOA is skipped.
        write_sys(g, "idVendor", AOA_VENDOR)?;
        write_sys(g, "idProduct", AOA_PRODUCT)?;
        write_sys(g, "bcdDevice", "0x0100")?;
        write_sys(g, "bcdUSB", "0x0200")?;

        let strings = g.join("strings/0x409");
        mkdir(&strings)?;
        write_sys(&strings, "manufacturer", "cp2cl")?;
        write_sys(&strings, "product", "cp2cl")?;
        write_sys(&strings, "serialnumber", "000001")?;

        let config = g.join("configs/c.1");
        mkdir(&config)?;
        mkdir(&config.join("strings/0x409"))?;
        write_sys(
            &config.join("strings/0x409"),
            "configuration",
            "Android Auto",
        )?;
        // 500 mA, bus powered: enough for a board that is being powered by the
        // head unit anyway.
        write_sys(&config, "MaxPower", "500")?;
        write_sys(&config, "bmAttributes", "0x80")?;

        let function = g.join(format!("functions/ffs.{FFS_NAME}"));
        mkdir(&function)?;
        symlink(&function, &config.join(format!("ffs.{FFS_NAME}")))?;

        // FunctionFS has to be mounted before the descriptors can be written,
        // and it is not mounted by default: this is the one step that needs
        // root, along with everything above.
        mkdir(&PathBuf::from(FFS_DIR))?;
        if !is_mounted(FFS_DIR) {
            run("mount", &["-t", "functionfs", FFS_NAME, FFS_DIR])?;
        }

        let mut ep0 = OpenOptions::new()
            .write(true)
            .open(Path::new(FFS_DIR).join("ep0"))?;
        write_descriptors(&mut ep0)?;

        // Binding is what makes the host see us. It has to come after the
        // descriptors, or the kernel refuses the endpoint layout.
        let udc = udc_name()?;
        write_sys(g, "UDC", &udc)?;
        // The kernel enables the endpoints asynchronously; a moment is
        // measurably more reliable than racing it.
        std::thread::sleep(Duration::from_millis(300));
        Ok(Self { udc })
    }

    /// The two bulk endpoints, as one stream. `ep1` is the first endpoint the
    /// descriptors declare — out, host to us — and `ep2` the second, in.
    fn connect(&self) -> io::Result<UsbStream> {
        let dir = Path::new(FFS_DIR);
        let out = File::from_std(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join("ep1"))?,
        );
        let in_ = File::from_std(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join("ep2"))?,
        );
        Ok(UsbStream { out, in_ })
    }
}

/// Two endpoints, read and write. Android Auto over USB needs no framing of its
/// own: the channel framing already says where each message ends, so the pair
/// behaves like a socket.
pub struct UsbStream {
    out: File,
    in_: File,
}

impl AsyncRead for UsbStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().out).poll_read(cx, buf)
    }
}

impl AsyncWrite for UsbStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().in_).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().in_).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().in_).poll_shutdown(cx)
    }
}

/// The descriptors the host reads: one vendor-specific interface — Android
/// Auto's own subclass and protocol — with a bulk endpoint each way.
fn interface_descriptors(max_packet: u16) -> Vec<u8> {
    let mut d = Vec::new();
    // Interface 0, no alternate setting, two endpoints.
    d.extend_from_slice(&[9, 0x04, 0x00, 0x00, 0x02, 0xff, 0x42, 0x01, 0x00]);
    // Endpoint 1, OUT, bulk.
    d.extend_from_slice(&[
        7,
        0x05,
        0x01,
        0x02,
        (max_packet & 0xff) as u8,
        (max_packet >> 8) as u8,
        0x00,
    ]);
    // Endpoint 2, IN, bulk.
    d.extend_from_slice(&[
        7,
        0x05,
        0x82,
        0x02,
        (max_packet & 0xff) as u8,
        (max_packet >> 8) as u8,
        0x00,
    ]);
    d
}

fn write_descriptors(ep0: &mut fs::File) -> io::Result<()> {
    use std::io::Write;

    let fs_descs = interface_descriptors(FS_MAX_PACKET);
    let hs_descs = interface_descriptors(HS_MAX_PACKET);
    let length = 16 + fs_descs.len() + hs_descs.len();

    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&FFS_DESCRIPTORS_MAGIC.to_le_bytes());
    out.extend_from_slice(&(length as u32).to_le_bytes());
    // Counts are byte counts, not descriptor counts: the kernel walks each
    // block by `bLength` until it has consumed exactly this many bytes.
    out.extend_from_slice(&(fs_descs.len() as u32).to_le_bytes());
    out.extend_from_slice(&(hs_descs.len() as u32).to_le_bytes());
    out.extend_from_slice(&fs_descs);
    out.extend_from_slice(&hs_descs);
    ep0.write_all(&out)?;

    // Strings: one language, one interface string. Optional, but a host that
    // asks for a missing string descriptor gets a stall for its trouble.
    let lang = 0x0409u16.to_le_bytes();
    let string = b"cp2cl Android Auto\0";
    let length = 16 + lang.len() + string.len();
    let mut out = Vec::with_capacity(length);
    out.extend_from_slice(&FFS_STRINGS_MAGIC.to_le_bytes());
    out.extend_from_slice(&(length as u32).to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes()); // str_count
    out.extend_from_slice(&1u32.to_le_bytes()); // lang_count
    out.extend_from_slice(&lang);
    out.extend_from_slice(string);
    ep0.write_all(&out)
}

fn mount_configfs() -> io::Result<()> {
    if is_mounted(CONFIGFS) {
        return Ok(());
    }
    run("mount", &["-t", "configfs", "none", CONFIGFS])
}

fn is_mounted(path: &str) -> bool {
    // Reading `/proc/mounts` beats guessing: configfs is usually mounted by
    // systemd on a distro, and by us on a board that does not.
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return false;
    };
    mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .map(|mount| mount == path)
            .unwrap_or(false)
    })
}

/// The first UDC the kernel offers. A board has one; a desktop has none.
fn udc_name() -> io::Result<String> {
    let entries = fs::read_dir("/sys/class/udc")?;
    for entry in entries {
        let entry = entry?;
        return Ok(entry.file_name().to_string_lossy().into_owned());
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no UDC in /sys/class/udc — is this a USB device controller?",
    ))
}

fn mkdir(dir: &PathBuf) -> io::Result<()> {
    match fs::create_dir_all(dir) {
        Ok(()) => Ok(()),
        // Already there from a previous run: fine, the files below are
        // rewritten anyway.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

fn symlink(from: &Path, to: &PathBuf) -> io::Result<()> {
    if to.exists() || to.is_symlink() {
        return Ok(());
    }
    std::os::unix::fs::symlink(from, to)
}

fn write_sys(dir: &Path, file: &str, value: &str) -> io::Result<()> {
    fs::write(dir.join(file), value)
}

fn run(program: &str, args: &[&str]) -> io::Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|e| io::Error::new(e.kind(), format!("{program}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{program} {} -> {status}",
            args.join(" ")
        )))
    }
}
