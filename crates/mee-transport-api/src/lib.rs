use std::fmt;
use std::io;

// Newtypes for better type safety
#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProfileName(pub String);

impl From<&str> for ProfileName {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for ProfileName {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl AsRef<str> for ProfileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ticket(pub String);

impl From<&str> for Ticket {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for Ticket {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl AsRef<str> for Ticket {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MessageKind {
    Ping,
    Text,
    Caps,
    Unknown(String),
}
impl MessageKind {
    pub fn as_str(&self) -> &str {
        match self {
            MessageKind::Ping => "ping",
            MessageKind::Text => "text",
            MessageKind::Caps => "caps",
            MessageKind::Unknown(s) => s.as_str(),
        }
    }
}
impl From<&str> for MessageKind {
    fn from(s: &str) -> Self {
        match s {
            "ping" => MessageKind::Ping,
            "text" => MessageKind::Text,
            "caps" => MessageKind::Caps,
            other => MessageKind::Unknown(other.to_string()),
        }
    }
}
impl From<String> for MessageKind {
    fn from(s: String) -> Self {
        MessageKind::from(s.as_str())
    }
}
impl fmt::Display for MessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub from: ProfileName,
    pub kind: MessageKind,
    pub body: Vec<u8>,
}

#[allow(async_fn_in_trait)]
pub trait Session {
    async fn send(&mut self, msg: &Message) -> io::Result<()>;
    async fn recv(&mut self) -> io::Result<Option<Message>>;
}

#[allow(async_fn_in_trait)]
pub trait Transport {
    type Sess: Session;
    async fn ticket(&self, profile: &ProfileName) -> io::Result<Ticket>;
    async fn open_session(&self, local: &ProfileName, remote: &Ticket) -> io::Result<Self::Sess>;
}
