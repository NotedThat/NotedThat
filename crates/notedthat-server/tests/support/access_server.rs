use notedthat_server::config::Config;
use std::{thread, time::Duration};
use tokio::sync::oneshot;

use super::access_env::{DAV_PASS, DAV_USER};

const READY_TIMEOUT: Duration = Duration::from_secs(60);

pub struct ServerInstance {
    pub http_url: String,
    pub dav_url: String,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<(), String>>>,
}

impl ServerInstance {
    pub fn start(config: Config) -> Self {
        let http_url = format!("http://{}", config.listen_addr);
        let dav_url = format!("http://{}/webdav", config.listen_addr);
        let (stop, stopped) = oneshot::channel();
        let server_thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .enable_all()
                .build()
                .map_err(|error| format!("build server runtime: {error}"))?;
            let result = runtime.block_on(async move {
                let mut server = tokio::spawn(notedthat_server::run::run(config));
                tokio::select! {
                    result = &mut server => Err(format!("server stopped before fixture teardown: {result:?}")),
                    _ = stopped => {
                        server.abort();
                        let _ = server.await;
                        Ok(())
                    }
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(3));
            result
        });
        Self {
            http_url,
            dav_url,
            stop: Some(stop),
            thread: Some(server_thread),
        }
    }

    pub fn stop(mut self) {
        self.stop.take().expect("stop sender present").send(()).ok();
        let result = self
            .thread
            .take()
            .expect("server thread present")
            .join()
            .expect("server fixture thread panicked");
        result.expect("server fixture failed");
        println!("TEARDOWN server listeners stopped");
    }
}

impl Drop for ServerInstance {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
        if let Some(server_thread) = self.thread.take() {
            let _ = server_thread.join();
        }
    }
}

pub async fn wait_ready(client: &reqwest::Client, server: &ServerInstance) {
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "real server readiness timed out: {}",
            server.http_url
        );
        let http_ready = client
            .get(format!("{}/healthz", server.http_url))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        let dav_ready = client
            .request(reqwest::Method::OPTIONS, &server.dav_url)
            .basic_auth(DAV_USER, Some(DAV_PASS))
            .send()
            .await
            .is_ok_and(|response| response.status().as_u16() == 204);
        if http_ready && dav_ready {
            println!("READY http={} dav={}", server.http_url, server.dav_url);
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
