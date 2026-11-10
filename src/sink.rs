//! Where the phone's streams go once CarPlay has handed them over.
//!
//! One consumer takes them: the head unit on the other end of a CarLife session.
//! The phone's audio does not all go to one place — the music is the car's media
//! channel, the navigation prompt is its Navi TTS channel, and a call and the
//! voice assistant are its voice channel, the one that carries speech in both
//! directions. Which is which is decided by the name the phone gives the stream
//! in its SETUP — see `is_media`, `is_navigation` and `is_voice`.
//!
//! Nothing here interprets either half's protocol: an event from the CarPlay
//! crate is mapped onto a queue of the CarLife crate's, and what the phone is
//! playing is translated between the two crates' records of it.

use std::sync::{Arc, Mutex};

use carplay_hu::now_playing::NowPlaying;
use carplay_hu::sink::{StreamSink, StreamEvent, StreamId};
use carplay_hu::sink::{is_media, is_voice};
use tokio::sync::mpsc::UnboundedSender;

use crate::now_playing::{NowPlaying as Playing, Position, Track};

// The three shapes the car's channels carry, which belong to the CarLife half.
pub use carlife_md::media::{Media, Navi, Voice};

/// Which of the car's channels an audio stream feeds.
enum Role {
    Media,
    Navi,
    /// The car's speech channel, which is the one that runs both ways: a call
    /// and the voice assistant go down it, and the microphone comes back up it.
    Voice,
}

fn role(name: &str, two_way: bool) -> Role {
    // The microphone decides first: a stream the phone wants to hear on is a
    // call or the assistant, whatever it is labelled, and the music never is.
    if is_voice(name, two_way) {
        Role::Voice
    } else if is_media(name) {
        Role::Media
    } else {
        // Everything else the phone names an audio stream with is a prompt: see
        // `is_navigation`, which is written as exactly that.
        Role::Navi
    }
}

/// Whether one of the car's channels is being fed from this stream.
fn feeding(slot: &Mutex<Option<StreamId>>, stream: StreamId) -> bool {
    slot.lock().map(|held| *held == Some(stream)).unwrap_or(false)
}

/// Points a channel at a stream. A stream that opens replaces what the channel
/// was on, which is what the phone does when it moves its music to a new one.
fn feed(slot: &Mutex<Option<StreamId>>, stream: StreamId) {
    if let Ok(mut held) = slot.lock() {
        *held = Some(stream);
    }
}

/// Lets go of a channel if this is the stream feeding it, and says whether it
/// was — an instance that ends after its replacement took over has nothing to
/// say to the car.
fn starve(slot: &Mutex<Option<StreamId>>, stream: StreamId) -> bool {
    match slot.lock() {
        Ok(mut held) if *held == Some(stream) => {
            *held = None;
            true
        }
        _ => false,
    }
}

/// Where the CarPlay streams go when the car is the one reading them: the
/// queues are the CarLife half's, and what is kept here is the mapping from the
/// phone's streams onto them.
pub struct ClSink {
    queues: Arc<carlife_md::outlet::Sink>,
    /// Where the phone's cover art goes, as it arrived. `artwork::carry` takes it
    /// from here, makes it fit, and puts the result on the queue.
    covers: UnboundedSender<Vec<u8>>,
    /// Parameter sets from the newest codec description, put in front of the
    /// next frame. Held outside the queue deliberately: the queue can be full,
    /// and a dropped SPS/PPS costs everything that follows it.
    pending: Mutex<Vec<u8>>,
    /// Which instance of which stream each of the three audio channels is being
    /// fed from. A packet carries no name — only the stream's opening does — so
    /// this is what the ones that follow are measured against. They are kept
    /// apart so that one stream ending cannot take another channel with it.
    media_stream: Mutex<Option<StreamId>>,
    navi_stream: Mutex<Option<StreamId>>,
    voice_stream: Mutex<Option<StreamId>>,
}

impl ClSink {
    pub fn new(queues: Arc<carlife_md::outlet::Sink>, covers: UnboundedSender<Vec<u8>>) -> Self {
        Self {
            queues,
            covers,
            pending: Mutex::new(Vec::new()),
            media_stream: Mutex::new(None),
            navi_stream: Mutex::new(None),
            voice_stream: Mutex::new(None),
        }
    }
}

impl StreamSink for ClSink {
    fn push(&self, event: StreamEvent) {
        match event {
            // The car reads an elementary stream, so the parameter sets have to
            // be in it. There is nothing else in the event for the car's sake.
            StreamEvent::VideoConfig { parameter_sets } => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.clear();
                    for set in parameter_sets {
                        pending.extend_from_slice(&set);
                    }
                }
            }
            StreamEvent::VideoFrame { data, keyframe } => {
                let mut access_unit = self
                    .pending
                    .lock()
                    .map(|mut pending| std::mem::take(&mut *pending))
                    .unwrap_or_default();
                access_unit.extend_from_slice(&data);
                // Whether a decoder can start here goes with the picture: the
                // queue is what has to know it, when a picture arrives with no
                // room left.
                self.queues.video(access_unit, keyframe);
            }
            // Which of the car's channels a stream feeds is said once, when the
            // stream opens, by the name the phone gave it and by whether it
            // named a port for a microphone.
            StreamEvent::AudioConfig {
                stream,
                codec,
                name,
                two_way,
            } => match role(&name, two_way) {
                Role::Media => {
                    feed(&self.media_stream, stream);
                    // The format the car is told is the one the phone
                    // negotiated. Decoding does not change the rate or the
                    // channel count, so those are the two numbers the PCM under
                    // it arrives with; see `aac`.
                    self.queues.media(Media::Format {
                        rate: codec.rate(),
                        channels: codec.channels(),
                    });
                }
                Role::Navi => {
                    feed(&self.navi_stream, stream);
                    // Here decoding does change both: the prompt arrives as
                    // Opus at 48 kHz and the car is given 16 kHz mono. The
                    // codec on the event is what the car gets, not what the
                    // phone sends — see `airplay::audio::delivered`.
                    self.queues.navi(Navi::Format {
                        rate: codec.rate(),
                        channels: codec.channels(),
                    });
                }
                // The same two numbers as a prompt, and for the same reason: the
                // car's speech channels take 16 kHz mono whatever Opus carried.
                Role::Voice => {
                    feed(&self.voice_stream, stream);
                    self.queues.voice(Voice::Format {
                        rate: codec.rate(),
                        channels: codec.channels(),
                    });
                }
            },
            StreamEvent::AudioPacket {
                stream,
                timestamp_us,
                data,
            } => {
                let at = (timestamp_us / 1_000) as u32;
                if feeding(&self.media_stream, stream) {
                    self.queues.media(Media::Pcm {
                        timestamp_ms: at,
                        data,
                    });
                } else if feeding(&self.navi_stream, stream) {
                    self.queues.navi(Navi::Pcm {
                        timestamp_ms: at,
                        data,
                    });
                } else if feeding(&self.voice_stream, stream) {
                    self.queues.voice(Voice::Pcm {
                        timestamp_ms: at,
                        data,
                    });
                }
            }
            // One instance ended. If it is the one the channel is on, the
            // channel ends with it; if it is the one it replaced, the channel
            // has nothing to say, which is the whole reason an event names an
            // instance rather than a type.
            StreamEvent::AudioStopped { stream } => {
                if starve(&self.media_stream, stream) {
                    self.queues.media(Media::Ended);
                }
                if starve(&self.navi_stream, stream) {
                    self.queues.navi(Navi::Ended);
                }
                if starve(&self.voice_stream, stream) {
                    self.queues.voice(Voice::Ended);
                }
            }
            // The car learns nothing from this: it is the video channel that
            // tells it whether there is a picture. Both readers are on this side
            // of the bridge — a screen stream that is not open is a car with no
            // pictures, which is what the guide is for, and a keyframe can only
            // be asked for while one is open.
            StreamEvent::ScreenActive(open) => {
                self.queues.stream(open);
            }
        }
    }
}

impl carplay_hu::link::Panel for ClSink {
    /// What the phone is playing, said in the CarLife side's own terms: the two
    /// records carry the same fields, and the queues are the car's.
    ///
    /// The cover is the exception: it is handed to `artwork::carry` instead of
    /// the queue. A picture the car's message cannot carry has to be re-encoded
    /// first, which is tens of milliseconds of work, and the task that does it
    /// is not this one.
    fn playing(&self, item: NowPlaying) {
        let playing = match item {
            NowPlaying::Track(track) => Playing::Track(Track {
                source: track.source,
                song: track.song,
                artist: track.artist,
                album: track.album,
                duration_s: track.duration_s,
            }),
            NowPlaying::Position(position) => Playing::Position(Position {
                elapsed_ms: position.elapsed_ms,
                playing: position.playing,
            }),
            NowPlaying::Artwork(bytes) => {
                let _ = self.covers.send(bytes);
                return;
            }
        };
        self.queues.playing(playing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use carlife_md::outlet::Outlet;
    use carplay_hu::sink::AudioCodec;

    /// A sink whose covers go nowhere in particular, for the tests that are not
    /// about one.
    fn sink(outlet: &Outlet) -> Arc<ClSink> {
        let (covers, _) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(ClSink::new(outlet.sink(), covers))
    }

    /// One stream: a type the phone names it with, and the instance it opened.
    fn id(stream_type: u8, connection_id: i64) -> StreamId {
        StreamId {
            stream_type,
            connection_id,
        }
    }

    /// The phone mixes three different things onto one stream of events and only
    /// one of them is the car's media channel. A call is the car's voice channel,
    /// and the music playing under it is not the call.
    #[test]
    fn only_the_music_finds_its_way_to_the_media_channel() {
        let outlet = Outlet::new();
        let sink = sink(&outlet);
        let mut media = outlet.take_media().expect("nothing has taken it");
        let mut navi = outlet.take_navi().expect("nothing has taken it");
        let mut voice = outlet.take_voice().expect("nothing has taken it");

        let opened = |stream, name: &str| StreamEvent::AudioConfig {
            stream,
            codec: AudioCodec::Pcm {
                rate: 44_100,
                channels: 2,
            },
            name: name.to_string(),
            two_way: false,
        };
        let sound = |stream| StreamEvent::AudioPacket {
            stream,
            timestamp_us: 1_234_567,
            data: vec![0x11, 0x22],
        };

        sink.push(opened(id(100, 1), "telephony"));
        sink.push(sound(id(100, 1)));
        assert!(media.try_recv().is_err(), "a call is not the car's music");
        assert!(navi.try_recv().is_err(), "nor is it a prompt");
        assert!(
            matches!(
                voice.try_recv(),
                Ok(Voice::Format {
                    rate: 44_100,
                    channels: 2
                })
            ),
            "a call is the car's voice channel"
        );
        assert!(matches!(voice.try_recv(), Ok(Voice::Pcm { .. })));

        sink.push(opened(id(102, 2), "media"));
        sink.push(sound(id(102, 2)));
        assert!(
            matches!(
                media.try_recv(),
                Ok(Media::Format {
                    rate: 44_100,
                    channels: 2
                })
            ),
            "the car is told the format before the samples"
        );
        match media.try_recv() {
            Ok(Media::Pcm { timestamp_ms, data }) => {
                assert_eq!(timestamp_ms, 1_234);
                assert_eq!(data, vec![0x11, 0x22]);
            }
            other => panic!("expected sound on the media channel, got {other:?}"),
        }
    }

    #[test]
    fn a_session_takes_its_queues_empty() {
        let outlet = Outlet::new();
        let sink = sink(&outlet);
        let _video = outlet.take_video().expect("nothing has taken it");
        sink.push(StreamEvent::VideoFrame {
            keyframe: true,
            data: vec![1, 2, 3],
        });
        outlet.return_video();

        let next = outlet.take_video().expect("nothing has taken it");
        assert!(
            next.try_recv().is_none(),
            "the frame the last session left is not the next one's"
        );
    }

    /// A prompt is a stream of its own, and it goes to the car's other channel
    /// rather than through the music's — which is what lets sound come out of
    /// the navigation while the music keeps playing.
    #[test]
    fn a_prompt_goes_to_the_navigation_channel_and_the_music_carries_on() {
        let outlet = Outlet::new();
        let sink = sink(&outlet);
        let mut media = outlet.take_media().expect("nothing has taken it");
        let mut navi = outlet.take_navi().expect("nothing has taken it");

        sink.push(StreamEvent::AudioConfig {
            stream: id(102, 7),
            codec: AudioCodec::AacLc {
                rate: 48_000,
                channels: 2,
            },
            name: "media".into(),
            two_way: false,
        });
        // The prompt opens as its own type and its own instance, with the codec
        // it is delivered in rather than the one on the wire.
        sink.push(StreamEvent::AudioConfig {
            stream: id(100, 8),
            codec: AudioCodec::Pcm {
                rate: 16_000,
                channels: 1,
            },
            name: "default".into(),
            two_way: false,
        });
        sink.push(StreamEvent::AudioPacket {
            stream: id(100, 8),
            timestamp_us: 2_000_000,
            data: vec![0xaa],
        });

        assert!(
            matches!(
                navi.try_recv(),
                Ok(Navi::Format {
                    rate: 16_000,
                    channels: 1
                })
            ),
            "the prompt channel is told what its samples are"
        );
        assert!(matches!(navi.try_recv(), Ok(Navi::Pcm { data, .. }) if data == vec![0xaa]));
        // The music is still where it was: opening a prompt says nothing on the
        // media channel at all.
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));
        assert!(media.try_recv().is_err(), "the prompt is not the music");
    }

    /// The phone re-opens a type for every track change, so an instance that
    /// ends can be one the channel has already moved off — and then the channel
    /// has nothing to say. Ending the music whenever an older instance of it
    /// says goodbye is what used to stop the sound for good.
    #[test]
    fn an_instance_that_was_replaced_cannot_end_the_channel() {
        let outlet = Outlet::new();
        let sink = sink(&outlet);
        let mut media = outlet.take_media().expect("nothing has taken it");

        let opened = |connection| StreamEvent::AudioConfig {
            stream: id(102, connection),
            codec: AudioCodec::AacLc {
                rate: 48_000,
                channels: 2,
            },
            name: "media".into(),
            two_way: false,
        };
        sink.push(opened(3));
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));

        // A newer instance of the same type takes the channel over.
        sink.push(opened(4));
        assert!(matches!(media.try_recv(), Ok(Media::Format { .. })));

        // The one it replaced is torn down late. Nothing is said to the car.
        sink.push(StreamEvent::AudioStopped { stream: id(102, 3) });
        assert!(
            media.try_recv().is_err(),
            "the teardown of a replaced instance is not the end of the music"
        );

        // And the music still reaches it.
        sink.push(StreamEvent::AudioPacket {
            stream: id(102, 4),
            timestamp_us: 1_000,
            data: vec![0x55],
        });
        assert!(matches!(media.try_recv(), Ok(Media::Pcm { .. })));

        // The instance the channel is actually on is a different matter.
        sink.push(StreamEvent::AudioStopped { stream: id(102, 4) });
        assert!(matches!(media.try_recv(), Ok(Media::Ended)));
    }

    /// The stream's own state is what the session reads — for the guide it shows
    /// while there are no pictures, and for the keyframe it can ask for — and
    /// the sink is what sees the stream open and close.
    #[test]
    fn the_screen_stream_is_open_for_exactly_as_long_as_it_is() {
        let outlet = Outlet::new();
        let sink = sink(&outlet);
        let streaming = outlet.streaming();
        let open = || *streaming.borrow();

        assert!(!open(), "nothing is open yet");
        sink.push(StreamEvent::ScreenActive(true));
        assert!(open());
        sink.push(StreamEvent::ScreenActive(false));
        assert!(!open(), "a stream that closed is not open");
    }

    /// What the phone says about the song is the same five fields on both sides
    /// of the bridge, and the panel is where the car's copy is written: the two
    /// records differ in name only. The cover is the one thing that does not go
    /// straight to the queue — it is the program's to size first.
    #[test]
    fn what_the_phone_is_playing_becomes_the_cars_own_record() {
        let outlet = Outlet::new();
        let (covers, mut arriving) = tokio::sync::mpsc::unbounded_channel();
        let sink = Arc::new(ClSink::new(outlet.sink(), covers));
        let mut queue = outlet.take_playing().expect("nothing has taken it");

        carplay_hu::link::Panel::playing(
            &*sink,
            NowPlaying::Track(carplay_hu::now_playing::Track {
                source: Some("Music".into()),
                song: Some("Reeperbahn".into()),
                artist: Some("Hamburg".into()),
                album: Some("Stadt".into()),
                duration_s: Some(239),
            }),
        );
        match queue.try_recv() {
            Ok(Playing::Track(track)) => {
                assert_eq!(track.song.as_deref(), Some("Reeperbahn"));
                assert_eq!(track.duration_s, Some(239));
            }
            other => panic!("expected the song, got {other:?}"),
        }

        carplay_hu::link::Panel::playing(
            &*sink,
            NowPlaying::Position(carplay_hu::now_playing::Position {
                elapsed_ms: 42_000,
                playing: Some(true),
            }),
        );
        assert!(matches!(
            queue.try_recv(),
            Ok(Playing::Position(Position {
                elapsed_ms: 42_000,
                playing: Some(true)
            }))
        ));

        carplay_hu::link::Panel::playing(&*sink, NowPlaying::Artwork(vec![0xff, 0xd8]));
        assert_eq!(
            arriving.try_recv().expect("the cover, as it arrived"),
            vec![0xff, 0xd8]
        );
        assert!(
            queue.try_recv().is_err(),
            "the queue is given a cover that fits, not the phone's bytes"
        );
    }
}
