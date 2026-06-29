use defmt::debug;
use embassy_futures::select::{Either, select};
use embassy_rp::{
    gpio::Output,
    uart::{BufferedUart, Error as UartError},
};
use embassy_time::{Duration, Timer};
use embedded_crc32c::crc32c;
use embedded_hal_1::digital::OutputPin;
use embedded_io_async::{Read, Write};

const FLASH_SIZE_KB: usize = 4096;
const FLASH_SIZE: usize = FLASH_SIZE_KB * 1024;
const FLASH_PAGE_SIZE: usize = 256;
const FLASH_SECTOR_SIZE: usize = 4096;

const SLAVE_APPCODE_OFFSET: u32 = 32 * 1024;

const SLAVE_BIN: &[u8] = include_bytes!("../../target/thumbv8m.main-none-eabihf/release/slave.bin");

#[derive(Debug)]
pub enum Error {
    UartError(UartError),
    InvalidLength,
    InvalidData,
    Timeout,
    InvalidOffset,
    Verification,
    Uf2Error,
    Activation,
    UnexpectedEof,
}

impl From<UartError> for Error {
    fn from(err: UartError) -> Self {
        Error::UartError(err)
    }
}

#[repr(u8)]
pub enum BootloaderCommand {
    ReadyBusy = 0x1,
    Version = 0x2,
    Read = 0x10,
    Program = 0x20,
    Erase = 0x30,
    GotoAppcode = 0x40,
    FlashSize = 0x50,
    Activate = 0xA5,
}

pub struct SlaveBootloader<'b> {
    slave_en_pin: Output<'b>,
    tx_buf: [u8; FLASH_PAGE_SIZE + 7],
    rx_buf: [u8; FLASH_PAGE_SIZE],
    timeout: Duration,
}

impl<'a> SlaveBootloader<'a> {
    pub fn new(slave_en_pin: Output<'a>) -> Self {
        Self {
            tx_buf: [0; FLASH_PAGE_SIZE + 7],
            rx_buf: [0; FLASH_PAGE_SIZE],
            slave_en_pin,
            timeout: Duration::from_millis(1000),
        }
    }
    pub async fn boot_slave(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        if self.slave_en_pin.is_set_high() {
            return Ok(());
        }

        // Enable power to slave board and wait for it to come up
        self.slave_en_pin.set_high();
        Timer::after_millis(500).await;

        // NOTE - When slave board boots it will briefly pull UART TX pin low
        // causing receiver to think data is coming when it is not triggering a frame error.
        // Do a double dummy read to clear error and RX FIFO
        _ = uart.read(&mut self.rx_buf[..1]).await;
        _ = select(uart.read(&mut self.rx_buf[..1]), Timer::after_millis(100)).await;

        debug!("Activating bootloader...");
        self.activate(uart).await?;

        let mut slave_crc: u32 = 0;
        let crc_page_offset = if SLAVE_BIN.len().is_multiple_of(FLASH_PAGE_SIZE) {
            SLAVE_BIN.len() / FLASH_PAGE_SIZE
        } else {
            (SLAVE_BIN.len() / FLASH_PAGE_SIZE) + 1
        };
        let crc_offset = (crc_page_offset * FLASH_PAGE_SIZE) as u32 + SLAVE_APPCODE_OFFSET;
        self.read(uart, crc_offset, unsafe {
            core::slice::from_raw_parts_mut(&mut slave_crc as *mut _ as *mut u8, 4)
        })
        .await?;

        let crc = crc32c(SLAVE_BIN);
        if crc != slave_crc {
            defmt::info!(
                "CRC mismatch, {} != {} reprogramming slave...",
                crc,
                slave_crc
            );
            self.program_slave(uart).await?;
        }

        // Jump to application code and start program
        self.goto_appcode(uart).await?;

        _ = uart.read(&mut self.rx_buf[..1]).await;
        _ = select(uart.read(&mut self.rx_buf[..1]), Timer::after_millis(100)).await;
        Ok(())
    }

    pub async fn reboot_slave(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        defmt::info!("Rebooting slave...");
        self.slave_en_pin.set_low();
        Timer::after_millis(100).await;
        self.boot_slave(uart).await
    }

    pub async fn try_read(&mut self, uart: &mut BufferedUart, n: usize) -> Result<(), Error> {
        let fut = select(
            Timer::after(self.timeout),
            uart.read_exact(&mut self.rx_buf[..n]),
        );
        match fut.await {
            Either::First(()) => Err(Error::Timeout),
            Either::Second(res) => Ok(res.map_err(|_| Error::UnexpectedEof)?),
        }
    }

    pub async fn try_write(&self, uart: &mut BufferedUart, buf: &[u8]) -> Result<(), Error> {
        let fut = select(Timer::after(self.timeout), uart.write_all(buf));
        match fut.await {
            Either::First(()) => Err(Error::Timeout),
            Either::Second(res) => Ok(res.map_err(|_| Error::UnexpectedEof)?),
        }
    }

    /// Waits for picoboot slave bootloader to be ready for next instruction
    async fn activate(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        self.tx_buf[0] = BootloaderCommand::Activate as u8;
        self.try_write(uart, &self.tx_buf[..1]).await?;

        debug!("Reading activation response!");
        self.try_read(uart, 4).await?;

        if &self.rx_buf[..4] == b"pbt3" {
            defmt::info!("Slave bootloader activated");
            Ok(())
        } else {
            defmt::error!(
                "Activation failed! Expected: 0x33746270, got: {:#010x}",
                u32::from_le_bytes(self.rx_buf[0..4].try_into().unwrap()),
            );
            Err(Error::Activation)
        }
    }

    /// Waits for picoboot slave bootloader to be ready for next instruction
    pub async fn is_ready(&mut self, uart: &mut BufferedUart) -> Result<bool, Error> {
        self.tx_buf[0] = BootloaderCommand::ReadyBusy as u8;
        self.try_write(uart, &self.tx_buf[..1]).await?;

        self.rx_buf[0] = 0;
        self.try_read(uart, 1).await?;
        Ok(self.rx_buf[0] != 0)
    }

    /// Waits for picoboot slave bootloader to be ready for next instruction
    pub async fn wait_ready(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        let timeout = self.timeout;
        let read_fut = async {
            while !self.is_ready(uart).await? {}
            Ok::<(), Error>(())
        };
        let fut = select(Timer::after(timeout), read_fut);
        match fut.await {
            Either::First(()) => Err(Error::Timeout),
            Either::Second(res) => Ok(res?),
        }
    }

    /// Waits for picoboot slave bootloader to be ready for next instruction
    pub async fn read(
        &mut self,
        uart: &mut BufferedUart,
        offset: u32,
        buf: &mut [u8],
    ) -> Result<(), Error> {
        // Check length of data to read
        if buf.is_empty() || buf.len() > 4096 {
            return Err(Error::InvalidLength);
        }

        self.tx_buf[0] = BootloaderCommand::Read as u8;
        self.tx_buf[1..5].clone_from_slice(&offset.to_le_bytes());
        self.tx_buf[5..7].clone_from_slice(&(buf.len() as u16).to_le_bytes());

        self.try_write(uart, &self.tx_buf[..7]).await?;

        let fut = select(Timer::after(self.timeout), uart.read_exact(buf));
        match fut.await {
            Either::First(()) => Err(Error::Timeout),
            Either::Second(res) => Ok(res.map_err(|_| Error::UnexpectedEof)?),
        }
    }

    async fn erase_sector(&mut self, uart: &mut BufferedUart, sector: u16) -> Result<(), Error> {
        if sector as usize * FLASH_SECTOR_SIZE >= FLASH_SIZE {
            return Err(Error::InvalidOffset);
        }
        debug!(
            "BOOTLOADER - Erasing flash sector: {:#06x}, offset = {:#010x}",
            sector,
            sector as usize * FLASH_SECTOR_SIZE
        );
        self.tx_buf[0] = BootloaderCommand::Erase as u8;
        self.tx_buf[1..3].clone_from_slice(&sector.to_le_bytes());
        self.try_write(uart, &self.tx_buf[..3]).await?;
        self.wait_ready(uart).await?;

        Ok(())
    }

    async fn verify_page(
        &mut self,
        uart: &mut BufferedUart,
        page: u16,
        data: &[u8],
    ) -> Result<(), Error> {
        if page as usize * FLASH_PAGE_SIZE >= FLASH_SIZE {
            return Err(Error::InvalidOffset);
        }
        self.tx_buf[0] = BootloaderCommand::Read as u8;
        let offset = page as u32 * FLASH_PAGE_SIZE as u32;
        self.tx_buf[1..5].clone_from_slice(&offset.to_le_bytes());
        self.tx_buf[5..7].clone_from_slice(&(FLASH_PAGE_SIZE as u16).to_le_bytes());

        self.try_write(uart, &self.tx_buf[..7]).await?;

        self.try_read(uart, FLASH_PAGE_SIZE).await?;
        if data != &self.rx_buf[..data.len()] {
            defmt::error!(
                "BOOTLOADER - Page verification failed for page = {:#06x}, offset = {:#010x}",
                page,
                page as usize * FLASH_PAGE_SIZE
            );
            Err(Error::Verification)
        } else {
            Ok(())
        }
    }

    async fn program(
        &mut self,
        uart: &mut BufferedUart,
        offset: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        if !offset.is_multiple_of(FLASH_PAGE_SIZE as u32) {
            return Err(Error::InvalidOffset);
        }

        if offset as usize + data.len() > FLASH_SIZE {
            return Err(Error::InvalidLength);
        }

        let pages = data.chunks_exact(FLASH_PAGE_SIZE);
        let rem = pages.remainder();
        let mut curr_offset = offset;
        for page in pages {
            self.program_page(uart, curr_offset, page).await?;
            curr_offset += FLASH_PAGE_SIZE as u32;
        }

        // Write remaining data if any
        if !rem.is_empty() {
            self.program_page(uart, curr_offset, rem).await?;
            curr_offset += FLASH_PAGE_SIZE as u32;
        }

        // Program CRC to beginning of next page
        let crc = crc32c(data);
        self.program_page(uart, curr_offset, &crc.to_le_bytes())
            .await?;
        Ok(())
    }

    async fn program_page(
        &mut self,
        uart: &mut BufferedUart,
        offset: u32,
        data: &[u8],
    ) -> Result<(), Error> {
        if !offset.is_multiple_of(FLASH_PAGE_SIZE as u32) {
            return Err(Error::InvalidOffset);
        }

        if offset >= FLASH_SIZE as u32 {
            return Err(Error::InvalidLength);
        }

        if data.len() > 256 {
            return Err(Error::InvalidLength);
        }

        let page_offset = (offset / FLASH_PAGE_SIZE as u32) as u16;
        debug!(
            "BOOTLOADER - Writing flash page = {:#06x}, offset = {:#010x}",
            page_offset, offset,
        );

        self.tx_buf[0] = BootloaderCommand::Program as u8;
        self.tx_buf[1..5].clone_from_slice(&offset.to_le_bytes());
        self.tx_buf[5..7].clone_from_slice(&(FLASH_PAGE_SIZE as u16).to_le_bytes());
        // Fill transmit buffer and pad with zeros
        self.tx_buf.as_mut_slice()[7..7 + data.len()].clone_from_slice(data);
        self.tx_buf.as_mut_slice()[7 + data.len()..]
            .iter_mut()
            .for_each(|v| *v = 0xff);

        self.try_write(uart, &self.tx_buf).await?;
        self.wait_ready(uart).await?;
        debug!("BOOTLOADER - Verifying page...");
        self.verify_page(uart, page_offset, data).await
    }

    async fn goto_appcode(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        self.tx_buf[0] = BootloaderCommand::GotoAppcode as u8;
        self.try_write(uart, &self.tx_buf[..1]).await?;
        Ok(())
    }

    async fn program_slave(&mut self, uart: &mut BufferedUart) -> Result<(), Error> {
        let num_sectors = SLAVE_BIN.len() / FLASH_SECTOR_SIZE;
        let first_erase_sector = (SLAVE_APPCODE_OFFSET / FLASH_SECTOR_SIZE as u32) as u16;
        for sector in 0..num_sectors + 1 {
            self.erase_sector(uart, first_erase_sector + sector as u16)
                .await?;
        }
        self.program(uart, SLAVE_APPCODE_OFFSET, SLAVE_BIN).await?;
        Ok(())
    }
}
