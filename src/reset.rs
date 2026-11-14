//! The way back from a pairing that is out of step: every phone this machine is
//! paired with is unpaired, and the wired link is re-plugged.
//!
//! It lives here rather than next to either caller because there are two of
//! them — a hold on the board's button and a button on the settings page — and
//! they have to do the same thing. `reason` is the only difference: it names the
//! caller in the one line that says a replug was asked for.

use tokio::sync::mpsc;

/// Why a reset did not do what was asked.
pub enum Refusal {
    /// BlueZ could not be asked to remove the pairings. The string is what it
    /// said, which is the only thing that tells a missing controller from a bus
    /// that is not answering.
    Unpair(String),
    /// The wired half is not running, so there is nothing to re-plug.
    NotRunning,
}

/// Unpairs every phone, then asks the wired half for one replug.
///
/// Both are tried even when the first fails: a replug is worth asking for
/// either way, and a phone that pairs after it is answered like any other. The
/// answer is the first step that did not work.
///
/// Not a restart of the program: a hold, or a click, taken while someone is
/// watching the panel should cost the link a second, not the session.
pub async fn reset(
    adapter: &str,
    replug: &mpsc::Sender<()>,
    reason: &str,
) -> Result<(), Refusal> {
    // A count of zero is a reset that found nothing to clear, which the
    // per-device line inside `unpair_all` is the detail of.
    let unpaired = match carplay_hu::unpair_all(adapter).await {
        Ok(count) => {
            tracing::info!("bt.unpaired count={count} adapter={adapter}");
            Ok(())
        }
        Err(e) => {
            tracing::warn!("bt.unpaired_unavailable adapter={adapter} err={e}");
            Err(Refusal::Unpair(e.to_string()))
        }
    };
    let replugged = match replug.try_send(()) {
        // A full channel is one replug already on its way, which is what this
        // would have asked for.
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {
            tracing::info!("main.replug reason={reason}");
            true
        }
        Err(mpsc::error::TrySendError::Closed(())) => {
            tracing::warn!("main.replug_refused err=carlife_half_not_running");
            false
        }
    };
    unpaired?;
    if !replugged {
        return Err(Refusal::NotRunning);
    }
    Ok(())
}
