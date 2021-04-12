use std::sync::{Arc, Mutex};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_std::{task, task::{Waker, JoinHandle}};

use futures::{StreamExt, future::Future};

use core_bluetooth::central::{CentralManager, CentralEvent};


pub struct CentralFuture<T> {
    state: Arc<Mutex<CentralFutureState<T>>>
}

struct CentralFutureState<T> {
    event: Option<T>,
    waker: Option<Waker>,
}

impl<T> Future for CentralFuture<T> {
    type Output = T;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().unwrap();
        if state.event.is_some() {
            let ev = state.event.take().unwrap();
            Poll::Ready(ev)
        } else {
            // N.B. it's possible to check for this using the `Waker::will_wake`
            // function, but we omit that here to keep things simple.
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// ---------------------------


pub struct CentralAsync {
    central: Arc<Mutex<CentralManager>>,
    receiver: async_channel::Receiver<Arc<Mutex<CentralEvent>>>,
    handle: JoinHandle<std::result::Result<(), Box<dyn std::error::Error + Send + Sync>>>,
}
/*
impl Drop for CentralAsync {
    fn drop(&mut self) {
        self.handle.cancel();
    }
}
*/

impl CentralAsync {
    pub fn new() -> Self {

        // it is possible to use Arc<Mutex<_>> instead of cloneable receiver
        // https://users.rust-lang.org/t/multiple-receiver-mpsc-channel-using-arc-mutex-receiver/37798
        let (sender, receiver) = async_channel::unbounded();

        let (central, mut central_receiver) = CentralManager::new();

        let handle = task::spawn(
            async move {
                loop {
                    while let Some(event) = central_receiver.next().await {
                        let event = Arc::new(Mutex::new(event));
                        sender.send(event).await?;
                    }
                }
            });

        Self {
            central: Arc::new(Mutex::new(central)),
            receiver,
            handle,
        }
    }

    //fn execute_and_wait(&'static mut self, predicate: Box<dyn Fn(&CentralEvent) -> bool + Send + 'static>, execute: impl FnOnce(&CentralManager)) -> CentralFuture {
    pub fn wait<T: Send + 'static>(&mut self, predicate: impl Fn(&CentralEvent, Arc<Mutex<CentralManager>>) -> Option<T> + Send + 'static) -> CentralFuture<T> {
        let state = Arc::new(Mutex::new(CentralFutureState {
            event: None,
            waker: None,
        }));
//
        let mut receiver = self.receiver.clone();
        let cloned = state.clone();
        let central = self.central.clone();
        task::spawn(async move {
            while let Some(event_origin) = receiver.next().await {
                let event = event_origin.lock().unwrap();
                let res = predicate(&event, central.clone());
                match res {
                    Some(val) => {
                        let mut state = cloned.lock().unwrap();
                        state.event = Some(val);
                        if let Some(waker) = state.waker.take() {
                            waker.wake()
                        }
                    },
                    None => {}
                }
            }
        });

        //TODO(df): we may want to start central here
        CentralFuture { state }
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