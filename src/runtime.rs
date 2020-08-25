use async_executor::Executor;
use lazy_static::lazy_static;
use smol::prelude::*;
use std::sync::Arc;
use std::thread;

lazy_static! {
    static ref EXECUTOR: Arc<Executor> = {
        let ex = Arc::new(Executor::new());
        let builder = thread::Builder::new().name("sosistab-worker".into());
        {
            let ex = ex.clone();
            builder
                .spawn(move || {
                    ex.run(smol::future::pending::<()>());
                })
                .unwrap();
        }
        ex
    };
}

/// Spawns a future onto the sosistab worker.
pub fn spawn<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> smol::Task<T> {
    EXECUTOR.spawn(future)
}
