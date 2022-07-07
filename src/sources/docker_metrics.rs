use std::{
    collections::BTreeMap, collections::HashMap, future::ready, pin::Pin, sync::Arc, time::Duration,
};

use bollard::{
    container::{
        InspectContainerOptions, ListContainersOptions, MemoryStatsStats, Stats, StatsOptions,
    },
    errors::Error as DockerError,
    service::{ContainerInspectResponse, EventMessage},
    system::EventsOptions,
    Docker,
};
use bytes::Bytes;
use chrono::{DateTime, Local, ParseError, Utc};
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use vector_config::configurable_component;
use vector_core::ByteSizeOf;
// use vector_core::ByteSizeOf;

use super::util::MultilineConfig;
use crate::{
    config::{log_schema, DataType, Output, SourceConfig, SourceContext, SourceDescription},
    docker::{docker, DockerTlsConfig},
    event::{self, Metric, MetricKind, MetricValue},
    internal_events::{
        BytesReceived, DockerMetricsCommunicationError, DockerMetricsContainerEventReceived,
        DockerMetricsContainerMetadataFetchError, DockerMetricsContainerUnwatch,
        DockerMetricsContainerWatch, DockerMetricsEventsReceived,
        DockerMetricsLoggingDriverUnsupportedError, DockerMetricsTimestampParseError,
        StreamClosedError,
    },
    shutdown::ShutdownSignal,
    SourceSender,
};

// Prevent short hostname from being wrongly regconized as a container's short ID.
const MIN_HOSTNAME_LENGTH: usize = 6;

/// Configuration for the `docker_metrics` source.
#[configurable_component(source)]
#[derive(Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct DockerMetricsConfig {
    /// Overrides the name of the log field used to add the current hostname to each event.
    ///
    /// The value will be the current hostname for wherever Vector is running.
    ///
    /// By default, the [global `host_key` option](https://vector.dev/docs/reference/configuration//global-options#log_schema.host_key) is used.
    #[serde(default = "host_key")]
    host_key: String,

    /// Docker host to connect to.
    ///
    /// Use an HTTPS URL to enable TLS encryption.
    ///
    /// If absent, Vector will try to use `DOCKER_HOST` environment variable. If `DOCKER_HOST` is also absent, Vector will use default Docker local socket (`/var/run/docker.sock` on Unix platforms, `//./pipe/docker_engine` on Windows).
    docker_host: Option<String>,

    /// A list of container IDs or names of containers to exclude from log collection.
    ///
    /// Matching is prefix first, so specifying a value of `foo` would match any container named `foo` as well as any
    /// container whose name started with `foo`. This applies equally whether matching container IDs or names.
    ///
    /// By default, the source will collect logs for all containers. If `exclude_containers` is configured, any
    /// container that matches a configured exclusion will be excluded even if it is also included via
    /// `include_containers`, so care should be taken when utilizing prefix matches as they cannot be overridden by a
    /// corresponding entry in `include_containers` e.g. excluding `foo` by attempting to include `foo-specific-id`.
    ///
    /// This can be used in conjunction with `include_containers`.
    exclude_containers: Option<Vec<String>>, // Starts with actually, not exclude

    /// A list of container IDs or names of containers to include in log collection.
    ///
    /// Matching is prefix first, so specifying a value of `foo` would match any container named `foo` as well as any
    /// container whose name started with `foo`. This applies equally whether matching container IDs or names.
    ///
    /// By default, the source will collect logs for all containers. If `include_containers` is configured, only
    /// containers that match a configured inclusion and are also not excluded will be matched.
    ///
    /// This can be used in conjunction with `include_containers`.
    include_containers: Option<Vec<String>>, // Starts with actually, not include

    /// A list of container object labels to match against when filtering running containers.
    ///
    /// Labels should follow the syntax described in the [Docker object labels](https://docs.docker.com/config/labels-custom-metadata/) documentation.
    include_labels: Option<Vec<String>>,

    /// A list of image names to match against.
    ///
    /// If not provided, all images will be included.
    include_images: Option<Vec<String>>,

    /// Overrides the name of the log field used to mark an event as partial.
    ///
    /// If `auto_partial_merge` is disabled, partial events will be emitted with a log field, controlled by this
    /// configuration value, is set, indicating that the event is not complete.
    ///
    /// By default, `"_partial"` is used.
    partial_event_marker_field: Option<String>,

    /// Enables automatic merging of partial events.
    auto_partial_merge: bool,

    /// The amount of time, in seconds, to wait before retrying after an error.
    retry_backoff_secs: u64,

    /// Multiline aggregation configuration.
    ///
    /// If not specified, multiline aggregation is disabled.
    multiline: Option<MultilineConfig>,

    #[configurable(derived)]
    tls: Option<DockerTlsConfig>,
}

impl Default for DockerMetricsConfig {
    fn default() -> Self {
        Self {
            host_key: host_key(),
            docker_host: None,
            tls: None,
            exclude_containers: None,
            include_containers: None,
            include_labels: None,
            include_images: None,
            partial_event_marker_field: Some(event::PARTIAL.to_string()),
            auto_partial_merge: true,
            multiline: None,
            retry_backoff_secs: 2,
        }
    }
}

fn host_key() -> String {
    log_schema().host_key().to_string()
}

impl DockerMetricsConfig {
    fn container_name_or_id_included<'a>(
        &self,
        id: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let containers: Vec<String> = names.into_iter().map(Into::into).collect();

        self.include_containers
            .as_ref()
            .map(|include_list| Self::name_or_id_matches(id, &containers, include_list))
            .unwrap_or(true)
            && !(self
                .exclude_containers
                .as_ref()
                .map(|exclude_list| Self::name_or_id_matches(id, &containers, exclude_list))
                .unwrap_or(false))
    }

    fn name_or_id_matches(id: &str, names: &[String], items: &[String]) -> bool {
        items.iter().any(|flag| id.starts_with(flag))
            || names
                .iter()
                .any(|name| items.iter().any(|item| name.starts_with(item)))
    }

    fn with_empty_partial_event_marker_field_as_none(mut self) -> Self {
        if let Some(val) = &self.partial_event_marker_field {
            if val.is_empty() {
                self.partial_event_marker_field = None;
            }
        }
        self
    }
}

inventory::submit! {
    SourceDescription::new::<DockerMetricsConfig>("docker_metrics")
}

impl_generate_config_from_default!(DockerMetricsConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "docker_metrics")]
impl SourceConfig for DockerMetricsConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        let source = DockerMetricsSource::new(
            self.clone().with_empty_partial_event_marker_field_as_none(),
            cx.out,
            cx.shutdown.clone(),
        )?;

        // Capture currently running containers, and do main future(run)
        let fut = async move {
            match source.handle_running_containers().await {
                Ok(source) => source.run().await,
                Err(error) => {
                    error!(
                        message = "Listing currently running containers failed.",
                        %error
                    );
                }
            }
        };

        let shutdown = cx.shutdown;
        // Once this ShutdownSignal resolves it will drop DockerMetricsSource and by extension it's ShutdownSignal.
        Ok(Box::pin(async move {
            Ok(tokio::select! {
                _ = fut => {}
                _ = shutdown => {}
            })
        }))
    }

    fn outputs(&self) -> Vec<Output> {
        vec![Output::default(DataType::Metric)]
    }

    fn source_type(&self) -> &'static str {
        "docker_metrics"
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

// Add a compatibility alias to avoid breaking existing configs
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DockerCompatConfig {
    #[serde(flatten)]
    config: DockerMetricsConfig,
}

#[async_trait::async_trait]
#[typetag::serde(name = "docker")]
impl SourceConfig for DockerCompatConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        self.config.build(cx).await
    }

    fn outputs(&self) -> Vec<Output> {
        self.config.outputs()
    }

    fn source_type(&self) -> &'static str {
        "docker"
    }

    fn can_acknowledge(&self) -> bool {
        false
    }
}

struct DockerMetricsSourceCore {
    config: DockerMetricsConfig,
    docker: Docker,
    /// Only logs created at, or after this moment are logged.
    now_timestamp: DateTime<Utc>,
}

impl DockerMetricsSourceCore {
    fn new(config: DockerMetricsConfig) -> crate::Result<Self> {
        // ?NOTE: Constructs a new Docker instance for a docker host listening at url specified by an env var DOCKER_HOST.
        // ?      Otherwise connects to unix socket which requires sudo privileges, or docker group membership.
        let docker = docker(config.docker_host.clone(), config.tls.clone())?;

        // Only log events created at-or-after this moment are logged.
        let now = Local::now();
        info!(
            message = "Capturing metrics from now on.",
            now = %now.to_rfc3339()
        );

        Ok(DockerMetricsSourceCore {
            config,
            docker,
            now_timestamp: now.into(),
        })
    }

    /// Returns event stream coming from docker.
    fn docker_metrics_event_stream(
        &self,
    ) -> impl Stream<Item = Result<EventMessage, DockerError>> + Send {
        let mut filters = HashMap::new();

        // event  | emitted on commands
        // -------+-------------------
        // start  | docker start, docker run, restart policy, docker restart
        // unpause | docker unpause
        // die    | docker restart, docker stop, docker kill, process exited, oom
        // pause  | docker pause
        filters.insert(
            "event".to_owned(),
            vec![
                "start".to_owned(),
                "unpause".to_owned(),
                "die".to_owned(),
                "pause".to_owned(),
            ],
        );
        filters.insert("type".to_owned(), vec!["container".to_owned()]);

        // Apply include filters
        if let Some(include_labels) = &self.config.include_labels {
            filters.insert("label".to_owned(), include_labels.clone());
        }

        if let Some(include_images) = &self.config.include_images {
            filters.insert("image".to_owned(), include_images.clone());
        }

        self.docker.events(Some(EventsOptions {
            since: Some(self.now_timestamp),
            until: None,
            filters,
        }))
    }
}

/// Main future which listens for events coming from docker, and maintains
/// a fan of event_stream futures.
/// Where each event_stream corresponds to a running container marked with ContainerMetricInfo.
/// While running, event_stream streams Events to out channel.
/// Once a log stream has ended, it sends ContainerMetricInfo back to main.
///
/// Future  channel     Future      channel
///           |<---- event_stream ---->out
/// main <----|<---- event_stream ---->out
///           | ...                 ...out
///
struct DockerMetricsSource {
    esb: EventStreamBuilder,
    /// event stream from docker
    events: Pin<Box<dyn Stream<Item = Result<EventMessage, DockerError>> + Send>>,
    ///  mappings of seen container_id to their data
    containers: HashMap<ContainerId, ContainerState>,
    ///receives ContainerMetricInfo coming from event stream futures
    main_recv:
        mpsc::UnboundedReceiver<Result<ContainerMetricInfo, (ContainerId, ErrorPersistence)>>,
    /// It may contain shortened container id.
    hostname: Option<String>,
    backoff_duration: Duration,
}

impl DockerMetricsSource {
    fn new(
        config: DockerMetricsConfig,
        out: SourceSender,
        shutdown: ShutdownSignal,
    ) -> crate::Result<DockerMetricsSource> {
        let backoff_secs = config.retry_backoff_secs;

        let host_key = config.host_key.clone();
        let hostname = crate::get_hostname().ok();

        // Only logs created at, or after this moment are logged.
        let core = DockerMetricsSourceCore::new(config)?;

        // main event stream, with whom only newly started/restarted containers will be logged.
        let events = core.docker_metrics_event_stream();
        info!(message = "Listening to docker log events.");

        // Channel of communication between main future and event_stream futures
        let (main_send, main_recv) = mpsc::unbounded_channel::<
            Result<ContainerMetricInfo, (ContainerId, ErrorPersistence)>,
        >();

        // Starting with logs from now.
        // TODO: Is this exception acceptable?
        // Only somewhat exception to this is case where:
        // t0 -- outside: container running
        // t1 -- now_timestamp
        // t2 -- outside: container stopped
        // t3 -- list_containers
        // In that case, logs between [t1,t2] will be pulled to vector only on next start/unpause of that container.
        let esb = EventStreamBuilder {
            host_key,
            hostname: hostname.clone(),
            core: Arc::new(core),
            out,
            main_send,
            shutdown,
        };

        Ok(DockerMetricsSource {
            esb,
            events: Box::pin(events),
            containers: HashMap::new(),
            main_recv,
            hostname,
            backoff_duration: Duration::from_secs(backoff_secs),
        })
    }

    /// Future that captures currently running containers, and starts event streams for them.
    async fn handle_running_containers(mut self) -> crate::Result<Self> {
        let mut filters = HashMap::new();

        // Apply include filters
        if let Some(include_labels) = &self.esb.core.config.include_labels {
            filters.insert("label".to_owned(), include_labels.clone());
        }

        if let Some(include_images) = &self.esb.core.config.include_images {
            filters.insert("ancestor".to_owned(), include_images.clone());
        }

        self.esb
            .core
            .docker
            .list_containers(Some(ListContainersOptions {
                all: false, // only running containers
                filters,
                ..Default::default()
            }))
            .await?
            .into_iter()
            .for_each(|container| {
                let id = container.id.unwrap();
                let names = container.names.unwrap();

                trace!(message = "Found already running container.", id = %id, names = ?names);

                if self.exclude_self(id.as_str()) {
                    info!(message = "Excluded self container.", id = %id);
                    return;
                }

                if !self.esb.core.config.container_name_or_id_included(
                    id.as_str(),
                    names.iter().map(|s| {
                        // In this case bollard / shiplift gives names with starting '/' so it needs to be removed.
                        let s = s.as_str();
                        if s.starts_with('/') {
                            s.split_at('/'.len_utf8()).1
                        } else {
                            s
                        }
                    }),
                ) {
                    info!(message = "Excluded container.", id = %id);
                    return;
                }

                let id = ContainerId::new(id);
                self.containers.insert(id.clone(), self.esb.start(id, None));
            });

        Ok(self)
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                value = self.main_recv.recv() => {
                    match value {
                        Some(Ok(info)) => {
                            let state = self
                                .containers
                                .get_mut(&info.id)
                                .expect("Every ContainerMetricInfo has it's ContainerState");
                            if state.return_info(info) {
                                self.esb.restart(state);
                            }
                        },
                        Some(Err((id,persistence))) => {
                            let state = self
                                .containers
                                .remove(&id)
                                .expect("Every started ContainerId has it's ContainerState");
                            match persistence{
                                ErrorPersistence::Transient => if state.is_running() {
                                    let backoff= Some(self.backoff_duration);
                                    self.containers.insert(id.clone(), self.esb.start(id, backoff));
                                }
                                // Forget the container since the error is permanent.
                                ErrorPersistence::Permanent => (),
                            }
                        }
                        None => {
                            error!(message = "The docker_metrics source main stream has ended unexpectedly.");
                            info!(message = "Shutting down docker_metrics source.");
                            return;
                        }
                    };
                }
                value = self.events.next() => {
                    match value {
                        Some(Ok(mut event)) => {
                            let action = event.action.unwrap();
                            let actor = event.actor.take().unwrap();
                            let id = actor.id.unwrap();
                            let attributes = actor.attributes.unwrap();

                            emit!(DockerMetricsContainerEventReceived { container_id: &id, action: &action });

                            let id = ContainerId::new(id.to_owned());

                            // Update container status
                            match action.as_str() {
                                "die" | "pause" => {
                                    if let Some(state) = self.containers.get_mut(&id) {
                                        state.stopped();
                                    }
                                }
                                "start" | "unpause" => {
                                    if let Some(state) = self.containers.get_mut(&id) {
                                        state.running();
                                        self.esb.restart(state);
                                    } else {
                                        let include_name =
                                            self.esb.core.config.container_name_or_id_included(
                                                id.as_str(),
                                                attributes.get("name").map(|s| s.as_str()),
                                            );

                                        let exclude_self = self.exclude_self(id.as_str());

                                        if include_name && !exclude_self {
                                            self.containers.insert(id.clone(), self.esb.start(id, None));
                                        }
                                    }
                                }
                                _ => {},
                            };
                        }
                        Some(Err(error)) => {
                            emit!(DockerMetricsCommunicationError {
                                error,
                                container_id: None,
                            });
                            return;
                        },
                        None => {
                            // TODO: this could be fixed, but should be tried with some timeoff and exponential backoff
                            error!(message = "Docker log event stream has ended unexpectedly.");
                            info!(message = "Shutting down docker_metrics source.");
                            return;
                        }
                    };
                }
            };
        }
    }

    fn exclude_self(&self, id: &str) -> bool {
        self.hostname
            .as_ref()
            .map(|hostname| id.starts_with(hostname) && hostname.len() >= MIN_HOSTNAME_LENGTH)
            .unwrap_or(false)
    }
}

/// Used to construct and start event stream futures
#[derive(Clone)]
struct EventStreamBuilder {
    host_key: String,
    hostname: Option<String>,
    core: Arc<DockerMetricsSourceCore>,
    /// Event stream futures send events through this
    out: SourceSender,
    /// End through which event stream futures send ContainerMetricInfo to main future
    main_send: mpsc::UnboundedSender<Result<ContainerMetricInfo, (ContainerId, ErrorPersistence)>>,
    /// Self and event streams will end on this.
    shutdown: ShutdownSignal,
}

impl EventStreamBuilder {
    /// Spawn a task to runs event stream until shutdown.
    fn start(&self, id: ContainerId, backoff: Option<Duration>) -> ContainerState {
        let this = self.clone();
        tokio::spawn(async move {
            if let Some(duration) = backoff {
                tokio::time::sleep(duration).await;
            }
            match this
                .core
                .docker
                .inspect_container(id.as_str(), None::<InspectContainerOptions>)
                .await
            {
                Ok(details) => match ContainerMetadata::from_details(details) {
                    Ok(metadata) => {
                        let info = ContainerMetricInfo::new(id, metadata);
                        this.run_event_stream(info).await;
                        return;
                    }
                    Err(error) => emit!(DockerMetricsTimestampParseError {
                        error,
                        container_id: id.as_str()
                    }),
                },
                Err(error) => emit!(DockerMetricsContainerMetadataFetchError {
                    error,
                    container_id: id.as_str()
                }),
            }

            this.finish(Err((id, ErrorPersistence::Transient)));
        });

        ContainerState::new_running()
    }

    /// If info is present, restarts event stream which will run until shutdown.
    fn restart(&self, container: &mut ContainerState) {
        if let Some(info) = container.take_info() {
            let this = self.clone();
            tokio::spawn(this.run_event_stream(info));
        }
    }

    async fn run_event_stream(mut self, mut info: ContainerMetricInfo) {
        // Establish connection
        let options = Some(StatsOptions {
            stream: true,
            one_shot: false,
        });

        // TODO HERE!!!
        let stream = self.core.docker.stats(info.id.as_str(), options);
        emit!(DockerMetricsContainerWatch {
            container_id: info.id.as_str()
        });

        // Create event streamer
        // let core = Arc::clone(&self.core);

        let mut error = None;
        let events_stream = stream
            .map(|value| {
                match value {
                    Ok(message) => Ok(info.new_events(message)),
                    Err(error) => {
                        // On any error, restart connection
                        match &error {
                            DockerError::DockerResponseServerError { status_code, .. }
                                if *status_code == http::StatusCode::NOT_IMPLEMENTED =>
                            {
                                emit!(DockerMetricsLoggingDriverUnsupportedError {
                                    error,
                                    container_id: info.id.as_str(),
                                });
                                Err(ErrorPersistence::Permanent)
                            }
                            _ => {
                                emit!(DockerMetricsCommunicationError {
                                    error,
                                    container_id: Some(info.id.as_str())
                                });
                                Err(ErrorPersistence::Transient)
                            }
                        }
                    }
                }
            })
            .take_while(|v| {
                error = v.as_ref().err().cloned();
                ready(v.is_ok())
            })
            .flat_map(|v| stream::iter(v.unwrap()))
            .take_until(self.shutdown.clone());

        // let events_stream: Box<dyn Stream<Item = Metric> + Unpin + Send> =
        //     if let Some(ref line_agg_config) = core.line_agg_config {
        //         Box::new(line_agg_adapter(
        //             events_stream,
        //             line_agg::Logic::new(line_agg_config.clone()),
        //         ))
        //     } else {
        //         Box::new(events_stream)
        //     };
        let events_stream: Box<dyn Stream<Item = Metric> + Unpin + Send> = Box::new(events_stream);

        let host_key = self.host_key.clone();
        let hostname = self.hostname.clone();
        let result = {
            let mut stream =
                events_stream.map(move |event| add_hostname(event, &host_key, &hostname));
            self.out
                .send_event_stream(&mut stream)
                .await
                .map_err(|error| {
                    let (count, _) = stream.size_hint();
                    emit!(StreamClosedError { error, count });
                })
        };

        // End of stream
        emit!(DockerMetricsContainerUnwatch {
            container_id: info.id.as_str()
        });

        let result = match (result, error) {
            (Ok(()), None) => Ok(info),
            (Err(()), _) => Err((info.id, ErrorPersistence::Permanent)),
            (_, Some(occurrence)) => Err((info.id, occurrence)),
        };

        self.finish(result);
    }

    fn finish(self, result: Result<ContainerMetricInfo, (ContainerId, ErrorPersistence)>) {
        // This can legaly fail when shutting down, and any other
        // reason should have been logged in the main future.
        let _ = self.main_send.send(result);
    }
}

fn add_hostname(mut event: Metric, host_key: &str, hostname: &Option<String>) -> Metric {
    if let Some(hostname) = hostname {
        event.insert_tag(host_key.to_string(), hostname.clone());
    }

    event
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ErrorPersistence {
    Transient,
    Permanent,
}

/// Container ID as assigned by Docker.
/// Is actually a string.
#[derive(Hash, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct ContainerId(Bytes);

impl ContainerId {
    fn new(id: String) -> Self {
        ContainerId(id.into())
    }

    fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("Container Id Bytes aren't String")
    }
}

/// Kept by main to keep track of container state
struct ContainerState {
    /// None if there is a event_stream of this container.
    info: Option<ContainerMetricInfo>,
    /// True if Container is currently running
    running: bool,
    /// Of running
    generation: u64,
}

impl ContainerState {
    /// It's ContainerMetricInfo pair must be created exactly once.
    const fn new_running() -> Self {
        ContainerState {
            info: None,
            running: true,
            generation: 0,
        }
    }

    fn running(&mut self) {
        self.running = true;
        self.generation += 1;
    }

    fn stopped(&mut self) {
        self.running = false;
    }

    const fn is_running(&self) -> bool {
        self.running
    }

    /// True if it needs to be restarted.
    #[must_use]
    fn return_info(&mut self, info: ContainerMetricInfo) -> bool {
        debug_assert!(self.info.is_none());
        // Generation is the only one strictly necessary,
        // but with v.running, restarting event_stream is automatically done.
        let restart = self.running || info.generation < self.generation;
        self.info = Some(info);
        restart
    }

    fn take_info(&mut self) -> Option<ContainerMetricInfo> {
        self.info.take().map(|mut info| {
            // Uplogsdate info
            info.generation = self.generation;
            info
        })
    }
}

/// Exchanged between main future and event_stream futures
struct ContainerMetricInfo {
    /// Container docker ID
    id: ContainerId,
    name: String,
    generation: u64,
    tags: BTreeMap<String, String>,
}

macro_rules! build_metric {
    ($name:expr, $value:expr) => {
        Metric::new(
            $name,
            MetricKind::Absolute,
            MetricValue::Counter {
                value: $value as f64,
            },
        )
    };
    ($pattern:expr, $name:expr, $value:expr) => {
        build_metric!(format!($pattern, $name), $value)
    };
}

impl ContainerMetricInfo {
    /// Container docker ID
    fn new(id: ContainerId, metadata: ContainerMetadata) -> Self {
        let tags: BTreeMap<String, String> = [
            ("container_id".to_string(), id.as_str().to_string()),
            ("container_name".to_string(), metadata.name.clone()),
            ("image_name".to_string(), metadata.image),
        ]
        .into_iter()
        .collect();
        // TODO maybe adding all the labels as part of the tags
        ContainerMetricInfo {
            id,
            name: metadata.name,
            generation: 0,
            tags,
        }
    }

    // yes, it's long...
    fn new_events(&mut self, stats: Stats) -> Vec<Metric> {
        let mut res = Vec::new();

        emit!(BytesReceived {
            // byte_size: bytes_message.len(),
            byte_size: 42,
            protocol: "http"
        });

        println!("received: {:#?}", stats);

        res.push(build_metric!("num_procs", stats.num_procs));
        if let Some(value) = stats.pids_stats.current {
            res.push(build_metric!("pid_current", value));
        }
        if let Some(value) = stats.pids_stats.limit {
            res.push(build_metric!("pid_limit", value));
        }
        if let Some(network) = stats.network {
            res.push(build_metric!("network_rx_bytes", network.rx_bytes));
            res.push(build_metric!("network_rx_errors", network.rx_errors));
            res.push(build_metric!("network_rx_dropped", network.rx_dropped));
            res.push(build_metric!("network_rx_packets", network.rx_packets));
            res.push(build_metric!("network_tx_bytes", network.tx_bytes));
            res.push(build_metric!("network_tx_errors", network.tx_errors));
            res.push(build_metric!("network_tx_dropped", network.tx_dropped));
            res.push(build_metric!("network_tx_packets", network.tx_packets));
        }
        if let Some(networks) = stats.networks {
            for (name, network) in networks {
                res.push(build_metric!("network_{}_rx_bytes", name, network.rx_bytes));
                res.push(build_metric!(
                    "network_{}_rx_errors",
                    name,
                    network.rx_errors
                ));
                res.push(build_metric!(
                    "network_{}_rx_dropped",
                    name,
                    network.rx_dropped
                ));
                res.push(build_metric!(
                    "network_{}_rx_packets",
                    name,
                    network.rx_packets
                ));
                res.push(build_metric!("network_{}_tx_bytes", name, network.tx_bytes));
                res.push(build_metric!(
                    "network_{}_tx_errors",
                    name,
                    network.tx_errors
                ));
                res.push(build_metric!(
                    "network_{}_tx_dropped",
                    name,
                    network.tx_dropped
                ));
                res.push(build_metric!(
                    "network_{}_tx_packets",
                    name,
                    network.tx_packets
                ));
            }
        }
        match stats.memory_stats.stats {
            Some(MemoryStatsStats::V1(v1)) => {
                res.push(build_metric!("memory_stats_v1_cache", v1.cache));
                res.push(build_metric!("memory_stats_v1_dirty", v1.dirty));
                res.push(build_metric!("memory_stats_v1_mapped_file", v1.mapped_file));
                res.push(build_metric!(
                    "memory_stats_v1_total_inactive_file",
                    v1.total_inactive_file
                ));
                res.push(build_metric!("memory_stats_v1_pgpgout", v1.pgpgout));
                res.push(build_metric!("memory_stats_v1_rss", v1.rss));
                res.push(build_metric!(
                    "memory_stats_v1_total_mapped_file",
                    v1.total_mapped_file
                ))
            }
            Some(MemoryStatsStats::V2(_v2)) => (), // TODO
            None => (),                            // TODO
        };
        if let Some(value) = stats.memory_stats.max_usage {
            res.push(build_metric!("memory_max_usage", value));
        }
        if let Some(value) = stats.memory_stats.usage {
            res.push(build_metric!("memory_usage", value));
        }
        if let Some(value) = stats.memory_stats.failcnt {
            res.push(build_metric!("memory_failcnt", value));
        }
        if let Some(value) = stats.memory_stats.limit {
            res.push(build_metric!("memory_limit", value));
        }
        if let Some(value) = stats.memory_stats.commit {
            res.push(build_metric!("memory_commit", value));
        }
        if let Some(value) = stats.memory_stats.commit_peak {
            res.push(build_metric!("memory_commit_peak", value));
        }
        if let Some(value) = stats.memory_stats.commitbytes {
            res.push(build_metric!("memory_commit_bytes", value));
        }
        if let Some(value) = stats.memory_stats.commitpeakbytes {
            res.push(build_metric!("memory_commit_peak_bytes", value));
        }
        if let Some(value) = stats.memory_stats.privateworkingset {
            res.push(build_metric!("memory_private_working_set", value));
        }

        let res = res
            .into_iter()
            .map(|item| item.with_tags(Some(self.tags.clone())))
            .collect::<Vec<_>>();

        // Partial or not partial - we return the event we got here, because all
        // other cases were handled earlier.
        emit!(DockerMetricsEventsReceived {
            byte_size: res.size_of(),
            container_id: self.id.as_str(),
            container_name: self.name.as_str(),
        });

        res
    }
}

struct ContainerMetadata {
    // labels: HashMap<String, String>,
    name: String,
    image: String,
}

impl ContainerMetadata {
    fn from_details(details: ContainerInspectResponse) -> Result<Self, ParseError> {
        let config = details.config.unwrap();
        let name = details.name.unwrap();
        // let created = details.created.unwrap();

        // let labels = config.labels.unwrap_or_default();

        Ok(ContainerMetadata {
            // labels,
            name: name.as_str().trim_start_matches('/').to_owned(),
            image: config.image.unwrap(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<DockerMetricsConfig>();
    }

    #[test]
    fn exclude_self() {
        let (tx, _rx) = SourceSender::new_test();
        let mut source =
            DockerMetricsSource::new(DockerMetricsConfig::default(), tx, ShutdownSignal::noop())
                .unwrap();
        source.hostname = Some("451062c59603".to_owned());
        assert!(
            source.exclude_self("451062c59603a1cf0c6af3e74a31c0ae63d8275aa16a5fc78ef31b923baaffc3")
        );

        // hostname too short
        source.hostname = Some("a".to_owned());
        assert!(!source.exclude_self("a29d569bd46c"));
    }
}

#[cfg(all(test, feature = "docker-metrics-integration-tests"))]
mod integration_tests {
    use bollard::{
        container::{
            Config as ContainerConfig, CreateContainerOptions, KillContainerOptions,
            RemoveContainerOptions, StartContainerOptions, WaitContainerOptions,
        },
        image::{CreateImageOptions, ListImagesOptions},
    };
    use futures::{stream::TryStreamExt, FutureExt};

    use super::*;
    use crate::{
        event::Event,
        test_util::{collect_n, collect_ready, trace_init},
        SourceSender,
    };

    /// None if docker is not present on the system
    fn source_with<'a, L: Into<Option<&'a str>>>(
        names: &[&str],
        label: L,
    ) -> impl Stream<Item = Event> {
        source_with_config(DockerMetricsConfig {
            include_containers: Some(names.iter().map(|&s| s.to_owned()).collect()),
            include_labels: Some(label.into().map(|l| vec![l.to_owned()]).unwrap_or_default()),
            ..DockerMetricsConfig::default()
        })
    }

    fn source_with_config(config: DockerMetricsConfig) -> impl Stream<Item = Event> {
        let (sender, recv) = SourceSender::new_test();
        tokio::spawn(async move {
            config
                .build(SourceContext::new_test(sender, None))
                .await
                .unwrap()
                .await
                .unwrap();
        });
        recv
    }

    /// Users should ensure to remove container before exiting.
    async fn log_container(
        name: &str,
        label: Option<&str>,
        log: &str,
        docker: &Docker,
        tty: bool,
    ) -> String {
        cmd_container(name, label, vec!["echo", log], docker, tty).await
    }

    /// Users should ensure to remove container before exiting.
    /// Will resend message every so often.
    async fn eternal_container(
        name: &str,
        label: Option<&str>,
        log: &str,
        docker: &Docker,
    ) -> String {
        cmd_container(
            name,
            label,
            vec![
                "sh",
                "-c",
                format!("echo before; i=0; while [ $i -le 50 ]; do sleep 0.1; echo {}; i=$((i+1)); done", log).as_str(),
            ],
            docker,
            false
        ).await
    }

    /// Users should ensure to remove container before exiting.
    async fn cmd_container(
        name: &str,
        label: Option<&str>,
        cmd: Vec<&str>,
        docker: &Docker,
        tty: bool,
    ) -> String {
        if let Some(id) = cmd_container_for_real(name, label, cmd, docker, tty).await {
            id
        } else {
            // Maybe a before created container is present
            info!(
                message = "Assumes that named container remained from previous tests.",
                name = name
            );
            name.to_owned()
        }
    }

    /// Users should ensure to remove container before exiting.
    async fn cmd_container_for_real(
        name: &str,
        label: Option<&str>,
        cmd: Vec<&str>,
        docker: &Docker,
        tty: bool,
    ) -> Option<String> {
        pull_busybox(docker).await;

        trace!("Creating container.");

        let options = Some(CreateContainerOptions { name });
        let config = ContainerConfig {
            image: Some("busybox"),
            cmd: Some(cmd),
            labels: label.map(|label| vec![(label, "")].into_iter().collect()),
            tty: Some(tty),
            ..Default::default()
        };

        let container = docker.create_container(options, config).await;
        container.ok().map(|c| c.id)
    }

    async fn pull_busybox(docker: &Docker) {
        let mut filters = HashMap::new();
        filters.insert("reference", vec!["busybox:latest"]);

        let options = Some(ListImagesOptions {
            filters,
            ..Default::default()
        });

        let images = docker.list_images(options).await.unwrap();
        if images.is_empty() {
            // If `busybox:latest` not found, pull it
            let options = Some(CreateImageOptions {
                from_image: "busybox",
                tag: "latest",
                ..Default::default()
            });

            docker
                .create_image(options, None, None)
                .for_each(|item| async move {
                    let info = item.unwrap();
                    if let Some(error) = info.error {
                        panic!("{:?}", error);
                    }
                })
                .await
        }
    }

    /// Returns once container has started
    async fn container_start(id: &str, docker: &Docker) -> Result<(), bollard::errors::Error> {
        trace!("Starting container.");

        let options = None::<StartContainerOptions<&str>>;
        docker.start_container(id, options).await
    }

    /// Returns once container is done running
    async fn container_wait(id: &str, docker: &Docker) -> Result<(), bollard::errors::Error> {
        trace!("Waiting for container.");

        docker
            .wait_container(id, None::<WaitContainerOptions<&str>>)
            .try_for_each(|exit| async move {
                info!(message = "Container exited with status code.", status_code = ?exit.status_code);
                Ok(())
            })
            .await
    }

    /// Returns once container is killed
    async fn container_kill(id: &str, docker: &Docker) -> Result<(), bollard::errors::Error> {
        trace!("Waiting for container to be killed.");

        docker
            .kill_container(id, None::<KillContainerOptions<&str>>)
            .await
    }

    /// Returns once container is done running
    async fn container_run(id: &str, docker: &Docker) -> Result<(), bollard::errors::Error> {
        container_start(id, docker).await?;
        container_wait(id, docker).await
    }

    async fn container_remove(id: &str, docker: &Docker) {
        trace!("Removing container.");

        // Don't panic, as this is unrelated to the test, and there are possibly other containers that need to be removed
        let _ = docker
            .remove_container(id, None::<RemoveContainerOptions>)
            .await
            .map_err(|e| error!(%e));
    }

    /// Returns once it's certain that log has been made
    /// Expects that this is the only one with a container
    async fn container_log_n(
        n: usize,
        name: &str,
        label: Option<&str>,
        log: &str,
        docker: &Docker,
    ) -> String {
        container_with_optional_tty_log_n(n, name, label, log, docker, false).await
    }
    async fn container_with_optional_tty_log_n(
        n: usize,
        name: &str,
        label: Option<&str>,
        log: &str,
        docker: &Docker,
        tty: bool,
    ) -> String {
        let id = log_container(name, label, log, docker, tty).await;
        for _ in 0..n {
            if let Err(error) = container_run(&id, docker).await {
                container_remove(&id, docker).await;
                panic!("Container failed to start with error: {:?}", error);
            }
        }
        id
    }

    /// Once function returns, the container has entered into running state.
    /// Container must be killed before removed.
    async fn running_container(
        name: &'static str,
        label: Option<&'static str>,
        log: &'static str,
        docker: &Docker,
    ) -> String {
        let out = source_with(&[name], None);
        let docker = docker.clone();

        let id = eternal_container(name, label, log, &docker).await;
        if let Err(error) = container_start(&id, &docker).await {
            container_remove(&id, &docker).await;
            panic!("Container start failed with error: {:?}", error);
        }

        // Wait for before message
        let events = collect_n(out, 1).await;
        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            "before".into()
        );

        id
    }

    fn is_empty<T>(mut rx: impl Stream<Item = T> + Unpin) -> bool {
        rx.next().now_or_never().is_none()
    }

    #[tokio::test]
    async fn container_with_tty() {
        trace_init();

        let message = "log container_with_tty";
        let name = "container_with_tty";

        let out = source_with(&[name], None);

        let docker = docker(None, None).unwrap();

        let id = container_with_optional_tty_log_n(1, name, None, message, &docker, true).await;
        let events = collect_n(out, 1).await;
        container_remove(&id, &docker).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            message.into()
        );
    }

    #[tokio::test]
    async fn newly_started() {
        trace_init();

        let message = "9";
        let name = "vector_test_newly_started";
        let label = "vector_test_label_newly_started";

        let out = source_with(&[name], None);

        let docker = docker(None, None).unwrap();

        let id = container_log_n(1, name, Some(label), message, &docker).await;
        let events = collect_n(out, 1).await;
        container_remove(&id, &docker).await;

        let log = events[0].as_log();
        assert_eq!(log[log_schema().message_key()], message.into());
        assert_eq!(log[&*super::CONTAINER], id.into());
        assert!(log.get(&*super::CREATED_AT).is_some());
        assert_eq!(log[&*super::IMAGE], "busybox".into());
        assert!(log.get(format!("label.{}", label).as_str()).is_some());
        assert_eq!(events[0].as_log()[&super::NAME], name.into());
        assert_eq!(
            events[0].as_log()[log_schema().source_type_key()],
            "docker".into()
        );
    }

    #[tokio::test]
    async fn restart() {
        trace_init();

        let message = "10";
        let name = "vector_test_restart";

        let out = source_with(&[name], None);

        let docker = docker(None, None).unwrap();

        let id = container_log_n(2, name, None, message, &docker).await;
        let events = collect_n(out, 2).await;
        container_remove(&id, &docker).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            message.into()
        );
        assert_eq!(
            events[1].as_log()[log_schema().message_key()],
            message.into()
        );
    }

    #[tokio::test]
    async fn include_containers() {
        trace_init();

        let message = "11";
        let name0 = "vector_test_include_container_0";
        let name1 = "vector_test_include_container_1";

        let out = source_with(&[name1], None);

        let docker = docker(None, None).unwrap();

        let id0 = container_log_n(1, name0, None, "11", &docker).await;
        let id1 = container_log_n(1, name1, None, message, &docker).await;
        let events = collect_n(out, 1).await;
        container_remove(&id0, &docker).await;
        container_remove(&id1, &docker).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            message.into()
        );
    }

    #[tokio::test]
    async fn exclude_containers() {
        trace_init();

        let will_be_read = "12";

        let prefix = "vector_test_exclude_containers";
        let included0 = format!("{}_{}", prefix, "include0");
        let included1 = format!("{}_{}", prefix, "include1");
        let excluded0 = format!("{}_{}", prefix, "excluded0");

        let docker = docker(None, None).unwrap();

        let out = source_with_config(DockerMetricsConfig {
            include_containers: Some(vec![prefix.to_owned()]),
            exclude_containers: Some(vec![excluded0.to_owned()]),
            ..DockerMetricsConfig::default()
        });

        let id0 = container_log_n(1, &excluded0, None, "will not be read", &docker).await;
        let id1 = container_log_n(1, &included0, None, will_be_read, &docker).await;
        let id2 = container_log_n(1, &included1, None, will_be_read, &docker).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = collect_ready(out).await;
        container_remove(&id0, &docker).await;
        container_remove(&id1, &docker).await;
        container_remove(&id2, &docker).await;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            will_be_read.into()
        );

        assert_eq!(
            events[1].as_log()[log_schema().message_key()],
            will_be_read.into()
        );
    }

    #[tokio::test]
    async fn include_labels() {
        trace_init();

        let message = "13";
        let name0 = "vector_test_include_labels_0";
        let name1 = "vector_test_include_labels_1";
        let label = "vector_test_include_label";

        let out = source_with(&[name0, name1], label);

        let docker = docker(None, None).unwrap();

        let id0 = container_log_n(1, name0, None, "13", &docker).await;
        let id1 = container_log_n(1, name1, Some(label), message, &docker).await;
        let events = collect_n(out, 1).await;
        container_remove(&id0, &docker).await;
        container_remove(&id1, &docker).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            message.into()
        );
    }

    #[tokio::test]
    async fn currently_running() {
        trace_init();

        let message = "14";
        let name = "vector_test_currently_running";
        let label = "vector_test_label_currently_running";

        let docker = docker(None, None).unwrap();
        let id = running_container(name, Some(label), message, &docker).await;
        let out = source_with(&[name], None);

        let events = collect_n(out, 1).await;
        let _ = container_kill(&id, &docker).await;
        container_remove(&id, &docker).await;

        let log = events[0].as_log();
        assert_eq!(log[log_schema().message_key()], message.into());
        assert_eq!(log[&*super::CONTAINER], id.into());
        assert!(log.get(&*super::CREATED_AT).is_some());
        assert_eq!(log[&*super::IMAGE], "busybox".into());
        assert!(log.get(format!("label.{}", label).as_str()).is_some());
        assert_eq!(events[0].as_log()[&super::NAME], name.into());
        assert_eq!(
            events[0].as_log()[log_schema().source_type_key()],
            "docker".into()
        );
    }

    #[tokio::test]
    async fn include_image() {
        trace_init();

        let message = "15";
        let name = "vector_test_include_image";
        let config = DockerMetricsConfig {
            include_containers: Some(vec![name.to_owned()]),
            include_images: Some(vec!["busybox".to_owned()]),
            ..DockerMetricsConfig::default()
        };

        let out = source_with_config(config);

        let docker = docker(None, None).unwrap();

        let id = container_log_n(1, name, None, message, &docker).await;
        let events = collect_n(out, 1).await;
        container_remove(&id, &docker).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            message.into()
        );
    }

    #[tokio::test]
    async fn not_include_image() {
        trace_init();

        let message = "16";
        let name = "vector_test_not_include_image";
        let config_ex = DockerMetricsConfig {
            include_images: Some(vec!["some_image".to_owned()]),
            ..DockerMetricsConfig::default()
        };

        let exclude_out = source_with_config(config_ex);

        let docker = docker(None, None).unwrap();

        let id = container_log_n(1, name, None, message, &docker).await;
        container_remove(&id, &docker).await;

        assert!(is_empty(exclude_out));
    }

    #[tokio::test]
    async fn not_include_running_image() {
        trace_init();

        let message = "17";
        let name = "vector_test_not_include_running_image";
        let config_ex = DockerMetricsConfig {
            include_images: Some(vec!["some_image".to_owned()]),
            ..DockerMetricsConfig::default()
        };
        let config_in = DockerMetricsConfig {
            include_containers: Some(vec![name.to_owned()]),
            include_images: Some(vec!["busybox".to_owned()]),
            ..DockerMetricsConfig::default()
        };

        let docker = docker(None, None).unwrap();

        let id = running_container(name, None, message, &docker).await;
        let exclude_out = source_with_config(config_ex);
        let include_out = source_with_config(config_in);

        let _ = collect_n(include_out, 1).await;
        let _ = container_kill(&id, &docker).await;
        container_remove(&id, &docker).await;

        assert!(is_empty(exclude_out));
    }

    #[tokio::test]
    async fn flat_labels() {
        trace_init();

        let message = "18";
        let name = "vector_test_flat_labels";
        let label = "vector.test.label.flat.labels";

        let docker = docker(None, None).unwrap();
        let id = running_container(name, Some(label), message, &docker).await;
        let out = source_with(&[name], None);

        let events = collect_n(out, 1).await;
        let _ = container_kill(&id, &docker).await;
        container_remove(&id, &docker).await;

        let log = events[0].as_log();
        assert_eq!(log[log_schema().message_key()], message.into());
        assert_eq!(log[&*super::CONTAINER], id.into());
        assert!(log.get(&*super::CREATED_AT).is_some());
        assert_eq!(log[&*super::IMAGE], "busybox".into());
        assert!(log
            .get("label")
            .unwrap()
            .as_object()
            .unwrap()
            .get(label)
            .is_some());
        assert_eq!(events[0].as_log()[&super::NAME], name.into());
        assert_eq!(
            events[0].as_log()[log_schema().source_type_key()],
            "docker".into()
        );
    }

    #[tokio::test]
    async fn log_longer_than_16kb() {
        trace_init();

        let mut message = String::with_capacity(20 * 1024);
        for _ in 0..message.capacity() {
            message.push('0');
        }
        let name = "vector_test_log_longer_than_16kb";

        let out = source_with(&[name], None);

        let docker = docker(None, None).unwrap();

        let id = container_log_n(1, name, None, message.as_str(), &docker).await;
        let events = collect_n(out, 1).await;
        container_remove(&id, &docker).await;

        let log = events[0].as_log();
        assert_eq!(log[log_schema().message_key()], message.into());
    }

    #[tokio::test]
    async fn merge_multiline() {
        trace_init();

        let emitted_messages = vec![
            "java.lang.Exception",
            "    at com.foo.bar(bar.java:123)",
            "    at com.foo.baz(baz.java:456)",
        ];
        let expected_messages = vec![concat!(
            "java.lang.Exception\n",
            "    at com.foo.bar(bar.java:123)\n",
            "    at com.foo.baz(baz.java:456)",
        )];
        let name = "vector_test_merge_multiline";
        let config = DockerMetricsConfig {
            include_containers: Some(vec![name.to_owned()]),
            include_images: Some(vec!["busybox".to_owned()]),
            multiline: Some(MultilineConfig {
                start_pattern: "^[^\\s]".to_owned(),
                condition_pattern: "^[\\s]+at".to_owned(),
                mode: line_agg::Mode::ContinueThrough,
                timeout_ms: 10,
            }),
            ..DockerMetricsConfig::default()
        };

        let out = source_with_config(config);

        let docker = docker(None, None).unwrap();

        let command = emitted_messages
            .into_iter()
            .map(|message| format!("echo {:?}", message))
            .collect::<Box<_>>()
            .join(" && ");

        let id = cmd_container(name, None, vec!["sh", "-c", &command], &docker, false).await;
        if let Err(error) = container_run(&id, &docker).await {
            container_remove(&id, &docker).await;
            panic!("Container failed to start with error: {:?}", error);
        }
        let events = collect_n(out, expected_messages.len()).await;
        container_remove(&id, &docker).await;

        let actual_messages = events
            .into_iter()
            .map(|event| {
                event
                    .into_log()
                    .remove(&*crate::config::log_schema().message_key())
                    .unwrap()
                    .to_string_lossy()
            })
            .collect::<Vec<_>>();
        assert_eq!(actual_messages, expected_messages);
    }
}