use std::sync::{Arc, Mutex};

use async_std::{task, task::JoinHandle};
use core_bluetooth::central::{CentralEvent, CentralManager};
use futures::StreamExt;

// ---------------------------


pub struct CentralAsync {
    pub central: Arc<Mutex<CentralManager>>,
    pub receiver: async_channel::Receiver<Arc<Mutex<CentralEvent>>>,
    handle: JoinHandle<std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>>,
}

/*
impl Drop for CentralAsync {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}
*/

impl<'a> CentralAsync {
    pub fn new() -> Self {

        // it is possible to use Arc<Mutex<_>> instead of cloneable receiver
        // https://users.rust-lang.org/t/multiple-receiver-mpsc-channel-using-arc-mutex-receiver/37798

        let (sender, receiver) = async_channel::unbounded();
        let (central, mut central_receiver) = CentralManager::new();

        Self {
            central: Arc::new(Mutex::new(central)),
            receiver,
            handle: task::spawn(
                async move {
                    loop {
                        while let Some(event) = central_receiver.next().await {
                            let event = Arc::new(Mutex::new(event));
                            sender.send(event).await?;
                        }
                    }
                }),
        }
    }

    pub async fn wait<T>(&mut self, mut func: impl FnMut(&CentralEvent, &CentralManager) -> Option<T> + 'a) -> Option<T> {
        let central = &self.central.lock().unwrap();
        use async_std::stream::StreamExt;

        self.receiver.clone().find_map(move |event| {
            let event = event.lock().unwrap();
            func(&event, &central)
        }).await
    }

    // -----------------

    // The aim is to split composite operations into async composable parts.

    // This is composable operation: ensure Central is connected, connect to Device, get Peripheral,
    // get Characteristic object, write to it.
    //async fn write_to_characteristic(&mut self, device: Uuid, service: Uuid, characteristic: Uuid, value: u32) {}

    // This is "atomic" operation: Peripheral and Characteristic references are already available,
    // just write the value and return after confirmation event.
    //async fn just_write_to_characteristic(&mut self, peripheral: &Peripheral, characteristic: &Characteristic, value: &[u8]) {
    /* let result = self.execute(
         CentralEventType::WriteCharacteristicResult,
         async move {
             peripheral.write_characteristic(characteristic, value, WriteKind::WithResponse);
         }, );
     */
    // we have to read events for https://docs.rs/core_bluetooth/0.1.0/core_bluetooth/central/enum.CentralEvent.html#variant.WriteCharacteristicResult now

    //
    // event = self.waitForEvent(CentralEvent::WriteCharacteristicResult);
    //}
}