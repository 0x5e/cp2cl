//! What the phone says it is playing, read off the iAP2 messages it says it in.
//!
//! The phone describes one attribute at a time, so a record built from one
//! message is a fragment: `Track::merge` is what folds a run of them into the
//! song the car's panel is shown, and that lives on the other side of the
//! bridge. Nothing here decides what a song is.

use iap2_csm::messages::now_playing::{NowPlayingUpdate, PlaybackStatus};
use iap2_csm::CsmMessage;

/// What the phone said about the song.
///
/// A field is `None` until the phone has mentioned it, which is not the same as
/// a field the phone has said is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    /// The app it is playing from, which is what the page shows as the source.
    pub source: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds. The phone sends milliseconds, and this is the unit the car's
    /// message carries.
    pub duration_s: Option<i32>,
}

impl Track {
    /// The phone's `NowPlayingUpdate` — iAP2's 0x5001 — as the fields the panel
    /// is given.
    ///
    /// `None` when the frame is not one: the message is the phone's, and a
    /// message this side cannot read says nothing about the song.
    pub fn from_iap2(frame: &[u8]) -> Option<Self> {
        let update = NowPlayingUpdate::decode(frame).ok()?;
        let mut track = Self::default();
        if let Some(item) = update.media_item_attributes {
            track.song = item.title;
            track.artist = item.artist;
            track.album = item.album;
            track.duration_s = item.duration_ms.map(|ms| (ms / 1_000) as i32);
        }
        if let Some(playback) = update.playback_attributes {
            track.source = playback.app_name;
        }
        Some(track)
    }
}

/// Where in the song the phone says it is, which is what the panel's bar is
/// drawn from.
///
/// The phone reports this with the play state and with every change of it, and
/// never on a clock: what it says is true at the moment it arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    /// Milliseconds into the song when the phone said so.
    pub elapsed_ms: u32,
    /// Whether the song was moving through it, when the phone said so.
    ///
    /// `None` when the message gave a position without a play state: that is a
    /// message about where the song is and not about whether it moved.
    pub playing: Option<bool>,
}

impl Position {
    /// The playback half of the phone's `NowPlayingUpdate`, when it says where
    /// the song is.
    ///
    /// `None` for a message about the song rather than about its playback: the
    /// phone sends one of these for each attribute it changes, and a new title
    /// is not a position.
    pub fn from_iap2(frame: &[u8]) -> Option<Self> {
        let update = NowPlayingUpdate::decode(frame).ok()?;
        let play = update.playback_attributes?;
        Some(Self {
            elapsed_ms: play.elapsed_ms?,
            // A drag is a song that is not moving through its own time, so it
            // is not playing either: the bar follows the drag when it is over.
            playing: play
                .status
                .map(|status| matches!(status, PlaybackStatus::Playing)),
        })
    }
}

/// One thing the phone said, on its way to the car's panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NowPlaying {
    /// What the phone said about the song, as far as it said it.
    Track(Track),
    /// Where in it the song was playing.
    Position(Position),
    /// The cover art, as the phone sent the file. It arrives after the update
    /// that named the song and names none itself, so it belongs to whichever
    /// song is newest.
    Artwork(Vec<u8>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use iap2_csm::messages::now_playing::{MediaItemAttributes, PlaybackAttributes};

    /// An update carrying the fields it is given and nothing else, which is how
    /// the phone sends them.
    fn update(
        item: Option<MediaItemAttributes>,
        playback: Option<PlaybackAttributes>,
    ) -> Vec<u8> {
        NowPlayingUpdate {
            media_item_attributes: item,
            playback_attributes: playback,
        }
        .encode()
    }

    fn item(title: Option<&str>, artist: Option<&str>, album: Option<&str>) -> MediaItemAttributes {
        MediaItemAttributes {
            persistent_id: None,
            title: title.map(str::to_string),
            duration_ms: None,
            album: album.map(str::to_string),
            artist: artist.map(str::to_string),
            album_artist: None,
            genre: None,
            artwork_ftid: None,
        }
    }

    #[test]
    fn one_message_from_the_phone_is_the_five_fields_the_car_has() {
        let frame = update(
            Some(MediaItemAttributes {
                duration_ms: Some(239_400),
                ..item(Some("Reeperbahn"), Some("Hamburg"), Some("Stadt"))
            }),
            Some(PlaybackAttributes {
                status: Some(PlaybackStatus::Playing),
                elapsed_ms: Some(42_000),
                app_name: Some("Music".into()),
                app_bundle_id: None,
            }),
        );

        let track = Track::from_iap2(&frame).expect("a now-playing update");
        assert_eq!(track.source.as_deref(), Some("Music"));
        assert_eq!(track.song.as_deref(), Some("Reeperbahn"));
        assert_eq!(track.artist.as_deref(), Some("Hamburg"));
        assert_eq!(track.album.as_deref(), Some("Stadt"));
        // The car's field is in seconds; the phone's is in milliseconds.
        assert_eq!(track.duration_s, Some(239));
    }

    /// What a frame that is not one of the phone's says about the song is
    /// nothing, rather than a song with no name.
    #[test]
    fn a_frame_that_is_not_a_now_playing_update_reads_as_nothing() {
        assert_eq!(Track::from_iap2(&[0x40, 0x40, 0x00, 0x06, 0x99, 0x99]), None);
        assert_eq!(Track::from_iap2(&[]), None);
    }

    /// Where the song is comes from the playback half of the same message, and
    /// a message about the song alone carries none of it.
    #[test]
    fn a_position_is_read_from_the_playback_half() {
        let playing = update(
            None,
            Some(PlaybackAttributes {
                status: Some(PlaybackStatus::Playing),
                elapsed_ms: Some(42_000),
                app_name: None,
                app_bundle_id: None,
            }),
        );
        assert_eq!(
            Position::from_iap2(&playing),
            Some(Position {
                elapsed_ms: 42_000,
                playing: Some(true),
            })
        );

        // A paused song has a position and is not moving through it.
        let paused = update(
            None,
            Some(PlaybackAttributes {
                status: Some(PlaybackStatus::Paused),
                elapsed_ms: Some(42_000),
                app_name: None,
                app_bundle_id: None,
            }),
        );
        assert_eq!(
            Position::from_iap2(&paused).map(|p| p.playing),
            Some(Some(false))
        );

        // A position with no play state says where the song is and not whether
        // it moved, which is not the same as saying it stopped.
        let placed = update(
            None,
            Some(PlaybackAttributes {
                status: None,
                elapsed_ms: Some(42_000),
                app_name: None,
                app_bundle_id: None,
            }),
        );
        assert_eq!(
            Position::from_iap2(&placed),
            Some(Position {
                elapsed_ms: 42_000,
                playing: None,
            })
        );

        // A song, and a play state with no position in it, are both silent
        // about where the song is.
        assert_eq!(
            Position::from_iap2(&update(Some(item(Some("Stern"), None, None)), None)),
            None
        );
        assert_eq!(
            Position::from_iap2(&update(
                None,
                Some(PlaybackAttributes {
                    status: Some(PlaybackStatus::Playing),
                    elapsed_ms: None,
                    app_name: Some("Music".into()),
                    app_bundle_id: None,
                }),
            )),
            None
        );
    }
}
