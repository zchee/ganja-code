//! Hosted startup assembly and its interrupt handoff to the terminal loop.

use std::future::Future;
use std::io;

use anyhow::Result;
use futures::future::BoxFuture;

pub(crate) type Interrupt = BoxFuture<'static, io::Result<()>>;

pub(crate) async fn during_startup<T>(
    interrupt: &mut Option<Interrupt>,
    work: impl Future<Output = T>,
) -> Result<T> {
    let Some(interrupt) = interrupt.as_mut() else {
        return Ok(work.await);
    };
    tokio::select! {
        biased;
        _ = interrupt => anyhow::bail!("cloud connection interrupted"),
        value = work => Ok(value),
    }
}

#[cfg(test)]
#[path = "cloud_assembly_tests.rs"]
mod tests;
