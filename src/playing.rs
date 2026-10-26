//! What the phone says it is playing, on its way to the car's panel.
//!
//! Two shapes of one thing meet here. The phone describes a song over iAP2 —
//! `NowPlayingUpdate`, one attribute per message, and the cover art as a file
//! transfer of its own — so what arrives is a series of partial statements about
//! the same song. The car's panel is told the whole of it in a single message,
//! `MSG_CMD_MEDIA_INFO`, whose fields are the song, the artist, the album, its
//! length and where it is playing from.
//!
//! `Track` is the record both sides work from: `Track::from_iap2` reads one of
//! the phone's updates and `merge` folds it into what is known, which is what
//! turns a series of partial statements into one song. The queue it travels on is
//! in `sink`, beside the two stream channels, because a CarLife session takes it
//! and gives it back the same way.
//!
//! Three of the car's nine fields have nothing behind them and are never written:
//! the playlist's length, the song's id and the play mode. The phone is not
//! subscribed for the first two — see LIVI's `bringup::subscriptions`, where
//! `persistent_id` and `app_bundle_id` are the two flags left false — and iAP2
//! has no message for the third at all. What the car's parser makes of a field
//! that is absent is the same empty value it would have been handed anyway, so
//! leaving them out claims nothing; a zero would claim single-track repeat.
//!
//! The cover art is the fourth thing that is not a field: it is a file of its
//! own, it names no song, and `Playing::Artwork` is how it is handed over.
//!
//! Where in the song it is playing is the fifth, and the car has a message of
//! its own for it — `MSG_CMD_MEDIA_PROGRESS_BAR` — so it travels as
//! `Playing::Position` rather than as another field of the song.

use iap2_csm::messages::now_playing::{NowPlayingUpdate, PlaybackStatus};
use iap2_csm::CsmMessage;

/// What is known about the song that is playing.
///
/// A field is `None` until the phone has mentioned it, which is not the same as
/// a field the phone has said is empty. A new song with no album is left showing
/// the old album's name only if the phone did not mention one; if it said the
/// album is empty, the panel is told that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Track {
    /// The app it is playing from, which the car shows as the song's source.
    pub source: Option<String>,
    pub song: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Seconds, which is the unit the car's field is in. The phone sends
    /// milliseconds.
    pub duration_s: Option<i32>,
}

impl Track {
    /// The phone's `NowPlayingUpdate` — iAP2's 0x5001 — as the car's fields.
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

/// One thing the phone said, on its way to the car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playing {
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
    use iap2_csm::messages::now_playing::{
        MediaItemAttributes, PlaybackAttributes, PlaybackStatus,
    };

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
        track.merge(
            Track::from_iap2(&update(Some(item(Some("Stern"), None, None)), None))
                .expect("a now-playing update"),
        );
        assert_eq!(track.song.as_deref(), Some("Stern"));
        assert_eq!(track.artist.as_deref(), Some("Hamburg"));
        assert_eq!(track.duration_s, Some(239));

        // An empty album is the phone saying there is none, not saying nothing.
        track.merge(
            Track::from_iap2(&update(Some(item(None, None, Some(""))), None))
                .expect("a now-playing update"),
        );
        assert_eq!(track.album.as_deref(), Some(""));

        // And the play state on its own mentions no attribute at all.
        track.merge(
            Track::from_iap2(&update(
                None,
                Some(PlaybackAttributes {
                    status: Some(PlaybackStatus::Paused),
                    elapsed_ms: Some(0),
                    app_name: None,
                    app_bundle_id: None,
                }),
            ))
            .expect("a now-playing update"),
        );
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
