//! PostHog: discovering recordings (HogQL) and fetching their snapshots (`blob_v2`).

use crate::http::{Http, credential};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use spoiler_core::{
    artifact::{Header, Kind, RecordingArtifact, RecordingSource},
    recording::{DecodeError, Limits, Recording},
};

const TOKEN: &str = "POSTHOG_API_KEY";
/// The personal-API-key cap on blob keys per snapshot request.
const BLOB_KEYS_PER_REQUEST: usize = 20;
/// Snapshot requests per fetch, staying under PostHog's 60/minute snapshot throttle.
pub const MAX_SNAPSHOT_REQUESTS: usize = 59;
pub const MAX_PAGE_SIZE: usize = 1000;

/// Where to resume discovery: after the last recording returned, within the same window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub host: String,
    pub project: u64,
    pub since: String,
    pub until: String,
    pub start_time: String,
    pub session_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RecordingPage {
    #[serde(flatten)]
    pub header: Header,
    /// One object per recording, keyed by HogQL column.
    pub recordings: Vec<Map<String, Value>>,
    /// Absent when this page is the last in the window.
    pub next_cursor: Option<Cursor>,
}

pub struct Discovery<'a> {
    pub host: &'a str,
    pub project: u64,
    pub since: &'a str,
    pub until: &'a str,
    pub limit: usize,
}

fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Recordings that started at or after `since` and ended by `until`, oldest first.
///
/// Every product view counts: there is no host, route or activity filter. Keyset pagination
/// over (start time, id) keeps pages stable while newer recordings arrive. Recordings still
/// ingesting past `until` are excluded; choose a settled window and overlap successive syncs.
pub fn list(http: &Http, query: &Discovery<'_>, cursor: Option<&Cursor>) -> Result<RecordingPage> {
    ensure!(
        (1..=MAX_PAGE_SIZE).contains(&query.limit),
        "limit must be between 1 and {MAX_PAGE_SIZE}"
    );
    let after = match cursor {
        Some(cursor) => {
            let same_window = cursor.host == query.host
                && cursor.project == query.project
                && cursor.since == query.since
                && cursor.until == query.until;
            ensure!(
                same_window,
                "cursor belongs to a different discovery window"
            );
            let start = sql_string(&cursor.start_time);
            format!(
                " AND (start_time > parseDateTimeBestEffort({start}) OR (start_time = parseDateTimeBestEffort({start}) AND session_id > {}))",
                sql_string(&cursor.session_id)
            )
        }
        None => String::new(),
    };
    let hogql = format!(
        "SELECT session_id, any(distinct_id) AS distinct_id, \
         min(min_first_timestamp) AS start_time, max(max_last_timestamp) AS end_time, \
         sum(active_milliseconds) / 1000 AS active_s, sum(click_count) AS clicks, \
         sum(keypress_count) AS keypresses, sum(console_error_count) AS console_errors, \
         argMinMerge(first_url) AS first_url, sum(size) AS bytes \
         FROM raw_session_replay_events \
         GROUP BY session_id \
         HAVING start_time >= parseDateTimeBestEffort({since}) \
         AND end_time <= parseDateTimeBestEffort({until}){after} \
         ORDER BY start_time ASC, session_id ASC \
         LIMIT {limit}",
        since = sql_string(query.since),
        until = sql_string(query.until),
        // One extra row tells whether another page exists.
        limit = query.limit + 1,
    );
    let url = format!(
        "{}/api/projects/{}/query/",
        query.host.trim_end_matches('/'),
        query.project
    );
    let body = json!({ "query": { "kind": "HogQLQuery", "query": hogql } });
    let response: Value =
        serde_json::from_str(&http.post_json(&url, &credential(TOKEN)?, &body)?)
            .context("query response is not JSON")?;

    let mut recordings = rows(&response)?;
    let has_more = recordings.len() > query.limit;
    recordings.truncate(query.limit);
    let next_cursor = match recordings.last() {
        Some(last) if has_more => Some(Cursor {
            host: query.host.to_owned(),
            project: query.project,
            since: query.since.to_owned(),
            until: query.until.to_owned(),
            start_time: last["start_time"]
                .as_str()
                .context("row has no start_time")?
                .to_owned(),
            session_id: last["session_id"]
                .as_str()
                .context("row has no session_id")?
                .to_owned(),
        }),
        _ => None,
    };
    Ok(RecordingPage {
        header: Header::new(Kind::RecordingPage),
        recordings,
        next_cursor,
    })
}

/// HogQL results as one object per row.
fn rows(response: &Value) -> Result<Vec<Map<String, Value>>> {
    let columns: Vec<&str> = response["columns"]
        .as_array()
        .context("query response has no columns")?
        .iter()
        .map(|c| c.as_str().context("query column is not a string"))
        .collect::<Result<_>>()?;
    let results = response["results"]
        .as_array()
        .context("query response has no results")?;
    results
        .iter()
        .map(|row| {
            let cells = row.as_array().context("query row is not an array")?;
            ensure!(
                cells.len() == columns.len(),
                "query row width does not match its columns"
            );
            Ok(columns
                .iter()
                .map(|c| (*c).to_owned())
                .zip(cells.iter().cloned())
                .collect())
        })
        .collect()
}

pub struct Snapshot<'a> {
    pub host: &'a str,
    pub project: u64,
    pub session: &'a str,
    pub max_requests: usize,
    pub limits: Limits,
}

/// Download a recording. Fails before downloading bodies if it needs more than `max_requests`.
pub fn fetch(http: &Http, snapshot: &Snapshot<'_>) -> Result<RecordingArtifact> {
    ensure!(
        (1..=MAX_SNAPSHOT_REQUESTS).contains(&snapshot.max_requests),
        "max requests must be between 1 and {MAX_SNAPSHOT_REQUESTS}"
    );
    let valid_id = !snapshot.session.is_empty()
        && snapshot
            .session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ensure!(valid_id, "invalid recording id");
    let token = credential(TOKEN)?;
    let base = format!(
        "{}/api/environments/{}/session_recordings/{}/snapshots",
        snapshot.host.trim_end_matches('/'),
        snapshot.project,
        snapshot.session
    );

    let listing: Value = serde_json::from_str(&http.get(&base, &token)?)
        .context("snapshot source listing is not JSON")?;
    let mut blob_keys = Vec::new();
    for source in listing["sources"]
        .as_array()
        .context("no snapshot sources")?
    {
        ensure!(
            source["source"] == "blob_v2",
            "unsupported snapshot source {}",
            source["source"]
        );
        let key = source["blob_key"]
            .as_str()
            .context("snapshot source has no blob key")?;
        blob_keys.push(key.to_owned());
    }
    let requests = blob_keys.len().div_ceil(BLOB_KEYS_PER_REQUEST);
    ensure!(
        requests <= snapshot.max_requests,
        "recording needs {requests} snapshot requests; --max-requests allows {}",
        snapshot.max_requests
    );

    let mut bodies = Vec::with_capacity(requests);
    let mut downloaded = 0_u64;
    for chunk in blob_keys.chunks(BLOB_KEYS_PER_REQUEST) {
        let (Some(first), Some(last)) = (chunk.first(), chunk.last()) else {
            continue;
        };
        let mut url = url::Url::parse(&base)?;
        url.query_pairs_mut()
            .append_pair("source", "blob_v2")
            .append_pair("start_blob_key", first)
            .append_pair("end_blob_key", last);
        let separator = u64::from(!bodies.is_empty());
        let remaining = snapshot
            .limits
            .max_bytes
            .checked_sub(downloaded)
            .and_then(|bytes| bytes.checked_sub(separator))
            .ok_or(DecodeError::TooLarge {
                max_bytes: snapshot.limits.max_bytes,
            })?;
        let body = http.get_limited(url.as_str(), &token, remaining)?;
        downloaded += separator + body.len() as u64;
        bodies.push(body);
    }
    let events = Recording::from_snapshot_bodies(&bodies, snapshot.limits)?;
    Ok(RecordingArtifact::new(
        RecordingSource::Posthog {
            host: snapshot.host.to_owned(),
            project: snapshot.project,
            session_id: snapshot.session.to_owned(),
            blob_keys,
        },
        events,
    ))
}
