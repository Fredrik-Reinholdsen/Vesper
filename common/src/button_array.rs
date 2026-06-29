use core::{future::pending, ops::Add, pin};

use defmt::Format;
use embassy_futures::select::{Either, select, select_slice};
use embassy_rp::gpio::{Input, Level};
use embassy_time::{Duration, Instant, Timer};
use serde::{Deserialize, Serialize};

#[derive(Debug, Format, PartialEq, Serialize, Deserialize)]
pub enum KeyEvent {
    KeyPressed(usize),
    KeyReleased(usize),
    KeyHeld(usize),
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum EdgeState {
    Waiting {
        last_level: Level,
    },
    Debounce {
        last_level: Level,
        deadline: Instant,
    },
}

impl Add<usize> for KeyEvent {
    type Output = Self;
    fn add(self, rhs: usize) -> Self::Output {
        match self {
            KeyEvent::KeyPressed(id) => KeyEvent::KeyPressed(id + rhs),
            KeyEvent::KeyHeld(id) => KeyEvent::KeyHeld(id + rhs),
            KeyEvent::KeyReleased(id) => KeyEvent::KeyReleased(id + rhs),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum KeyState {
    Pressed(Instant),
    Held,
    None,
}

async fn wait_for_edge_event(
    id: usize,
    pin: &mut Input<'_>,
    edge_state: &mut EdgeState,
    debounce_t: embassy_time::Duration,
) -> KeyEvent {
    loop {
        match edge_state {
            EdgeState::Waiting { last_level } => {
                pin.wait_for_any_edge().await;

                *edge_state = EdgeState::Debounce {
                    deadline: Instant::now() + debounce_t,
                    last_level: *last_level,
                };
            }

            EdgeState::Debounce {
                deadline,
                last_level,
            } => {
                Timer::at(*deadline).await;

                let current = pin.get_level();

                let level = *last_level;
                *edge_state = EdgeState::Waiting { last_level: level };

                if current != level {
                    *edge_state = EdgeState::Waiting {
                        last_level: current,
                    };

                    break match current {
                        Level::Low => KeyEvent::KeyPressed(id),
                        Level::High => KeyEvent::KeyReleased(id),
                    };
                }
            }
        }
    }
}

// Helper: hold detection
async fn wait_for_hold_event(
    id: usize,
    state: &KeyState,
    hold_t: embassy_time::Duration,
) -> KeyEvent {
    match state {
        KeyState::Pressed(t) => {
            Timer::at(*t + hold_t).await;
            KeyEvent::KeyHeld(id)
        }
        _ => pending().await,
    }
}

pub struct ButtonArray<'a, const NPINS: usize> {
    inputs: [Input<'a>; NPINS],
    key_states: [KeyState; NPINS],
    edge_states: [EdgeState; NPINS],
    debounce: Duration,
    hold: Duration,
}

impl<'a, const NPINS: usize> ButtonArray<'a, NPINS> {
    pub fn new(mut inputs: [Input<'a>; NPINS], debounce: Duration) -> Self {
        // Enable schmitt trigger for each pin
        inputs.iter_mut().for_each(|pin| pin.set_schmitt(true));

        Self {
            inputs,
            debounce,
            key_states: [KeyState::None; NPINS],
            edge_states: [EdgeState::Waiting {
                last_level: Level::High,
            }; NPINS],
            hold: Duration::from_millis(1000),
        }
    }

    pub async fn wait_key_event(&mut self) -> KeyEvent {
        let debounce_t = self.debounce;
        let hold_t = self.hold;

        // Build edge futures
        let mut edge_futs: heapless::Vec<_, NPINS> = self
            .inputs
            .iter_mut()
            .zip(self.edge_states.iter_mut())
            .enumerate()
            .map(|(id, (pin, edge_state))| wait_for_edge_event(id, pin, edge_state, debounce_t))
            .collect();

        // Build hold futures
        let mut held_futs: heapless::Vec<_, NPINS> = self
            .key_states
            .iter()
            .enumerate()
            .map(|(id, state)| wait_for_hold_event(id, state, hold_t))
            .collect();

        // Select first edge event
        let edge_fut = async {
            let (evt, _) = select_slice(core::pin::pin!(&mut edge_futs)).await;
            evt
        };

        // Select first hold event
        let held_fut = async {
            let (evt, _) =
                embassy_futures::select::select_slice(core::pin::pin!(&mut held_futs)).await;
            evt
        };

        // Race edge vs hold
        let evt = match select(edge_fut, held_fut).await {
            Either::First(e) | Either::Second(e) => e,
        };

        // Explicitly drop to cancel and release borrow for key_states
        drop(edge_futs);
        drop(held_futs);

        // Update state
        match evt {
            KeyEvent::KeyPressed(id) => {
                self.key_states[id] = KeyState::Pressed(Instant::now());
            }
            KeyEvent::KeyReleased(id) => {
                self.key_states[id] = KeyState::None;
            }
            KeyEvent::KeyHeld(id) => {
                self.key_states[id] = KeyState::Held;
            }
        }

        evt
    }
}
