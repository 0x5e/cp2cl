//! The car's position, in the sentences an iPhone reads its maps from.
//!
//! iAP2 carries a position one NMEA sentence per message, and the phone takes
//! two of them as a pair: `$GPGGA` for the fix itself and `$GPRMC` for where the
//! car is and where it is going. The car reports the same thing in the units of
//! its own message — a millionth of a degree, a tenth of a metre, a hundredth of
//! a kilometre an hour — so this is where those become the units and the layout
//! a satellite receiver would have produced.
//!
//! Two things the car does not report at all: how many satellites the fix was
//! made of, and which ones. The count is written as none and no `$GPGSV` is
//! produced, because a number made up here is one the phone would navigate by.
//! The horizontal error it does report, and that becomes the HDOP.

use super::proto::CarGps;

/// The two sentences for one fix, or `None` when the car has no fix to give.
///
/// `gcj02` says the car's coordinates are in the shifted datum Chinese maps are
/// drawn in rather than in WGS84. A phone's maps are in the datum the satellites
/// are in, so those are moved back before they are written down.
pub fn sentences(fix: &CarGps, gcj02: bool) -> Option<String> {
    if fix.antenna_state != 1 {
        return None;
    }
    // Both sentences begin with the time of the fix, and a clock the car has not
    // set is no clock at all: a wrong time is worse than nothing sent.
    let (time, date) = clock(fix)?;

    let mut lat = f64::from(fix.latitude) / 1e6;
    let mut lng = f64::from(fix.longitude) / 1e6;
    if gcj02 {
        (lat, lng) = gcj02_to_wgs84(lat, lng);
    }
    // A position off the globe is a message that did not hold one.
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return None;
    }

    let (lat_ddmm, lat_hemi) = ddmm(lat, true);
    let (lng_ddmm, lng_hemi) = ddmm(lng, false);

    // The phone weighs our fix against its own by this, at about five metres a
    // unit, and the car reports it in metres.
    let hdop = (f64::from(fix.hor_pos_error) / 5.0).clamp(0.5, 50.0);
    let altitude = f64::from(fix.height) / 10.0;
    // RMC's speed is in knots, whatever the rest of the message set is in.
    let knots = f64::from(fix.speed) / 100.0 / 1.852;
    let course = f64::from(fix.heading) / 10.0;

    let gga = format!(
        "GPGGA,{time},{lat_ddmm},{lat_hemi},{lng_ddmm},{lng_hemi},1,00,{hdop:.1},{altitude:.1},M,0.0,M,,"
    );
    let rmc = format!(
        "GPRMC,{time},A,{lat_ddmm},{lat_hemi},{lng_ddmm},{lng_hemi},{knots:.2},{course:.2},{date},,"
    );
    Some(format!(
        "${gga}*{}\r\n${rmc}*{}\r\n",
        checksum(&gga),
        checksum(&rmc)
    ))
}

/// The instant the car reports, as the two fields every sentence begins with:
/// `hhmmss.ss` and `ddmmyy`. `None` when the clock was never set.
fn clock(fix: &CarGps) -> Option<(String, String)> {
    if fix.year == 0
        || !(1..=12).contains(&fix.month)
        || !(1..=31).contains(&fix.day)
        || !(0..=23).contains(&fix.hour)
        || !(0..=59).contains(&fix.minute)
        || !(0..=60).contains(&fix.second)
    {
        return None;
    }
    let time = format!("{:02}{:02}{:02}.00", fix.hour, fix.minute, fix.second);
    let date = format!("{:02}{:02}{:02}", fix.day, fix.month, fix.year.rem_euclid(100));
    Some((time, date))
}

/// Degrees as `ddmm.mmmm` (latitude) or `dddmm.mmmm` (longitude), and the
/// hemisphere letter that goes with it.
fn ddmm(deg: f64, lat: bool) -> (String, char) {
    let abs = deg.abs();
    let whole = abs.floor();
    let minutes = format!("{:07.4}", (abs - whole) * 60.0);
    let ddmm = if lat {
        format!("{:02}{minutes}", whole as u32)
    } else {
        format!("{:03}{minutes}", whole as u32)
    };
    let hemi = match (lat, deg >= 0.0) {
        (true, true) => 'N',
        (true, false) => 'S',
        (false, true) => 'E',
        (false, false) => 'W',
    };
    (ddmm, hemi)
}

/// The XOR of everything between the `$` and the `*`.
fn checksum(body: &str) -> String {
    format!("{:02X}", body.bytes().fold(0u8, |cs, b| cs ^ b))
}

/// The datum Chinese maps are drawn in, undone.
///
/// GCJ-02 shifts a WGS84 position by up to several hundred metres, by a rule
/// with no inverse in closed form: the shift is worked out again for the answer
/// and taken off, which is what the two rounds below do. Each round is worth
/// three orders of magnitude, and two put the error under a millimetre. Outside
/// China the shift is not applied to maps at all, so nothing is undone either.
pub fn gcj02_to_wgs84(lat: f64, lng: f64) -> (f64, f64) {
    if !in_china(lat, lng) {
        return (lat, lng);
    }
    let (mut wgs_lat, mut wgs_lng) = (lat, lng);
    for _ in 0..2 {
        let (gcj_lat, gcj_lng) = wgs84_to_gcj02(wgs_lat, wgs_lng);
        wgs_lat -= gcj_lat - lat;
        wgs_lng -= gcj_lng - lng;
    }
    (wgs_lat, wgs_lng)
}

/// The same shift, applied — only ever used to work out what to undo.
fn wgs84_to_gcj02(lat: f64, lng: f64) -> (f64, f64) {
    /// The ellipsoid GCJ-02's shift is defined on, not the one used today.
    const A: f64 = 6_378_245.0;
    const EE: f64 = 0.006_693_421_622_965_943;

    let mut lat_shift = shift_lat(lng - 105.0, lat - 35.0);
    let mut lng_shift = shift_lng(lng - 105.0, lat - 35.0);
    let rad = lat.to_radians();
    let magic = 1.0 - EE * rad.sin() * rad.sin();
    let root = magic.sqrt();
    lat_shift = (lat_shift * 180.0) / ((A * (1.0 - EE)) / (magic * root) * std::f64::consts::PI);
    lng_shift = (lng_shift * 180.0) / (A / root * rad.cos() * std::f64::consts::PI);
    (lat + lat_shift, lng + lng_shift)
}

fn shift_lat(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut d = -100.0 + 2.0 * x + 3.0 * y + 0.2 * y * y + 0.1 * x * y + 0.2 * x.abs().sqrt();
    d += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    d += (20.0 * (y * pi).sin() + 40.0 * (y / 3.0 * pi).sin()) * 2.0 / 3.0;
    d += (160.0 * (y / 12.0 * pi).sin() + 320.0 * (y * pi / 30.0).sin()) * 2.0 / 3.0;
    d
}

fn shift_lng(x: f64, y: f64) -> f64 {
    let pi = std::f64::consts::PI;
    let mut d = 300.0 + x + 2.0 * y + 0.1 * x * x + 0.1 * x * y + 0.1 * x.abs().sqrt();
    d += (20.0 * (6.0 * x * pi).sin() + 20.0 * (2.0 * x * pi).sin()) * 2.0 / 3.0;
    d += (20.0 * (x * pi).sin() + 40.0 * (x / 3.0 * pi).sin()) * 2.0 / 3.0;
    d += (150.0 * (x / 12.0 * pi).sin() + 300.0 * (x / 30.0 * pi).sin()) * 2.0 / 3.0;
    d
}

/// The rectangle GCJ-02 is defined over, roughly: China's own bounds.
fn in_china(lat: f64, lng: f64) -> bool {
    (73.66..=135.05).contains(&lng) && (3.86..=53.55).contains(&lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fix the rest of the suite uses: Hangzhou, 30.317880 / 120.128094.
    fn fix() -> CarGps {
        CarGps {
            antenna_state: 1,
            latitude: 30_317_880,
            longitude: 120_128_094,
            height: 456,
            speed: 1234,
            heading: 900,
            hor_pos_error: 35,
            vert_pos_error: 20,
            year: 2026,
            month: 10,
            day: 20,
            hour: 14,
            minute: 33,
            second: 57,
            timestamp_ms: 0,
        }
    }

    /// Both sentences are written with the checksum a receiver would give them,
    /// because the phone discards a sentence whose checksum it cannot verify.
    #[test]
    fn a_fix_becomes_the_two_sentences_a_phone_reads() {
        let said = sentences(&fix(), false).expect("a fix with a clock is a position");
        assert_eq!(
            said,
            "$GPGGA,143357.00,3019.0728,N,12007.6856,E,1,00,7.0,45.6,M,0.0,M,,*64\r\n\
             $GPRMC,143357.00,A,3019.0728,N,12007.6856,E,6.66,90.00,201026,,*03\r\n"
        );
        // The checksums are the sentences', not a constant of this test.
        for line in said.lines() {
            let (body, said) = line[1..].split_once('*').expect("a sentence names its checksum");
            assert_eq!(checksum(body), said, "in {line}");
        }
    }

    /// A car with no fix is a car with nothing to say about where it is: a
    /// position of zeroes would put the phone in the Gulf of Guinea.
    #[test]
    fn a_car_without_a_fix_has_no_position_to_give() {
        assert!(sentences(&CarGps { antenna_state: -1, ..fix() }, false).is_none());
        assert!(sentences(&CarGps { antenna_state: 0, ..fix() }, false).is_none());
        // And one whose clock was never set has no time to stamp it with.
        assert!(sentences(&CarGps { year: 0, ..fix() }, false).is_none());
        assert!(sentences(&CarGps { month: 0, ..fix() }, false).is_none());
        assert!(sentences(&CarGps { latitude: 95_000_000, ..fix() }, false).is_none());
    }

    /// The datum shift is hundreds of metres in China and nothing outside it,
    /// which is what makes it worth undoing here rather than leaving to the
    /// phone's maps.
    #[test]
    fn the_chinese_datum_is_undone_where_it_is_used() {
        let (gcj_lat, gcj_lng) = wgs84_to_gcj02(30.317_880, 120.128_094);
        // A shift of this size is what GCJ-02 is: a different place on the map.
        assert!((gcj_lat - 30.317_880).abs() > 0.0005);
        assert!((gcj_lng - 120.128_094).abs() > 0.0005);

        let (back_lat, back_lng) = gcj02_to_wgs84(gcj_lat, gcj_lng);
        assert!((back_lat - 30.317_880).abs() < 1e-7, "{back_lat}");
        assert!((back_lng - 120.128_094).abs() < 1e-7, "{back_lng}");

        // Berlin is not in China, so nothing about it is shifted.
        assert_eq!(gcj02_to_wgs84(52.520_008, 13.404_954), (52.520_008, 13.404_954));
    }
}
