//! M6 completeness gate — live MQTT outage / recovery at the PHASE level.
//!
//! Matrix row 6 of the connection-chain review: a broker that goes away
//! while the Gateway is running must demote the aggregated phase out of
//! READY, and its return must restore READY — with no client
//! reconstruction, no latch, and no consumer-side timer doing the work.
//!
//! The unit tests in `mqtt/readiness.rs` drive the projection with a
//! synthetic state stream; `bootstrap_integration.rs` covers node LWT
//! demotion. This file exercises the WHOLE chain with real transport:
//!
//!   real rumqttd broker <- TCP proxy <- real GatewayMqttClient session
//!        -> readiness projection -> registry -> orchestrator phase
//!
//! The proxy exists because the embedded broker's listener is
//! process-lifetime (rumqttd's `Broker::start()` joins its server
//! threads and never returns), so a test cannot stop the broker itself.
//! Cutting the proxy severs the live socket (immediate EOF -> Retrying)
//! and pauses accepting, so every retry dial lands in the kernel
//! backlog and hits the session's own watchdog — a durable, realistic
//! outage. Restoring the proxy is the broker "coming back".

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use acowork_gateway::bootstrap::orchestrator::BootstrapPhase;
use acowork_gateway::bootstrap::{
    BootstrapOrchestrator, ReadinessKind, SubsystemReadinessRegistry, SubsystemState,
};
use acowork_gateway::mqtt::broker::start_broker;
use acowork_gateway::mqtt::client::GatewayMqttClient;
use acowork_gateway::mqtt::readiness::spawn_projection;

/// Grab two distinct free ports. The broker thread never exits, so each
/// test must own its ports for the rest of the process.
fn free_ports() -> (u16, u16) {
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let ports = (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    );
    drop(a);
    drop(b);
    ports
}

/// A TCP proxy whose live connections can be severed and whose accept
/// loop can be paused, without ever releasing the listening socket.
struct Proxy {
    paused: watch::Sender<bool>,
    splices: Arc<Mutex<Vec<JoinHandle<()>>>>,
    accept_task: JoinHandle<()>,
}

impl Proxy {
    async fn start(listen_port: u16, upstream_port: u16) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", listen_port))
            .await
            .expect("proxy must bind its port");
        let (paused_tx, mut paused_rx) = watch::channel(false);
        let splices: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::new(Mutex::new(Vec::new()));
        let splices_for_task = splices.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                // While paused, do not accept: dials complete via the
                // kernel backlog but never reach the broker, so the
                // session's own watchdog bounds each attempt.
                if *paused_rx.borrow_and_update() {
                    if paused_rx.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                tokio::select! {
                    changed = paused_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    accepted = listener.accept() => {
                        let (mut downstream, _) = match accepted {
                            Ok(pair) => pair,
                            Err(_) => return,
                        };
                        let mut upstream = match TcpStream::connect(("127.0.0.1", upstream_port)).await {
                            Ok(s) => s,
                            Err(_) => continue,
                        };
                        let splice = tokio::spawn(async move {
                            let _ = copy_bidirectional(&mut downstream, &mut upstream).await;
                        });
                        splices_for_task.lock().unwrap().push(splice);
                    }
                }
            }
        });
        Self {
            paused: paused_tx,
            splices,
            accept_task,
        }
    }

    /// Sever every live connection and stop accepting. The client sees
    /// EOF immediately; subsequent dials black-hole until `restore`.
    fn cut(&self) {
        let _ = self.paused.send(true);
        for handle in self.splices.lock().unwrap().drain(..) {
            handle.abort();
        }
    }

    fn restore(&self) {
        let _ = self.paused.send(false);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept_task.abort();
        for handle in self.splices.lock().unwrap().drain(..) {
            handle.abort();
        }
    }
}

async fn wait_phase(
    orch: &BootstrapOrchestrator,
    want: BootstrapPhase,
    budget: Duration,
) {
    let deadline = tokio::time::Instant::now() + budget;
    while orch.snapshot().phase != want {
        assert!(
            tokio::time::Instant::now() < deadline,
            "phase never reached {want:?} (now: {:?}, detail: {:?})",
            orch.snapshot().phase,
            orch.snapshot().phase_detail
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn mqtt_detail(registry: &SubsystemReadinessRegistry) -> Option<String> {
    registry
        .snapshot()
        .into_iter()
        .find(|(id, _, _, _)| id == &"mqtt".into())
        .and_then(|(_, _, _, detail)| detail)
}

/// Matrix #6 (and the live-transport counterpart of #1/#3): READY ->
/// outage -> BOOTING with visible retry progress -> recovery -> READY,
/// one session throughout.
///
/// Falsification: with the old readiness latch (a record that reached
/// Ready ignored later transitions) the phase never leaves READY and
/// this test fails at the outage assertion.
#[tokio::test]
async fn live_broker_outage_downgrades_phase_and_recovery_restores_it() {
    let (broker_port, proxy_port) = free_ports();
    let mut broker = start_broker("127.0.0.1", broker_port).expect("broker should start");
    assert!(
        broker.wait_listening(Duration::from_secs(5)).await,
        "broker must be listening before the test starts"
    );
    let proxy = Proxy::start(proxy_port, broker_port).await;

    let registry = SubsystemReadinessRegistry::new_shared();
    let orch = BootstrapOrchestrator::new("instance-live-outage".into(), registry.clone());
    let handle = registry.register("mqtt", ReadinessKind::Required);

    let client = GatewayMqttClient::spawn(
        "127.0.0.1",
        proxy_port,
        "gateway:test-live-outage",
        None,
        None,
    )
    .await
    .expect("session construction must not depend on the broker");
    let projection = spawn_projection(client.state_rx(), handle);

    // Baseline: the full chain reaches READY on its own.
    wait_phase(&orch, BootstrapPhase::Ready, Duration::from_secs(15)).await;

    // The broker "goes away": sever the live socket, black-hole retries.
    proxy.cut();
    wait_phase(&orch, BootstrapPhase::Booting, Duration::from_secs(15)).await;
    assert_eq!(
        registry.state(&"mqtt".into()),
        Some(SubsystemState::Booting),
        "the subsystem record must demote, not latch on Ready"
    );

    // The outage must be *visible* while it lasts: retry progress
    // reaches the subsystem detail (the operator-facing diagnostic).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(d) = mqtt_detail(&registry)
            && d.contains("attempt")
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "retry progress never reached the record (detail: {:?})",
            mqtt_detail(&registry)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The broker "comes back". Nobody re-creates the client.
    proxy.restore();
    wait_phase(&orch, BootstrapPhase::Ready, Duration::from_secs(30)).await;

    projection.abort();
    drop(client);
    drop(proxy);
    let _ = broker.signal_shutdown();
}
