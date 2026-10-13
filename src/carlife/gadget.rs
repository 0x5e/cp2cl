//! The USB device side: the gadget the car enumerates, and the AOA requests on
//! top of it.
//!
//! The board's kernel has no `f_accessory` and is not built with `raw-gadget`, so
//! the vendor interface the car is looking for comes from FunctionFS instead: the
//! descriptors are written from here, and the AOA requests arrive as events on
//! the same endpoint. That is enough because the descriptors ask for every
//! control recipient (`FUNCTIONFS_ALL_CTRL_RECIP`), which is what makes the
//! kernel hand a request addressed to the device to this program rather than
//! turning it away. The plan records the source lines for that.
//!
//! The gadget itself is not ours. The board already carries one, made at boot to
//! provide the ECM link this program is usually driven over, and adding a
//! function to it is what makes the car's interface appear. That is also the
//! risk: the gadget has to be unbound while it is changed, so the management link
//! goes down for a moment, and everything done here is undone on the way out.
//! `CP2CL_CARLIFE_VIDPID=none` avoids the identity change altogether, which is
//! the setting to fall back to if a host turns out not to care what we claim to
//! be.
//!
//! Only the accessory form is built. The car's guide describes a second route —
//! enumerate as a phone, be asked to switch, and re-enumerate — but the car's own
//! reference implementation on a real head unit skips straight past it, so a
//! device that is already an accessory is the case to get working first.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Context as _, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc;

use crate::config::Config;

use super::session::{self, Frames};

/// Full speed gets 64 bytes per packet and high speed 512; the kernel picks the
/// endpoint and rewrites the address, and the interface number too, so the
/// values written here are placeholders. What must be right is the class, which
/// is what the car's host side looks at to recognise an accessory.
const BULK_CLASS: (u8, u8, u8) = (0xff, 0xff, 0x00);

const ACCESSORY_VID: u16 = 0x18d1;
const ACCESSORY_PID: u16 = 0x2d00;

const ACCESSORY_GET_PROTOCOL: u8 = 51;
const ACCESSORY_SEND_STRING: u8 = 52;
const ACCESSORY_START: u8 = 53;

/// The car's reference implementation accepts either, and version 2 is what a
/// head unit of this age will have been written against.
const ACCESSORY_VERSION: u16 = 2;

// `include/uapi/linux/usb/functionfs.h`.
const DESCRIPTORS_MAGIC_V2: u32 = 3;
const STRINGS_MAGIC: u32 = 2;
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
const ALL_CTRL_RECIP: u32 = 64;

const EVENT_ENABLE: u8 = 2;
const EVENT_DISABLE: u8 = 3;
const EVENT_SETUP: u8 = 4;

const USB_DIR_IN: u8 = 0x80;

/// The endpoint files FunctionFS makes, in descriptor order: the first is the
/// one the car writes to and the second is the one it reads from.
const FROM_CAR: &str = "ep1";
const TO_CAR: &str = "ep2";

/// What the control endpoint tells the session loop.
enum Event {
    /// The car has enumerated us and the endpoints are usable.
    Enabled,
    /// It has gone, or the gadget was unbound underneath us.
    Disabled,
}

/// Runs the gadget and, over it, one CarLife session after another.
///
/// Everything is set up before the first car arrives and taken down after the
/// last, so the function survives the car coming and going without the gadget
/// hopping about underneath it.
pub async fn run<F>(cfg: Arc<Config>, frames: &Frames, carplay: &mut F) -> Result<()>
where
    F: FnMut(bool),
{
    let rig = Rig::prepare()?;
    let mut ep0 = tokio::fs::File::from_std(rig.open_control()?);
    // Registration comes first and the bind comes second, and the order is not a
    // choice: the kernel refuses to bind a FunctionFS function that has not been
    // told what it looks like.
    register(&mut ep0).await?;
    rig.bind()?;
    tracing::info!("[cl-usb] the gadget is up; waiting for the car to enumerate us");

    let (events_tx, mut events_rx) = mpsc::channel(4);
    let control = tokio::spawn(control_loop(ep0, events_tx));

    while let Some(event) = events_rx.recv().await {
        match event {
            Event::Enabled => {
                match rig.bulk() {
                    Ok(bulk) => {
                        tracing::info!("[cl-usb] the car has enumerated us; starting the session");
                        if let Err(e) = session::run(bulk, frames, &cfg, carplay).await {
                            tracing::warn!("[cl] the session ended badly: {e}");
                        }
                    }
                    Err(e) => tracing::error!("[cl-usb] the endpoints are not usable: {e}"),
                }
            }
            Event::Disabled => tracing::info!("[cl-usb] the car has gone"),
        }
    }

    control.abort();
    tracing::info!("[cl-usb] the control endpoint is gone; the gadget is being taken down");
    Ok(())
}

/// Reads the control endpoint for the life of the gadget.
///
/// It owns the endpoint because it is also what answers on it: a setup event and
/// the reply to it are two halves of the same exchange, and nothing else may
/// interleave with them.
async fn control_loop(mut ep0: tokio::fs::File, events: mpsc::Sender<Event>) -> io::Result<()> {
    loop {
        // The whole event is twelve bytes: the eight of a `usb_ctrlrequest`, the
        // type, and three of padding. Read as bytes and unpacked by hand, so
        // there is no packed struct to get wrong.
        let mut raw = [0u8; 12];
        if let Err(e) = ep0.read_exact(&mut raw).await {
            tracing::debug!("[cl-usb] the control endpoint stopped reading: {e}");
            return Ok(());
        }
        match raw[8] {
            EVENT_ENABLE => {
                let _ = events.send(Event::Enabled).await;
            }
            EVENT_DISABLE => {
                let _ = events.send(Event::Disabled).await;
            }
            EVENT_SETUP => {
                if let Err(e) = answer(&mut ep0, &raw).await {
                    tracing::warn!("[cl-usb] a control request could not be answered: {e}");
                }
            }
            other => tracing::debug!("[cl-usb] control event {other}"),
        }
    }
}

/// Answers one setup event, when there is an answer to give.
///
/// Every request is reported whether or not it is understood: which ones a given
/// head unit sends is not known yet, and a line per request is the cheapest way
/// to find out. The ones that are understood are the three that ask a device to
/// become an accessory.
async fn answer(ep0: &mut tokio::fs::File, raw: &[u8; 12]) -> io::Result<()> {
    let request_type = raw[0];
    let request = raw[1];
    let value = u16::from_le_bytes([raw[2], raw[3]]);
    let index = u16::from_le_bytes([raw[4], raw[5]]);
    let length = u16::from_le_bytes([raw[6], raw[7]]);
    let direction = if request_type & USB_DIR_IN != 0 {
        "in"
    } else {
        "out"
    };
    tracing::info!(
        "[cl-usb] control request {request} {direction}, type {request_type:#04x}, value {value}, \
         index {index}, length {length}"
    );

    match (request_type, request) {
        // "What Android Open Accessory version do you speak?"
        (0xc0, ACCESSORY_GET_PROTOCOL) => {
            let reply = ACCESSORY_VERSION.to_le_bytes();
            ep0.write_all(&reply).await?;
            tracing::info!("[cl-usb] answered with AOA {ACCESSORY_VERSION}");
        }
        // One of the six strings the host wants the device to report.
        (0x40, ACCESSORY_SEND_STRING) => {
            let mut text = vec![0u8; length as usize];
            ep0.read_exact(&mut text).await?;
            tracing::info!(
                "[cl-usb] identifier {index}: {:?}",
                String::from_utf8_lossy(&text)
            );
        }
        // "Switch to accessory mode and re-enumerate." There is nothing to do:
        // this device only ever enumerates in that form, which is why the car's
        // host side is expected to skip the whole handshake.
        (0x40, ACCESSORY_START) => tracing::info!(
            "[cl-usb] the host asked us to switch to accessory mode; we are already enumerated \
             as one, so there is nothing to re-enumerate"
        ),
        _ => tracing::info!("[cl-usb] that one is not acted on"),
    }
    Ok(())
}

/// A byte pipe over the two bulk endpoints, the shape `session` wants.
///
/// Reads and writes are blocking calls on the endpoint files, handed to tokio's
/// blocking pool, which is what a bulk endpoint is: a read waits until the car
/// sends something and a write waits until it has taken it.
struct Bulk {
    from_car: tokio::fs::File,
    to_car: tokio::fs::File,
}

impl AsyncRead for Bulk {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().from_car).poll_read(cx, buf)
    }
}

impl AsyncWrite for Bulk {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().to_car).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().to_car).poll_shutdown(cx)
    }
}

/// A gadget that has been lent a function, and what it takes to give it back.
struct Rig {
    /// Where the gadget is under configfs.
    gadget: PathBuf,
    /// The FunctionFS instance, which names the function, the mount under it and
    /// the endpoint files inside that.
    instance: String,
    /// Where the function was linked into the configuration.
    link: PathBuf,
    /// The identity the gadget had before, when this changed it.
    identity: Option<(String, String)>,
    /// Whether the mount here is ours to take down. One left behind by an earlier
    /// run is not.
    we_mounted: bool,
    udc: String,
}

impl Rig {
    /// Finds the gadget, adds the function to it, and mounts the instance.
    ///
    /// The gadget is unbound for the length of this and left unbound: nothing
    /// can be added to a configuration that is in use, and the function cannot be
    /// bound until it has been given descriptors, which is what `bind` is for.
    fn prepare() -> Result<Self> {
        let instance = env("CP2CL_CARLIFE_FFS").unwrap_or_else(|| "carlife".to_string());
        let gadget = find_gadget()?;
        let udc = match env("CP2CL_CARLIFE_UDC") {
            Some(udc) => udc,
            None => first_entry("/sys/class/udc").ok_or_else(|| anyhow!("no UDC is available"))?,
        };

        let configs = gadget.join("configs");
        let config = first_entry(&configs)
            .ok_or_else(|| anyhow!("the gadget has no configuration to add a function to"))?;
        let link = configs.join(config).join(format!("ffs.{instance}"));

        // Taking the gadget down is what may interrupt the management link, and
        // it cannot be avoided: configfs will not let a bound configuration be
        // changed. What can be avoided is the identity change below.
        unbind(&gadget)?;

        let changing_identity = env("CP2CL_CARLIFE_VIDPID").as_deref() != Some("none");
        let identity = if changing_identity {
            Some((
                fs::read_to_string(gadget.join("idVendor"))?.trim().to_string(),
                fs::read_to_string(gadget.join("idProduct"))?.trim().to_string(),
            ))
        } else {
            None
        };

        let function = gadget.join("functions").join(format!("ffs.{instance}"));
        fs::create_dir_all(&function).with_context(|| format!("create {}", function.display()))?;
        // A link left over from a run that ended badly points nowhere useful.
        let _ = fs::remove_file(&link);
        std::os::unix::fs::symlink(&function, &link)
            .with_context(|| format!("link {}", link.display()))?;

        if changing_identity {
            write_attr(&gadget.join("idVendor"), &format!("0x{ACCESSORY_VID:04x}"))?;
            write_attr(&gadget.join("idProduct"), &format!("0x{ACCESSORY_PID:04x}"))?;
        }

        let mount = PathBuf::from("/dev/usb-ffs").join(&instance);
        fs::create_dir_all(&mount).with_context(|| format!("create {}", mount.display()))?;
        let already = mount_is_ready(&mount);
        if !already {
            // FunctionFS is mounted by instance name, which is why the function
            // above had to be created before this could work.
            let ok = Command::new("mount")
                .args([
                    "-t",
                    "functionfs",
                    instance.as_str(),
                    mount.to_str().unwrap_or_default(),
                ])
                .status()
                .context("run mount")?
                .success();
            if !ok {
                let _ = fs::remove_file(&link);
                let _ = fs::remove_dir(&function);
                return Err(anyhow!(
                    "functionfs could not be mounted on {}",
                    mount.display()
                ));
            }
        }

        tracing::info!(
            "[cl-usb] {} has an ffs function; it will be bound as {ACCESSORY_VID:04x}:\
             {ACCESSORY_PID:04x} once its descriptors are written",
            gadget.display()
        );

        Ok(Self {
            gadget,
            instance,
            link,
            identity,
            we_mounted: !already,
            udc,
        })
    }

    /// Binds the gadget to the controller, which is what makes the car see it.
    fn bind(&self) -> Result<()> {
        write_attr(&self.gadget.join("UDC"), &self.udc)
    }


    fn mount_point(&self) -> PathBuf {
        PathBuf::from("/dev/usb-ffs").join(&self.instance)
    }

    fn open_control(&self) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.mount_point().join("ep0"))
    }

    fn bulk(&self) -> io::Result<Bulk> {
        let endpoint = |name: &str| -> io::Result<tokio::fs::File> {
            let path = self.mount_point().join(name);
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|e| io::Error::new(e.kind(), format!("open {}: {e}", path.display())))?;
            Ok(tokio::fs::File::from_std(file))
        };
        Ok(Bulk {
            // Both are opened read-write: FunctionFS reports which direction an
            // endpoint runs in by which calls succeed on it, and an endpoint
            // file opened for one direction only fails the other with EACCES.
            from_car: endpoint(FROM_CAR)?,
            to_car: endpoint(TO_CAR)?,
        })
    }
}

impl Drop for Rig {
    /// Puts the gadget back the way it was found.
    ///
    /// Anything left behind here is left in configfs, which lives in memory: the
    /// next boot starts from the script that made the gadget in the first place,
    /// so a run that ends badly costs a replug and not a reflash.
    fn drop(&mut self) {
        let _ = unbind(&self.gadget);
        let _ = fs::remove_file(&self.link);
        let _ = fs::remove_dir(self.gadget.join("functions").join(format!("ffs.{}", self.instance)));
        if let Some((vid, pid)) = &self.identity {
            let _ = write_attr(&self.gadget.join("idVendor"), vid);
            let _ = write_attr(&self.gadget.join("idProduct"), pid);
        }
        if self.we_mounted {
            let _ = Command::new("umount")
                .arg(self.mount_point().to_str().unwrap_or_default())
                .status();
        }
        let _ = write_attr(&self.gadget.join("UDC"), &self.udc);
    }
}

/// The descriptors the car reads: one vendor interface and two bulk endpoints,
/// written twice because the car may attach at either speed.
fn descriptors() -> Vec<u8> {
    let interface = |endpoints: u8| -> Vec<u8> {
        vec![
            9,
            4, // interface
            0, // number, which the kernel replaces
            0,
            endpoints,
            BULK_CLASS.0,
            BULK_CLASS.1,
            BULK_CLASS.2,
            0,
        ]
    };
    let endpoint = |address: u8, packet: u16| -> Vec<u8> {
        let mut out = vec![7, 5, address, 2];
        out.extend_from_slice(&packet.to_le_bytes());
        out.push(0); // interval, unused by bulk
        out
    };
    let mut fs_descriptors = interface(2);
    fs_descriptors.extend(endpoint(0x01, 64));
    fs_descriptors.extend(endpoint(0x82, 64));
    let mut hs_descriptors = interface(2);
    hs_descriptors.extend(endpoint(0x01, 512));
    hs_descriptors.extend(endpoint(0x82, 512));

    let count = 3u32;
    let length = 20 + fs_descriptors.len() + hs_descriptors.len();
    let mut blob = Vec::with_capacity(length);
    blob.extend_from_slice(&DESCRIPTORS_MAGIC_V2.to_le_bytes());
    blob.extend_from_slice(&(length as u32).to_le_bytes());
    // `ALL_CTRL_RECIP` is the whole reason the AOA requests reach this program.
    blob.extend_from_slice(&(HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP).to_le_bytes());
    blob.extend_from_slice(&count.to_le_bytes());
    blob.extend_from_slice(&count.to_le_bytes());
    blob.extend_from_slice(&fs_descriptors);
    blob.extend_from_slice(&hs_descriptors);
    blob
}

/// The strings blob, which is what the second of the two registration writes
/// carries.
///
/// Registration is two writes and not one: the descriptors say what the
/// endpoints are and this says what the strings are, and the instance only
/// reaches the state a bind accepts once it has been given both. Written empty,
/// because the strings a head unit cares about — what the device calls itself —
/// travel over the accessory requests below rather than over USB string
/// descriptors.
fn strings() -> Vec<u8> {
    let mut blob = Vec::with_capacity(16);
    blob.extend_from_slice(&STRINGS_MAGIC.to_le_bytes());
    blob.extend_from_slice(&16u32.to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes());
    blob.extend_from_slice(&0u32.to_le_bytes());
    blob
}

/// Registers the instance: both blobs, in the order the kernel reads them.
///
/// Until the second one lands the instance is not active, and a bind of the
/// gadget would be refused — which is the whole reason the bind comes after this
/// rather than before it.
async fn register(ep0: &mut tokio::fs::File) -> Result<()> {
    let descriptors = descriptors();
    // The blob below is what the kernel reads; keeping the two in step matters
    // more than what the numbers are, so it is checked rather than trusted.
    debug_assert_eq!(descriptors.len(), 20 + 23 + 23);
    ep0.write_all(&descriptors)
        .await
        .context("write the functionfs descriptors")?;
    ep0.write_all(&strings())
        .await
        .context("write the functionfs strings")?;
    Ok(())
}

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The gadget to add the function to: the one named, or the only one there is.
fn find_gadget() -> Result<PathBuf> {
    if let Some(gadget) = env("CP2CL_CARLIFE_GADGET") {
        let path = PathBuf::from("/sys/kernel/config/usb_gadget").join(gadget);
        return if path.is_dir() {
            Ok(path)
        } else {
            Err(anyhow!("{} is not a gadget", path.display()))
        };
    }
    let gadgets: Vec<String> = entries("/sys/kernel/config/usb_gadget");
    match gadgets.len() {
        0 => Err(anyhow!(
            "there is no gadget under /sys/kernel/config/usb_gadget; the ECM link is made by a \
             script at boot and it has to be there first"
        )),
        1 => Ok(PathBuf::from("/sys/kernel/config/usb_gadget").join(&gadgets[0])),
        _ => Err(anyhow!(
            "there is more than one gadget ({gadgets:?}); name one with CP2CL_CARLIFE_GADGET"
        )),
    }
}

fn entries(directory: impl AsRef<Path>) -> Vec<String> {
    fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default()
}

fn first_entry(directory: impl AsRef<Path>) -> Option<String> {
    entries(directory).into_iter().next()
}

/// Whether the instance's endpoints are already there, which is what a mount
/// that survived an earlier run looks like.
fn mount_is_ready(mount: &Path) -> bool {
    mount.join("ep0").exists()
}

/// Writes one of configfs' single-value attributes, which is the only way most
/// of the gadget is configured.
///
/// The newline is what makes the write happen. configfs reads a value out of the
/// bytes written to the file, and `fs::write` with an empty string writes no
/// bytes — it makes no call into the kernel at all — so an empty `write_attr` is
/// a silent no-op rather than a write of nothing. Unbinding is the attribute
/// that depends on this: it has to be told something, and what the kernel reads
/// as "the empty name" is a newline.
fn write_attr(path: &Path, value: &str) -> Result<()> {
    fs::write(path, format!("{value}\n"))
        .with_context(|| format!("write {value:?} to {}", path.display()))
}

/// Takes the gadget off the controller, if it is on one.
///
/// It has to come off before it is changed: configfs will not add a function to
/// a configuration that is in use, and the bind at the far end of the change is
/// then refused with a `-EBUSY` that reaches nothing but the write's own return
/// value. A gadget that is already off is not an error — a run that ended badly
/// leaves one, and that is the state to carry on from.
fn unbind(gadget: &Path) -> Result<()> {
    let udc = gadget.join("UDC");
    let bound = fs::read_to_string(&udc).unwrap_or_default();
    if bound.trim().is_empty() {
        return Ok(());
    }
    tracing::info!("[cl-usb] taking {} off {}", gadget.display(), bound.trim());
    write_attr(&udc, "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_descriptors_say_what_the_kernel_reads_them_as() {
        let blob = descriptors();
        let word = |at: usize| u32::from_le_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);

        assert_eq!(word(0), DESCRIPTORS_MAGIC_V2);
        assert_eq!(word(4) as usize, blob.len(), "the length field covers the blob");
        assert_eq!(
            word(8),
            HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP,
            "the control recipient flag is what makes the AOA requests arrive here"
        );
        assert_eq!(word(12), 3, "three full speed descriptors");
        assert_eq!(word(16), 3, "three high speed descriptors");

        // Interface, endpoint out, endpoint in — each with its own length.
        let after_head = &blob[20..];
        assert_eq!(after_head[0], 9);
        assert_eq!(after_head[9], 7);
        assert_eq!(after_head[16], 7);
        assert_eq!(after_head[1], 4, "an interface descriptor");
        assert_eq!(after_head[10], 5, "an endpoint descriptor");
        assert_eq!(after_head[5], BULK_CLASS.0, "the class the car looks for");
        assert_eq!(after_head[6], BULK_CLASS.1);
        // The second copy is the high speed one, whose packets are larger.
        assert_eq!(&after_head[23..25], &[9, 4]);
        assert_eq!(&after_head[9 + 23 + 4..9 + 23 + 6], &512u16.to_le_bytes());
    }

    #[test]
    fn a_mount_that_is_not_there_is_not_claimed_to_be_a_mount() {
        assert!(!mount_is_ready(Path::new("/nonexistent/usb-ffs/carlife")));
    }

    #[test]
    fn the_empty_value_is_written_as_a_newline_rather_than_not_at_all() {
        // Unbinding is this call with an empty value, and it only works because
        // the newline makes it a write: an empty slice writes no bytes and
        // reaches no kernel at all, which is how the gadget stayed bound.
        let path = std::env::temp_dir().join("cp2cl-write-attr");
        write_attr(&path, "").expect("a plain file can be written");
        assert_eq!(fs::read_to_string(&path).unwrap(), "\n");

        write_attr(&path, "musb-hdrc.4.auto").expect("a plain file can be written");
        assert_eq!(fs::read_to_string(&path).unwrap(), "musb-hdrc.4.auto\n");

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn the_strings_blob_says_there_are_none() {
        let blob = strings();
        let word = |at: usize| u32::from_le_bytes([blob[at], blob[at + 1], blob[at + 2], blob[at + 3]]);

        assert_eq!(word(0), STRINGS_MAGIC, "the strings magic, not the descriptors one");
        assert_eq!(word(4) as usize, blob.len(), "the length field covers the blob");
        assert_eq!(word(8), 0, "no strings");
        assert_eq!(word(12), 0, "and so no languages either");
    }
}
