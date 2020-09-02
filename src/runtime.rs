use lazy_static::lazy_static;
use smol::prelude::*;
use smol::Executor;
use std::sync::Arc;
use std::thread;

lazy_static! {
    static ref EXECUTOR: Arc<Executor> = {
        let ex = Arc::new(Executor::new());
        for i in 1..=1 {
            let builder = thread::Builder::new().name(format!("sosistab-{}", i));
            {
                let ex = ex.clone();
                builder
                    .spawn(move || {
                        smol::future::block_on(ex.run(smol::future::pending::<()>()));
                    })
                    .unwrap();
            }
        }
        ex
    };
}

/// Spawns a future onto the sosistab worker.
pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> smol::Task<T> {
    EXECUTOR.spawn(future)
}
