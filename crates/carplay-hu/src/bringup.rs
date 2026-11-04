//! The iAP2 session, on whichever link the phone arrived on.
//!
//! Three links run this same conversation — the cable, the Bluetooth channel a
//! wireless phone is paged onto, and the data stream it moves that session onto
//! afterwards — so the event loop lives here once rather than in any of them.
//! The links themselves are `crate::wired` and `crate::wireless`.

use std::sync::Arc;

use livi_runtime::bringup::BringupEvent;
use livi_runtime::driver::ArtworkRx;
use tokio::sync::{broadcast, mpsc, watch};

use crate::link::Panel;
use crate::now_playing::{NowPlaying, Position, Track};
use crate::report::Report;

/// iAP2's `NowPlayingUpdate`, which is what the phone describes a song with. It
/// is subscribed to with everything else at the start of a session — see LIVI's
/// `bringup::subscriptions` — and arrives as one more incoming message.
const NOW_PLAYING: u16 = 0x5001;

/// Why a bringup ended, which is what says whether to start another.
pub(crate) enum Ended {
    /// The phone left, or the session failed. There is nothing to restart.
    Finished,
    /// What we render changed, and the phone can only be told by starting
    /// CarPlay over — it reads `/info` once per session and never asks again.
    Restart,
}

/// Prints the progress of one iAP2 bringup, and says how it ended.
///
/// Three links run this same conversation — the cable, the Bluetooth channel a
/// wireless phone is paged onto, and the data stream it moves that session onto
/// afterwards — and they differ only in which events they can produce, so the
/// loop lives here once. The name is the same for all three: this is the iAP2
/// session, and `transport` is what says which link it is running on, since a
/// wireless phone runs it twice and the log would otherwise say the same thing
/// twice with nothing to tell the two apart. `ssid` is the network a wireless
/// session tells the phone to join; the wired one never has that step and
/// passes `None`.
///
/// `changed` fires when what the phone should render has changed, which the
/// caller turns into a new session.
///
/// `panel` is where what the phone says about itself goes: the bringup hands on
/// every incoming message, and these are the ones this side acts on rather than
/// leaves alone.
///
/// `status` is told how far the conversation got, which is the only place any of
/// these steps is known: see `crate::report`.
///
/// `shutdown` fires when the car's session ends. Ending the iAP2 session with
/// it is what lets the link that owns the slot give it back: a phone left on the
/// cable after the car has gone is not a CarPlay session to keep running.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_bringup(
    peer: &str,
    transport: &str,
    airplay_port: u32,
    ssid: Option<&str>,
    mut rx: mpsc::Receiver<BringupEvent>,
    changed: &mut watch::Receiver<u64>,
    shutdown: &mut broadcast::Receiver<()>,
    panel: &Arc<dyn Panel>,
    status: &Report,
) -> Ended {
    loop {
        let event = tokio::select! {
            event = rx.recv() => event,
            signal = changed.changed() => {
                return match signal {
                    // Any change ends it: one new session covers them all.
                    Ok(()) => Ended::Restart,
                    // Every sender is gone, so nothing can ask for a restart
                    // again; ending is both correct and the only way to stop
                    // this branch from firing forever.
                    Err(_) => Ended::Finished,
                };
            }
            // The car went away. This is neither a restart nor a phone that
            // left on its own: there is nothing to start over for.
            _ = shutdown.recv() => return Ended::Finished,
        };
        match event {
            Some(BringupEvent::Identified) => {
                status.with(|s| s.carplay.identified = true);
                tracing::info!("carplay.iap2.identified peer={peer} transport={transport}");
            }
            Some(BringupEvent::Authenticated) => {
                status.with(|s| s.carplay.authenticated = true);
                tracing::info!("carplay.iap2.mfi_passed peer={peer} transport={transport}");
            }
            Some(BringupEvent::Subscribed) => {
                status.with(|s| s.carplay.subscribed = true);
                tracing::info!("carplay.iap2.subscriptions_sent peer={peer} transport={transport}");
            }
            Some(BringupEvent::WifiConfigSent) => {
                if let Some(ssid) = ssid {
                    tracing::info!(
                        "carplay.iap2.wifi_config_sent peer={peer} transport={transport} \
                         ssid={ssid:?}"
                    );
                }
            }
            Some(BringupEvent::CarPlayStartSent { ip }) => {
                status.with(|s| s.carplay.advertised = Some(format!("{ip}:{airplay_port}")));
                tracing::info!(
                    "carplay.iap2.carplay_start_sent peer={peer} transport={transport} \
                     airplay={ip}:{airplay_port}"
                );
            }
            Some(BringupEvent::Failed(reason)) => {
                status.with(|s| s.carplay.failed = Some(reason.to_string()));
                tracing::error!(
                    "carplay.iap2.session_failed peer={peer} transport={transport} \
                     reason={reason:?}"
                );
                return Ended::Finished;
            }
            Some(BringupEvent::Closed) => {
                tracing::info!("carplay.iap2.session_closed peer={peer} transport={transport}");
                return Ended::Finished;
            }
            // What the phone is playing, which the car's panel is owed. It is
            // one of the messages the bringup hands on rather than acts on, and
            // the only one of them that goes any further than the log.
            Some(BringupEvent::Incoming {
                msg_id: NOW_PLAYING,
                frame,
            }) => match Track::from_iap2(&frame) {
                // The phone describes one attribute at a time, so this is the
                // line per message rather than per song: what it just said, not
                // what is known. `cl.media_info` is the other end of it.
                //
                // It also sends this message for things the car has no field
                // for — whether it is playing, where it is in the song — and
                // those carry none of the four. A line saying nothing is worse
                // than no line, so only the ones with something in them are
                // printed.
                //
                // `debug` rather than `info`: an app that writes its lyrics
                // into the title has the phone describing one every few
                // seconds, and at `info` that is most of the log.
                Some(track) => {
                    if let Some(line) = said(&track) {
                        tracing::debug!("carplay.now_playing {line}");
                    }
                    panel.playing(NowPlaying::Track(track));
                    // Where in the song it is rides on the same message but
                    // goes to the car as a message of its own: see
                    // `NowPlaying::Position`.
                    if let Some(position) = Position::from_iap2(&frame) {
                        panel.playing(NowPlaying::Position(position));
                    }
                }
                // A song this side cannot read is one the panel is never told
                // about, and nothing further down would say so.
                None => tracing::warn!(
                    "carplay.now_playing_dropped bytes={} reason=decode_failed",
                    frame.len()
                ),
            },
            Some(_) => {}
            // The bringup task ended without saying so, which only happens when
            // it was torn down; there is nothing left to watch.
            None => return Ended::Finished,
        }
    }
}

/// The fields one `NowPlayingUpdate` carried, as key=value pairs.
///
/// Only the ones it mentioned: a field it left out is one it did not change,
/// and printing all five would make every line the same shape and none of them
/// say anything. The phone sends a handful of these per song, so it is `debug`.
///
/// `None` when it mentioned none of them, which the phone does for the messages
/// that are about what it is doing rather than what it is playing.
fn said(track: &Track) -> Option<String> {
    let mut parts: Vec<String> = [
        ("song", track.song.as_deref()),
        ("artist", track.artist.as_deref()),
        ("album", track.album.as_deref()),
        ("source", track.source.as_deref()),
    ]
    .iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name}={value:?}")))
    .collect();
    if let Some(seconds) = track.duration_s {
        parts.push(format!("duration_s={seconds}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// The phone's cover art, as it lands, on its way to the car's panel.
///
/// It is its own file transfer on the link rather than part of the message that
/// named the song, so this is where the two are put back together — as far as
/// they can be: the file says nothing about which song it belongs to, so the
/// CarLife side takes it as belonging to the newest one, and holds it until a
/// song turns up to attach it to.
///
/// The bytes are handed on as the phone sent them. What the car's message can
/// carry, and what to show when the phone sent nothing, are the program's to
/// decide: they are facts about the other end of the bridge, which this half
/// does not have.
pub(crate) async fn carry_artwork(mut artwork: ArtworkRx, panel: Arc<dyn Panel>) {
    while let Some(image) = artwork.recv().await {
        panel.playing(NowPlaying::Artwork(image));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Where a carried cover ends up: a list of its own, standing in for the
    /// panel on the other side of the bridge.
    #[derive(Default)]
    struct Collected(Mutex<Vec<NowPlaying>>);

    impl Panel for Collected {
        fn playing(&self, item: NowPlaying) {
            self.0.lock().expect("a cover").push(item);
        }
    }

    /// The car going away ends an iAP2 session that is still waiting on the
    /// phone. Without this the wired link keeps its slot after the car has
    /// gone, and neither link can offer CarPlay to the next phone.
    #[tokio::test]
    async fn the_car_going_away_ends_the_bringup() {
        let (_events, rx) = mpsc::channel(8);
        let (_restart, mut changed) = watch::channel(0u64);
        let (stop, mut shutdown) = broadcast::channel(1);
        let panel: Arc<dyn Panel> = Arc::new(Collected::default());
        let status = Report::new();

        stop.send(()).expect("the bringup is listening");
        let ended = run_bringup(
            "peer",
            "usb",
            7000,
            None,
            rx,
            &mut changed,
            &mut shutdown,
            &panel,
            &status,
        )
        .await;
        assert!(matches!(ended, Ended::Finished));
    }

    /// The line says what the phone said, and only that: an update that
    /// mentions one field is one pair, not five with four of them empty. One
    /// that mentions none is no line at all.
    #[test]
    fn a_line_says_only_what_the_phone_said() {
        let update = Track {
            song: Some("So What".into()),
            duration_s: Some(545),
            ..Track::default()
        };
        assert_eq!(
            said(&update).as_deref(),
            Some("song=\"So What\" duration_s=545")
        );
        assert_eq!(said(&Track::default()), None);
    }

    /// Every cover the phone sends is handed on as it arrived, whatever it is:
    /// what the car's message can carry is decided past this half.
    #[tokio::test]
    async fn a_cover_is_handed_on_as_it_arrived() {
        let panel = Arc::new(Collected::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let carrying = tokio::spawn(carry_artwork(rx, panel.clone()));

        // Three bytes of a JPEG, and something that is no picture at all: both
        // are the phone's to send and neither is this half's to judge.
        let small = vec![0xff, 0xd8, 0xff];
        tx.send(small.clone()).expect("a cover");
        tx.send(vec![0x42; 40 * 1024]).expect("a cover");

        drop(tx);
        carrying.await.expect("the task ends with the link");

        let carried = panel.0.lock().expect("a cover").clone();
        assert_eq!(
            carried,
            vec![
                NowPlaying::Artwork(small),
                NowPlaying::Artwork(vec![0x42; 40 * 1024])
            ],
            "the bytes went on as they came"
        );
    }
}
