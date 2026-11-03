//! What the CarLife half knows about the machine it runs on.
//!
//! Nothing here is read from `/etc` or from the device tree: the program that
//! owns the board reads those and hands the values over.

use std::path::{Path, PathBuf};

/// A size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// The screen the settings page picked, every field optional: `None` is the
/// car's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScreenOverride {
    /// Both halves or neither.
    pub size: Option<Size>,
    pub fps: Option<u32>,
}

impl ScreenOverride {
    /// Whether anything at all is set.
    pub fn is_empty(&self) -> bool {
        self.size.is_none() && self.fps.is_none()
    }
}

/// `1920x720@60`, the shape the channel table writes a size in.
impl std::fmt::Display for ScreenOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.size {
            Some(r) => write!(f, "{}x{}", r.width, r.height)?,
            None => f.write_str("default")?,
        }
        match self.fps {
            Some(fps) => write!(f, "@{fps}"),
            None => f.write_str("@default"),
        }
    }
}

/// Everything the CarLife half is told about the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    /// The screen the operator picked. `main` reads it at startup and the gadget
    /// again for each session, through `Delegate::screen_chosen`.
    pub screen: ScreenOverride,
    /// The name the device goes by: the machine's hostname.
    pub name: String,
    /// The board's maker, from the device tree.
    pub vendor: String,
    /// The Bluetooth controller's own address.
    pub bt_address: String,
    /// Where the firmware installs this half's credentials.
    pub data_dir: PathBuf,
}

impl Default for Machine {
    /// What a machine that cannot say anything about itself reports.
    fn default() -> Self {
        Self {
            screen: ScreenOverride::default(),
            name: "cp2cl".to_string(),
            vendor: "Unknown".to_string(),
            bt_address: "00:11:22:33:44:55".to_string(),
            data_dir: PathBuf::new(),
        }
    }
}

impl Machine {
    /// One of the files that live in `data_dir`, by its bare name.
    pub fn data_file(&self, name: &str) -> PathBuf {
        Path::new(&self.data_dir).join(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_screen_is_written_the_way_the_table_writes_one() {
        let full = ScreenOverride {
            size: Some(Size {
                width: 1920,
                height: 720,
            }),
            fps: Some(60),
        };
        assert_eq!(full.to_string(), "1920x720@60");
        assert_eq!(ScreenOverride::default().to_string(), "default@default");
        assert!(ScreenOverride::default().is_empty());
        assert!(!full.is_empty());
    }

    #[test]
    fn a_file_is_named_beside_the_others() {
        let machine = Machine {
            data_dir: PathBuf::from("/etc/cp2cl"),
            ..Machine::default()
        };
        assert_eq!(
            machine.data_file("carlife_md_rsa_private.pem"),
            PathBuf::from("/etc/cp2cl/carlife_md_rsa_private.pem")
        );
    }
}
