use core_bluetooth::central::CentralEvent;
use core_bluetooth::central::characteristic::Characteristic;
use core_bluetooth::central::peripheral::Peripheral;
use core_bluetooth::central::service::Service;
use core_bluetooth::error::Error;
use core_bluetooth::uuid::Uuid;
use log::error;

use crate::commands::*;

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

// it is critical EventMatchers are stateless

pub trait EventMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch;
}

pub struct PeripheralDiscoveredMatcherByServiceUUID {
    uuid: Uuid,
}

impl PeripheralDiscoveredMatcherByServiceUUID {
    pub fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

impl EventMatcher for PeripheralDiscoveredMatcherByServiceUUID {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::PeripheralDiscovered { peripheral, advertisement_data, rssi: _ } = event {
            if advertisement_data.service_uuids().iter()
                .find(|&uuid| uuid.eq(&self.uuid)).is_some() {
                return EventMatch::Result(CommandResult::FindPeripheral(Some((peripheral.clone(), advertisement_data.clone()))))
            }
        }
        EventMatch::None
    }
}

pub struct PeripheralConnectedMatcher {
    peripheral_id: Uuid,
}

impl PeripheralConnectedMatcher {
    pub fn new(peripheral: &Peripheral) -> Self {
        Self {
            peripheral_id: peripheral.id()
        }
    }
}

impl EventMatcher for PeripheralConnectedMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::PeripheralConnected { peripheral } = event {
            if peripheral.id() == self.peripheral_id {
                return EventMatch::Next(Command::RegisterConnectedPeripheral(peripheral.clone()))
            }
        } else if let CentralEvent::PeripheralConnectFailed { peripheral, error } = event {
            if peripheral.id() == self.peripheral_id {
                let id = peripheral.id().clone();
                error!("failed to connect to peripheral {:?}: {:?}", peripheral, error);
                return EventMatch::Next(Command::UnregisterConnectedPeripheral(id))
            }
        }
        EventMatch::None
    }
}


pub struct ServicesDiscoveredMatcher {
    peripheral_id: Uuid,
    uuid_substr: String,
}

impl ServicesDiscoveredMatcher {
    pub fn new(peripheral: &Peripheral, uuid_substr: String) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            uuid_substr,
        }
    }
}

impl EventMatcher for ServicesDiscoveredMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::ServicesDiscovered { peripheral, services } = event {
            if peripheral.id() == self.peripheral_id {
                return EventMatch::Result(CommandResult::FindService(
                    peripheral.clone(),
                    find_by_id_substr(&self.uuid_substr, services)))
            }
        }
        EventMatch::None
    }
}


pub struct CharacteristicsDiscoveredMatcher {
    peripheral_id: Uuid,
    service_id: Uuid,
    uuid_substr: String,
}

impl CharacteristicsDiscoveredMatcher {
    pub fn new(peripheral: &Peripheral, service: &Service, uuid_substr: String) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            service_id: service.id(),
            uuid_substr,
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

fn find_by_id_substr<T>(id_substr: &String, items: &Result<Vec<T>, Error>) -> Option<T> where T: HasId + Clone {
    let id_substr = id_substr.as_str();
    return items.as_ref().ok()
        .and_then(|items| items.iter().find(|item| item.get_id().to_string().contains(id_substr)))
        .map(|item| item.clone());
}

impl EventMatcher for CharacteristicsDiscoveredMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::CharacteristicsDiscovered { peripheral, service, characteristics } = event {
            if peripheral.id() == self.peripheral_id && service.id() == self.service_id {
                return EventMatch::Result(CommandResult::FindCharacteristic(
                    peripheral.clone(),
                    find_by_id_substr(&self.uuid_substr, characteristics)))
            }
        }
        EventMatch::None
    }
}

pub struct CharacteristicValueMatcher {
    peripheral_id: Uuid,
    characteristic_id: Uuid,
}

impl CharacteristicValueMatcher {
    pub fn new(peripheral: &Peripheral, characteristic: &Characteristic) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
        }
    }
}

impl EventMatcher for CharacteristicValueMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::CharacteristicValue { peripheral, characteristic, value } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return EventMatch::Result(CommandResult::ReadCharacteristic(
                    peripheral.clone(),
                    characteristic.clone(),
                    if let Ok(value) = value { Some(value.clone()) } else { None }))
            }
        }
        EventMatch::None
    }
}

pub struct WriteCharacteristicResultMatcher {
    peripheral_id: Uuid,
    characteristic_id: Uuid,
}

impl WriteCharacteristicResultMatcher {
    pub fn new(peripheral: &Peripheral, characteristic: &Characteristic) -> Self {
        Self {
            peripheral_id: peripheral.id(),
            characteristic_id: characteristic.id(),
        }
    }
}

impl EventMatcher for WriteCharacteristicResultMatcher {
    fn matches(&self, event: &CentralEvent) -> EventMatch {
        if let CentralEvent::WriteCharacteristicResult { peripheral, characteristic, result } = event {
            if peripheral.id() == self.peripheral_id && characteristic.id() == self.characteristic_id {
                return EventMatch::Result(
                    CommandResult::WriteCharacteristic(peripheral.clone(), characteristic.clone(), result.clone()));
            }
        }
        EventMatch::None
    }
}

