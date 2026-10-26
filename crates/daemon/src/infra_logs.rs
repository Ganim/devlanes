//! `infra.logs`: an infra container's Docker logs as [`LogLine`]s. The engine
//! keeps the logs; the daemon reads them on request and stores nothing.

use std::{collections::HashMap, convert::Infallible, sync::Arc, time::Duration};

use devlanes_logs::{Framer, MAX_LINE_BYTES, extract_level};
use devlanes_proto::{Context, InfraLogsParams, LogLine, Page, RpcError, events};
use futures_util::{
    StreamExt,
    future::join_all,
    stream::{BoxStream, select_all},
};
use serde_json::{Value, json};

use crate::{
    Session,
    engine::{Inner, required},
    integrations::{ContainerLogChunk, ContainerLogOptions, InfraContainer, InfraError},
    model::{LaneRt, ProjectRt},
    registry::internal,
    units::now_ms,
};

/// Lines sent when the request names no `tail`.
const DEFAULT_TAIL: usize = 200;
/// Largest `tail` served; Docker reads the whole log file to find the last lines.
const MAX_TAIL: usize = 5000;
/// How soon a follow first checks whether its stopped container runs again.
const RESUME_POLL: Duration = Duration::from_secs(1);
/// The longest wait between those checks. Each check can run Compose and a
/// Docker read, so a pane left on a removed container backs off to this.
const RESUME_MAX: Duration = Duration::from_secs(10);
/// Bytes of lines one answer without `follow` carries at most, well under the
/// IPC frame limit even with every line sent twice (`text` and `styled`).
const MAX_PAGE_BYTES: usize = 4 * 1024 * 1024;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

/// One line of container output before it gets its sequence number.
#[derive(Debug, Clone, PartialEq)]
struct Decoded {
    /// Docker's timestamp in nanoseconds; `None` when the line had none.
    nanos: Option<i128>,
    timestamp_ms: u64,
    text: String,
    styled: Option<String>,
    level: Option<String>,
}

impl Decoded {
    /// The merge order across replicas.
    fn order(&self) -> i128 {
        self.nanos
            .unwrap_or_else(|| i128::from(self.timestamp_ms) * 1_000_000)
    }
}

/// Splits Docker's `--timestamps` prefix (`2024-05-01T10:00:00.123456789Z `) off
/// a line. A line without a valid one comes back whole.
fn split_timestamp(line: &[u8]) -> (Option<jiff::Timestamp>, &[u8]) {
    // An empty line can come as the bare timestamp followed by the newline.
    let Some(end) = line.iter().position(|byte| matches!(byte, b' ' | b'\n')) else {
        return (None, line);
    };
    let parsed = std::str::from_utf8(&line[..end])
        .ok()
        .and_then(|text| text.parse::<jiff::Timestamp>().ok());
    match parsed {
        Some(timestamp) if line[end] == b' ' => (Some(timestamp), &line[end + 1..]),
        Some(timestamp) => (Some(timestamp), &line[end..]),
        None => (None, line),
    }
}

/// Orders a service's replicas the way their labels count them. Compose names
/// end in the replica number, so shorter names first keeps `db-10` after `db-9`.
fn sort_replicas(containers: &mut [InfraContainer]) {
    containers.sort_by(|a, b| (a.docker.len(), &a.docker).cmp(&(b.docker.len(), &b.docker)));
}

/// The text before each replica's lines: `<container>-<n> | ` when there are
/// several, nothing for a single container.
fn replica_prefixes(container: &str, count: usize) -> Vec<String> {
    if count <= 1 {
        return vec![String::new(); count];
    }
    (1..=count)
        .map(|replica| format!("{container}-{replica} | "))
        .collect()
}

/// Whether a container in this `ContainerSummary.state` writes output. A
/// container whose state the provider cannot tell is tried.
fn container_runs(state: Option<&str>) -> bool {
    state.is_none_or(|state| matches!(state, "running" | "healthy" | "starting" | "unhealthy"))
}

/// The Unix second `nanos` falls in, for Docker's `since`.
fn unix_second(nanos: i128) -> i64 {
    i64::try_from(nanos.div_euclid(NANOS_PER_SECOND)).unwrap_or(i64::MAX)
}

/// Lines of one output stream of one container. Docker's chunks do not follow
/// line boundaries, so a partial line waits here for the rest.
#[derive(Default)]
struct StreamDecoder {
    pending: Vec<u8>,
    framer: Framer,
    /// The last timestamp seen, for lines that carry none.
    last_ms: Option<u64>,
}

impl StreamDecoder {
    /// Decodes every complete line of `bytes` into `out`.
    fn push(&mut self, bytes: &[u8], out: &mut Vec<Decoded>) {
        self.pending.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(offset) = self.pending[start..].iter().position(|b| *b == b'\n') {
            let end = start + offset + 1;
            let line = self.pending[start..end].to_vec();
            self.decode(&line, out);
            start = end;
        }
        self.pending.drain(..start);
        // A process that never writes a newline must not grow the buffer forever.
        if self.pending.len() >= MAX_LINE_BYTES {
            self.finish(out);
        }
    }

    /// Decodes an unterminated last line once the stream ended.
    fn finish(&mut self, out: &mut Vec<Decoded>) {
        if self.pending.is_empty() {
            return;
        }
        let mut line = std::mem::take(&mut self.pending);
        line.push(b'\n');
        self.decode(&line, out);
    }

    /// One newline-terminated line: the timestamp comes off, the framer strips
    /// terminal controls except colors.
    fn decode(&mut self, line: &[u8], out: &mut Vec<Decoded>) {
        let (timestamp, rest) = split_timestamp(line);
        if let Some(timestamp) = timestamp {
            self.last_ms = Some(u64::try_from(timestamp.as_millisecond()).unwrap_or(0));
        }
        let timestamp_ms = *self.last_ms.get_or_insert_with(now_ms);
        let nanos = timestamp.map(|timestamp| timestamp.as_nanosecond());
        let _ = self
            .framer
            .ingest(rest, timestamp_ms, |frame| -> Result<(), Infallible> {
                out.push(Decoded {
                    nanos,
                    timestamp_ms,
                    level: extract_level(&frame.text),
                    text: frame.text,
                    styled: frame.styled,
                });
                Ok(())
            });
    }
}

/// Decoders of one container: stdout and stderr each keep their own partial line.
#[derive(Default)]
struct ReplicaDecoder {
    stdout: StreamDecoder,
    stderr: StreamDecoder,
}

impl ReplicaDecoder {
    /// Decodes the complete lines of `chunk`.
    fn push(&mut self, chunk: &ContainerLogChunk) -> Vec<Decoded> {
        let mut out = Vec::new();
        if chunk.stderr {
            self.stderr.push(&chunk.bytes, &mut out);
        } else {
            self.stdout.push(&chunk.bytes, &mut out);
        }
        out
    }

    /// Decodes what is left once the stream ended.
    fn finish(&mut self) -> Vec<Decoded> {
        let mut out = Vec::new();
        self.stdout.finish(&mut out);
        self.stderr.finish(&mut out);
        out
    }
}

/// The lines of one request: the container they belong to and the numbering
/// the tail and the followed lines share.
struct Reader {
    container: String,
    styled: bool,
    seq: u64,
}

impl Reader {
    /// The wire line for `decoded`, numbered next.
    fn line(&mut self, prefix: &str, decoded: Decoded) -> LogLine {
        self.seq += 1;
        LogLine {
            unit: self.container.clone(),
            seq: self.seq,
            timestamp_ms: decoded.timestamp_ms,
            text: format!("{prefix}{}", decoded.text),
            level: decoded.level,
            styled: if self.styled {
                decoded.styled.map(|styled| format!("{prefix}{styled}"))
            } else {
                None
            },
        }
    }

    /// The `log` event carrying `line`.
    fn event(&self, line: &LogLine) -> devlanes_proto::Event {
        devlanes_proto::Event {
            seq: 0,
            event: events::LOG.into(),
            data: json!({"unit": self.container, "line": line}),
        }
    }
}

/// The terminal response of a follow, shaped like `logs`.
fn followed() -> Value {
    json!({"items": [], "truncated": false, "next_cursor": null})
}

impl Inner {
    /// `infra.logs`: the last lines of a lane's infra container, and with
    /// `follow` its new lines as `log` events until the client leaves.
    pub(crate) async fn infra_logs_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: InfraLogsParams = required(value)?;
        let (project, lane) = self.target_lane(context).await?;
        let tail = args.tail.unwrap_or(DEFAULT_TAIL).min(MAX_TAIL);
        let targets = self
            .infra_log_targets(&project, &lane, &args.container)
            .await?;
        let mut reader = Reader {
            container: args.container.clone(),
            styled: args.styled,
            seq: 0,
        };
        let read_at = now_ms();
        let (lines, truncated, mut last_seen) = self.read_tail(&reader, &targets, tail).await?;
        let page: Vec<LogLine> = lines
            .into_iter()
            .map(|(prefix, decoded)| reader.line(&prefix, decoded))
            .collect();
        if !args.follow {
            return serde_json::to_value(Page {
                items: page,
                truncated,
                next_cursor: None,
            })
            .map_err(|error| internal("encode", error));
        }
        for line in &page {
            if session.event(reader.event(line)).await.is_err() {
                return Ok(followed());
            }
        }
        let since = i64::try_from(read_at / 1000).unwrap_or(i64::MAX);
        self.follow_infra_logs(
            (&project, &lane),
            &mut reader,
            targets,
            &mut last_seen,
            since,
            session,
        )
        .await;
        Ok(followed())
    }

    /// The Docker containers behind `container`, in replica order: the lane's
    /// own, else a shared one another lane of the project started, else what
    /// the provider finds running without starting anything.
    async fn infra_log_targets(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        container: &str,
    ) -> Result<Vec<InfraContainer>, RpcError> {
        let mut candidates: Vec<String> = Vec::new();
        let mut pick = |containers: &[InfraContainer], shared_only: bool| {
            let mut found: Vec<InfraContainer> = Vec::new();
            for known in containers {
                if !candidates.contains(&known.name) {
                    candidates.push(known.name.clone());
                }
                if known.name == container
                    && (!shared_only || known.shared)
                    && !found.iter().any(|f| f.docker == known.docker)
                {
                    found.push(known.clone());
                }
            }
            sort_replicas(&mut found);
            found
        };
        let own = pick(&lane.st().infra_containers, false);
        if !own.is_empty() {
            return Ok(own);
        }
        let others: Vec<InfraContainer> = project
            .lanes()
            .iter()
            .flat_map(|other| other.st().infra_containers.clone())
            .collect();
        let shared = pick(&others, true);
        if !shared.is_empty() {
            return Ok(shared);
        }
        // Plain containers need their host ports in the spec to be listed.
        if let Ok(spec) = self.infra_spec(project, lane, &[]).await {
            let needs = self.integ.infra.port_needs(&spec).await.unwrap_or_default();
            let spec = match self.infra_spec(project, lane, &needs).await {
                Ok(with_ports) => with_ports,
                Err(_) => spec,
            };
            if let Ok(up) = self.integ.infra.namespace(&spec).await {
                let found = pick(&up.containers, false);
                if !found.is_empty() {
                    return Ok(found);
                }
            }
        }
        candidates.sort();
        Err(RpcError::new(
            "not_found",
            json!({"reference": container, "candidates": candidates}),
            3,
        ))
    }

    /// The last `tail` lines of `targets` merged by time, whether lines were
    /// left out, and each container's last timestamp. Fails only when no
    /// container could be read.
    async fn read_tail(
        &self,
        reader: &Reader,
        targets: &[InfraContainer],
        tail: usize,
    ) -> Result<(Vec<(String, Decoded)>, bool, HashMap<String, i128>), RpcError> {
        let prefixes = replica_prefixes(&reader.container, targets.len());
        let options = ContainerLogOptions {
            tail: Some(tail),
            follow: false,
            since: None,
            timestamps: true,
        };
        let reads = targets.iter().map(|target| async move {
            let mut stream = self
                .integ
                .infra
                .container_logs(&target.docker, options)
                .await?;
            let mut decoder = ReplicaDecoder::default();
            let mut lines = Vec::new();
            while let Some(chunk) = stream.next().await {
                lines.extend(decoder.push(&chunk?));
            }
            lines.extend(decoder.finish());
            Ok::<_, InfraError>(lines)
        });
        let mut merged: Vec<(String, Decoded)> = Vec::new();
        let mut last_seen = HashMap::new();
        let mut truncated = false;
        let mut failure = None;
        let mut read_any = false;
        for ((target, prefix), read) in targets.iter().zip(&prefixes).zip(join_all(reads).await) {
            match read {
                Ok(lines) => {
                    read_any = true;
                    // Docker gave as many as asked, so older lines likely exist.
                    truncated |= lines.len() >= tail;
                    if let Some(nanos) = lines.iter().filter_map(|line| line.nanos).max() {
                        last_seen.insert(target.docker.clone(), nanos);
                    }
                    merged.extend(lines.into_iter().map(|line| (prefix.clone(), line)));
                }
                Err(error) => {
                    tracing::debug!(container = %target.docker, code = %error.code, "infra logs read failed");
                    failure.get_or_insert(error);
                }
            }
        }
        if let (false, Some(error)) = (read_any, failure) {
            return Err(error.into());
        }
        merged.sort_by_key(|(_, line)| line.order());
        if merged.len() > tail {
            merged.drain(..merged.len() - tail);
            truncated = true;
        }
        // The newest lines that fit the byte budget; long lines can fill it first.
        let size = |(prefix, line): &(String, Decoded)| {
            prefix.len() * 2 + line.text.len() + line.styled.as_ref().map_or(0, String::len) + 64
        };
        let mut bytes = 0;
        let keep = merged
            .iter()
            .rev()
            .take_while(|line| {
                bytes += size(line);
                bytes <= MAX_PAGE_BYTES
            })
            .count();
        if keep < merged.len() {
            merged.drain(..merged.len() - keep);
            truncated = true;
        }
        Ok((merged, truncated, last_seen))
    }

    /// Streams new lines of `targets` until the client leaves. When the
    /// containers stop, waits until one runs again and resumes after the last
    /// line sent, so a restart shows as a gap in time and never repeats lines.
    /// `since` is the Unix second to read from for a container without lines yet.
    /// Replicas resume together: one that restarts while others keep running
    /// shows its new lines only once all of their streams have ended.
    async fn follow_infra_logs(
        self: &Arc<Self>,
        (project, lane): (&Arc<ProjectRt>, &Arc<LaneRt>),
        reader: &mut Reader,
        mut targets: Vec<InfraContainer>,
        last_seen: &mut HashMap<String, i128>,
        mut since: i64,
        session: &Session,
    ) {
        let disconnected = session.disconnected();
        let mut delay = RESUME_POLL;
        loop {
            let prefixes = replica_prefixes(&reader.container, targets.len());
            let mut decoders: Vec<ReplicaDecoder> =
                targets.iter().map(|_| ReplicaDecoder::default()).collect();
            // Lines up to here were sent before; Docker's `since` is whole seconds.
            let cutoffs: Vec<Option<i128>> = targets
                .iter()
                .map(|target| last_seen.get(&target.docker).copied())
                .collect();
            let mut streams: Vec<
                BoxStream<'static, (usize, Result<ContainerLogChunk, InfraError>)>,
            > = Vec::new();
            for (index, target) in targets.iter().enumerate() {
                let options = ContainerLogOptions {
                    tail: None,
                    follow: true,
                    since: Some(cutoffs[index].map_or(since, unix_second)),
                    timestamps: true,
                };
                match self
                    .integ
                    .infra
                    .container_logs(&target.docker, options)
                    .await
                {
                    Ok(stream) => streams.push(stream.map(move |chunk| (index, chunk)).boxed()),
                    Err(error) => {
                        tracing::debug!(container = %target.docker, code = %error.code, "infra logs follow failed");
                    }
                }
            }
            let mut merged = select_all(streams);
            loop {
                let next = tokio::select! {
                    () = disconnected.cancelled() => return,
                    next = merged.next() => next,
                };
                let Some((index, chunk)) = next else { break };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(error) => {
                        tracing::debug!(container = %targets[index].docker, code = %error.code, "infra logs stream failed");
                        continue;
                    }
                };
                // Output means the container came back: check soon after its next stop.
                delay = RESUME_POLL;
                let lines = decoders[index].push(&chunk);
                let docker = &targets[index].docker;
                if !send(
                    reader,
                    session,
                    last_seen,
                    docker,
                    cutoffs[index],
                    &prefixes[index],
                    lines,
                )
                .await
                {
                    return;
                }
            }
            for (index, decoder) in decoders.iter_mut().enumerate() {
                let lines = decoder.finish();
                let docker = &targets[index].docker;
                if !send(
                    reader,
                    session,
                    last_seen,
                    docker,
                    cutoffs[index],
                    &prefixes[index],
                    lines,
                )
                .await
                {
                    return;
                }
            }
            since = i64::try_from(now_ms() / 1000).unwrap_or(i64::MAX);
            // Compose can recreate the container under a new id, so look it up again.
            // A container that is gone, or whose state the provider cannot tell,
            // ends each reopened stream at once; the growing wait keeps that cheap.
            targets = loop {
                tokio::select! {
                    () = disconnected.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                delay = (delay * 2).min(RESUME_MAX);
                if let Ok(found) = self
                    .infra_log_targets(project, lane, &reader.container)
                    .await
                    && found
                        .iter()
                        .any(|target| container_runs(target.state.as_deref()))
                {
                    break found;
                }
            };
        }
    }
}

/// Sends `lines` of the container `docker` as `log` events, skipping those at
/// or before `cutoff`. False once the client is gone.
async fn send(
    reader: &mut Reader,
    session: &Session,
    last_seen: &mut HashMap<String, i128>,
    docker: &str,
    cutoff: Option<i128>,
    prefix: &str,
    lines: Vec<Decoded>,
) -> bool {
    for decoded in lines {
        if let Some(nanos) = decoded.nanos {
            if cutoff.is_some_and(|cutoff| nanos <= cutoff) {
                continue;
            }
            let last = last_seen.entry(docker.to_owned()).or_insert(nanos);
            *last = (*last).max(nanos);
        }
        let line = reader.line(prefix, decoded);
        if session.event(reader.event(&line)).await.is_err() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decodes `chunks` of one stdout stream, flushing at the end.
    fn decode(chunks: &[&[u8]]) -> Vec<Decoded> {
        let mut decoder = StreamDecoder::default();
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    #[test]
    fn timestamp_comes_off_the_line() {
        let (timestamp, rest) = split_timestamp(b"2024-05-01T10:00:00.123456789Z hello world\n");
        let timestamp = timestamp.expect("timestamp");
        assert_eq!(timestamp.as_millisecond(), 1_714_557_600_123);
        assert_eq!(timestamp.as_nanosecond() % 1_000_000, 456_789);
        assert_eq!(rest, b"hello world\n");
    }

    #[test]
    fn empty_line_keeps_its_newline() {
        let (timestamp, rest) = split_timestamp(b"2024-05-01T10:00:00Z\n");
        assert!(timestamp.is_some());
        assert_eq!(rest, b"\n");
        let (timestamp, rest) = split_timestamp(b"2024-05-01T10:00:00Z \n");
        assert!(timestamp.is_some());
        assert_eq!(rest, b"\n");
    }

    #[test]
    fn line_without_a_timestamp_stays_whole() {
        assert_eq!(
            split_timestamp(b"plain text\n"),
            (None, &b"plain text\n"[..])
        );
        assert_eq!(split_timestamp(b"no-newline"), (None, &b"no-newline"[..]));
    }

    #[test]
    fn lines_spanning_chunks_are_joined_and_cleaned() {
        let lines = decode(&[
            b"2024-05-01T10:00:00Z \x1b[32minfo\x1b[0m ready",
            b"\n2024-05-01T10:00:01Z {\"level\":\"error\",\"msg\":\"x\"}\n2024-05-01T10:00:02Z tail",
        ]);
        let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
        assert_eq!(
            texts,
            ["info ready", "{\"level\":\"error\",\"msg\":\"x\"}", "tail"]
        );
        assert_eq!(
            lines[0].styled.as_deref(),
            Some("\x1b[32minfo\x1b[0m ready\x1b[0m")
        );
        assert_eq!(lines[1].level.as_deref(), Some("error"));
        assert_eq!(lines[1].timestamp_ms, 1_714_557_601_000);
        assert!(lines[0].nanos < lines[1].nanos);
    }

    #[test]
    fn replicas_get_a_prefix_only_when_there_are_several() {
        assert_eq!(replica_prefixes("db", 1), [""]);
        assert_eq!(replica_prefixes("db", 2), ["db-1 | ", "db-2 | "]);
        assert!(replica_prefixes("db", 0).is_empty());
    }

    #[test]
    fn replicas_sort_by_their_number() {
        let container = |docker: &str| InfraContainer {
            name: "db".into(),
            docker: docker.into(),
            shared: false,
            state: None,
        };
        let mut replicas = vec![
            container("acme-db-10"),
            container("acme-db-2"),
            container("acme-db-1"),
        ];
        sort_replicas(&mut replicas);
        let names: Vec<&str> = replicas.iter().map(|c| c.docker.as_str()).collect();
        assert_eq!(names, ["acme-db-1", "acme-db-2", "acme-db-10"]);
    }

    #[test]
    fn running_states_resume_a_follow() {
        assert!(container_runs(Some("running")));
        assert!(container_runs(Some("healthy")));
        assert!(container_runs(None));
        assert!(!container_runs(Some("exited 0")));
        assert!(!container_runs(Some("removed")));
    }

    #[test]
    fn since_rounds_down_to_the_second() {
        assert_eq!(unix_second(1_714_557_600_999_999_999), 1_714_557_600);
    }
}
