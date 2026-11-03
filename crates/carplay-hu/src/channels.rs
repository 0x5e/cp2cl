//! The channel table: which head unit answers on which number, and what the
//! panel behind that number is.
//!
//! `assets/carlife_channel_map.json` is one entry per make, holding the channel
//! numbers that make answers on. A make with several numbers is one entry with
//! several `channels`, which is why the table is not keyed by the number — each
//! reader below keys what it takes by the number itself.
//!
//! Read twice, once for the labels and once for the sizes to serve, and each
//! reader keeps only its own field: an entry may name a label, a size, or both.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The label under our icon when no channel has named one.
pub const DEFAULT_LABEL: &str = "App";

/// The density a panel is taken to have when the channel table says nothing about
/// it: 120 pixels per inch, which is what the 8-inch panels these head units
/// carry work out to.
///
/// The density is what decides how finely the phone draws: the physical size it is
/// told is turned back into a dpi, and inventing too large a size drops it to 1×
/// and the picture goes blurry. The size in millimetres follows from this and the
/// canvas, because no CarLife message carries one. A make that knows its own says
/// so in its entry.
pub const DEFAULT_PPI: u32 = 120;

/// A size and rate a head unit's make is served at, from the `video` field of its
/// entry in the channel table: `"1920x720@30"`.
///
/// The field is for a head unit whose own CarLife application asks for a smaller
/// screen than the panel it drives. The phone is then asked for these numbers
/// instead of the ones the car named, and the car scales what it receives to the
/// panel — which is what it does with a size it did not ask for anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// What the channel table says about the panel behind a head unit's number.
///
/// Both fields are optional and neither implies the other: a make may name the
/// size to serve, the density of its panel, or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelEntry {
    /// The size and rate the phone is asked for, when the entry names one.
    pub preset: Option<Preset>,
    /// The density the panel's millimetres are worked out at: the entry's own
    /// `ppi` field, or [`DEFAULT_PPI`] where it names none.
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

/// The table as it is written, parsed.
fn map_entries() -> Vec<serde_json::Value> {
    let table = crate::assets::text("carlife_channel_map.json")
        .expect("the channel map is built into the binary");
    serde_json::from_str(table).expect("the channel map is valid JSON")
}

/// The channel number a head unit reports, and the label worth showing for it.
///
/// For the makes we ship a picture for, the entry also carries the `label` the
/// phone draws under the icon. Only entries that name one are kept: `code` names
/// the head unit on the page and is not part of this answer.
static CHANNEL_LABELS: OnceLock<HashMap<String, String>> = OnceLock::new();

fn channel_labels() -> &'static HashMap<String, String> {
    CHANNEL_LABELS.get_or_init(|| {
        let mut labels = HashMap::new();
        for entry in map_entries() {
            // No label, or a blank one, is a make with no picture of its own.
            let label = entry["label"].as_str().unwrap_or("").trim();
            if label.is_empty() {
                continue;
            }
            let channels = entry["channels"].as_array().into_iter().flatten();
            for id in channels.filter_map(|channel| channel["id"].as_str()) {
                labels.insert(id.trim().to_string(), label.to_string());
            }
        }
        labels
    })
}

/// The label the phone shows under our icon for `channel`.
pub fn label(channel: Option<&str>) -> &'static str {
    channel
        .and_then(|c| channel_labels().get(c.trim()))
        .map_or(DEFAULT_LABEL, String::as_str)
}

/// The same table, keyed by the channel number, holding only the numbers that say
/// something about their panel. The fields belong to the number rather than to
/// the make: one make answers on several numbers, and only some of them are wrong
/// about their screen.
static CHANNEL_ENTRIES: OnceLock<HashMap<String, ChannelEntry>> = OnceLock::new();

fn channel_entries() -> &'static HashMap<String, ChannelEntry> {
    CHANNEL_ENTRIES.get_or_init(|| {
        let mut entries = HashMap::new();
        for entry in map_entries() {
            let channels = entry["channels"].as_array().into_iter().flatten();
            for channel in channels {
                let Some(id) = channel["id"].as_str() else {
                    continue;
                };
                let preset = channel["video"].as_str().and_then(parse_preset);
                let ppi = channel["ppi"]
                    .as_u64()
                    .and_then(|density| u32::try_from(density).ok())
                    .filter(|density| *density > 0);
                if preset.is_none() && ppi.is_none() {
                    continue;
                }
                let entry = ChannelEntry {
                    preset,
                    ppi: ppi.unwrap_or(DEFAULT_PPI),
                };
                entries.insert(id.trim().to_string(), entry);
            }
        }
        entries
    })
}

/// What the table says about the panel behind `channel`. A number it does not
/// hold, or no number at all, is the default: 120 pixels per inch, and the size
/// the car itself asked for.
pub fn of(channel: Option<&str>) -> ChannelEntry {
    channel
        .and_then(|number| channel_entries().get(number.trim()))
        .copied()
        .unwrap_or_default()
}

/// A `video` field as the table writes it: `"1920x720@30"`.
///
/// One that does not parse is a preset that is not there rather than a failure:
/// the table is written by hand, and a line that is wrong should cost the one
/// make it is wrong for and nothing else. `every_field_the_table_carries_parses`
/// is what says so before it ships, because a typo here looks like nothing
/// happening.
fn parse_preset(text: &str) -> Option<Preset> {
    let (size, fps) = text.trim().split_once('@')?;
    let (width, height) = size.split_once(['x', 'X'])?;
    let preset = Preset {
        width: width.trim().parse().ok()?,
        height: height.trim().parse().ok()?,
        fps: fps.trim().parse().ok()?,
    };
    (preset.width > 0 && preset.height > 0 && preset.fps > 0).then_some(preset)
}

/// Pixels to millimetres at a density, in integer arithmetic: 25.4 millimetres to
/// the inch. A density of zero would be a division by zero, so it is read as one
/// pixel per inch — a physical size that large is what tells the phone to draw
/// at 1×, which is what asking for it means.
pub(crate) fn millimetres(pixels: u32, ppi: u32) -> u32 {
    (pixels as u64 * 254 / (ppi.max(1) as u64 * 10)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel with a label of its own gets that label; every other channel,
    /// and no channel at all, gets the default. The label belongs to the make, so
    /// every number the make answers on gets it — not just the first.
    #[test]
    fn a_channel_names_its_own_label() {
        for channel in ["20062100", "20062101", "20062104"] {
            assert_eq!(label(Some(channel)), "Mercedes", "{channel} is a Mercedes");
        }
        assert_eq!(label(Some(" 20062100 ")), "Mercedes");
        assert_eq!(label(Some("20062106")), DEFAULT_LABEL, "not a number it answers on");
        assert_eq!(label(Some("20012100")), DEFAULT_LABEL, "a make with no label");
        assert_eq!(label(None), DEFAULT_LABEL);
    }

    /// A number the table gives a size or a density to is served at those, and one
    /// it says nothing about gets the default size — whatever the car asked for —
    /// and 120 dpi. The fields belong to the number, so another number of the same
    /// make is not covered by them.
    #[test]
    fn a_number_names_the_panel_behind_it() {
        let mercedes = Preset {
            width: 1920,
            height: 720,
            fps: 30,
        };
        assert_eq!(
            of(Some("20062104")),
            ChannelEntry {
                preset: Some(mercedes),
                ppi: 120
            }
        );
        assert_eq!(
            of(Some(" 20062104 ")),
            of(Some("20062104")),
            "the number is trimmed as it is everywhere else"
        );
        assert_eq!(
            of(Some("20062100")),
            ChannelEntry::default(),
            "the same make, a number with nothing of its own"
        );
        assert_eq!(of(Some("20062106")), ChannelEntry::default(), "not a number it answers on");
        assert_eq!(of(None), ChannelEntry::default());
        assert_eq!(
            ChannelEntry::default(),
            ChannelEntry {
                preset: None,
                ppi: DEFAULT_PPI
            }
        );

        assert_eq!(parse_preset("1920x720@30"), Some(mercedes));
        assert_eq!(parse_preset(" 1920 X 720 @ 30 "), Some(mercedes));
        for bad in ["1920x720", "1920x720@", "x720@30", "1920x@30", "1920x720@0", ""] {
            assert_eq!(parse_preset(bad), None, "{bad:?} is not a size and a rate");
        }
    }

    /// Every field the table carries has to be the thing it claims to be. These
    /// are the mistakes in a hand-written table that look like nothing happening:
    /// the field is there, it is not read, and the head unit is served what it
    /// asked for at the default density.
    #[test]
    fn every_field_the_table_carries_parses() {
        let mut fields = 0;
        for entry in map_entries() {
            let channels = entry["channels"].as_array().into_iter().flatten();
            for channel in channels {
                let id = channel["id"].as_str().unwrap_or_default();
                if let Some(field) = channel["video"].as_str() {
                    fields += 1;
                    assert!(
                        parse_preset(field).is_some(),
                        "{id} names a size that is not one: {field:?}"
                    );
                }
                if let Some(density) = channel.get("ppi") {
                    fields += 1;
                    let usable = density
                        .as_u64()
                        .and_then(|density| u32::try_from(density).ok())
                        .is_some_and(|density| density > 0);
                    assert!(usable, "{id} names a density that is not one: {density}");
                }
            }
        }
        assert!(fields > 0, "at least one number names something");
    }

    /// The table is edited by hand, and the two mistakes that cost something are
    /// silent ones: a make with no number answers nothing, and a number listed
    /// twice is answered by whichever entry came last. Both fail here instead.
    #[test]
    fn a_channel_number_belongs_to_one_make() {
        let table = crate::assets::text("carlife_channel_map.json")
            .expect("the channel map is built into the binary");
        let entries: Vec<serde_json::Value> =
            serde_json::from_str(table).expect("the channel map is valid JSON");

        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            let code = entry["code"].as_str().unwrap_or_default().trim();
            assert!(!code.is_empty(), "every entry names a code");
            let channels = entry["channels"]
                .as_array()
                .unwrap_or_else(|| panic!("{code} lists its channels"));
            assert!(!channels.is_empty(), "{code} answers on a number");
            for channel in channels {
                let id = channel["id"].as_str().unwrap_or_default().trim();
                assert!(!id.is_empty(), "{code} has a number that is not blank");
                assert!(seen.insert(id.to_string()), "{id} is listed under two makes");
            }
        }
    }
}
