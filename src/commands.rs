use std::time::Duration;

use core_bluetooth::central::{AdvertisementData, CentralEvent, CentralManager, ScanOptions};
use core_bluetooth::central::characteristic::{Characteristic, WriteKind};
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::service::Service;
use core_bluetooth::error::Error;
use core_bluetooth::uuid::Uuid;
use log::{error, info};
use postage::oneshot;
use postage::prelude::Sink;

use crate::daemon_state::DaemonState;

#[derive(Debug, Clone)]
pub enum CommandResult {
    GetStatus(Duration, Option<usize>),
    ListPeripherals(Vec<(Peripheral, AdvertisementData)>),
    FindPeripheral(Option<(Peripheral, AdvertisementData)>),
    ConnectToPeripheral(Peripheral),
    FindService(Peripheral, Option<Service>),
    FindCharacteristic(Peripheral, Option<Characteristic>),
    ReadCharacteristic(Peripheral, Characteristic, Option<Vec<u8>>),
    WriteCharacteristic(Peripheral, Characteristic, Result<(), Error>),
}

#[derive(Debug, Clone)]
pub enum Command {
    GetStatus,
    ListPeripherals,
    FindPeripheralByService(Uuid),
    ConnectToPeripheral(Peripheral, AdvertisementData),
    RegisterConnectedPeripheral(Peripheral),
    UnregisterConnectedPeripheral(Uuid),
    FindService(Peripheral, String),
    FindCharacteristic(Peripheral, Service, String, uuid::Uuid),
    // last uuid is not BLE uuid, but internal uuid used for hashmap
    ReadCharacteristic(Peripheral, Characteristic),
    WriteCharacteristic(Peripheral, Characteristic, Vec<u8>),
}

impl Command {
    pub fn execute(&self, state: &mut DaemonState, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager) {
        match self {
            Command::GetStatus => sender.blocking_send(CommandResult::GetStatus(
                Duration::new(state.started_at.elapsed().as_secs(), 0),
                Some(state.peripherals.len()), //TODO(df): Update status output
            )).unwrap(),

            Command::ListPeripherals => sender.blocking_send(CommandResult::ListPeripherals({
                state.peripherals.values().into_iter()
                    .map(|p| return (p.clone(), state.advertisements.get(&p.id()).unwrap().clone()))
                    .collect()
            })).unwrap(),

            Command::FindPeripheralByService(uuid) => {
                info!("looking for peripheral with service [{}]", uuid);
                if let Some(result) = state.find_connected_peripheral_by_service(uuid.clone()) {
                    info!("found already connected peripheral [{}]", result.0.id());
                    central.cancel_scan();
                    sender.blocking_send(CommandResult::FindPeripheral(Some(result))).unwrap();
                } else {
                    info!("no matching peripheral found, starting scan");
                    // TODO(df): Add timeout and Ctrl+C handling
                    let uuid = uuid.clone();
                    state.add_matcher(sender, Box::new(move |event| {
                        if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
                            if advertisement_data.service_uuids().iter()
                                .find(|&u| u.eq(&uuid)).is_some() {
                                return EventMatch::Result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))));
                            }
                        }
                        EventMatch::None
                    }));
                    central.scan_with_options(ScanOptions::default().include_services(&vec![uuid]));
                }
            }
            Command::ConnectToPeripheral(ref peripheral, ref advertisement_data) => {
                let id = peripheral.id().clone();
                if let Some(result) = state.get_peripheral(id) {
                    sender.blocking_send(CommandResult::ConnectToPeripheral(result.0)).unwrap();
                } else {
                    state.advertisements.insert(id, advertisement_data.clone());
                    let peripheral_uuid = peripheral.id();
                    state.add_matcher(sender, Box::new(move |event| {
                        if let CentralEvent::PeripheralConnected { peripheral } = event {
                            if peripheral.id() == peripheral_uuid {
                                return EventMatch::Next(Command::RegisterConnectedPeripheral(peripheral.clone()));
                            }
                        } else if let CentralEvent::PeripheralConnectFailed { peripheral, error } = event {
                            if peripheral.id() == peripheral_uuid {
                                let id = peripheral.id().clone();
                                error!("failed to connect to peripheral {:?}: {:?}", peripheral, error);
                                return EventMatch::Next(Command::UnregisterConnectedPeripheral(id));
                            }
                        }
                        EventMatch::None
                    }));
                    central.connect(&peripheral);
                }
            }
            Command::RegisterConnectedPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                state.peripherals.insert(id, peripheral.clone());
                info!("peripheral [{}]{} registered, total {}", id,
                    state.advertisements.get(&id).and_then(|ad| ad.local_name()).map_or(String::new(), |s| format!(" ({})", s)),
                    state.peripherals.len());
                sender.blocking_send(CommandResult::ConnectToPeripheral(peripheral.clone())).unwrap();
            }
            Command::UnregisterConnectedPeripheral(uuid) => {
                state.peripherals.remove(&uuid);
                state.advertisements.remove(&uuid);
            }
            Command::FindService(ref peripheral, uuid_substr) => {
                let id = peripheral.id();
                let uuid_substr = uuid_substr.clone();

                state.add_matcher(sender, Box::new(move |event| {
                    if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
                        if peripheral.id() == id {
                            let uuid_substr = uuid_substr.clone();
                            return EventMatch::Result(CommandResult::FindService(
                                peripheral.clone(),
                                find_by_id_substr(uuid_substr, services)));
                        }
                    }
                    EventMatch::None
                }));
                peripheral.discover_services();
            }

            Command::FindCharacteristic(ref peripheral, ref service, uuid_substr, _) => {
                let peripheral_id = peripheral.id();
                let service_id = service.id();
                let uuid_substr = uuid_substr.clone();

                state.add_matcher(sender, Box::new(move |event| {
                    if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
                        if peripheral.id() == peripheral_id && service.id() == service_id {
                            let uuid_substr = uuid_substr.clone();
                            return EventMatch::Result(CommandResult::FindCharacteristic(
                                peripheral.clone(),
                                find_by_id_substr(uuid_substr, characteristics)));
                        }
                    }
                    EventMatch::None
                }));
                peripheral.discover_characteristics(service);
            }
            Command::ReadCharacteristic(ref peripheral, ref characteristic) => {
                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                state.add_matcher(sender,
                                  Box::new(move |event| {
                                      if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
                                          if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                                              return EventMatch::Result(CommandResult::ReadCharacteristic(
                                                  peripheral.clone(),
                                                  characteristic.clone(),
                                                  if let Ok(value) = value { Some(value.clone()) } else { None }));
                                          }
                                      }
                                      EventMatch::None
                                  }));
                peripheral.read_characteristic(characteristic);
            }
            Command::WriteCharacteristic(ref peripheral, ref characteristic, ref data) => {
                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();
                state.add_matcher(sender,
                                  Box::new(move |event| {
                                      if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
                                          if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                                              return EventMatch::Result(
                                                  CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone()));
                                          }
                                      }
                                      EventMatch::None
                                  }));
                peripheral.write_characteristic(characteristic, data, WriteKind::WithResponse);
            }
        }
    }
}

pub enum EventMatch {
    Next(Command),
    Result(CommandResult),
    None,
}

impl EventMatch {
    pub fn is_none(&self) -> bool {
        matches!(*self, EventMatch::None)
    }
}

pub type EventMatcher = dyn Fn(&CentralEvent) -> EventMatch + Send;


trait HasId {
    fn get_id(&self) -> Uuid;
}

impl HasId for Service {
    fn get_id(&self) -> Uuid {
        self.id().clone()
    }
}

impl HasId for Characteristic {
    fn get_id(&self) -> Uuid {
        self.id().clone()
    }
}

// TODO(df): Rework into `get_finder_by_id(id_substr: String)`
fn find_by_id_substr<T>(id_substr: String, items: &Result<Vec<T>, Error>) -> Option<T> where T: HasId + Clone {
    let id_substr = id_substr.as_str();
    return items.as_ref().ok()
        .and_then(|items| items.iter().find(|item| item.get_id().to_string().contains(id_substr)))
        .map(|item| item.clone());
}

