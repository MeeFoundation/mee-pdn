use std::fmt;
use std::io;

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Namespace(pub String);
impl From<&str> for Namespace {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for Namespace {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub String);
impl From<&str> for Key {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for Key {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Value(pub String);
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub trait KvStore {
    fn set(&self, ns: &Namespace, key: &Key, value: &Value) -> io::Result<()>;
    fn get(&self, ns: &Namespace, key: &Key) -> io::Result<Option<Value>>;
}
