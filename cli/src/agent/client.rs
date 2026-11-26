//! The Rust client of `agent.sock`, for `piton` running inside a session: sends one request with this process's token
//! and reads its answer. The launch helper reports pi's start with it; `piton config` and `piton sandbox` ask for the
//! session's facts with it.

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
};

use serde::{Serialize, de::DeserializeOwned};

/// Why a request got no answer of its kind.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("can't reach piton's daemon at {socket}: {source}")]
    Unreachable { socket: String, source: std::io::Error },
    #[error("piton's daemon refused: {message}")]
    Refused { message: String },
    #[error("piton's daemon sent an unreadable answer: {0}")]
    Unreadable(String),
}

/// What a request's line is: its kind's own answer, or the refusal every kind shares.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Line<T> {
    Refused(RefusedLine),
    Answer(T),
}

#[derive(serde::Deserialize)]
struct RefusedLine {
    #[serde(rename = "type")]
    kind: RefusedTag,
    message: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RefusedTag {
    Refused,
}

/// Sends `request` with `token` over `socket` and reads its one-line answer.
pub fn request<T: DeserializeOwned>(socket: &Path, token: &str, request: &impl Serialize) -> Result<T, ClientError> {
    let stream = UnixStream::connect(socket).map_err(|source| ClientError::Unreachable {
        socket: socket.display().to_string(),
        source,
    })?;
    exchange(stream, &socket.display().to_string(), token, request)
}

fn exchange<T: DeserializeOwned>(
    mut stream: UnixStream,
    socket: &str,
    token: &str,
    request: &impl Serialize,
) -> Result<T, ClientError> {
    let unreachable = |source| ClientError::Unreachable {
        socket: socket.to_string(),
        source,
    };
    let line = serde_json::json!({ "token": token, "request": request });
    writeln!(stream, "{line}").map_err(unreachable)?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer).map_err(unreachable)?;
    match serde_json::from_str::<Line<T>>(&answer) {
        Ok(Line::Refused(RefusedLine {
            kind: RefusedTag::Refused,
            message,
        })) => Err(ClientError::Refused { message }),
        Ok(Line::Answer(answer)) => Ok(answer),
        Err(_) => Err(ClientError::Unreadable(answer.trim().to_string())),
    }
}

/// The `launched` request: the helper is about to become pi.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename = "launched")]
pub struct Launched {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// What `launched` answers.
#[derive(Debug, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaunchedAnswer {
    Recorded,
    Ended,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub daemon on one end of a socket pair that answers one line, and what it was sent.
    fn answering(answer: &'static str) -> (UnixStream, std::thread::JoinHandle<String>) {
        let (client, daemon) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(&daemon).read_line(&mut line).unwrap();
            (&daemon).write_all(answer.as_bytes()).unwrap();
            line
        });
        (client, server)
    }

    #[test]
    fn sends_the_envelope_and_reads_the_answer() {
        let (stream, server) = answering("{\"type\":\"recorded\"}\n");
        let answer: LaunchedAnswer = exchange(
            stream,
            "s",
            "tok",
            &Launched {
                upstream: Some("http://u".into()),
            },
        )
        .unwrap();
        assert_eq!(answer, LaunchedAnswer::Recorded);
        let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(
            sent,
            serde_json::json!({"token": "tok", "request": {"type": "launched", "upstream": "http://u"}})
        );
    }

    /// `facts` is sent bare and read back as the session's view.
    #[test]
    fn asks_for_facts() {
        let (stream, server) = answering("{\"type\":\"ended\"}\n");
        let answer: super::super::facts::FactsAnswer =
            exchange(stream, "s", "tok", &super::super::facts::FactsRequest {}).unwrap();
        assert_eq!(answer, super::super::facts::FactsAnswer::Ended);
        let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(sent, serde_json::json!({"token": "tok", "request": {"type": "facts"}}));
    }

    #[test]
    fn a_refusal_is_an_error() {
        let (stream, server) = answering(
            "{\"type\":\"refused\",\"why\":\"token\",\"reason\":\"unknown_token\",\"message\":\"unknown or ended session\"}\n",
        );
        let error = exchange::<LaunchedAnswer>(stream, "s", "tok", &Launched { upstream: None }).unwrap_err();
        assert!(matches!(error, ClientError::Refused { ref message } if message == "unknown or ended session"));
        server.join().unwrap();
    }
}
