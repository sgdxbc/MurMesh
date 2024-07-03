use std::sync::Arc;

use bytes::Bytes;
use neatworks::{
    event::{
        task::{run_with_schedule, ScheduleState},
        Erase, Erased, SendEvent,
    },
    net::{combinators::Forward, task::udp},
    unreplicated,
    workload::events::{Invoke, InvokeOk},
};
use rand::random;
use tokio::{net::UdpSocket, select, sync::mpsc::unbounded_channel, time::Instant};

mod utils;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket = Arc::new(UdpSocket::bind("localhost:0").await?);
    let addr = socket.local_addr()?;
    let (upcall_sender, mut upcall_receiver) = unbounded_channel::<InvokeOk<Bytes>>();
    let (schedule_sender, mut schedule_receiver) = unbounded_channel();
    let mut context = unreplicated::context::Client {
        net: unreplicated::codec::client_encode(Forward(
            ([127, 0, 0, 1], 3000).into(),
            socket.clone(),
        )),
        upcall: upcall_sender,
        schedule: Erase::new(ScheduleState::new(schedule_sender)),
    };
    let (sender, mut receiver) = unbounded_channel();
    let net_task = udp::run(
        &socket,
        unreplicated::codec::client_decode(Erase::new(sender.clone())),
    );
    let client_task = run_with_schedule(
        Erased::new(unreplicated::ClientState::new(random(), addr)),
        &mut context,
        &mut receiver,
        &mut schedule_receiver,
        |context| &mut *context.schedule,
    );
    let invoke_task = async {
        let mut sender = Erase::new(sender);
        for _ in 0..10 {
            let start = Instant::now();
            sender.send(Invoke(Default::default()))?;
            let recv = upcall_receiver.recv().await;
            anyhow::ensure!(recv.is_some());
            println!("{:?}", start.elapsed())
        }
        anyhow::Ok(())
    };
    utils::run_until(invoke_task, async {
        select! {
            result = net_task => result,
            result = client_task => result,
        }
    })
    .await
}
