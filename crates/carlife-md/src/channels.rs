//! What the channel table says about the panel behind a head unit's number.
//!
//! The table itself belongs to the binary — it is a JSON file built in, and the
//! CarPlay half reads the same entries for `/info`. The host looks an entry up
//! and hands the result here.

/// The density a panel is measured at when the table names none.
pub const DEFAULT_PPI: u32 = 120;

/// A size and rate a head unit's make is served at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// One entry of the table, as the CarLife half uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelEntry {
    /// The size and rate the phone is asked for, when the entry names one.
    pub preset: Option<Preset>,
    /// The density the panel's millimetres are worked out at.
    pub ppi: u32,
}

impl Default for ChannelEntry {
    fn default() -> Self {
        Self {
            preset: None,
            ppi: DEFAULT_PPI,
        }
    }
}

/// Pixels as millimetres, at a density. Nothing in the protocol carries a
/// physical size, so this is derived rather than measured.
pub fn millimetres(pixels: u32, ppi: u32) -> u32 {
    (pixels as u64 * 254 / (ppi.max(1) as u64 * 10)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panel_with_no_entry_is_the_default_density() {
        assert_eq!(ChannelEntry::default().ppi, DEFAULT_PPI);
        assert!(ChannelEntry::default().preset.is_none());
    }

    #[test]
    fn millimetres_are_pixels_over_a_density() {
        // 1280 px at 160 dpi is 8 inches, which is 203 mm.
        assert_eq!(millimetres(1280, 160), 203);
        assert_eq!(millimetres(1280, 0), millimetres(1280, 1), "a zero is a one");
    }
}
