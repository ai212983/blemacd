use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_bluetooth::central::{AdvertisementData, CentralEvent, CentralManager, ScanOptions};
use core_bluetooth::central::characteristic::{Characteristic, WriteKind};
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::service::Service;
use core_bluetooth::error::Error;
use core_bluetooth::uuid::Uuid;
use log::{error, info};
use postage::mpsc::Sender;
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
// https://stackoverflow.com/questions/49142453/is-it-possible-to-call-a-fnonce-from-a-fn-without-a-mutex
// https://stackoverflow.com/questions/30411594/cannot-move-a-value-of-type-fnonce-when-moving-a-boxed-function

impl Command {
    pub fn execute(&self, state: &mut DaemonState, mut sender: oneshot::Sender<CommandResult>, central: &CentralManager,
                   mut command_sender: Sender<(Command, oneshot::Sender<CommandResult>)>) -> Option<EventMatcher> {

        // Sending command:
        // command_sender.try_send((command, sender)).ok();
        //
        // Sending result:
        // sender.blocking_send(result).unwrap();

        let command_sender = Arc::new(Mutex::new(Some(command_sender)));
        let result_sender = Arc::new(Mutex::new(Some(sender)));

        match self {
            Command::GetStatus => {
                result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::GetStatus(
                    Duration::new(state.started_at.elapsed().as_secs(), 0),
                    Some(state.peripherals.len()), //TODO(df): Update status output
                ));
                None
            },

            Command::ListPeripherals => {
                result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::ListPeripherals({
                    state.peripherals.values().into_iter()
                        .map(|p| return (p.clone(), state.advertisements.get(&p.id()).unwrap().clone()))
                        .collect()
                }));
                None
            },

            Command::FindPeripheralByService(uuid) => {
                info!("looking for peripheral with service [{}]", uuid);
                if let Some(result) = state.find_connected_peripheral_by_service(uuid.clone()) {
                    info!("found already connected peripheral [{}]", result.0.id());
                    central.cancel_scan();
                    result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::FindPeripheral(Some(result)));
                    return None;
                } else {
                    info!("no matching peripheral found, starting scan");
                    let uuid = uuid.clone();

                    central.scan_with_options(ScanOptions::default().include_services(&vec![uuid]));

                    // TODO(df): Add timeout and Ctrl+C handling

                    Some(Box::new(move |event| {
                        if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
                            if advertisement_data.service_uuids().iter()
                                .find(|&u| u.eq(&uuid)).is_some() {
                                result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))));
                                return true;
                                // return send_result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))));
                            }
                        }
                        false
                    }))
                }
            }
            Command::ConnectToPeripheral(ref peripheral, ref advertisement_data) => {
                let id = peripheral.id().clone();
                if let Some(result) = state.get_peripheral(id) {
                    result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::ConnectToPeripheral(result.0));
                    None
                } else {
                    state.advertisements.insert(id, advertisement_data.clone());
                    let peripheral_uuid = peripheral.id();
                    central.connect(&peripheral);
                    Some(Box::new(move |event| {
                        if let CentralEvent::PeripheralConnected { peripheral } = event {
                            if peripheral.id() == peripheral_uuid {
                                command_sender.lock().unwrap().take().unwrap().blocking_send((
                                    Command::RegisterConnectedPeripheral(peripheral.clone()),
                                    result_sender.lock().unwrap().take().unwrap()));
                                return true;
                            }
                        } else if let CentralEvent::PeripheralConnectFailed { peripheral, error } = event {
                            if peripheral.id() == peripheral_uuid {
                                let id = peripheral.id().clone();
                                error!("failed to connect to peripheral {:?}: {:?}", peripheral, error);
                                command_sender.lock().unwrap().take().unwrap().blocking_send((
                                    Command::UnregisterConnectedPeripheral(id),
                                    result_sender.lock().unwrap().take().unwrap()));
                                return true;
                            }
                        }
                        false
                    }))
                }
            }
            Command::RegisterConnectedPeripheral(peripheral) => {
                let id = peripheral.id().clone();
                state.peripherals.insert(id, peripheral.clone());
                info!("peripheral [{}]{} registered, total {}", id,
                    state.advertisements.get(&id).and_then(|ad| ad.local_name()).map_or(String::new(), |s| format!(" ({})", s)),
                    state.peripherals.len());
                result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::ConnectToPeripheral(peripheral.clone()));
                None
            }
            Command::UnregisterConnectedPeripheral(uuid) => {
                state.peripherals.remove(&uuid);
                state.advertisements.remove(&uuid);
                None
            }
            Command::FindService(ref peripheral, uuid_substr) => {
                peripheral.discover_services();

                let id = peripheral.id();
                let uuid_substr = uuid_substr.clone();

                Some(Box::new(move |event| {
                    if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
                        if peripheral.id() == id {
                            let uuid_substr = uuid_substr.clone();
                            result_sender.lock().unwrap().take().unwrap().blocking_send(
                                CommandResult::FindService(peripheral.clone(), find_by_id_substr(uuid_substr, services)),
                            );
                            return true;
                        }
                    }
                    false
                }))
            }

            Command::FindCharacteristic(ref peripheral, ref service, uuid_substr, _) => {
                peripheral.discover_characteristics(service);

                let peripheral_id = peripheral.id();
                let service_id = service.id();
                let uuid_substr = uuid_substr.clone();

                Some(Box::new(move |event| {
                    if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
                        if peripheral.id() == peripheral_id && service.id() == service_id {
                            let uuid_substr = uuid_substr.clone();
                            result_sender.lock().unwrap().take().unwrap().blocking_send(
                                CommandResult::FindCharacteristic(
                                    peripheral.clone(),
                                    find_by_id_substr(uuid_substr, characteristics)));
                            return true;
                        }
                    }
                    false
                }))
            }
            Command::ReadCharacteristic(ref peripheral, ref characteristic) => {
                peripheral.read_characteristic(characteristic);

                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Some(Box::new(move |event| {
                    if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
                        if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                            result_sender.lock().unwrap().take().unwrap().blocking_send(CommandResult::ReadCharacteristic(
                                peripheral.clone(),
                                characteristic.clone(),
                                if let Ok(value) = value { Some(value.clone()) } else { None }));
                            return true;
                        }
                    }
                    false
                }))
            }
            Command::WriteCharacteristic(ref peripheral, ref characteristic, ref data) => {
                peripheral.write_characteristic(characteristic, data, WriteKind::WithResponse);

                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Some(Box::new(move |event| {
                    if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
                        if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                            result_sender.lock().unwrap().take().unwrap().blocking_send(
                                CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone()));
                            return true;
                        }
                    }
                    false
                }))
            }
        }
    }
}

pub enum EventMatch {
    //Next(Command),
    //Result(CommandResult),
    None,
    Present,
}

impl EventMatch {
    pub fn is_none(&self) -> bool {
        matches!(*self, EventMatch::None)
    }
}

pub type EventMatcher = Box<dyn Fn(&CentralEvent) -> bool + Send>;

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

