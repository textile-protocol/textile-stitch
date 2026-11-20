// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The hosted panel's [`DockerApi`]: the runtime broker over its Unix socket.
//!
//! The panel's handlers stay as they are and call this like any runtime. What
//! the broker allows (list, start/stop/restart, logs, image details, digest
//! swaps) goes through; everything else (create, remove, pull, one-shot runs,
//! panel self-update) fails here before reaching the socket, and the broker
//! would refuse it anyway.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use futures_util::{stream, StreamExt};

use crate::panel::broker::{ImageRequest, SwapErrorBody, WireContainer, WireImage, WireLogLine};
use crate::panel::broker_config::{WireBotFiles, WireQuotingSave, WireSaved};
use crate::panel::docker::swap::failed;
use crate::panel::docker::{
    ContainerInfo, CreateSpec, DockerApi, Keepalive, LogLine, LogOptions, LogSource, LogStream,
    RunStream,
};

const REFUSED: &str = "not available on a hosted panel: the runtime broker only starts, stops, \
                       restarts, reads logs and swaps a bot's image by digest";

pub struct BrokerDocker {
    client: reqwest::Client,
    /// Separate client for followed log streams, which have no overall timeout.
    streaming: reqwest::Client,
}

impl BrokerDocker {
    pub fn connect(socket: &Path) -> Result<Self> {
        let build = |timeout: Option<Duration>| {
            let builder = reqwest::Client::builder()
                .unix_socket(socket)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none());
            match timeout {
                Some(t) => builder.timeout(t),
                None => builder,
            }
            .build()
            .context("building the runtime broker client")
        };
        Ok(Self {
            // An image swap pulls first, so it gets the same budget as a pull.
            client: build(Some(Duration::from_secs(300)))?,
            streaming: build(None)?,
        })
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!("http://broker{path}"))
            .send()
            .await
            .context("the runtime broker isn't answering")?;
        let response = ok_or_error(response).await?;
        Ok(response.json().await?)
    }

    async fn post(&self, path: &str, body: Option<&ImageRequest>) -> Result<()> {
        let mut request = self.client.post(format!("http://broker{path}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .context("the runtime broker isn't answering")?;
        ok_or_error(response).await.map(|_| ())
    }

    /// The enrolled container running `image`, for the broker's image route.
    async fn container_running(&self, image: &str) -> Result<Option<ContainerInfo>> {
        Ok(self
            .list_all()
            .await?
            .into_iter()
            .find(|c| c.image == image || c.image_id == image))
    }

    pub async fn ping(&self) -> Result<()> {
        self.list_all().await.map(|_| ())
    }
}

/// Turn a broker refusal into an error carrying its message.
async fn ok_or_error(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    let message = body
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or("no detail");
    bail!("the runtime broker refused ({status}): {message}")
}

/// Turn the broker's image-swap reply into the panel's result, keeping a known
/// `SwapOutcome` in the error.
async fn swap_reply(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let text = response.text().await.unwrap_or_default();
    match serde_json::from_str::<SwapErrorBody>(&text) {
        Ok(SwapErrorBody {
            error,
            outcome: Some(outcome),
        }) => Err(failed(outcome, anyhow!("{error}"))),
        Ok(SwapErrorBody { error, .. }) => {
            bail!("the runtime broker refused ({status}): {error}")
        }
        Err(_) => {
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or_else(|| "no detail".into());
            bail!("the runtime broker refused ({status}): {message}")
        }
    }
}

#[async_trait]
impl crate::panel::hosted::mirror::ConfigStore for BrokerDocker {
    async fn configs(&self) -> Result<Vec<WireBotFiles>> {
        self.get_json("/v1/configs").await
    }

    async fn save(&self, container: &str, save: &WireQuotingSave) -> Result<WireSaved> {
        let response = self
            .client
            .post(format!(
                "http://broker/v1/bots/{}/settings",
                segment(container)
            ))
            .json(save)
            .send()
            .await
            .context("the runtime broker isn't answering")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json().await?);
        }
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        Err(crate::panel::hosted::mirror::StoreRefused {
            status: status.as_u16(),
            message: body
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("no detail")
                .to_string(),
        }
        .into())
    }
}

fn segment(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes()).collect()
}

#[async_trait]
impl DockerApi for BrokerDocker {
    async fn list_all(&self) -> Result<Vec<ContainerInfo>> {
        let wire: Vec<WireContainer> = self.get_json("/v1/bots").await?;
        Ok(wire.into_iter().map(ContainerInfo::from).collect())
    }

    async fn ensure_image(&self, image: &str, _refresh: bool) -> Result<()> {
        bail!("pulling {image} is {REFUSED}")
    }

    async fn require_fresh_image(&self, image: &str) -> Result<()> {
        bail!("pulling {image} is {REFUSED}")
    }

    async fn local_image_digests(&self, image: &str) -> Result<Vec<String>> {
        Ok(self.image(image).await?.repo_digests)
    }

    async fn local_image_labels(&self, image: &str) -> Result<HashMap<String, String>> {
        Ok(self.image(image).await?.labels)
    }

    async fn schedule_image_swap(
        &self,
        name: &str,
        _new_image: &str,
        _socket: &Path,
    ) -> Result<()> {
        bail!("swapping {name} is {REFUSED}")
    }

    async fn replace_image(&self, name: &str, new_image: &str) -> Result<()> {
        bail!("moving {name} to {new_image} needs its build attestation; the broker refuses a swap without one")
    }

    async fn replace_attested_image(
        &self,
        name: &str,
        new_image: &str,
        attestations: &[String],
    ) -> Result<()> {
        // No `post`: a failed swap's reply says what it left behind, and that
        // has to reach the panel as a `SwapFailed`. A lost reply carries no
        // outcome, so the panel treats the bot as possibly live.
        let response = self
            .client
            .post(format!("http://broker/v1/bots/{}/image", segment(name)))
            .json(&ImageRequest {
                image: new_image.to_string(),
                attestations: attestations.to_vec(),
            })
            .send()
            .await
            .context("the runtime broker isn't answering")?;
        swap_reply(response).await
    }

    async fn create(&self, spec: &CreateSpec) -> Result<String> {
        bail!("creating {} is {REFUSED}", spec.name)
    }

    async fn start(&self, name: &str) -> Result<()> {
        self.post(&format!("/v1/bots/{}/start", segment(name)), None)
            .await
    }

    async fn stop(&self, name: &str, _grace_secs: i64) -> Result<()> {
        // The broker uses the panel's fixed grace period; callers can't change it.
        self.post(&format!("/v1/bots/{}/stop", segment(name)), None)
            .await
    }

    async fn restart(&self, name: &str, _grace_secs: i64) -> Result<()> {
        self.post(&format!("/v1/bots/{}/restart", segment(name)), None)
            .await
    }

    async fn remove(&self, name: &str, _force: bool) -> Result<()> {
        bail!("removing {name} is {REFUSED}")
    }

    fn logs(&self, name: &str, opts: LogOptions) -> LogStream {
        let url = format!(
            "http://broker/v1/bots/{}/logs?tail={}&follow={}",
            segment(name),
            opts.tail,
            opts.follow
        );
        let client = self.streaming.clone();
        let lines = async move {
            let response = client
                .get(url)
                .send()
                .await
                .context("the runtime broker isn't answering")?;
            ok_or_error(response).await
        };
        Box::pin(
            stream::once(lines)
                .map(|response| match response {
                    Ok(response) => ndjson_lines(response).boxed(),
                    Err(e) => stream::once(async move { Err(e) }).boxed(),
                })
                .flatten(),
        )
    }

    fn run_one_shot(
        &self,
        spec: CreateSpec,
        _keepalive: Option<Keepalive>,
        _hold_until_started: Option<Keepalive>,
    ) -> RunStream {
        let error = anyhow!("running {} is {REFUSED}", spec.name);
        Box::pin(stream::once(async move { Err(error) }))
    }
}

impl BrokerDocker {
    async fn image(&self, image: &str) -> Result<WireImage> {
        let Some(container) = self.container_running(image).await? else {
            // Not an image any enrolled bot runs: nothing to report, same as a
            // Docker host that doesn't have it.
            return Ok(WireImage::default());
        };
        let query: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("image", image)
            .finish();
        self.get_json(&format!(
            "/v1/bots/{}/image?{query}",
            segment(&container.name)
        ))
        .await
    }
}

/// Newline-delimited [`WireLogLine`]s from a streaming response, one item per line.
fn ndjson_lines(
    response: reqwest::Response,
) -> impl futures_util::Stream<Item = Result<LogLine>> + Send {
    stream::unfold(
        (response, Vec::<u8>::new(), false),
        |(mut response, mut buffer, done)| async move {
            loop {
                if let Some(at) = buffer.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buffer.drain(..=at).collect();
                    return Some((parse_line(&line), (response, buffer, done)));
                }
                if done {
                    return None;
                }
                match response.chunk().await {
                    Ok(Some(chunk)) => buffer.extend_from_slice(&chunk),
                    Ok(None) if buffer.is_empty() => return None,
                    Ok(None) => {
                        buffer.push(b'\n');
                        return Some((
                            parse_line(&std::mem::take(&mut buffer)),
                            (response, buffer, true),
                        ));
                    }
                    Err(e) => {
                        return Some((
                            Err(anyhow!(e).context("reading broker logs")),
                            (response, Vec::new(), true),
                        ))
                    }
                }
            }
        },
    )
}

fn parse_line(bytes: &[u8]) -> Result<LogLine> {
    let wire: WireLogLine =
        serde_json::from_slice(bytes).context("a broker log line didn't parse")?;
    Ok(LogLine {
        source: if wire.stderr {
            LogSource::Stderr
        } else {
            LogSource::Stdout
        },
        text: wire.text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_line_round_trips() {
        let line = parse_line(br#"{"stderr":true,"text":"boom"}"#).unwrap();
        assert_eq!(line.source, LogSource::Stderr);
        assert_eq!(line.text, "boom");
        assert!(parse_line(b"not json").is_err());
    }

    #[test]
    fn names_are_encoded_as_one_path_segment() {
        assert_eq!(segment("stitch-a"), "stitch-a");
        assert_eq!(segment("../x"), "..%2Fx");
    }

    #[tokio::test]
    async fn refused_operations_never_reach_the_socket() {
        let docker = BrokerDocker::connect(Path::new("/nonexistent/broker.sock")).unwrap();
        for result in [
            docker.ensure_image("x", true).await,
            docker.require_fresh_image("x").await,
            docker.remove("x", true).await,
            docker.schedule_image_swap("x", "y", Path::new("/s")).await,
        ] {
            assert!(
                format!("{:#}", result.unwrap_err()).contains("not available on a hosted panel")
            );
        }
        let mut one_shot = docker.run_one_shot(
            CreateSpec {
                name: "x".into(),
                image: "y".into(),
                labels: HashMap::new(),
                env: vec![],
                binds: vec![],
                cmd: None,
                restart_unless_stopped: false,
            },
            None,
            None,
        );
        assert!(one_shot.next().await.unwrap().is_err());
        // A swap always carries its attestation; the bare form never reaches the
        // socket.
        let bare = docker.replace_image("x", "y").await.unwrap_err();
        assert!(format!("{bare:#}").contains("needs its build attestation"));
    }

    /// The broker's attestation refusal reaches the panel as a definite
    /// "nothing changed", so the panel releases the wallet without stopping the bot.
    #[tokio::test]
    async fn an_attestation_refusal_crosses_the_socket_as_unchanged() {
        use crate::panel::broker::{Broker, Policy, DEFAULT_IMAGE_REPOSITORY};
        use crate::panel::docker::fake::{container, FakeDocker};
        use crate::panel::docker::swap::{swap_outcome, SwapOutcome};
        use crate::panel::docker::ContainerState;
        use axum::body::{to_bytes, Body};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let docker = std::sync::Arc::new(FakeDocker::new());
        docker.add_container(container("stitch-luno-a", ContainerState::Running));
        let policy = Policy::new("stitch-luno-a", DEFAULT_IMAGE_REPOSITORY).unwrap();
        let app = Broker::new(docker, policy).router();
        let image = format!("{DEFAULT_IMAGE_REPOSITORY}@sha256:{}", "b".repeat(64));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/bots/stitch-luno-a/image")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({ "image": image }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        assert_eq!(status, StatusCode::FORBIDDEN);
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let reply = reqwest::Response::from(
            axum::http::Response::builder()
                .status(status)
                .body(bytes.to_vec())
                .unwrap(),
        );
        let err = swap_reply(reply).await.unwrap_err();
        assert_eq!(swap_outcome(&err), Some(SwapOutcome::Unchanged), "{err:#}");
        assert!(
            format!("{err:#}").contains("no build attestation"),
            "{err:#}"
        );
    }

    /// The outcome a failed broker swap reports crosses the socket intact, and
    /// a reply without one stays unknown.
    #[tokio::test]
    async fn a_failed_swap_tells_the_panel_what_it_left_behind() {
        use crate::panel::broker::{Broker, Policy, DEFAULT_IMAGE_REPOSITORY};
        use crate::panel::docker::fake::{container, FakeDocker};
        use crate::panel::docker::swap::{swap_outcome, SwapOutcome};
        use crate::panel::docker::ContainerState;
        use axum::body::{to_bytes, Body};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::panel::attestation::tests::{BOT_BUNDLE, BOT_DIGEST};
        let image = format!("{DEFAULT_IMAGE_REPOSITORY}@{BOT_DIGEST}");
        for outcome in [
            Some(SwapOutcome::Unchanged),
            Some(SwapOutcome::Restored),
            None,
        ] {
            let docker = std::sync::Arc::new(FakeDocker::new());
            docker.add_container(container("stitch-luno-a", ContainerState::Running));
            docker.fail_replace("injected", outcome);
            let policy = Policy::new("stitch-luno-a", DEFAULT_IMAGE_REPOSITORY).unwrap();
            let app = Broker::new(docker, policy).router();
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/bots/stitch-luno-a/image")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({ "image": image, "attestations": [BOT_BUNDLE] })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = response.status();
            assert_eq!(status, StatusCode::BAD_GATEWAY);
            let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
            let reply = reqwest::Response::from(
                axum::http::Response::builder()
                    .status(status)
                    .body(bytes.to_vec())
                    .unwrap(),
            );
            let err = swap_reply(reply).await.unwrap_err();
            assert_eq!(swap_outcome(&err), outcome, "{err:#}");
            assert!(format!("{err:#}").contains("injected"), "{err:#}");
        }
    }
}
