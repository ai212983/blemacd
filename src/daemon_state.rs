use std::collections::HashMap;
use std::fmt;
use std::fmt::Formatter;
use std::process::exit;
use std::time::Instant;

use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::{AdvertisementData, CentralEvent};
use core_bluetooth::uuid::Uuid;
use core_bluetooth::ManagerState;
use log::info;

pub struct Advertisement {
    pub uuid: Uuid,
    pub data: AdvertisementData,
    pub rssi: i32,
}

impl Advertisement {
    pub fn new(uuid: Uuid, data: AdvertisementData, rssi: i32) -> Self {
        Self { uuid, data, rssi }
    }
}

impl Clone for Advertisement {
    fn clone(&self) -> Self {
        Advertisement::new(self.uuid.clone(), self.data.clone(), self.rssi)
    }
}

impl fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let service_uuids = self
            .data
            .service_uuids()
            .iter()
            .map(|u| hex::encode(u.shorten()))
            .collect::<Vec<String>>();
        write!(
            f,
            "{} ({}), {}/{}",
            self.data.local_name().unwrap_or(&*self.uuid.to_string()),
            service_uuids.join(", "),
            self.rssi,
            self.data.tx_power_level().unwrap_or(0)
        )
        .unwrap();
        Ok(())
    }
}

pub struct DaemonState {
    pub started_at: Instant,
    pub peripherals: HashMap<Uuid, Peripheral>,
    pub advertisements: HashMap<Uuid, Advertisement>,

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

    pub fn find_connected_peripheral_by_service(&self, uuid: Uuid) -> Option<Peripheral> {
        self.advertisements
            .iter()
            .find(|(_, ad)| {
                ad.data
                    .service_uuids()
                    .iter()
                    .find(|&service_uuid| service_uuid.eq(&uuid))
                    .is_some()
            })
            .and_then(|(peripheral_uuid, _ad)| {
                Some(self.peripherals.get(peripheral_uuid).unwrap().clone())
            })
    }

    pub fn get_peripheral(&self, uuid: Uuid) -> Option<(Peripheral, AdvertisementData)> {
        if let Some(peripheral) = self.peripherals.get(&uuid) {
            Some((
                peripheral.clone(),
                self.advertisements
                    .get(&peripheral.id())
                    .unwrap()
                    .data
                    .clone(),
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

            CentralEvent::PeripheralDiscovered {
                peripheral,
                advertisement_data,
                rssi,
            } => {
                self.advertisements.insert(
                    peripheral.id(),
                    Advertisement::new(peripheral.id(), advertisement_data.clone(), *rssi),
                );
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
