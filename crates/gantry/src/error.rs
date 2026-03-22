use std::fmt;

#[derive(Debug)]
pub enum GantryError {
    Config(String),
    Validation(String),
    Docker(String),
    Operation(String),
    Timeout,
    Conflict(String),
    NotFound(String),
}

impl fmt::Display for GantryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config error: {msg}"),
            Self::Validation(msg) => write!(f, "validation error: {msg}"),
            Self::Docker(msg) => write!(f, "docker error: {msg}"),
            Self::Operation(msg) => write!(f, "operation error: {msg}"),
            Self::Timeout => write!(f, "operation timed out"),
            Self::Conflict(msg) => write!(f, "conflict: {msg}"),
            Self::NotFound(msg) => write!(f, "not found: {msg}"),
        }
    }
}

impl std::error::Error for GantryError {}

impl From<bollard::errors::Error> for GantryError {
    fn from(e: bollard::errors::Error) -> Self {
        Self::Docker(e.to_string())
    }
}

impl From<serde_yaml::Error> for GantryError {
    fn from(e: serde_yaml::Error) -> Self {
        Self::Config(e.to_string())
    }
}

impl From<serde_json::Error> for GantryError {
    fn from(e: serde_json::Error) -> Self {
        Self::Config(e.to_string())
    }
}

impl From<std::io::Error> for GantryError {
    fn from(e: std::io::Error) -> Self {
        Self::Operation(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, GantryError>;
