use base64::{engine::general_purpose, Engine as _};
use mee_transport_api::{Message, ProfileName, Session, Ticket, Transport};
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Clone, Debug)]
pub struct HttpTransport {
    pub base_url: String,
}

impl HttpTransport {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

pub struct HttpSession {
    remote_inbox_url: String,
    _local: ProfileName,
}

#[derive(Serialize, Deserialize)]
struct HttpMessagePayload<'a> {
    from: &'a str,
    kind: &'a str,
    body_b64: String,
}

#[allow(async_fn_in_trait)]
impl Session for HttpSession {
    async fn send(&mut self, msg: &Message) -> io::Result<()> {
        let payload = HttpMessagePayload {
            from: msg.from.as_ref(),
            kind: msg.kind.as_str(),
            body_b64: general_purpose::STANDARD.encode(&msg.body),
        };

        let req = ureq::post(&self.remote_inbox_url)
            .set("content-type", "application/json")
            .send_json(serde_json::to_value(&payload).map_err(to_io_err)?);

        match req {
            Ok(resp) => {
                // Drain body to complete request lifecycle
                let _ = resp.into_string();
                Ok(())
            }
            Err(e) => Err(to_io_err(e)),
        }
    }

    async fn recv(&mut self) -> io::Result<Option<Message>> {
        // Not used for HTTP in this demo
        Ok(None)
    }
}

fn to_io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[allow(async_fn_in_trait)]
impl Transport for HttpTransport {
    type Sess = HttpSession;

    async fn ticket(&self, profile: &ProfileName) -> io::Result<Ticket> {
        Ok(Ticket(format!(
            "{}/profiles/{}/inbox",
            self.base_url.trim_end_matches('/'),
            profile
        )))
    }

    async fn open_session(&self, local: &ProfileName, remote: &Ticket) -> io::Result<Self::Sess> {
        Ok(HttpSession {
            remote_inbox_url: remote.as_ref().to_string(),
            _local: local.clone(),
        })
    }
}
