use futures_util::StreamExt;
mod config;
mod metrics;

use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::str::FromStr;
use std::sync::Arc;

use crate::config::Config;
use crate::metrics::Metrics;
use fern::colors::ColoredLevelConfig;
use log::{error, info, warn};
use regex_automata::meta::Regex;
use regex_automata::Input;
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;

struct ContainerWatcher {
    cancel: CancellationToken,
    name: String,
    task: JoinHandle<()>,
}

#[tokio::main]
async fn main() {
    let colors = ColoredLevelConfig::new();
    fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{}[{}][{}] {}",
                chrono::Utc::now().format("[%Y-%m-%d_%H:%M:%S]"),
                colors.color(record.level()),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        .apply()
        .ok();

    let config = match config::Config::load_from_file("config.json") {
        Ok(config) => Arc::new(config),
        Err(e) => {
            error!("Failed to load config: {}", e);
            exit(1);
        }
    };

    let cancel = CancellationToken::new();

    let metrics = Metrics::new(&config, cancel.clone());
    let client = Client::builder()
        .unix_socket(config.docker_socket.as_str())
        .build()
        .unwrap();
    let dispatcher = tokio::spawn(dispatcher(
        config.clone(),
        client.clone(),
        metrics,
        cancel.clone(),
    ));

    match signal::ctrl_c().await {
        Ok(()) => {
            info!("Ctrl+C received, shutting down..");
            cancel.cancel();
        }
        Err(err) => {
            error!("Unable to listen for shutdown signal: {}", err);
            exit(1);
        }
    }

    if let Err(e) = dispatcher.await {
        error!("Failed to join dispatcher: {}", e);
    }

    info!("Goodbye.")
}

async fn dispatcher(
    config: Arc<Config>,
    client: Client,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) {
    let na_name: String = "<none>".to_string();
    let mut containers = HashMap::<String, ContainerWatcher>::new();
    loop {
        let mut sleep_duration = std::time::Duration::from_secs(60);
        select! {
            _ = cancel.cancelled() => {
                break;
            }
            c = list_containers(client.clone()) => {
                match c {
                    Ok(list_containers) => {
                        let mut alive = HashSet::<String>::new();
                        for container in list_containers {
                            let image = container.image.rsplit_once(":").unwrap_or((&container.image, "")).0;

                            let mut found = false;
                            for docker_image in &config.docker_images {
                                if docker_image == image {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                continue;
                            }

                            if !containers.contains_key(&container.id) {
                                let watcher_cancel = CancellationToken::new();
                                info!("Starting watcher for container {} (id {})", container.names.first().or(Some(&na_name)).unwrap(), container.id);
                                containers.insert(container.id.clone(), ContainerWatcher {
                                    task: tokio::spawn(watcher(container.id.clone(), client.clone(), metrics.clone(), watcher_cancel.clone())),
                                    cancel: watcher_cancel,
                                    name: container.names.first().or(Some(&container.id)).unwrap().to_string(),
                                });
                            }
                            alive.insert(container.id);
                        }
                        let mut to_remove = vec![];
                        // remove containers that are no longer alive
                        for id in containers.keys() {
                            if !alive.contains(id) {
                                to_remove.push(id.clone())
                            }
                        }

                        for id in to_remove {
                            let container = containers.remove(&id).unwrap();
                            info!("Stopping watcher for container {}/{}", container.name, id);
                            container.cancel.cancel();
                            if let Err(e) = container.task.await {
                                warn!("Failed to join the watcher task for container {}/{}: {}", container.name, id, e);
                            }
                        }
                    },
                    Err(e) => {
                        warn!("Failed to list containers: {e}");
                        sleep_duration = std::time::Duration::from_secs(10);
                    },
                }
            }
        }

        select! {
            _ = cancel.cancelled() => {
                break;
            }
            _ = sleep(sleep_duration) => {}
        }
    }

    for container in containers.values() {
        container.cancel.cancel();
    }

    for (id, container) in containers {
        if let Err(e) = container.task.await {
            error!(
                "Failed to join the watcher task for container {}/{}: {}",
                id, container.name, e
            );
        }
    }
}

async fn watcher(
    container_id: String,
    client: Client,
    metrics: Arc<Metrics>,
    cancel: CancellationToken,
) {
    let uri = format!(
        "http://docker/v1.30/containers/{}/logs?follow=true&stdout=true&since={}",
        container_id,
        chrono::Utc::now().timestamp()
    );
    let re = Regex::new("[0-9]+=([0-9]+) https?://([^/]+)/").expect("invalid regex (how?)");
    loop {
        select! {
            _ = cancel.cancelled() => {
                break;
            }
            r = async {
                    let res = client
                        .get(&uri)
                        .send()
                        .await
                        .map_err(|e| format!("request failed: {e:#?}"))?;
                    if res.status() != StatusCode::OK {
                        return Err(format!("got non-200 status code: {}", res.status()));
                    }
                    // Ensure success
                    let stream = res.bytes_stream();

                    // Convert to async reader
                    let stream_reader = tokio_util::io::StreamReader::new(
                        stream.map(|res| res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
                    );

                    let mut lines = BufReader::new(stream_reader);
                    let mut buf = Vec::new();
                    let mut captures = re.create_captures();
                    loop {
                        buf.clear();
                        captures.clear();
                        match lines.read_until(b'\n', &mut buf).await {
                            Ok(0) => {
                                // eof
                                break;
                            },
                            Ok(n) => {
                                let line = &buf[..n];
                                re.captures(Input::new(line), &mut captures);
                                if captures.is_match() {
                                    let Some(status) = captures.get_group(1) else {
                                        continue;
                                    };
                                    let status = str::from_utf8(&line[status.range()]).expect("invalid utf-8 for status capture, this should never happen.");
                                    let Ok(status) = u16::from_str(status) else {
                                        error!("Failed to parse status code as u16 from '{}'", status);
                                        continue;
                                    };
                                    let Some(domain) = captures.get_group(2) else {
                                        continue;
                                    };
                                    let Ok(domain) = str::from_utf8(&line[domain.range()]) else {
                                        // domains should be valid utf-8, just ignore if they're not
                                        continue;
                                    };
                                    metrics.request(domain.to_string(), status).await;
                                }
                            },
                            Err(e) => {
                                return Err(format!("line read loop failed: {e}"));
                            }
                        }
                    }
                    Ok(())
            } => {
                if let Err(e) = r {
                    warn!("Watcher for {container_id} had an issue: {}", e);
                }
            }
        }

        select! {
            _ = cancel.cancelled() => {
                break;
            }
            _ = sleep(std::time::Duration::from_secs(1)) => {}
        }
    }
}

#[derive(Deserialize)]
struct DockerContainer {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Names")]
    names: Vec<String>,
    #[serde(rename = "Image")]
    image: String,
}

async fn list_containers(client: Client) -> Result<Vec<DockerContainer>, String> {
    let res = client
        .get("http://docker/v1.30/containers/json")
        .send()
        .await
        .map_err(|e| format!("request failed: {e:?}"))?;

    let status = res.status();

    if status != StatusCode::OK {
        return Err(format!(
            "request failed with status code: {} and body {}",
            res.status(),
            res.text()
                .await
                .map_err(|e| format!("could not get response body (status {}): {e}", status))?
        ));
    }

    res.json()
        .await
        .map_err(|e| format!("could not parse response body: {e}"))
}
