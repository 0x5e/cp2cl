//! Everything the operator can tune, read once from the environment.
//!
//! Nothing here is required: the defaults build a working wired session and play
//! it with ffplay. See README for the full table.

use std::path::{Path, PathBuf};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

fn env_num<T: std::str::FromStr>(key: &str, default: T) -> T {
    env(key).and_then(|s| s.parse().ok()).unwrap_or(default)
}

/// `1280x720` → `(1280, 720)`. Anything unparseable falls back to `fallback`.
fn env_size(key: &str, fallback: (u32, u32)) -> (u32, u32) {
    env(key)
        .and_then(|s| {
            let (w, h) = s.split_once(['x', 'X', '*'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .unwrap_or(fallback)
}

/// Where the media is played. Two ffplay processes, one per stream.
#[derive(Debug, Clone)]
pub struct Player {
    /// `ffplay`
    pub program: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Directory holding identity.pk8 + certificate.p7b (the MFi credentials).
    pub auth_dir: Option<PathBuf>,
    pub player: Player,
    /// What the phone is asked to render. 1280x720 is the safe CarPlay default.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Screen size in millimetres, which is how CarPlay decides its dpi — and
    /// therefore how finely it draws. This is the whole difference between a
    /// sharp picture and a blurry one: MacPlay reports the window's real share
    /// of the panel and gets ~254 dpi, while inventing a large value drops it to
    /// ~162 and the phone quietly renders at 1×.
    pub width_mm: u32,
    pub height_mm: u32,
    /// Video window size. SDL measures this in **points** on a HiDPI display, so
    /// it defaults to half the logical size: one stream pixel per physical
    /// pixel. `CP2AA_WINDOW` overrides it.
    pub window: (u32, u32),
    /// Advertise HEVC. Video then arrives as hvcC / H.265 NALs.
    pub hevc: bool,
    /// Advertised as deviceID / bluetoothIDs. The phone only wants something
    /// stable and shaped like a MAC; it never resolves these.
    pub device_id: String,
    pub bt_mac: String,
    /// Accessory name shown on the phone.
    pub name: String,
    /// Linux only: which USB network gadget carries the airplay stream.
    /// Auto-detected when unset.
    pub av_iface: Option<String>,
    pub log_filter: String,
}

impl Config {
    pub fn from_env() -> Self {
        // Named first so the window and the physical size can default to them.
        let width = env_num("CP2AA_WIDTH", 1280);
        let height = env_num("CP2AA_HEIGHT", 720);
        let width_mm = env_num("CP2AA_WIDTH_MM", 0);
        Self {
            auth_dir: Self::resolve_auth_dir(),
            player: Player { program: PathBuf::from(env_or("CP2AA_PLAYER", "ffplay")) },
            width,
            height,
            // Half, because the window is measured in points and the stream in
            // pixels: that is what puts one stream pixel on each physical pixel.
            window: env_size("CP2AA_WINDOW", (width / 2, height / 2)),
            fps: env_num("CP2AA_FPS", 60),
            width_mm,
            // Left to `info.rs` to derive from the aspect ratio when zero.
            height_mm: env_num("CP2AA_HEIGHT_MM", 0),
            // HEVC by default: at the bitrate the phone sends, H.265 is visibly
            // sharper than H.264. MacPlay reports `codec=h265` for the same
            // session, which is where the difference in clarity comes from.
            hevc: std::env::var("CP2AA_HEVC")
                .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            device_id: env_or("CP2AA_DEVICE_ID", "00:11:22:33:44:55"),
            bt_mac: env_or("CP2AA_BT_MAC", "00:11:22:33:44:55"),
            name: env_or("CP2AA_NAME", "cp2aa"),
            av_iface: env("CP2AA_AV_IFACE"),
            log_filter: env_or("CP2AA_LOG", "info"),
        }
    }

    /// Look for credentials next to the binary, then in the working directory,
    /// then wherever CP2AA_AUTH_DIR points. Same idea as LIVI's resolveAuthDir,
    /// minus Electron: order is explicit override first, then a couple of
    /// sensible spots so `cargo run` works from a clone with credentials
    /// dropped in `authentication/`.
    fn resolve_auth_dir() -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(dir) = env("CP2AA_AUTH_DIR") {
            candidates.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("authentication"));
            }
        }
        candidates.push(PathBuf::from("authentication"));
        candidates.push(PathBuf::from("assets/authentication"));
        candidates.into_iter().find(|dir| Self::has_credentials(dir))
    }

    fn has_credentials(dir: &Path) -> bool {
        dir.join("identity.pk8").is_file() && dir.join("certificate.p7b").is_file()
    }
}
