use std::fmt;

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NodeId(pub String);
impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl AsRef<str> for NodeId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserId(pub String);
impl From<&str> for UserId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for UserId {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl AsRef<str> for UserId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
// -- DID types

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Did(pub String);
impl From<&str> for Did {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for Did {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl AsRef<str> for Did {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for Did {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Did {
    pub fn method(&self) -> DidMethod {
        let s = self.as_ref();
        if let Some(rest) = s.strip_prefix("did:") {
            let method = rest.split(':').next().unwrap_or("");
            if !method.is_empty() {
                return DidMethod::from(method);
            }
        }
        DidMethod::Unknown(String::new())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DidMethod {
    Key,
    Web,
    Peer,
    Unknown(String),
}
impl DidMethod {
    pub fn as_str(&self) -> &str {
        match self {
            DidMethod::Key => "key",
            DidMethod::Web => "web",
            DidMethod::Peer => "peer",
            DidMethod::Unknown(s) => s.as_str(),
        }
    }
}
impl From<&str> for DidMethod {
    fn from(s: &str) -> Self {
        match s {
            "key" => Self::Key,
            "web" => Self::Web,
            "peer" => Self::Peer,
            other => Self::Unknown(other.to_string()),
        }
    }
}
impl From<String> for DidMethod {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}
impl AsRef<str> for DidMethod {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for DidMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DidUrl(pub String);
impl From<&str> for DidUrl {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for DidUrl {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl fmt::Display for DidUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl DidUrl {
    pub fn did(&self) -> Did {
        match self.0.split_once('#') {
            Some((d, _)) => Did(d.to_string()),
            None => Did(self.0.clone()),
        }
    }
    pub fn fragment(&self) -> Option<&str> {
        self.0.split_once('#').map(|(_, frag)| frag)
    }
}
