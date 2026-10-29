//! What has to be on the wire before CarLife is: the descriptors the kernel
//! reads out of the FunctionFS control endpoint, and the three requests a head
//! unit sends to ask a device to become an accessory.
//!
//! Bytes and parsing only, with no I/O in them, so the tests here run anywhere;
//! `usb` is the part that needs the board.

/// The descriptor header, version 2: the descriptors that follow are counted per
/// speed rather than one blob per speed. `uapi/linux/usb/functionfs.h`.
const DESCRIPTORS_MAGIC_V2: u32 = 3;
/// The strings blob's own magic, which is not the same one.
const STRINGS_MAGIC: u32 = 2;

/// Which speeds have descriptors below.
const HAS_FS_DESC: u32 = 1;
const HAS_HS_DESC: u32 = 2;
/// Ask for every control request, whatever it is addressed to. Without it the
/// kernel answers device-addressed requests itself and the three below never
/// reach this program, which is the whole reason the endpoint is useful.
const ALL_CTRL_RECIP: u32 = 64;

/// The interface's own form: class `ff`/`ff`/`00`, which is what both a head
/// unit and the reference implementation look for.
const INTERFACE_CLASS: [u8; 3] = [0xff, 0xff, 0x00];

/// Placeholder endpoint addresses. FunctionFS hands both to the controller's own
/// allocator (`usb_ep_autoconfig`), which rewrites them and keeps the direction,
/// so what is written here is only the pair and their order. The order is not
/// free: a head unit takes the interface's endpoints by position, and the one it
/// reads is the first. A real phone's accessory interface has the same order.
const ENDPOINT_IN: u8 = 0x81;
const ENDPOINT_OUT: u8 = 0x02;

/// What one bulk transfer carries at each speed.
const BULK_MAX_FS: u16 = 64;
const BULK_MAX_HS: u16 = 512;

const DESCRIPTOR_ENDPOINT: u8 = 5;
const DESCRIPTOR_INTERFACE: u8 = 4;
const TRANSFER_BULK: u8 = 2;

/// The descriptor blob for the accessory interface: one interface, two bulk
/// endpoints, described once for each of the two speeds a full-speed controller
/// may pick from.
pub fn descriptors() -> Vec<u8> {
    let mut blob = Vec::with_capacity(66);

    blob.extend_from_slice(&DESCRIPTORS_MAGIC_V2.to_le_bytes());
    let length_at = blob.len();
    blob.extend_from_slice(&0u32.to_le_bytes()); // filled in below, once it is known
    blob.extend_from_slice(&(HAS_FS_DESC | HAS_HS_DESC | ALL_CTRL_RECIP).to_le_bytes());
    // Three descriptors per speed: the interface and its two endpoints.
    blob.extend_from_slice(&3u32.to_le_bytes());
    blob.extend_from_slice(&3u32.to_le_bytes());

    for max_packet in [BULK_MAX_FS, BULK_MAX_HS] {
        blob.extend_from_slice(&interface());
        blob.extend_from_slice(&bulk(ENDPOINT_IN, max_packet));
        blob.extend_from_slice(&bulk(ENDPOINT_OUT, max_packet));
    }

    let length = blob.len() as u32;
    blob[length_at..length_at + 4].copy_from_slice(&length.to_le_bytes());
    blob
}

/// The interface descriptor. Endpoint addresses and numbers are FunctionFS's to
/// assign; the two written here are placeholders.
fn interface() -> [u8; 9] {
    [
        9,
        DESCRIPTOR_INTERFACE,
        0, // interface number
        0, // alternate setting
        2, // endpoints
        INTERFACE_CLASS[0],
        INTERFACE_CLASS[1],
        INTERFACE_CLASS[2],
        0, // no interface string: this device is not named over USB descriptors
    ]
}

/// One bulk endpoint descriptor.
fn bulk(address: u8, max_packet: u16) -> [u8; 7] {
    [
        7,
        DESCRIPTOR_ENDPOINT,
        address,
        TRANSFER_BULK,
        (max_packet & 0xff) as u8,
        (max_packet >> 8) as u8,
        0, // interval: nothing for bulk
    ]
}

/// The strings blob, with no strings in it.
///
/// It is not optional. FunctionFS writes descriptors and then strings, and the
/// instance stays unregistered until the second write has landed, and the
/// controller refuses to bind a gadget whose function is unregistered.
pub fn strings() -> [u8; 16] {
    let mut blob = [0u8; 16];
    blob[0..4].copy_from_slice(&STRINGS_MAGIC.to_le_bytes());
    blob[4..8].copy_from_slice(&16u32.to_le_bytes());
    // Two counts and their offset tables, both zero: nothing to point at.
    blob
}

/// The version answered to the first of the three requests. Version 2 is what a
/// head unit of this age was written against, and the two implementations this
/// was checked against accept either.
pub const AOA_VERSION: u16 = 2;

/// One control request, as FunctionFS hands it over: the eight bytes of the
/// setup packet, in the order the kernel has them.
#[derive(Debug, PartialEq, Eq)]
pub struct Setup {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

const DIRECTION_IN: u8 = 0x80;
const TYPE_MASK: u8 = 0x60;
const TYPE_VENDOR: u8 = 0x40;

const REQUEST_GET_PROTOCOL: u8 = 51;
const REQUEST_SEND_STRING: u8 = 52;
const REQUEST_START: u8 = 53;

impl Setup {
    /// Unpacks a setup packet. `None` only when there are not eight bytes of it.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        let body = raw.get(..8)?;
        Some(Self {
            request_type: body[0],
            request: body[1],
            value: u16::from_le_bytes([body[2], body[3]]),
            index: u16::from_le_bytes([body[4], body[5]]),
            length: u16::from_le_bytes([body[6], body[7]]),
        })
    }

    /// Which way the data stage goes, for a line in the log.
    pub fn direction(&self) -> &'static str {
        if self.request_type & DIRECTION_IN != 0 {
            "in"
        } else {
            "out"
        }
    }

    /// The request, when it is one of the three that ask a device to become an
    /// accessory. Anything else is the kernel's or nobody's, and this device
    /// answers only these.
    pub fn accessory(&self) -> Option<Accessory> {
        if self.request_type & TYPE_MASK != TYPE_VENDOR {
            return None;
        }
        match self.request {
            REQUEST_GET_PROTOCOL => Some(Accessory::GetProtocol),
            REQUEST_SEND_STRING => Some(Accessory::Identify {
                index: self.index as usize,
            }),
            REQUEST_START => Some(Accessory::Start),
            _ => None,
        }
    }
}

/// What a head unit asks for, in the order it asks: which version of the
/// protocol the device speaks, what it should call itself, and that it should
/// come back as an accessory.
#[derive(Debug, PartialEq, Eq)]
pub enum Accessory {
    GetProtocol,
    Identify { index: usize },
    Start,
}

/// The answer to `GetProtocol`, in the byte order the protocol wants it: two
/// little-endian bytes, low first.
pub fn version_reply() -> [u8; 2] {
    AOA_VERSION.to_le_bytes()
}

/// The numbers the device carries: the accessory's own, which a head unit checks
/// before it will talk CarLife at all, and the pair it has before one has
/// switched it.
const ACCESSORY_VID: u16 = 0x18d1;
const ACCESSORY_PID: u16 = 0x2d00;
/// Google's vendor number, and the product number an Android phone presents in
/// file-transfer mode — which is what a phone on a head unit's port is when a
/// head unit first looks at it. Nothing here transfers files; the pair is there
/// so that a host which decides by numbers rather than by shape finds a phone.
const PHONE_VID: u16 = 0x18d1;
const PHONE_PID: u16 = 0x4ee1;

/// What a host reads before it reads anything else: the numbers and the three
/// strings of a USB device descriptor. The interface it also reads comes from
/// `descriptors`, and is the same in both forms.
pub struct Identity {
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,
    pub product: String,
    pub serial: String,
}

impl Identity {
    /// What the device presents before any handshake, which is what a head unit
    /// expects to find on its port and what it does the handshake with. The maker
    /// is the caller's: hosts that decide what they are looking at by the strings
    /// read that one.
    ///
    /// The product and the serial are what the machine is called: a head unit
    /// that keeps a device apart from the next one it sees has these strings to
    /// do it with, and a name that is the same on every board is no use for that.
    pub fn phone(name: &str, manufacturer: &str) -> Self {
        Self {
            vid: PHONE_VID,
            pid: PHONE_PID,
            manufacturer: manufacturer.to_string(),
            product: name.to_string(),
            serial: name.to_string(),
        }
    }

    /// What it presents afterwards: the same device under the accessory's
    /// numbers.
    ///
    /// Only the two numbers change. A phone is the case this is copied from:
    /// checked against a Pixel 7a, its accessory form still reports Google /
    /// Pixel 7a / its own hardware serial, not the six strings the head unit
    /// sent. FunctionFS keeps the accessory function away from a device
    /// descriptor's strings for the same reason — the protocol's `send string`
    /// writes into the function, and the identity a host enumerated is left
    /// alone.
    ///
    /// So what a head unit sends here is recorded for the page and goes no
    /// further. What the host is left with is one device whose numbers changed,
    /// which is all a switch is. A device whose *name* changed looks like a
    /// different device to whatever was watching it before, and a head unit that
    /// matches the accessory it finds against the phone it just switched is
    /// entitled to match on the name.
    pub fn accessory(&self) -> Self {
        Self {
            vid: ACCESSORY_VID,
            pid: ACCESSORY_PID,
            manufacturer: self.manufacturer.clone(),
            product: self.product.clone(),
            serial: self.serial.clone(),
        }
    }

    /// Whether the device is already in the form a `start accessory` request asks
    /// for, which is what makes a second one nothing to do.
    pub fn is_accessory(&self) -> bool {
        self.vid == ACCESSORY_VID && self.pid == ACCESSORY_PID
    }

    /// For a line in the log.
    pub fn describe(&self) -> String {
        format!(
            "{:04x}:{:04x} as {} / {}",
            self.vid, self.pid, self.manufacturer, self.product
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Vec<u8> {
        let mut raw = vec![request_type, request];
        raw.extend_from_slice(&value.to_le_bytes());
        raw.extend_from_slice(&index.to_le_bytes());
        raw.extend_from_slice(&length.to_le_bytes());
        raw
    }

    #[test]
    fn the_descriptors_are_the_shape_a_head_unit_looks_for() {
        let blob = descriptors();

        assert_eq!(&blob[0..4], &3u32.to_le_bytes(), "header magic, version 2");
        assert_eq!(
            u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize,
            blob.len(),
            "the length the header carries is the length of the blob"
        );
        assert_eq!(
            &blob[8..12],
            &(1u32 | 2 | 64).to_le_bytes(),
            "full speed, high speed, and every control recipient"
        );
        assert_eq!(&blob[12..16], &3u32.to_le_bytes(), "descriptors per speed");
        assert_eq!(&blob[16..20], &3u32.to_le_bytes(), "and again for the other");

        // Both speeds describe the same interface, so the second copy starts
        // three descriptors after the first.
        let first = &blob[20..43];
        let second = &blob[43..66];
        assert_eq!(first[0] as usize, 9, "interface descriptor length");
        assert_eq!(first[1], 4, "and its type");
        assert_eq!(first[4], 2, "two endpoints");
        assert_eq!(&first[5..8], &[0xff, 0xff, 0x00], "the accessory's class");
        assert_eq!(first[8], 0, "no interface string");

        // The endpoint a head unit reads by position is the first one, and it is
        // the one the device sends on.
        assert_eq!(first[9], 7, "endpoint descriptor length");
        assert_eq!(first[10], 5, "and its type");
        assert_eq!(first[11], 0x81, "in first");
        assert_eq!(first[12], 2, "bulk");
        assert_eq!([first[13], first[14]], [64, 0], "full-speed packet size");
        assert_eq!(first[18], 0x02, "then out");
        assert_eq!([first[20], first[21]], [64, 0], "and the same size");

        assert_eq!(second[11], 0x81, "the high-speed copy is in, then out");
        assert_eq!([second[13], second[14]], [0, 2], "512-byte packets");
        assert_eq!(second[18], 0x02);
        assert_eq!([second[20], second[21]], [0, 2]);
    }

    #[test]
    fn the_strings_blob_says_it_has_no_strings_and_not_that_it_is_empty() {
        let blob = strings();

        assert_eq!(&blob[0..4], &2u32.to_le_bytes(), "its own magic");
        assert_eq!(&blob[4..8], &16u32.to_le_bytes(), "its own length");
        assert_eq!(&blob[8..16], &[0u8; 8], "no strings and no languages");
    }

    #[test]
    fn the_three_requests_are_recognised_and_nothing_else_is() {
        let cases = [
            (setup(0xc0, 51, 0, 0, 2), Accessory::GetProtocol),
            (setup(0x40, 52, 0, 1, 5), Accessory::Identify { index: 1 }),
            (setup(0x40, 53, 0, 0, 0), Accessory::Start),
        ];
        for (raw, expected) in cases {
            let parsed = Setup::decode(&raw).expect("eight bytes is a setup packet");
            assert_eq!(parsed.accessory(), Some(expected), "{raw:02x?}");
        }

        // The requests that come with them, and which are not this device's to
        // answer: one the protocol defines and CarLife never sends, two standard
        // ones the kernel handles itself, and one that is not one of the three.
        for raw in [
            setup(0x40, 58, 0, 0, 0),
            setup(0x80, 51, 0, 0, 2),
            setup(0x01, 52, 0, 1, 5),
            setup(0xc0, 50, 0, 0, 2),
        ] {
            let parsed = Setup::decode(&raw).expect("eight bytes is a setup packet");
            assert_eq!(parsed.accessory(), None, "{raw:02x?}");
        }
    }

    #[test]
    fn a_request_is_unpacked_field_by_field() {
        let raw = setup(0x40, 53, 0x1234, 0x5678, 0xfffe);
        let parsed = Setup::decode(&raw).expect("eight bytes is a setup packet");

        assert_eq!(parsed.request_type, 0x40);
        assert_eq!(parsed.request, 53);
        assert_eq!(parsed.value, 0x1234);
        assert_eq!(parsed.index, 0x5678);
        assert_eq!(parsed.length, 0xfffe);
        assert_eq!(parsed.direction(), "out");

        let inbound = Setup::decode(&setup(0xc0, 51, 0, 0, 2)).expect("eight bytes");
        assert_eq!(inbound.direction(), "in");
    }

    #[test]
    fn a_short_read_is_not_a_request() {
        assert_eq!(Setup::decode(&[0xc0, 51, 0, 0, 0, 0, 0]), None);
        assert_eq!(Setup::decode(&[]), None);
    }

    #[test]
    fn the_version_answer_is_two_little_endian_bytes() {
        assert_eq!(version_reply(), [2, 0], "version 2, low byte first");
    }

    #[test]
    fn the_accessory_form_is_the_same_device_under_other_numbers() {
        let phone = Identity::phone("nanopi-duo2", "friendlyarm");
        assert!(!phone.is_accessory(), "a phone is not an accessory yet");

        let accessory = phone.accessory();
        assert_eq!((accessory.vid, accessory.pid), (0x18d1, 0x2d00));
        assert!(accessory.is_accessory());
        assert_eq!(
            accessory.manufacturer, "friendlyarm",
            "a phone keeps its manufacturer across the switch, and so does this"
        );
        assert_eq!(
            accessory.product, "nanopi-duo2",
            "and the name it enumerated under"
        );
        assert_eq!(accessory.serial, "nanopi-duo2", "and the serial");
    }
}
