//! Small shared helpers: error type, context extension, time formatting.

use std::fmt;

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A plain error message, optionally wrapping a cause.
#[derive(Debug)]
pub struct Msg {
    pub msg: String,
    pub source: Option<Error>,
}

impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.source {
            Some(s) => write!(f, "{}: {}", self.msg, s),
            None => f.write_str(&self.msg),
        }
    }
}

impl std::error::Error for Msg {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as _)
    }
}

pub fn msg(m: impl Into<String>) -> Error {
    Box::new(Msg {
        msg: m.into(),
        source: None,
    })
}

#[macro_export]
macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::util::msg(format!($($t)*))) };
}

pub trait Context<T> {
    fn context(self, m: impl Into<String>) -> Result<T>;
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context(self, m: impl Into<String>) -> Result<T> {
        self.map_err(|e| {
            Box::new(Msg {
                msg: m.into(),
                source: Some(e.into()),
            }) as Error
        })
    }
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T> {
        self.map_err(|e| {
            Box::new(Msg {
                msg: f(),
                source: Some(e.into()),
            }) as Error
        })
    }
}

/// Walks an error chain looking for a specific error type.
pub fn find_cause<'a, T: std::error::Error + 'static>(
    e: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(c) = cur {
        if let Some(t) = c.downcast_ref::<T>() {
            return Some(t);
        }
        cur = c.source();
    }
    None
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn hex128(v: u128) -> String {
    format!("{v:032x}")
}

pub fn parse_hex128(s: &str) -> Result<u128> {
    u128::from_str_radix(s, 16).map_err(|_| msg(format!("invalid 128-bit hex value {s:?}")))
}

impl<T> Context<T> for Option<T> {
    fn context(self, m: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| msg(m))
    }
    fn with_context<F: FnOnce() -> String>(self, f: F) -> Result<T> {
        self.ok_or_else(|| msg(f()))
    }
}
