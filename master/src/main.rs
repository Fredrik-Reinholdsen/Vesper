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
use embassy_usb::control::OutResponse;
use embassy_usb::driver::Driver;
use embassy_usb::{Builder, Config, Handler};
use embedded_hal_1::digital::OutputPin;
use embedded_io_async::{Read, Write};
use postcard::from_bytes;
use smart_leds::RGB8;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

pub mod hid;

use crate::hid::hid_task;

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
    USBCTRL_IRQ => embassy_rp::usb::InterruptHandler<USB>;
});

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

    let buttons = ButtonArray::new(key_inputs, Duration::from_micros(1000));

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
