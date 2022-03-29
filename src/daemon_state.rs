use std::collections::HashMap;
use std::process::exit;
use std::time::Instant;

use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::{AdvertisementData, CentralEvent};
use core_bluetooth::uuid::Uuid;
use core_bluetooth::ManagerState;
use log::info;

pub struct DaemonState {
    pub started_at: Instant,
    pub peripherals: HashMap<Uuid, Peripheral>,
    pub advertisements: HashMap<Uuid, AdvertisementData>,

    state: ManagerState,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            peripherals: Default::default(),
            advertisements: Default::default(),

            state: ManagerState::Unknown,
        }
    }

    pub fn find_connected_peripheral_by_service(
        &self,
        uuid: Uuid,
    ) -> Option<(Peripheral, AdvertisementData)> {
        self.advertisements
            .iter()
            .find(|(_, ad)| {
                ad.service_uuids()
                    .iter()
                    .find(|&service_uuid| service_uuid.eq(&uuid))
                    .is_some()
            })
            .and_then(|(peripheral_uuid, ad)| {
                Some((
                    self.peripherals.get(peripheral_uuid).unwrap().clone(),
                    ad.clone(),
                ))
            })
    }

    pub fn get_peripheral(&self, uuid: Uuid) -> Option<(Peripheral, AdvertisementData)> {
        if let Some(peripheral) = self.peripherals.get(&uuid) {
            Some((
                peripheral.clone(),
                self.advertisements.get(&peripheral.id()).unwrap().clone(),
            ))
        } else {
            None
        }
    }

    pub fn handle_event(&mut self, event: &CentralEvent) {
        match event {
            CentralEvent::ManagerStateChanged { new_state } => {
                self.state = *new_state;
                match new_state {
                    ManagerState::Unsupported => {
                        eprintln!("Bluetooth is not supported on this system");
                        exit(1);
                    }
                    ManagerState::Unauthorized => {
                        eprintln!("not authorized to use Bluetooth on this system");
                        exit(1);
                    }
                    ManagerState::PoweredOff => {
                        eprintln!("Bluetooth is disabled, please enable it");
                        // TODO(df): Clean up child handler
                    }
                    ManagerState::PoweredOn => {
                        info!("Bluetooth is powered on, ready for processing");

                        // Scanning without specifying service UUIDs will not work on MacOSX 12.1
                        // see https://stackoverflow.com/a/70657368/1016019
                        // central.scan();
                    }
                    _ => {}
                }
            }
            CentralEvent::PeripheralDisconnected {
                peripheral,
                error: _,
            } => {
                self.peripherals.remove(&peripheral.id());
                info!(
                    "unregistered peripheral {}, total {}",
                    peripheral.id(),
                    self.peripherals.len()
                );
            }
            _ => {}
        }
    }
}
