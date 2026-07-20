use embassy_futures::{
    join::join,
    select::{Either, select},
};
use serde_json_core::heapless;

use crate::keyboard::{KEY_MAP, KEY_MAP_EMPTY, KeyPress};
use common::button_array::{ButtonArray, KeyEvent};
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use defmt::*;
use embassy_rp::usb::Driver as UsbDriver;
use embassy_rp::{peripherals::USB, rom_data::reset_to_usb_boot};
use embassy_sync::blocking_mutex::raw::{NoopRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::Receiver;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embassy_usb::class::hid::{
    HidBootProtocol, HidProtocolMode, HidReader, HidReaderWriter, HidSubclass, HidWriter, ReportId,
    RequestHandler, State as HidState,
};
use embassy_usb::control::OutResponse;
use embassy_usb::{Builder, Config, Handler};
use heapless::Vec;
use usbd_hid::descriptor::{KeyboardReport, SerializedDescriptor};
use {defmt_rtt as _, panic_probe as _};

use crate::keyboard::{KeyMap, KeyMapping};
use serde::{Deserialize, Serialize};

pub const NUM_KEYS: usize = 56;
static HID_PROTOCOL_MODE: AtomicU8 = AtomicU8::new(HidProtocolMode::Boot as u8);
// Global shared state protected by a mutex
static SHARED: Mutex<ThreadModeRawMutex, RefCell<SharedState>> =
    Mutex::new(RefCell::new(SharedState {
        key_states: [None; NUM_KEYS],
        key_map: [
            KEY_MAP,
            KEY_MAP_EMPTY,
            KEY_MAP_EMPTY,
            KEY_MAP_EMPTY,
            KEY_MAP_EMPTY,
        ],
        current_layer: LayerState::Set(0),
    }));

pub const CONFIG_REPORT_DESCRIPTOR: &[u8] = &[
    0x06, 0x00, 0xFF, // Usage Page (Vendor Defined)
    0x09, 0x01, // Usage
    0xA1, 0x01, // Collection (Application)
    // INPUT (device -> host)
    0x09, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40, // 64 bytes
    0x81, 0x02, // Input (Data,Var,Abs)
    // OUTPUT (host -> device)
    0x09, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x40, // 64 bytes
    0x91, 0x02, // Output (Data,Var,Abs)
    0xC0, // End Collection
];

enum LayerState {
    Set(usize),
    Held { base: usize, held: usize },
}

// Shared data type
struct SharedState {
    pub key_states: [Option<KeyState>; NUM_KEYS],
    pub key_map: [[KeyMapping; 56]; 5],
    pub current_layer: LayerState,
}

impl SharedState {
    /// Check if we should go into USB bootloader mode,
    /// if the top two left and right keys are held.
    pub fn should_do_bootloader(&self) -> bool {
        self.key_states[0] == Some(KeyState::Held)
            && self.key_states[1] == Some(KeyState::Held)
            && self.key_states[32] == Some(KeyState::Held)
            && self.key_states[33] == Some(KeyState::Held)
    }

    fn get_current_layer_mapping(&self) -> &[KeyMapping; 56] {
        match self.current_layer {
            LayerState::Set(idx) => &self.key_map[idx],
            LayerState::Held { held, .. } => &self.key_map[held],
        }
    }

    fn get_current_base_layer(&self) -> usize {
        match self.current_layer {
            LayerState::Set(idx) => idx,
            LayerState::Held { base, .. } => base,
        }
    }

    /// Map current key states of IDs into actual HID key presses.
    fn map_hid_report_inner(&mut self) -> (u8, [u8; 6]) {
        let base_layer = self.key_map[0];

        let mut keycodes: [u8; 6] = [0, 0, 0, 0, 0, 0];
        let mut modifier: u8 = 0;
        for (idx, (id, state)) in self
            .key_states
            .iter()
            .enumerate()
            .filter_map(|(id, state)| Some((id, state.as_ref()?)))
            .take(6)
            .enumerate()
        {
            let curr_layer = self.get_current_layer_mapping();
            let layer_mapping = match state {
                KeyState::Pressed => curr_layer[id].press.or(base_layer[id].press),
                KeyState::Held => curr_layer[id]
                    .held
                    .or(curr_layer[id].press)
                    .or(base_layer[id].held)
                    .or(base_layer[id].press),
            };
            let keymap: KeyMap = if let Some(inner) = layer_mapping.map(|inner| inner) {
                inner
            } else {
                continue;
            };

            let base_layer = self.get_current_base_layer();
            match keymap.key {
                KeyPress::LayerSet0 => self.current_layer = LayerState::Set(0),
                KeyPress::LayerSet1 => self.current_layer = LayerState::Set(1),
                KeyPress::LayerSet2 => self.current_layer = LayerState::Set(2),
                KeyPress::LayerSet3 => self.current_layer = LayerState::Set(3),
                KeyPress::LayerSet4 => self.current_layer = LayerState::Set(4),
                KeyPress::LayerHold0 => {
                    self.current_layer = LayerState::Held {
                        base: base_layer,
                        held: 0,
                    }
                }
                KeyPress::LayerHold1 => {
                    self.current_layer = LayerState::Held {
                        base: base_layer,
                        held: 1,
                    }
                }
                KeyPress::LayerHold2 => {
                    self.current_layer = LayerState::Held {
                        base: base_layer,
                        held: 2,
                    }
                }
                KeyPress::LayerHold3 => {
                    self.current_layer = LayerState::Held {
                        base: base_layer,
                        held: 3,
                    }
                }
                KeyPress::LayerHold4 => {
                    self.current_layer = LayerState::Held {
                        base: base_layer,
                        held: 4,
                    }
                }
                _ => {
                    keycodes[idx] = keymap.key as u8;
                    modifier |= keymap.modifiers;
                }
            }
        }

        (modifier, keycodes)
    }

    pub fn map_hid_keyboard_report_boot(&mut self) -> [u8; 8] {
        let (modifier, keycodes) = self.map_hid_report_inner();

        let mut boot_report = [0_u8; 8];
        boot_report[0] = modifier;
        boot_report[2..].clone_from_slice(&keycodes);

        boot_report
    }

    pub fn map_hid_keyboard_report(&mut self) -> KeyboardReport {
        let (modifier, keycodes) = self.map_hid_report_inner();

        KeyboardReport {
            keycodes,
            leds: 0,
            modifier,
            reserved: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HidCfgCommand {
    WriteKeyMapping { id: u8, key_mapping: KeyMapping },
    ReadKeyMapping { id: u8 },
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum KeyState {
    Pressed,
    Held,
}

#[embassy_executor::task]
pub async fn hid_task(
    driver: UsbDriver<'static, USB>,
    mut buttons: ButtonArray<'static, 28>,
    evt_rx: Receiver<'static, NoopRawMutex, KeyEvent, 32>,
) {
    // Create embassy-usb Config
    let mut config = Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Nabla");
    config.product = Some("Vesper");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;
    config.composite_with_iads = false;
    config.device_class = 0;
    config.device_sub_class = 0;
    config.device_protocol = 0;

    // Create embassy-usb DeviceBuilder using the driver and config.
    // It needs some buffers for building the descriptors.
    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    // You can also add a Microsoft OS descriptor.
    let mut msos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut request_handler = MyRequestHandler {};
    let mut device_handler = MyDeviceHandler::new();

    let mut state = HidState::new();
    let mut cfg_state = HidState::new();

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );

    builder.handler(&mut device_handler);

    // Create classes on the builder.
    let config = embassy_usb::class::hid::Config {
        report_descriptor: KeyboardReport::desc(),
        request_handler: None,
        poll_ms: 60,
        max_packet_size: 64,
        hid_subclass: HidSubclass::Boot,
        hid_boot_protocol: HidBootProtocol::Keyboard,
    };
    let hid = HidReaderWriter::<_, 1, 8>::new(&mut builder, &mut state, config);

    let cfg_config = embassy_usb::class::hid::Config {
        report_descriptor: CONFIG_REPORT_DESCRIPTOR,
        request_handler: None,
        poll_ms: 10,
        max_packet_size: 64,
        hid_subclass: HidSubclass::No,
        hid_boot_protocol: HidBootProtocol::None,
    };

    let config_hid = HidReaderWriter::<_, 64, 64>::new(&mut builder, &mut cfg_state, cfg_config);

    // Build the builder.
    let mut usb = builder.build();

    // Run the USB device.
    let usb_fut = usb.run();

    let (reader, mut writer) = hid.split();
    let mut ticker = Ticker::every(Duration::from_millis(60));

    // Input future receiving keypresses, mapping them and sending the keyboard HID report to host
    let key_evt_fut = async {
        loop {
            // Future for next key event
            let key_evt_fut = async {
                match select(evt_rx.receive(), buttons.wait_key_event()).await {
                    Either::First(evt) => evt,
                    Either::Second(evt) => evt,
                }
            };

            let key_evt = key_evt_fut.await;
            let mut state_lock = SHARED.lock().await;

            match key_evt {
                KeyEvent::KeyPressed(id) => {
                    debug!("Key Pressed: {}", id);
                    (*state_lock).get_mut().key_states[id] = Some(KeyState::Pressed);
                }
                KeyEvent::KeyHeld(id) => {
                    debug!("Key Held: {}", id);
                    (*state_lock).get_mut().key_states[id] = Some(KeyState::Held);
                }
                KeyEvent::KeyReleased(id) => {
                    debug!("Key Released: {}", id);
                    (*state_lock).get_mut().key_states[id] = None
                }
            }

            // Check if we should reset into USB bootloader mode if both top corner keys are held
            if state_lock.get_mut().should_do_bootloader() {
                reset_to_usb_boot(0, 0);
            }
        }
    };

    let report_fut = async {
        loop {
            // Future for next key event
            ticker.next().await;
            let mut state_lock = SHARED.lock().await;

            if HID_PROTOCOL_MODE.load(Ordering::Relaxed) == HidProtocolMode::Boot as u8 {
                // In BOOT mode report is 8 bytes, first byte are the modifiers, second is
                // reservered 0, last 6 are keycodes
                let boot_report = state_lock.get_mut().map_hid_keyboard_report_boot();
                if let Err(e) = writer.write(&boot_report).await {
                    warn!("Failed to send report: {:?}", e);
                }
            } else {
                let report = state_lock.get_mut().map_hid_keyboard_report();
                // Send the report.
                match writer.write_serialize(&report).await {
                    Ok(()) => {}
                    Err(e) => warn!("Failed to send report: {:?}", e),
                };
            }
        }
    };

    let out_fut = async {
        reader.run(false, &mut request_handler).await;
    };

    // Configuration interface, receiving configuration data from the host computer,
    // including updated key-mappings firmware updates etc.
    let (mut cfg_reader, mut cfg_writer) = config_hid.split();
    let mut hid_buf: [u8; 64] = [0; 64];
    let cfg_fut = async {
        loop {
            match cfg_reader.read(&mut hid_buf).await {
                Ok(0) => {
                    warn!("Received 0 lengh read, CFG HID interface closed!");
                    break;
                }
                Ok(len) => {
                    info!("Incoming HID message: {:?}", hid_buf[..len]);
                    info!("Echoing HID message...");
                    if let Err(e) = cfg_writer.write(&hid_buf).await {
                        error!("CFG HID error: {}!", e);
                    }
                }
                Err(e) => {
                    defmt::error!("CFG HID read error {:?}", e);
                }
            }
        }
    };

    let in_fut = join(report_fut, key_evt_fut);
    // Run everything concurrently.
    join(cfg_fut, join(usb_fut, join(in_fut, out_fut))).await;
}

struct MyRequestHandler {}

impl RequestHandler for MyRequestHandler {
    fn get_report(&mut self, id: ReportId, _buf: &mut [u8]) -> Option<usize> {
        info!("Get report for {:?}", id);
        None
    }

    fn set_report(&mut self, id: ReportId, data: &[u8]) -> OutResponse {
        info!("Set report for {:?}: {=[u8]}", id, data);
        OutResponse::Accepted
    }

    fn get_protocol(&self) -> HidProtocolMode {
        let protocol = HidProtocolMode::from(HID_PROTOCOL_MODE.load(Ordering::Relaxed));
        info!("The current HID protocol mode is: {}", protocol);
        protocol
    }

    fn set_protocol(&mut self, protocol: HidProtocolMode) -> OutResponse {
        info!("Switching to HID protocol mode: {}", protocol);
        HID_PROTOCOL_MODE.store(protocol as u8, Ordering::Relaxed);
        OutResponse::Accepted
    }

    fn set_idle_ms(&mut self, id: Option<ReportId>, dur: u32) {
        info!("Set idle rate for {:?} to {:?}", id, dur);
    }

    fn get_idle_ms(&mut self, id: Option<ReportId>) -> Option<u32> {
        info!("Get idle rate for {:?}", id);
        None
    }
}

struct MyDeviceHandler {
    configured: AtomicBool,
}

impl MyDeviceHandler {
    fn new() -> Self {
        MyDeviceHandler {
            configured: AtomicBool::new(false),
        }
    }
}

impl Handler for MyDeviceHandler {
    fn enabled(&mut self, enabled: bool) {
        self.configured.store(false, Ordering::Relaxed);
        if enabled {
            info!("Device enabled");
        } else {
            info!("Device disabled");
        }
    }

    fn reset(&mut self) {
        self.configured.store(false, Ordering::Relaxed);
        info!("Bus reset, the Vbus current limit is 100mA");
    }

    fn addressed(&mut self, addr: u8) {
        self.configured.store(false, Ordering::Relaxed);
        info!("USB address set to: {}", addr);
    }

    fn configured(&mut self, configured: bool) {
        self.configured.store(configured, Ordering::Relaxed);
        if configured {
            info!(
                "Device configured, it may now draw up to the configured current limit from Vbus."
            )
        } else {
            info!("Device is no longer configured, the Vbus current limit is 100mA.");
        }
    }
}
