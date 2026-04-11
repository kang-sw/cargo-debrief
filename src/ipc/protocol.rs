//! Message types for client <-> daemon communication.
//!
//! Framing: 4-byte little-endian length prefix + JSON payload.

use std::io::{Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::service::{IndexResult, SearchResult};

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonRequest {
    Stop,
    Status,
    Index {
        path: PathBuf,
        include_deps: bool,
    },
    Search {
        query: String,
        top_k: usize,
        include_deps: bool,
    },
    Overview {
        file: PathBuf,
    },
    DepOverview {
        crate_name: String,
    },
    SetEmbeddingModel {
        model: String,
        global: bool,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonResponse {
    Ok {
        message: String,
    },
    Status {
        pid: u32,
        uptime_secs: u64,
    },
    IndexResult(IndexResult),
    SearchResults {
        results: Vec<SearchResult>,
    },
    Overview {
        content: String,
    },
    Error {
        message: String,
    },
    /// Keepalive sent by the daemon while a long operation is in progress.
    /// The client resets its per-message timeout on receipt and continues waiting.
    Progress {
        message: String,
    },
}

/// Write a length-prefixed JSON message to a stream.
pub fn write_message(writer: &mut impl Write, msg: &impl Serialize) -> Result<()> {
    let json = serde_json::to_vec(msg).context("failed to serialize message")?;
    let len = json.len() as u32;
    writer
        .write_all(&len.to_le_bytes())
        .context("failed to write message length")?;
    writer
        .write_all(&json)
        .context("failed to write message body")?;
    writer.flush().context("failed to flush stream")?;
    Ok(())
}

/// Read a length-prefixed JSON message from a stream.
pub fn read_message<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .context("failed to read message length")?;
    let len = u32::from_le_bytes(len_buf) as usize;

    if len > 16 * 1024 * 1024 {
        bail!("message too large: {len} bytes");
    }

    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("failed to read message body")?;
    serde_json::from_slice(&buf).context("failed to deserialize message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip<T: Serialize + DeserializeOwned + std::fmt::Debug>(msg: &T) -> T {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).unwrap();
        read_message(&mut Cursor::new(buf)).unwrap()
    }

    #[test]
    fn roundtrip_stop_request() {
        let got: DaemonRequest = roundtrip(&DaemonRequest::Stop);
        assert!(matches!(got, DaemonRequest::Stop));
    }

    #[test]
    fn roundtrip_status_request() {
        let got: DaemonRequest = roundtrip(&DaemonRequest::Status);
        assert!(matches!(got, DaemonRequest::Status));
    }

    #[test]
    fn roundtrip_search_request() {
        let req = DaemonRequest::Search {
            query: "foo bar".to_string(),
            top_k: 5,
            include_deps: true,
        };
        let got: DaemonRequest = roundtrip(&req);
        match got {
            DaemonRequest::Search {
                query,
                top_k,
                include_deps,
            } => {
                assert_eq!(query, "foo bar");
                assert_eq!(top_k, 5);
                assert!(include_deps);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_status_response() {
        let resp = DaemonResponse::Status {
            pid: 42,
            uptime_secs: 120,
        };
        let got: DaemonResponse = roundtrip(&resp);
        match got {
            DaemonResponse::Status { pid, uptime_secs } => {
                assert_eq!(pid, 42);
                assert_eq!(uptime_secs, 120);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_error_response() {
        let resp = DaemonResponse::Error {
            message: "something broke".to_string(),
        };
        let got: DaemonResponse = roundtrip(&resp);
        match got {
            DaemonResponse::Error { message } => {
                assert_eq!(message, "something broke");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_ok_response() {
        let resp = DaemonResponse::Ok {
            message: "stopping".to_string(),
        };
        let got: DaemonResponse = roundtrip(&resp);
        match got {
            DaemonResponse::Ok { message } => {
                assert_eq!(message, "stopping");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_index_request() {
        let req = DaemonRequest::Index {
            path: PathBuf::from("/some/path"),
            include_deps: false,
        };
        let got: DaemonRequest = roundtrip(&req);
        match got {
            DaemonRequest::Index { path, include_deps } => {
                assert_eq!(path, PathBuf::from("/some/path"));
                assert!(!include_deps);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_set_embedding_model_request() {
        let req = DaemonRequest::SetEmbeddingModel {
            model: "nomic-embed-text-v1.5".to_string(),
            global: true,
        };
        let got: DaemonRequest = roundtrip(&req);
        match got {
            DaemonRequest::SetEmbeddingModel { model, global } => {
                assert_eq!(model, "nomic-embed-text-v1.5");
                assert!(global);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn roundtrip_progress_response() {
        let resp = DaemonResponse::Progress {
            message: "working...".to_string(),
        };
        let got: DaemonResponse = roundtrip(&resp);
        match got {
            DaemonResponse::Progress { message } => {
                assert_eq!(message, "working...");
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn progress_then_final_response_sequence() {
        // Verify that a client can read Progress messages followed by a final response
        // from a single buffer, mimicking the keepalive loop behaviour.
        let mut buf = Vec::new();
        write_message(
            &mut buf,
            &DaemonResponse::Progress {
                message: "working...".to_string(),
            },
        )
        .unwrap();
        write_message(
            &mut buf,
            &DaemonResponse::Progress {
                message: "still working".to_string(),
            },
        )
        .unwrap();
        write_message(
            &mut buf,
            &DaemonResponse::Ok {
                message: "done".to_string(),
            },
        )
        .unwrap();

        let mut cursor = Cursor::new(buf);
        let mut progress_count = 0;
        loop {
            let msg: DaemonResponse = read_message(&mut cursor).unwrap();
            match msg {
                DaemonResponse::Progress { .. } => progress_count += 1,
                DaemonResponse::Ok { message } => {
                    assert_eq!(message, "done");
                    break;
                }
                _ => panic!("unexpected variant"),
            }
        }
        assert_eq!(progress_count, 2);
    }

    #[test]
    fn oversized_message_rejected() {
        // Craft a fake message with length > 16MB
        let mut buf = Vec::new();
        let len: u32 = 17 * 1024 * 1024;
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(&[0u8; 64]); // payload doesn't matter
        let err = read_message::<DaemonRequest>(&mut Cursor::new(buf)).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }
}
