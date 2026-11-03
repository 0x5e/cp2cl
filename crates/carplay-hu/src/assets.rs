//! What the binary carries besides code: the channel table, the icons, the page.
//!
//! This half reads them but does not own them — they are compiled into the
//! program, and the program hands them over once at startup.

use std::sync::OnceLock;

/// A table of files by the name each has inside the program's asset directory.
pub trait Assets: Send + Sync {
    fn get(&self, name: &str) -> Option<&'static [u8]>;

    /// The same, read as text: the channel table.
    fn text(&self, name: &str) -> Option<&'static str> {
        self.get(name)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }
}

/// Installed once, before the receiver accepts anything.
static ASSETS: OnceLock<&'static dyn Assets> = OnceLock::new();

/// Hands the table over. A second call is ignored: there is one binary.
pub fn install(assets: &'static dyn Assets) {
    let _ = ASSETS.set(assets);
}

fn assets() -> &'static dyn Assets {
    #[cfg(test)]
    {
        *ASSETS.get_or_init(|| REPOSITORY)
    }
    #[cfg(not(test))]
    {
        *ASSETS
            .get()
            .expect("the assets are installed before the first session")
    }
}

/// The bytes of one file, by the name it has inside the asset directory.
pub fn get(name: &str) -> Option<&'static [u8]> {
    assets().get(name)
}

/// The same, read as text.
pub fn text(name: &str) -> Option<&'static str> {
    assets().text(name)
}

/// The files under the repository's `assets/`, so that a test which reads the
/// channel table or a picture runs without the program that hands them over.
#[cfg(test)]
struct Repository;

#[cfg(test)]
const REPOSITORY: &dyn Assets = &Repository;

#[cfg(test)]
impl Assets for Repository {
    fn get(&self, name: &str) -> Option<&'static [u8]> {
        const FILES: &[(&str, &[u8])] = &[
            (
                "carlife_channel_map.json",
                include_bytes!("../../../assets/carlife_channel_map.json"),
            ),
            (
                "icons/Default-256.png",
                include_bytes!("../../../assets/icons/Default-256.png"),
            ),
            (
                "icons/Default-512.png",
                include_bytes!("../../../assets/icons/Default-512.png"),
            ),
            (
                "icons/Mercedes-256.png",
                include_bytes!("../../../assets/icons/Mercedes-256.png"),
            ),
            (
                "icons/Mercedes-512.png",
                include_bytes!("../../../assets/icons/Mercedes-512.png"),
            ),
        ];
        FILES
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, bytes)| *bytes)
    }
}
