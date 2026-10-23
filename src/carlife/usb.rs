//! The USB device side, owned by this program: the gadget in configfs, the
//! FunctionFS instance the accessory interface runs on, the descriptors written
//! into its control endpoint, and the bind to the controller.
//!
//! This half used to belong to the board. It is here because the device has two
//! states — a phone, and the accessory a head unit asks it to become — and moving
//! between them means taking the device off the bus and putting it back, which
//! only code that owns it can do. What the board keeps is its radio; see README,
//! "Setting the board up".
//!
//! Nothing here answers anything: what arrives on the endpoint this opens is
//! `gadget`'s business.

use std::ffi::CString;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use super::desc::{self, Identity};

/// Where configfs keeps gadgets, and the name this program's has in it.
const CONFIGFS: &str = "/sys/kernel/config/usb_gadget";
const NAME: &str = "cp2cl";
/// The FunctionFS instance. It is mounted by name and the endpoint files are
/// named after it, so the three have to agree.
const FUNCTION: &str = "carlife";
const MOUNTPOINT: &str = "/dev/usb-ffs/carlife";
/// What the device asks of the host, in milliamps. The board is not powered from
/// this port in every installation, so it asks for little.
const MAX_POWER_MA: u16 = 250;
/// A gadget some vendor images load onto this port at boot, which takes the
/// controller. Nothing to do where it is not loaded.
const SQUATTER: &str = "g_mass_storage";
/// Where the controller is named. Every board that can present a USB device has
/// one; a board with two would present two.
const CONTROLLER_DIR: &str = "/sys/class/udc";
/// How long the device stays off the bus between its two forms.
///
/// The pull-up is dropped and raised from a work queue, and the host has seen
/// nothing until it has actually gone down. Coming back inside that window is a
/// device the host never saw leave: it stops enumerating anything, and the
/// device that came back sits there unconfigured. So this gap is not a pause —
/// it is the disconnect the host is waiting for. The host's own side of an AOA
/// switch allows for seconds.
const REATTACH: Duration = Duration::from_millis(500);

/// The gadget this program composed, by path and by controller. Cheap to copy:
/// it holds a path and a name.
#[derive(Clone)]
pub struct Usb {
    root: PathBuf,
    controller: String,
}

/// The three endpoint files of the accessory interface.
///
/// Which is which is not a matter of taste: FunctionFS creates one file per
/// endpoint descriptor in the order the descriptors were written, so `ep1` is
/// the first — the one the host reads from, which this program writes to — and
/// `ep2` is the second. `desc` has the descriptor order and why it is that way.
pub struct Endpoints {
    pub control: fs::File,
    pub from_car: fs::File,
    pub to_car: fs::File,
}

impl Usb {
    /// Composes a gadget, describes the accessory interface in it, and binds it.
    ///
    /// Whatever an earlier run left is taken down first: a gadget is composed as
    /// a whole, and one that is bound cannot be reconfigured in place.
    pub fn open(identity: &Identity) -> Result<(Self, Endpoints)> {
        if !Path::new(CONFIGFS).is_dir() {
            return Err(anyhow!(
                "{CONFIGFS} is not there: configfs has to be mounted for a USB device to be \
                 composed, and this kernel builds gadgets only through it"
            ));
        }
        Self::clear()?;
        Self::evict_squatter()?;

        let usb = Self {
            root: Path::new(CONFIGFS).join(NAME),
            controller: controller()?,
        };

        // Before the gadget is composed: the port has to be a device by the time
        // one is put on it.
        usb.force_peripheral()?;

        fs::create_dir_all(&usb.root)
            .with_context(|| format!("creating {}", usb.root.display()))?;
        usb.write_identity(identity)?;
        // One configuration, and one function in it. The name `c.1` is not
        // written down anywhere else.
        let strings = usb.root.join("configs/c.1/strings/0x409");
        fs::create_dir_all(&strings)
            .with_context(|| format!("creating {}", strings.display()))?;
        usb.attribute(&strings.join("configuration"), &identity.serial)?;
        usb.attribute(&usb.root.join("configs/c.1/MaxPower"), &MAX_POWER_MA.to_string())?;

        // The FunctionFS function, linked into the configuration. Left empty on
        // purpose: what goes into it is written below, and the instance is not
        // registered — and the controller will not take the gadget — until both
        // of those writes have landed.
        let function = usb.function_dir();
        fs::create_dir_all(&function)
            .with_context(|| format!("creating {}", function.display()))?;
        let link = usb.root.join("configs/c.1").join(function_name());
        symlink(&function, &link)
            .with_context(|| format!("linking {} to {}", link.display(), function.display()))?;

        // A mount point outlives the mount in /dev, but not a cold boot.
        fs::create_dir_all(MOUNTPOINT).with_context(|| format!("creating {MOUNTPOINT}"))?;
        mount_functionfs()?;

        let control = endpoint("ep0")?;
        // Descriptors, then strings. FunctionFS hands the first to the kernel and
        // waits for the second before the instance counts as registered.
        (&control)
            .write_all(&desc::descriptors())
            .with_context(|| format!("writing descriptors into {MOUNTPOINT}/ep0"))?;
        (&control)
            .write_all(&desc::strings())
            .with_context(|| format!("writing the strings blob into {MOUNTPOINT}/ep0"))?;

        // And only now the endpoints the descriptors asked for.
        let to_car = endpoint("ep1")?;
        let from_car = endpoint("ep2")?;

        usb.bind()?;
        tracing::info!(
            "usb.device controller={} identity={:?}",
            usb.controller,
            identity.describe()
        );

        Ok((
            usb,
            Endpoints {
                control,
                from_car,
                to_car,
            },
        ))
    }

    /// Takes the device off the bus, changes what it calls itself, and puts it
    /// back.
    ///
    /// This is what a `start accessory` request asks for: not a different device,
    /// but the same one coming back in its new form, which is the only way a host
    /// gets to see a device it did not enumerate. The endpoint files stay open
    /// across it — FunctionFS keeps them, and the descriptors do not change.
    ///
    /// Calling it on a device that is already bound is why the unbind comes
    /// first: a bound gadget cannot be bound again, and its numbers cannot be
    /// rewritten while it is.
    pub async fn reidentify(&self, identity: &Identity) -> Result<()> {
        self.unbind()?;
        self.write_identity(identity)?;
        // See REATTACH: the device has to be gone for long enough that the host
        // notices, and this is the only moment it is.
        tokio::time::sleep(REATTACH).await;
        self.bind()
    }

    /// Takes the gadget off the controller. An unbound gadget is not an error
    /// here: this is called both when there is something to unbind and when there
    /// is not.
    pub fn unbind(&self) -> Result<()> {
        // A line, because an empty write never reaches the kernel: the write
        // syscall with no bytes at all returns before the filesystem sees it.
        // configfs reads the empty line as "off the controller".
        match fs::write(self.root.join("UDC"), b"\n") {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::ENODEV) => Ok(()),
            Err(e) => Err(e).with_context(|| format!("unbinding {}", self.root.display())),
        }
    }

    /// Puts the gadget on the controller, which is what makes a host see it.
    pub fn bind(&self) -> Result<()> {
        match fs::write(self.root.join("UDC"), format!("{}\n", self.controller)) {
            Ok(()) => Ok(()),
            // A controller takes one gadget. The usual reason there is another
            // is a gadget left by an arrangement that came before this one.
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                let who = match holder() {
                    Some(name) => format!(" by {name}"),
                    None => String::new(),
                };
                Err(anyhow!(
                    "{} is already held{who}: a gadget from an earlier arrangement is the usual \
                     reason, and a reboot clears it",
                    self.controller
                ))
            }
            Err(e) => Err(e).with_context(|| {
                format!(
                    "binding the gadget to {} (dmesg has what the controller said)",
                    self.controller
                )
            }),
        }
    }

    /// Writes the numbers and the strings a host reads out of the device
    /// descriptor. None of them says what the device is: that comes from the
    /// descriptors in `desc`, which stay the same in both forms.
    fn write_identity(&self, identity: &Identity) -> Result<()> {
        self.attribute(&self.root.join("idVendor"), &format!("{:#06x}", identity.vid))?;
        self.attribute(&self.root.join("idProduct"), &format!("{:#06x}", identity.pid))?;
        self.attribute(&self.root.join("bcdUSB"), "0x0200")?;

        let strings = self.root.join("strings/0x409");
        fs::create_dir_all(&strings)
            .with_context(|| format!("creating {}", strings.display()))?;
        self.attribute(&strings.join("serialnumber"), &identity.serial)?;
        self.attribute(&strings.join("manufacturer"), &identity.manufacturer)?;
        self.attribute(&strings.join("product"), &identity.product)?;
        Ok(())
    }

    /// One configfs attribute, written the way a shell would write it. The
    /// newline is what makes it a line rather than a value with something
    /// appended, and one attribute reads an empty line as a command: `bind` and
    /// `unbind` below are the same write.
    fn attribute(&self, path: &Path, value: &str) -> Result<()> {
        fs::write(path, format!("{value}\n"))
            .with_context(|| format!("writing {value:?} into {}", path.display()))
    }

    fn function_dir(&self) -> PathBuf {
        self.root.join("functions").join(function_name())
    }

    /// The controller the gadget is bound to, as the kernel names it. Reported
    /// rather than acted on: it is the one thing about the device that comes
    /// from the board rather than from this program.
    pub fn controller(&self) -> &str {
        &self.controller
    }

    /// What the controller says the link is doing, for a line in the log.
    ///
    /// Both halves are the board's answer rather than this program's, and both
    /// are what a car that enumerated and then went quiet has to be read
    /// against: `state` says whether it got as far as setting a configuration,
    /// and `speed` says which of the two descriptor sets it is talking to — a
    /// device that came up at full speed is one a head unit may refuse even
    /// though every control request is answered. Read per call rather than kept,
    /// since both change with the host's own state.
    pub fn link(&self) -> String {
        let dir = Path::new(CONTROLLER_DIR).join(&self.controller);
        let field = |name: &str| {
            fs::read_to_string(dir.join(name))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        };
        format!("speed={} state={}", field("current_speed"), field("state"))
    }

    /// Makes the controller a device, rather than a port that decides for itself.
    ///
    /// The board's device tree says `dr_mode = "otg"`, which leaves the choice to
    /// the ID pin on PG12 — and nothing drives that pin, so musb sits in
    /// `OTG_STATE_UNDEFINED` (its `mode` reads `(null)`), no pull-up is raised,
    /// and a host looking at the port sees nothing plugged in.
    ///
    /// `mode` is musb's own attribute for this, and `peripheral` is the runtime
    /// form of the device tree's other answer: the sunxi glue hands it to the
    /// PHY, which then stops consulting the pin and reports a device because it
    /// was told to. A board whose tree already says `peripheral` takes the write
    /// and changes nothing, which is why it is done unconditionally.
    fn force_peripheral(&self) -> Result<()> {
        let link = Path::new(CONTROLLER_DIR).join(&self.controller);
        // The attribute belongs to the controller rather than to the gadget: the
        // class link points into the controller's directory, a level or two below.
        let mode = fs::canonicalize(link).ok().and_then(|dir| {
            dir.ancestors()
                .take(4)
                .map(|dir| dir.join("mode"))
                .find(|path| path.is_file())
        });
        let Some(mode) = mode else {
            // Not a musb controller, or a kernel that offers no choice. There is
            // nothing here to undo, and a board that needs none is the usual
            // reason to find nothing.
            tracing::debug!("usb.mode_forced result=no_attribute");
            return Ok(());
        };
        fs::write(&mode, "peripheral\n")
            .with_context(|| format!("writing peripheral into {}", mode.display()))?;
        tracing::info!("usb.mode_forced mode=peripheral path={}", mode.display());
        Ok(())
    }

    /// Takes down whatever is at this gadget's name, and any mount of the
    /// instance it may have left behind.
    ///
    /// A gadget is all or nothing, and this program is the only thing that
    /// composes this one: starting from an empty tree is what makes a restart
    /// behave like a cold boot.
    fn clear() -> Result<()> {
        let root = Path::new(CONFIGFS).join(NAME);
        if !root.is_dir() {
            return Ok(());
        }
        // Off the controller first, so the endpoints the mount is holding stop
        // being used, and so the tree below can be removed at all.
        let _ = fs::write(root.join("UDC"), b"\n");
        unmount_functionfs();
        empty(&root)?;
        // What has to work: a gadget takes the groups configfs made for it with
        // it, so this is the one removal whose failure means something is still
        // in the way, and the one that has to be reported where it happens.
        fs::remove_dir(&root).with_context(|| {
            format!(
                "removing {}: a gadget goes only once everything in it is gone, and \
                 something this program left behind is not",
                root.display()
            )
        })?;
        tracing::debug!("usb.cleanup result=ok source=previous_run");
        Ok(())
    }

    /// Some vendor images load a mass-storage gadget onto the OTG port at boot,
    /// which holds the controller. It has to go before a gadget of ours can bind,
    /// and nothing else on a board like this is using it.
    fn evict_squatter() -> Result<()> {
        let modules = fs::read_to_string("/proc/modules").unwrap_or_default();
        if !modules
            .lines()
            .any(|line| line.split_whitespace().next() == Some(SQUATTER))
        {
            return Ok(());
        }
        tracing::warn!("usb.squatter_unload module={SQUATTER}");
        match std::process::Command::new("modprobe")
            .args(["-r", SQUATTER])
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(anyhow!("modprobe -r {SQUATTER} failed: {status}")),
            Err(e) => Err(anyhow!("modprobe -r {SQUATTER} could not be run: {e}")),
        }
    }
}

/// The endpoints of a mounted instance, both directions of each asked for:
/// FunctionFS decides from the direction of the call which way an endpoint runs,
/// and a file opened for one of them is refused when it is used the other way.
fn endpoint(name: &str) -> Result<fs::File> {
    let path = format!("{MOUNTPOINT}/{name}");
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {path}"))
}

/// The one file that says which gadget is on the controller, when one is.
fn holder() -> Option<String> {
    let entries = fs::read_dir(CONFIGFS).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let bound = fs::read_to_string(entry.path().join("UDC")).unwrap_or_default();
        if !bound.trim().is_empty() {
            return Some(name);
        }
    }
    None
}

/// The controller to bind to: the board presents one, and a board with two
/// presents two.
fn controller() -> Result<String> {
    let mut names: Vec<String> = fs::read_dir(CONTROLLER_DIR)
        .with_context(|| format!("reading {CONTROLLER_DIR}"))?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("this board presents no USB device controller"))
}

/// Mounts the FunctionFS instance. The kernel made the instance when the
/// function directory was created; the mount is what makes its endpoints usable.
fn mount_functionfs() -> Result<()> {
    let (source, target, kind) = (c(FUNCTION)?, c(MOUNTPOINT)?, c("functionfs")?);
    // Safe: three strings that outlive the call, and nothing in the data block.
    let rc = unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("mounting {FUNCTION} on {MOUNTPOINT}"));
    }
    Ok(())
}

/// Unmounts the instance, saying nothing when there is nothing to unmount: a
/// board that never had one and a run that stopped cleanly are both unremarkable.
///
/// Detached rather than plain, so that a mount left behind by a run that was
/// killed — with files still open on it — can be taken away as well.
fn unmount_functionfs() {
    let Ok(target) = c(MOUNTPOINT) else { return };
    // Safe: the path only has to outlive the call.
    let rc = unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    if rc != 0 {
        let e = io::Error::last_os_error();
        // EINVAL: nothing is mounted there. ENOENT: there is not even a path.
        if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) {
            tracing::warn!("usb.umount_failed path={MOUNTPOINT} err={e}");
        }
    }
}

/// Empties a configfs directory of everything that can be taken out of it.
///
/// Only links and directories can be removed; the attributes beside them — and
/// configfs is mostly attributes — go with the directory that holds them, and an
/// unlink of one is refused.
///
/// Not every directory can be removed either. A gadget has four configfs made
/// for it — `functions`, `configs`, `strings`, `os_desc` — and every config has
/// a `strings` of its own; `configfs_rmdir` answers `EPERM` for anything it made
/// itself, and a function may have an `os_desc` of the same kind. So a refused
/// rmdir is not reported: those are not being left behind, they go when the
/// directory holding them does. A refusal that does matter leaves a directory in
/// the way of the rmdir `clear` ends with, and that one says so.
fn empty(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("reading {}", path.display()))? {
        let entry = entry.with_context(|| format!("reading {}", path.display()))?;
        let child = entry.path();
        let kind = entry
            .file_type()
            .with_context(|| format!("reading the type of {}", child.display()))?;
        if kind.is_dir() {
            empty(&child)?;
            if let Err(e) = fs::remove_dir(&child) {
                tracing::debug!("usb.rmdir_refused path={} err={e}", child.display());
            }
        } else if kind.is_symlink() {
            fs::remove_file(&child).with_context(|| format!("removing {}", child.display()))?;
        }
    }
    Ok(())
}

/// One of the path's few C strings.
fn c(value: &str) -> Result<CString> {
    CString::new(value).map_err(|e| anyhow!("{value} has a NUL in it: {e}"))
}

fn function_name() -> String {
    format!("ffs.{FUNCTION}")
}
