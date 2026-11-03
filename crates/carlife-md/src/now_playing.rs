//! What the car's panel is told the phone is playing.
//!
//! The car takes the whole song in one message (`MSG_CMD_MEDIA_INFO`), the
//! position in another (`MSG_CMD_MEDIA_PROGRESS_BAR`) and the cover as a file of
//! its own. `Track` is the record both sides work from: the CarPlay half reads
//! one of the phone's updates into it and `merge` folds it into what is known,
//! which is what turns a series of partial statements into one song.
//!
//! Three of the car's nine fields have nothing behind them and are never
//! written: the playlist's length, the song's id and the play mode. The phone is
//! not subscribed for the first two, and iAP2 has no message for the third. An
//! absent field and a zero are not the same claim — a zero would claim
//! single-track repeat.

/// What is known about the song that is playing.
///
/// A field is `None` until the phone has mentioned it, which is not the same as
/// a field the phone has said is empty. A new song with no album is left showing
/// the old album's name only if the phone did not mention one; if it said the
/// album is empty, the panel is told that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    /// The app it is playing from, which the page shows as the song's source.
    /// The car is not given it: see `crate::proto::media_info`.
    pub source: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds, which is the unit the car's field is in. The phone sends
    /// milliseconds.
    pub duration_s: Option<i32>,
}

impl Track {
    /// One update from the phone folded into what is known.
    ///
    /// What the phone left out keeps what it had, because it sends only what
    /// changed: a pause is not a song with no name. What it did send replaces,
    /// empty or not, because a title that has changed is another song.
    pub fn merge(&mut self, update: Self) {
        let Self {
            source,
            song,
            artist,
            album,
            duration_s,
        } = update;
        if source.is_some() {
            self.source = source;
        }
        if song.is_some() {
            self.song = song;
        }
        if artist.is_some() {
            self.artist = artist;
        }
        if album.is_some() {
            self.album = album;
        }
        if duration_s.is_some() {
            self.duration_s = duration_s;
        }
    }

    /// Whether there is a song here to tell the car about.
    ///
    /// The phone sends the app it is playing from with every change of play
    /// state, and an app name is not a song: what the car's message is, is the
    /// song its panel shows, and a panel with no song on it is worse than one
    /// the car never updated.
    pub fn is_silent(&self) -> bool {
        self.song.as_deref().unwrap_or_default().is_empty()
    }
}

/// Where in the song the phone says it is playing, which the car draws as a
/// progress bar.
///
/// The phone reports this with the play state and with every change of it, and
/// never on a clock: what it says is true at the moment it arrives, so what is
/// handed on is the reading and the CarLife side adds the time since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Milliseconds into the song when the phone said so.
    pub elapsed_ms: u32,
    /// Whether the song was moving through it, when the phone said so. A paused
    /// song is at the same second a minute later, and a bar that moved for it
    /// would be wrong.
    ///
    /// `None` when the message gave a position without a play state: that is a
    /// message about where the song is and not about whether it moved, and the
    /// song is still doing what the last one said.
    pub playing: Option<bool>,
}

/// One thing the phone said, on its way to the car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NowPlaying {
    /// What the phone said about the song, as far as it said it.
    Track(Track),
    /// Where in it the song was playing.
    Position(Position),
    /// The cover art, as the phone sent the file. It arrives after the update
    /// that named the song and names none itself, so it belongs to whichever
    /// song is newest — which is what the CarLife side holds it for.
    Artwork(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The phone sends one attribute at a time, so what it left out is what it
    /// has already said — and what it sends replaces, empty or not.
    #[test]
    fn an_update_keeps_what_it_did_not_mention_and_replaces_what_it_did() {
        let mut track = Track {
            source: Some("Music".into()),
            song: Some("Reeperbahn".into()),
            artist: Some("Hamburg".into()),
            album: Some("Stadt".into()),
            duration_s: Some(239),
        };

        // A title and nothing else: the song changed, the rest of it did not.
        track.merge(Track {
            song: Some("Stern".into()),
            ..Track::default()
        });
        assert_eq!(track.song.as_deref(), Some("Stern"));
        assert_eq!(track.artist.as_deref(), Some("Hamburg"));
        assert_eq!(track.duration_s, Some(239));

        // An empty album is the phone saying there is none, not saying nothing.
        track.merge(Track {
            album: Some(String::new()),
            ..Track::default()
        });
        assert_eq!(track.album.as_deref(), Some(""));

        // And the play state on its own mentions no attribute at all.
        track.merge(Track::default());
        assert_eq!(track.song.as_deref(), Some("Stern"));
    }

    /// The app name is not a song, so an update carrying only that is not worth
    /// a message to the panel.
    #[test]
    fn an_app_name_on_its_own_is_not_a_song() {
        let track = Track {
            source: Some("Music".into()),
            ..Track::default()
        };
        assert!(track.is_silent());

        let track = Track {
            song: Some("Reeperbahn".into()),
            ..track
        };
        assert!(!track.is_silent());
    }
}
