//! This example test the RP Pico on board LED.
//!
//! It does not work with the RP Pico W board. See `blinky_wifi.rs`.

#![no_std]
#![no_main]

use common::command::SlaveEvent;
use embassy_futures::{
    join::join,
    select::{self, Either, select},
};
use embassy_rp::block::ImageDef;
use keyboard::{KeyMap, KeyPress};

use crate::keyboard::KEY_MAP;
use common::button_array::{self, ButtonArray, KeyEvent};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{Input, Output};
use embassy_rp::peripherals::USB;
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART0};
use embassy_rp::pio::Pio;
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_rp::uart::{
    BufferedInterruptHandler, BufferedUart, BufferedUartRx, Config as UartConfig,
};
use embassy_rp::usb::{Driver as UsbDriver, InterruptHandler};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::{Channel, Receiver};
use embassy_time::{Duration, Instant, Ticker, Timer};
use embassy_usb::class::hid::{
    HidBootProtocol, HidProtocolMode, HidReader, HidReaderWriter, HidSubclass, HidWriter, ReportId,
    RequestHandler, State as HidState,
};
use embassy_usb::control::OutResponse;
use embassy_usb::driver::Driver;
use embassy_usb::{Builder, Config, Handler};
use embedded_hal_1::digital::OutputPin;
use embedded_io_async::{Read, Write};
use heapless::LinearMap;
use postcard::from_bytes;
use smart_leds::RGB8;
use static_cell::StaticCell;
use usbd_hid::descriptor::AsInputReport;
use usbd_hid::descriptor::{KeyboardReport, SerializedDescriptor, gen_hid_descriptor};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
    USBCTRL_IRQ => embassy_rp::usb::InterruptHandler<USB>;
});

#[gen_hid_descriptor(
    (collection = APPLICATION, usage_page = VENDOR_DEFINED_START, usage = 0x01) = {
        (usage = 0x01,) = {
            #[item_settings(data, variable, absolute)]
            data = input;
            #[item_settings(data, variable, absolute)]
            data = output;
        };
    }
)]
pub struct ConfigReport {
    pub data: [u8; 64],
}

/// Input a value 0 to 255 to get a color value
/// The colours are a transition r - g - b - back to r.
fn wheel(mut wheel_pos: u8) -> RGB8 {
    wheel_pos = 255 - wheel_pos;
    if wheel_pos < 85 {
        return (255 - wheel_pos * 3, 0, wheel_pos * 3).into();
    }
    if wheel_pos < 170 {
        wheel_pos -= 85;
        return (0, wheel_pos * 3, 255 - wheel_pos * 3).into();
    }
    wheel_pos -= 170;
    (wheel_pos * 3, 255 - wheel_pos * 3, 0).into()
}

pub mod bootloader;
pub mod keyboard;

use bootloader::SlaveBootloader;

const NKEYS: usize = 28;

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Vesper Master"),
    embassy_rp::binary_info::rp_program_description!(
        c"Master program for the Vesper split design keyboard"
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

#[unsafe(link_section = ".start_block")]
#[used]
static IMAGE_DEF: ImageDef = ImageDef::secure_exe(); // Update this with your own implementation.

static HID_PROTOCOL_MODE: AtomicU8 = AtomicU8::new(HidProtocolMode::Boot as u8);
//static KEY_EVENT_CHANNEL: Channel<NoopRawMutex, KeyEvent, 32> = Channel::new();

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let (tx_pin, rx_pin, uart) = (p.PIN_46, p.PIN_47, p.UART0);
    let mut led = Output::new(p.PIN_2, embassy_rp::gpio::Level::Low);
    let slave_en = Output::new(p.PIN_3, embassy_rp::gpio::Level::Low);

    let key_inputs = [
        Input::new(p.PIN_9, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_8, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_7, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_6, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_5, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_4, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_15, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_14, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_13, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_12, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_11, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_10, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_22, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_21, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_20, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_19, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_18, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_17, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_33, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_32, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_31, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_30, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_29, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_28, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_35, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_36, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_37, embassy_rp::gpio::Pull::None),
        Input::new(p.PIN_34, embassy_rp::gpio::Pull::None),
    ];

    let mut buttons = ButtonArray::new(key_inputs, Duration::from_micros(1000));

    static TX_BUF: StaticCell<[u8; 32]> = StaticCell::new();
    let tx_buf = &mut TX_BUF.init([0; 32])[..];
    static RX_BUF: StaticCell<[u8; 32]> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init([0; 32])[..];
    let mut uart = BufferedUart::new(
        uart,
        tx_pin,
        rx_pin,
        Irqs,
        tx_buf,
        rx_buf,
        UartConfig::default(),
    );

    // Create the driver, from the HAL.
    let driver = UsbDriver::new(p.USB, Irqs);

    let channel: *mut Channel<NoopRawMutex, KeyEvent, 32> = {
        static mut CH: Channel<NoopRawMutex, KeyEvent, 32> = Channel::new();
        unsafe { &raw mut CH }
    };
    let key_event_tx = unsafe { (*channel).sender() };
    let key_event_rx: Receiver<'static, NoopRawMutex, KeyEvent, 32> =
        unsafe { (*channel).receiver() };

    // Boot salve keyboard
    let mut bl = SlaveBootloader::new(slave_en);
    let blinky_fut = async {
        loop {
            led.set_high();
            Timer::after_millis(100).await;
            led.set_low();
            Timer::after_millis(100).await;
        }
    };
    _ = select(bl.boot_slave(&mut uart), blinky_fut).await;
    led.set_low();

    // Start the LED bling
    spawner.spawn(unwrap!(led_task()));
    spawner.spawn(unwrap!(hid_task(driver, buttons, key_event_rx)));

    let mut watchdog_deadline: Instant = Instant::now() + Duration::from_millis(500);
    let mut buffer: [u8; 32] = [0; 32];
    let mut n_bytes: Option<usize> = None;

    loop {
        let uart_fut = async {
            loop {
                if let Some(n) = n_bytes {
                    match select(Timer::after_millis(100), uart.read_exact(&mut buffer[..n])).await
                    {
                        Either::First(_) => {
                            defmt::error!("UART timeout!");
                            n_bytes = None;
                        }
                        Either::Second(res) => {
                            res.unwrap();
                            n_bytes = None;
                            break from_bytes::<SlaveEvent>(&buffer[..n]).unwrap();
                        }
                    }
                } else {
                    uart.read_exact(&mut buffer[..1]).await.unwrap();
                    n_bytes = Some(buffer[0] as usize);
                }
            }
        };
        match select(Timer::at(watchdog_deadline), uart_fut).await {
            Either::First(_) => {
                defmt::error!("Watchdog timeout!");
            }
            Either::Second(evt) => match evt {
                SlaveEvent::Watchdog => {
                    watchdog_deadline = Instant::now() + Duration::from_millis(500);
                }
                SlaveEvent::KeyEvent(evt) => {
                    key_event_tx.send(evt + 28).await;
                }
            },
        }
    }
}

#[embassy_executor::task]
async fn led_task() {
    const NUM_LEDS: usize = 28;
    let p = unsafe { embassy_rp::Peripherals::steal() };
    let mut data = [RGB8::default(); 28];
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    // Common neopixel pins:
    // Thing plus: 8
    // Adafruit Feather: 16;  Adafruit Feather+RFM95: 4
    let program = PioWs2812Program::new(&mut common);
    let mut ws2812 = PioWs2812::new(&mut common, sm0, p.DMA_CH0, Irqs, p.PIN_0, &program);

    // Loop forever making RGB values and pushing them out to the WS2812.
    let mut ticker = Ticker::every(Duration::from_millis(10));
    loop {
        for j in 0..(256 * 5) {
            for i in 0..28 {
                data[i] = wheel((((i * 256) as u16 / NUM_LEDS as u16 + j as u16) & 255) as u8);
            }
            ws2812.write(&data).await;

            ticker.next().await;
        }
    }
}

#[embassy_executor::task]
async fn hid_task(
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
        report_descriptor: ConfigReport::desc(),
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

    // Map to hold key event to be sent
    let mut evt_map: LinearMap<usize, KeyMap, 6> = LinearMap::new();

    let (reader, mut writer) = hid.split();
    let mut ticker = Ticker::every(Duration::from_millis(60));

    // Input future receiving keypresses, mapping them and sending the keyboard HID report to host
    let in_fut = async {
        loop {
            // Future for next key event
            let key_evt_fut = async {
                match select(evt_rx.receive(), buttons.wait_key_event()).await {
                    Either::First(evt) => evt,
                    Either::Second(evt) => evt,
                }
            };

            match select(key_evt_fut, ticker.next()).await {
                Either::First(evt) => match evt {
                    KeyEvent::KeyPressed(id) => {
                        debug!("Key Pressed: {}", id);
                        if let Some(k) = KEY_MAP[id].press {
                            let _ = evt_map.insert(id, k);
                        }
                    }
                    KeyEvent::KeyHeld(id) => {
                        debug!("Key Held: {}", id);
                        if let Some(k) = KEY_MAP[id].held {
                            let _ = evt_map.insert(id, k);
                        } else if let Some(k) = KEY_MAP[id].press {
                            let _ = evt_map.insert(id, k);
                        }
                    }
                    KeyEvent::KeyReleased(id) => {
                        debug!("Key Released: {}", id);
                        evt_map.remove(&id);
                    }
                },
                Either::Second(_) => {
                    // Map pressed keys to keycodes and modifiers for report
                    let mut keycodes: [u8; 6] = [0, 0, 0, 0, 0, 0];
                    let mut modifier: u8 = 0;
                    for (i, keymap) in evt_map.values().enumerate() {
                        keycodes[i] = keymap.key as u8;
                        modifier |= keymap.modifiers;
                    }
                    evt_map.clear();
                    debug!(
                        "Sedining report, modifier: {}, keycodes: {}",
                        modifier, keycodes
                    );

                    if HID_PROTOCOL_MODE.load(Ordering::Relaxed) == HidProtocolMode::Boot as u8 {
                        // In BOOT mode report is 8 bytes, first byte are the modifiers, second is
                        // reservered 0, last 6 are keycodes
                        let mut boot_report = [0_u8; 8];
                        boot_report[0] = modifier;
                        boot_report[2..].clone_from_slice(&keycodes);
                        match writer.write(&boot_report).await {
                            Ok(()) => {}
                            Err(e) => warn!("Failed to send boot report: {:?}", e),
                        };
                    } else {
                        let report = KeyboardReport {
                            keycodes,
                            leds: 0,
                            modifier,
                            reserved: 0,
                        };
                        // Send the report.
                        match writer.write_serialize(&report).await {
                            Ok(()) => {}
                            Err(e) => warn!("Failed to send report: {:?}", e),
                        };
                    }
                }
            };
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
                Ok(_) => {}
                Err(e) => {
                    defmt::error!("CFG HID read error {:?}", e);
                }
            }
        }
    };

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
