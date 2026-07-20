#![no_std]
#![no_main]

use common::button_array::{ButtonArray, KeyEvent};
use common::command::SlaveEvent;
use embassy_futures::join::join3;
use embassy_rp::block::ImageDef;

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_rp::gpio::{Input, Output, Pull};
use embassy_rp::peripherals::{DMA_CH0, PIO0, UART0};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::pio_programs::ws2812::{PioWs2812, PioWs2812Program};
use embassy_rp::uart::{BufferedInterruptHandler, BufferedUart, BufferedUartRx, Config};
use embassy_rp::{Peripherals, bind_interrupts};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker, Timer};
use embedded_hal_1::digital::OutputPin;
use embedded_io_async::{Read, Write};
use postcard::to_slice;
use smart_leds::RGB8;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    UART0_IRQ => BufferedInterruptHandler<UART0>;
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>;
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

// Program metadata for `picotool info`.
// This isn't needed, but it's recomended to have these minimal entries.
#[unsafe(link_section = ".bi_entries")]
#[used]
pub static PICOTOOL_ENTRIES: [embassy_rp::binary_info::EntryAddr; 4] = [
    embassy_rp::binary_info::rp_program_name!(c"Vesper Slave"),
    embassy_rp::binary_info::rp_program_description!(
        c"Slave board program for the Vesper split keyboard."
    ),
    embassy_rp::binary_info::rp_cargo_version!(),
    embassy_rp::binary_info::rp_program_build_attribute!(),
];

#[unsafe(link_section = ".start_block")]
#[used]
static IMAGE_DEF: ImageDef = ImageDef::secure_exe(); // Update this with your own implementation.

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut led = Output::new(p.PIN_23, embassy_rp::gpio::Level::Low);
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    let (tx_pin, rx_pin, uart) = (p.PIN_28, p.PIN_29, p.UART0);

    static TX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
    let tx_buf = &mut TX_BUF.init([0; 16])[..];
    static RX_BUF: StaticCell<[u8; 16]> = StaticCell::new();
    let rx_buf = &mut RX_BUF.init([0; 16])[..];
    let mut uart = BufferedUart::new(
        uart,
        tx_pin,
        rx_pin,
        Irqs,
        tx_buf,
        rx_buf,
        Config::default(),
    );

    let input_keys = [
        Input::new(p.PIN_22, Pull::None),
        Input::new(p.PIN_21, Pull::None),
        Input::new(p.PIN_20, Pull::None),
        Input::new(p.PIN_19, Pull::None),
        Input::new(p.PIN_18, Pull::None),
        Input::new(p.PIN_17, Pull::None),
        Input::new(p.PIN_13, Pull::None),
        Input::new(p.PIN_12, Pull::None),
        Input::new(p.PIN_11, Pull::None),
        Input::new(p.PIN_10, Pull::None),
        Input::new(p.PIN_9, Pull::None),
        Input::new(p.PIN_8, Pull::None),
        Input::new(p.PIN_7, Pull::None),
        Input::new(p.PIN_6, Pull::None),
        Input::new(p.PIN_5, Pull::None),
        Input::new(p.PIN_4, Pull::None),
        Input::new(p.PIN_3, Pull::None),
        Input::new(p.PIN_2, Pull::None),
        Input::new(p.PIN_42, Pull::None),
        Input::new(p.PIN_43, Pull::None),
        Input::new(p.PIN_44, Pull::None),
        Input::new(p.PIN_45, Pull::None),
        Input::new(p.PIN_46, Pull::None),
        Input::new(p.PIN_47, Pull::None),
        Input::new(p.PIN_36, Pull::None),
        Input::new(p.PIN_37, Pull::None),
        Input::new(p.PIN_38, Pull::None),
        Input::new(p.PIN_39, Pull::None),
    ];

    spawner.spawn(unwrap!(led_task()));

    let mut buttons = ButtonArray::new(input_keys, Duration::from_millis(1));
    let mut buf: [u8; 512] = [0; 512];
    let evt_channel = Channel::<NoopRawMutex, SlaveEvent, 3>::new();
    let evt_tx = evt_channel.sender();

    let watchdog_fut = async {
        let mut watchdog_ticker = Ticker::every(Duration::from_millis(100));
        loop {
            watchdog_ticker.next().await;
            evt_tx.send(SlaveEvent::Watchdog).await;
        }
    };

    let key_fut = async {
        loop {
            let evt = buttons.wait_key_event().await;
            evt_tx.send(SlaveEvent::KeyEvent(evt)).await;
        }
    };

    let evt_rx = evt_channel.receiver();
    let uart_fut = async {
        loop {
            let evt = evt_rx.receive().await;
            let s = to_slice(&evt, &mut buf[1..]).unwrap();
            let msg_len = s.len();
            buf[0] = msg_len as u8;
            let _ = uart.write_all(&buf[..msg_len + 1]).await;
        }
    };

    join3(uart_fut, key_fut, watchdog_fut).await;
}

#[embassy_executor::task]
async fn led_task() {
    const NUM_LEDS: usize = 28;
    let p = unsafe { Peripherals::steal() };
    let mut data = [RGB8::default(); 28];
    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    // Common neopixel pins:
    // Thing plus: 8
    // Adafruit Feather: 16;  Adafruit Feather+RFM95: 4
    let program = PioWs2812Program::new(&mut common);
    let mut ws2812 = PioWs2812::new(&mut common, sm0, p.DMA_CH0, Irqs, p.PIN_32, &program);

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
