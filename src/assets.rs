//! What the binary carries besides code: the two pictures of our icon, the
//! channel table and the status page.
//!
//! All of it is compiled in — `build.rs` includes every file under `assets/` and
//! writes one table of name and bytes — so the executable is the whole program.
//! Nothing is unpacked to a temporary directory at startup, and the board needs
//! no files beside it.
//!
//! A name is a path inside `assets/`, with `/` between the parts:
//! `icons/Mercedes-256.png`, `web/index.html`.

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The bytes of one file, by the name it has inside `assets/`.
///
/// `None` is a name nothing was built with. That is not a runtime condition on a
/// shipped binary — the table is fixed at build time — so callers that know the
/// name is there say so rather than carrying a branch for it.
pub fn get(name: &str) -> Option<&'static [u8]> {
    FILES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, bytes)| *bytes)
}

/// The same, read as text: the page, its stylesheet, the channel table.
pub fn text(name: &str) -> Option<&'static str> {
    get(name).and_then(|bytes| std::str::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four things that have to be in the binary: what `/info` puts on the
    /// phone's screen, what decides the label above it, and the page.
    #[test]
    fn what_the_program_cannot_run_without_is_built_in() {
        for name in [
            "icons/Default-256.png",
            "icons/Default-512.png",
            "icons/Mercedes-256.png",
            "icons/Mercedes-512.png",
        ] {
            let png = get(name).unwrap_or_else(|| panic!("{name} is built in"));
            assert_eq!(&png[..4], b"\x89PNG", "{name} is a PNG");
        }
        assert!(text("carlife_channel_map.json").is_some());
        assert!(text("web/index.html").is_some());
        assert!(text("web/vendor/weui.min.css").is_some());
    }

    /// A name that is not there is a name that was never built in, not a panic.
    #[test]
    fn a_name_nobody_built_with_is_no_bytes() {
        assert!(get("nothing-like-this.png").is_none());
        assert!(text("nothing-like-this.png").is_none());
    }
}
