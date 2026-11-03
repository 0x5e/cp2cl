//! What the CarPlay half is told about the machine it runs on.
//!
//! Nothing here is read from `/etc` or from the device tree: the program that
//! owns the board reads those and hands the values over. The three radio fields
//! are the settings file's; the rest are properties of the machine.

/// Everything the CarPlay half is told about the machine.
pub struct Options {
    /// Whether the phone is to be found over Bluetooth and Wi-Fi instead of the
    /// cable. Decides whether a wireless phone's tunnel is set up at all.
    pub wireless: bool,
    /// The interface the access point runs on, which decides whose link-local
    /// address the phone is told to come back to.
    pub wifi_iface: String,
    /// Linux only: the Bluetooth adapter, as BlueZ names it.
    pub bt_adapter: String,
    /// The name the accessory goes by: the machine's hostname.
    pub name: String,
    /// The board's maker, from the device tree.
    pub vendor: String,
    /// The Bluetooth controller's own address.
    pub bt_address: String,
    /// The largest cover the car's message can carry, which is what decides when
    /// one has to be shrunk before it is sent. The limit is the consumer's.
    pub cover_limit: usize,
}

impl Default for Options {
    /// What a machine that cannot say anything about itself reports.
    fn default() -> Self {
        Self {
            wireless: true,
            wifi_iface: "wlan0".to_string(),
            bt_adapter: "hci0".to_string(),
            name: "cp2cl".to_string(),
            vendor: "Unknown".to_string(),
            bt_address: "00:11:22:33:44:55".to_string(),
            cover_limit: 0,
        }
    }
}
