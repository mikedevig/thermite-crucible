use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::handler::Handler;
use crate::protocol;
use crate::stats::Sampler;
use crate::ticket;

pub async fn run_session(
    handler: Arc<Mutex<Handler>>,
    stage_rx: &mut UnboundedReceiver<Value>,
    control_base: &str,
    token: &str,
    on_connect: Option<Box<dyn FnOnce() + Send>>,
    quiet: bool,
) -> Result<()> {
    let ws_url = ticket::clientws_url(control_base, token)?;
    if !quiet {
        tracing::info!(%ws_url, "connecting control channel");
    }
    let (stream, _resp) = connect_async(ws_url.as_str())
        .await
        .context("websocket upgrade")?;
    if let Some(cb) = on_connect {
        cb();
    }
    let (mut write, mut read) = stream.split();

    let mut sampler = Sampler::new();
    let mut hb = tokio::time::interval(Duration::from_secs(2));
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Handling a control message (especially `first.setup.config`, which
    // downloads/unpacks core binaries) can take a while. Rather than
    // `.await`-ing `handler.on_message()` directly inline inside the
    // `read.next()` branch - which would leave that whole branch's block
    // running to completion before `select!` ever looks at the other
    // branches again - we hand it off to its own task and just track the
    // `JoinHandle` here. That way `stage_rx` (setup.stage progress events)
    // and the heartbeat keep flowing to the dashboard the entire time a
    // message is being processed, instead of the browser's progress bar
    // sitting stuck on "Connecting..." until setup finishes.
    let mut inflight: Option<tokio::task::JoinHandle<Result<Option<Value>>>> = None;

    loop {
        tokio::select! {
            // Interim `setup.stage` progress events queued by Handler while it
            // works through first.setup.config (downloading binaries,
            // generating certs, etc). Forwarded immediately so the dashboard
            // can stream real progress to the browser over SSE.
            Some(stage_msg) = stage_rx.recv() => {
                write
                    .send(Message::Text(stage_msg.to_string().into()))
                    .await
                    .context("send stage event")?;
            }
            joined = async { inflight.as_mut().unwrap().await }, if inflight.is_some() => {
                inflight = None;
                let reply = joined.context("on_message task panicked")??;
                if let Some(reply) = reply {
                    write
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .context("send ACK")?;
                }
            }
            _ = hb.tick() => {
                let (core, traffic) = {
                    let h = handler.lock().await;
                    let core = h.current_core();
                    let traffic = h.runtime.collect_traffic(&h.http).await;
                    (core, traffic)
                };
                let sample = sampler.sample();
                let mut body = protocol::alive_json(core, &sample);
                // Real per-user traffic, polled live from Xray's StatsService
                // or Hysteria2's Traffic Stats API (core/xray_api.rs,
                // core/hy2_stats.rs) and merged straight into the heartbeat so
                // the control server's quota/abuse enforcement has real data
                // to work with (see clientHub.js::parseUserTraffic).
                if let (Some(dst), Some(src)) = (body.as_object_mut(), traffic.as_object()) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
                write
                    .send(Message::Text(body.to_string().into()))
                    .await
                    .context("send ALIVE")?;
            }
            // Only read a new control message once the previous one has
            // finished processing - Handler only handles one at a time.
            next = read.next(), if inflight.is_none() => {
                match next {
                    None => anyhow::bail!("control channel closed"),
                    Some(Err(e)) => anyhow::bail!("control channel error: {e}"),
                    Some(Ok(Message::Text(t))) => {
                        let msg: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                        tracing::debug!(%msg, "control <-");
                        let h = handler.clone();
                        inflight = Some(tokio::spawn(async move {
                            h.lock().await.on_message(msg).await
                        }));
                    }
                    Some(Ok(Message::Ping(p))) => {
                        write.send(Message::Pong(p)).await.ok();
                    }
                    Some(Ok(Message::Close(_))) => anyhow::bail!("control channel sent close"),
                    Some(Ok(_)) => {}
                }
            }
        }
    }
}
