use std::time::Duration;

use core_bluetooth::central::{AdvertisementData, CentralEvent};
use core_bluetooth::central::characteristic::Characteristic;
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::service::Service;
use core_bluetooth::error::Error;
use core_bluetooth::uuid::Uuid;
use log::error;

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

pub trait EventMatcherProducer {
    fn get_matcher(&self) -> Box<EventMatcher>;
}

impl EventMatcherProducer for Command {
    fn get_matcher(&self) -> Box<EventMatcher> {
        match self {
            Command::FindPeripheralByService(uuid) => {
                let uuid = uuid.clone();
                Box::new(move |event| {
                    if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
                        if advertisement_data.service_uuids().iter()
                            .find(|&u| u.eq(&uuid)).is_some() {
                            return EventMatch::Result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))));
                        }
                    }
                    EventMatch::None
                })
            }

            Command::ConnectToPeripheral(peripheral, _) => {
                let peripheral_uuid = peripheral.id();
                Box::new(move |event| {
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
                })
            }

            Command::FindService(peripheral, uuid_substr) => {
                let id = peripheral.id();
                let uuid_substr = uuid_substr.clone();

                Box::new(move |event| {
                    if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
                        if peripheral.id() == id {
                            let uuid_substr = uuid_substr.clone();
                            return EventMatch::Result(CommandResult::FindService(
                                peripheral.clone(),
                                find_by_id_substr(uuid_substr, services)))
                        }
                    }
                    EventMatch::None
                })
            }

            Command::FindCharacteristic(peripheral, service, uuid_substr, _) => {
                let peripheral_id = peripheral.id();
                let service_id = service.id();
                let uuid_substr = uuid_substr.clone();

                Box::new(move |event| {
                    if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
                        if peripheral.id() == peripheral_id && service.id() == service_id {
                            let uuid_substr = uuid_substr.clone();
                            return EventMatch::Result(CommandResult::FindCharacteristic(
                                peripheral.clone(),
                                find_by_id_substr(uuid_substr, characteristics)))
                        }
                    }
                    EventMatch::None
                })
            }

            Command::ReadCharacteristic(peripheral, characteristic) => {
                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Box::new(move |event| {
                    if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
                        if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                            return EventMatch::Result(CommandResult::ReadCharacteristic(
                                peripheral.clone(),
                                characteristic.clone(),
                                if let Ok(value) = value { Some(value.clone()) } else { None }))
                        }
                    }
                    EventMatch::None
                })
            }

            Command::WriteCharacteristic(peripheral, characteristic, _) => {
                let peripheral_id = peripheral.id();
                let characteristic_id = characteristic.id();

                Box::new(move |event| {
                    if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
                        if peripheral.id() == peripheral_id && characteristic.id() == characteristic_id {
                            return EventMatch::Result(
                                CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone()));
                        }
                    }
                    EventMatch::None
                })
            }
            _ => panic!("unsupported command")
        }
    }
}

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

