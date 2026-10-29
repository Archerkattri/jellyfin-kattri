use std::{error::Error, fmt, path::PathBuf};

#[derive(Debug)]
pub enum MpvError {
    LibraryNotFound {
        attempted: Vec<String>,
    },
    LibraryOpen {
        path: PathBuf,
        detail: String,
    },
    MissingSymbol {
        path: PathBuf,
        symbol: &'static str,
        detail: String,
    },
    UnsupportedClientApi {
        found: u64,
        required_major: u32,
        required_min_minor: u32,
    },
    CreateFailed,
    SetOptionFailed {
        option: &'static str,
        code: i32,
    },
    InitializeFailed(i32),
    WorkerUnavailable,
    /// The owner thread did not answer within the UI-safe deadline.
    WorkerTimeout,
    WorkerPanicked,
    EmptyCommand,
    CommandContainsNul,
    CommandQueueFull,
    WorkerQueueFull,
    ObservationIdExhausted,
    UnknownObservation(u64),
    ObservePropertyFailed {
        property: &'static str,
        code: i32,
    },
    UnobservePropertyFailed {
        observation_id: u64,
        code: i32,
    },
    RenderAlreadyAttached,
    UnsupportedBackend(&'static str),
    RenderContextCreate(i32),
    RenderCall(i32),
    InvalidFramebuffer,
    GlContextUnavailable(String),
}

impl fmt::Display for MpvError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibraryNotFound { attempted } => write!(
                formatter,
                "libmpv was not found (tried: {}). Install a matching mpv runtime or provide its library path",
                attempted.join(", ")
            ),
            Self::LibraryOpen { path, detail } => {
                write!(formatter, "could not load libmpv at {}: {detail}", path.display())
            }
            Self::MissingSymbol {
                path,
                symbol,
                detail,
            } => write!(
                formatter,
                "libmpv at {} is missing required symbol {symbol}: {detail}",
                path.display()
            ),
            Self::UnsupportedClientApi {
                found,
                required_major,
                required_min_minor,
            } => write!(
                formatter,
                "libmpv client API version {}.{} is unsupported; this adapter requires ABI major {required_major} with minor >= {required_min_minor}",
                (found >> 16) & 0xffff,
                found & 0xffff
            ),
            Self::CreateFailed => formatter.write_str("mpv_create returned a null handle"),
            Self::SetOptionFailed { option, code } => {
                write!(formatter, "could not set libmpv option {option}: error {code}")
            }
            Self::InitializeFailed(code) => {
                write!(formatter, "mpv_initialize failed with error {code}")
            }
            Self::WorkerUnavailable => formatter.write_str("libmpv owner thread is unavailable"),
            Self::WorkerTimeout => {
                formatter.write_str("libmpv owner thread did not answer in time")
            }
            Self::WorkerPanicked => formatter.write_str("libmpv owner thread panicked"),
            Self::EmptyCommand => formatter.write_str("an mpv command needs at least one argument"),
            Self::CommandContainsNul => {
                formatter.write_str("mpv command arguments cannot contain NUL bytes")
            }
            Self::CommandQueueFull => formatter.write_str("libmpv command queue is full"),
            Self::WorkerQueueFull => formatter.write_str("libmpv owner-thread queue is full"),
            Self::ObservationIdExhausted => {
                formatter.write_str("libmpv property observation IDs are exhausted")
            }
            Self::UnknownObservation(id) => {
                write!(formatter, "no libmpv property observation is registered as {id}")
            }
            Self::ObservePropertyFailed { property, code } => {
                write!(formatter, "could not observe libmpv property {property}: error {code}")
            }
            Self::UnobservePropertyFailed {
                observation_id,
                code,
            } => write!(
                formatter,
                "could not remove libmpv property observation {observation_id}: error {code}"
            ),
            Self::RenderAlreadyAttached => {
                formatter.write_str("libmpv allows one render context per core")
            }
            Self::UnsupportedBackend(backend) => {
                write!(formatter, "render backend {backend} is not supported by this proof")
            }
            Self::RenderContextCreate(code) => {
                write!(formatter, "mpv_render_context_create failed with error {code}")
            }
            Self::RenderCall(code) => {
                write!(formatter, "libmpv render call failed with error {code}")
            }
            Self::InvalidFramebuffer => formatter.write_str(
                "OpenGL framebuffer name and physical pixel dimensions must fit libmpv's FBO fields",
            ),
            Self::GlContextUnavailable(detail) => {
                write!(formatter, "could not make the host OpenGL context current: {detail}")
            }
        }
    }
}

impl Error for MpvError {}
