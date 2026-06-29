use crate::button_array::KeyEvent;
use serde::{Deserialize, Serialize};

pub const STX: u8 = 0x02;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum SlaveEvent {
    KeyEvent(KeyEvent),
    Watchdog,
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Handshake,
    Reset,
}
