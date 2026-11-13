//! Calling the phone's own iAP2 service.
//!
//! BlueZ can make this call for us — `Device1.ConnectProfile` takes the UUID and
//! does the SDP lookup and the RFCOMM connect itself — but it answers from its
//! own record of the last attempt. An attempt that died half way leaves it
//! answering `br-connection-already-connected` for as long as it keeps that
//! record, and every round in between is no call at all.
//!
//! A socket of our own has no such record: the channel comes from the phone's
//! SDP record, and the connect comes back with a descriptor or with an errno.

use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What the iPhone advertises for its own side of iAP2: the record whose RFCOMM
/// channel is dialled, and the string this looks for in `sdptool`'s answer.
pub(super) const IAP_CLIENT_UUID: &str = "00000000-deca-fade-deca-deafdecacafe";

/// The address family and protocol `AF_BLUETOOTH`/`BTPROTO_RFCOMM` are, which
/// `libc` does not name for this target.
const AF_BLUETOOTH: i32 = 31;
const BTPROTO_RFCOMM: i32 = 3;

/// How long `sdptool` is given. It pages the phone itself when the link is
/// down, so this covers a link and an SDP exchange; a phone that is away does
/// not answer at all, and this is what stops the round from waiting on it.
const SDP_CEILING: Duration = Duration::from_secs(20);

/// How often `sdptool`'s exit is looked for.
const SDP_POLL: Duration = Duration::from_millis(20);

/// A call that came back: the socket, the channel it was made to, and how long
/// each half took.
///
/// The halves fail for different reasons — the first is the phone answering a
/// query, the second is the link carrying a channel — and which one is slow is
/// not visible from the total.
pub(super) struct Dialed {
    pub fd: OwnedFd,
    pub channel: u8,
    pub sdp: Duration,
    pub connect: Duration,
}

/// The phone's iAP2 channel, and a connected socket to it.
///
/// Blocking on purpose: the caller runs it off the runtime, because a call to a
/// phone that is away is a wait of seconds and nothing else in this program is
/// waiting on it.
pub(super) fn open(mac: &str, timeout: Duration) -> io::Result<Dialed> {
    let asked = Instant::now();
    let channel = channel(mac)?;
    let sdp = asked.elapsed();
    let connected = Instant::now();
    let fd = rfcomm(mac, channel, timeout)?;
    Ok(Dialed {
        fd,
        channel,
        sdp,
        connect: connected.elapsed(),
    })
}

/// The RFCOMM channel the phone's iAP2 record names.
fn channel(mac: &str) -> io::Result<u8> {
    let started = Instant::now();
    let mut child = Command::new("sdptool")
        .args(["browse", mac])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    loop {
        if let Some(status) = child.try_wait()? {
            let mut text = String::new();
            stdout.read_to_string(&mut text)?;
            if !status.success() {
                // The reason is the whole value of this attempt, and the exit
                // status is 255 for all of them: "Connection timed out" and
                // "Host is down" are the phone not answering, "Device or
                // resource busy" is this side still holding something.
                //
                // `sdptool` writes it with `printf`, so it is on the standard
                // output beside the records and not on the error stream — the
                // first version of this looked in the wrong one and reported an
                // empty reason. Both are read, because which one a given
                // failure uses is `sdptool`'s business.
                let mut said = String::new();
                let _ = stderr.read_to_string(&mut said);
                // How far it got, in two numbers rather than in the hundreds of
                // lines a full record dump carries: no record means it never
                // reached the phone's SDP server, records but a failure means it
                // connected and died part way through the search.
                let records = text
                    .lines()
                    .filter(|line| line.trim_start().starts_with("Service Name:"))
                    .count();
                let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
                return Err(io::Error::other(format!(
                    "sdptool failed ({status}): {} lines={lines} records={records}",
                    reason(&text, &said)
                )));
            }
            return channel_from(&text).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SDP did not report one iAP2 RFCOMM channel",
                )
            });
        }
        if started.elapsed() >= SDP_CEILING {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "sdptool did not answer",
            ));
        }
        std::thread::sleep(SDP_POLL);
    }
}

/// What `sdptool` said about why it gave up.
///
/// The line worth having is the one that says something failed, not the first
/// one it wrote: a browse that connects and then dies prints `Browsing <mac> …`
/// before the reason, and taking the first line reported that greeting as the
/// failure. Both streams are read, because which one carries it is `sdptool`'s
/// business — its errors go to the standard output beside the records.
fn reason(out: &str, err: &str) -> String {
    failing(out)
        .or_else(|| failing(err))
        .unwrap_or_default()
}

/// The first line of `text` that names a failure, or its first line if none does.
fn failing(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .find(|line| line.to_lowercase().contains("fail"))
        .or_else(|| lines.first())
        .map(|line| line.to_string())
}

/// What the kernel itself holds for `mac`, as its connection table lists it.
///
/// A connect that answers `EALREADY` says the controller already has an attempt
/// to that address open, and nothing else: no link is up, no BlueZ object knows
/// about it, and further calls are answered from the same entry until it clears
/// on its own. The table behind `hcitool con` is the one place it is visible, and
/// it carries the handle and state that say where it stopped.
pub(super) fn held(mac: &str) -> String {
    match Command::new("hcitool").arg("con").output() {
        Ok(out) => held_from(&String::from_utf8_lossy(&out.stdout), mac),
        Err(e) => format!("unavailable ({e})"),
    }
}

/// The lines of `hcitool con` that name `mac`, or `none` when the table has none.
fn held_from(list: &str, mac: &str) -> String {
    let named: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("ACL") && line.contains(mac))
        .collect();
    if named.is_empty() {
        "none".to_string()
    } else {
        named.join("; ")
    }
}

/// The one iAP2 channel in `sdptool browse`'s output, if there is exactly one.
///
/// Records are read one at a time because the phone advertises several services
/// and only one of them is ours: a channel is taken only from a record that
/// carries the iAP2 UUID, and only from its RFCOMM descriptor. Two different
/// answers is not a channel to pick between — it is a record that says nothing
/// this program can act on.
fn channel_from(text: &str) -> Option<u8> {
    let mut found: Option<u8> = None;
    let mut ours = false;
    let mut rfcomm = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("Service Name:") || line.starts_with("Service RecHandle:") {
            ours = false;
            rfcomm = false;
        } else if line.starts_with("UUID 128:") {
            ours |= line.to_lowercase().contains(IAP_CLIENT_UUID);
        } else if line.contains("\"RFCOMM\"") && line.contains("0x0003") {
            rfcomm = ours;
        } else if rfcomm {
            if let Some(number) = line.strip_prefix("Channel:") {
                rfcomm = false;
                let number: u8 = number.trim().parse().ok()?;
                if !(1..=30).contains(&number) {
                    return None;
                }
                match found {
                    None => found = Some(number),
                    Some(seen) if seen == number => {}
                    Some(_) => return None,
                }
            } else if line.contains("(0x") {
                rfcomm = false;
            }
        }
    }
    found
}

/// A connected RFCOMM socket to `channel` on `mac`.
///
/// Non-blocking, so a phone that never answers costs the caller its own timeout
/// rather than the kernel's: the connect returns at once with `EINPROGRESS`, and
/// what came of it is read back once the descriptor is writable.
fn rfcomm(mac: &str, channel: u8, timeout: Duration) -> io::Result<OwnedFd> {
    let address = octets(mac)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a bluetooth address"))?;
    if !(1..=30).contains(&channel) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not an RFCOMM channel",
        ));
    }
    let raw = unsafe {
        libc::socket(
            AF_BLUETOOTH,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            BTPROTO_RFCOMM,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // From here on the descriptor is closed by `fd`, on every way out.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    #[repr(C)]
    struct SockAddrRc {
        family: libc::sa_family_t,
        address: [u8; 6],
        channel: u8,
    }
    let target = SockAddrRc {
        family: AF_BLUETOOTH as libc::sa_family_t,
        address,
        channel,
    };
    let called = unsafe {
        libc::connect(
            raw,
            &target as *const SockAddrRc as *const libc::sockaddr,
            std::mem::size_of_val(&target) as libc::socklen_t,
        )
    };
    if called < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
        if !writable(raw, timeout)? {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "RFCOMM connect timed out",
            ));
        }
        let mut code: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let read = unsafe {
            libc::getsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut code as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        if code != 0 {
            return Err(io::Error::from_raw_os_error(code));
        }
    }
    Ok(fd)
}

/// Whether the descriptor became writable within `timeout`, which is what a
/// connect that has been replied to looks like from here.
fn writable(fd: libc::c_int, timeout: Duration) -> io::Result<bool> {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let ready = unsafe {
            libc::poll(&mut poll, 1, timeout.as_millis().min(i32::MAX as u128) as libc::c_int)
        };
        if ready >= 0 {
            return Ok(ready > 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// `AA:BB:CC:DD:EE:FF` as the six octets `sockaddr_rc` holds, low address first.
fn octets(mac: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, part) in parts.iter().enumerate() {
        out[5 - i] = u8::from_str_radix(part, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `sdptool browse` printed for a phone that also runs a hands-free
    /// gateway, which is the other RFCOMM record on the same device.
    const BROWSED: &str = "\
Service Name: Hands-Free Audio Gateway
Service RecHandle: 0x10002
Service Class ID List:
  \"Handsfree Audio Gateway\" (0x111f)
Protocol Descriptor List:
  \"L2CAP\" (0x0100)
  \"RFCOMM\" (0x0003)
    Channel: 7

Service Name: iAP2
Service RecHandle: 0x1000c
Service Class ID List:
  UUID 128: 00000000-deca-fade-deca-deafdecacafe
Protocol Descriptor List:
  \"L2CAP\" (0x0100)
  \"RFCOMM\" (0x0003)
    Channel: 12
";

    #[test]
    fn the_channel_comes_from_the_iap2_record_and_not_from_a_neighbour() {
        assert_eq!(channel_from(BROWSED), Some(12));
    }

    #[test]
    fn a_phone_without_the_record_has_no_channel() {
        let only_other = BROWSED.split("Service Name: iAP2").next().unwrap();
        assert_eq!(channel_from(only_other), None);
    }

    #[test]
    fn two_records_that_disagree_are_not_a_channel() {
        let twice = format!("{BROWSED}{}", BROWSED.replace("Channel: 12", "Channel: 13"));
        assert_eq!(channel_from(&twice), None);
    }

    #[test]
    fn the_same_record_twice_is_still_one_channel() {
        let twice = format!("{BROWSED}{BROWSED}");
        assert_eq!(channel_from(&twice), Some(12));
    }

    #[test]
    fn the_uuid_is_matched_without_regard_to_case() {
        let upper = BROWSED.replace(IAP_CLIENT_UUID, &IAP_CLIENT_UUID.to_uppercase());
        assert_eq!(channel_from(&upper), Some(12));
    }

    #[test]
    fn an_empty_answer_has_no_channel() {
        assert_eq!(channel_from(""), None);
    }

    #[test]
    fn an_address_is_six_octets_low_first() {
        assert_eq!(
            octets("F4:39:A6:74:51:AF"),
            Some([0xAF, 0x51, 0x74, 0xA6, 0x39, 0xF4])
        );
        assert_eq!(octets("f4:39:a6:74:51:af"), octets("F4:39:A6:74:51:AF"));
    }

    #[test]
    fn a_malformed_address_is_refused() {
        assert_eq!(octets("F4:39:A6:74:51"), None);
        assert_eq!(octets("F4:39:A6:74:51:ZZ"), None);
        assert_eq!(octets(""), None);
    }

    #[test]
    fn the_reason_is_the_first_line_sdptool_wrote() {
        let out = "\nFailed to connect to SDP server on F4:39:A6:74:51:AF: \
                   Connection timed out\n";
        assert_eq!(
            reason(out, ""),
            "Failed to connect to SDP server on F4:39:A6:74:51:AF: Connection timed out"
        );
    }

    #[test]
    fn a_reason_on_the_error_stream_is_read_when_the_output_is_empty() {
        assert_eq!(reason("", "Host is down\n"), "Host is down");
        assert_eq!(reason("", ""), "");
    }

    #[test]
    fn a_browse_that_died_reports_the_failure_and_not_the_greeting() {
        let out = "Browsing F4:39:A6:74:51:AF ...\n\
                   Service search failed: Connection reset by peer\n";
        assert_eq!(
            reason(out, ""),
            "Service search failed: Connection reset by peer"
        );
    }

    #[test]
    fn a_connection_the_kernel_holds_is_named_with_its_state() {
        let table = "Connections:\n\
                     \tACL F4:39:A6:74:51:AF handle 256 state 1 lm MASTER \n\
                     \tACL AA:BB:CC:DD:EE:FF handle 257 state 1 lm SLAVE \n";
        assert_eq!(
            held_from(table, "F4:39:A6:74:51:AF"),
            "ACL F4:39:A6:74:51:AF handle 256 state 1 lm MASTER"
        );
    }

    #[test]
    fn a_table_without_that_address_holds_nothing() {
        assert_eq!(held_from("Connections:\n", "F4:39:A6:74:51:AF"), "none");
        assert_eq!(
            held_from("\tACL AA:BB:CC:DD:EE:FF handle 257\n", "F4:39:A6:74:51:AF"),
            "none"
        );
    }
}
