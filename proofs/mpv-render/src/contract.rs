use std::ffi::c_void;

/// libmpv render backend selection. The current upstream render API exposes
/// OpenGL and software; this proof implements only the OpenGL backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendSelection {
    OpenGlLibMpv,
    GpuNextLibMpv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfacePresentation {
    /// libmpv renders into the host's OpenGL FBO. This says nothing about how
    /// the decoder's input surface reaches libmpv's renderer.
    CallerOwnedOpenGlFbo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeSurfacePath {
    NotValidated,
    HardwareCopyBackToCpu,
    HardwareInteropCandidate,
    ZeroCopyVerified,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayOutputStatus {
    NotValidated,
    HdrMetadataObserved,
    HdrOutputVerified,
}

/// The claims made by this adapter are intentionally conservative. Hardware
/// decode, surface sharing, and HDR output require evidence from a real host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderContract {
    pub backend: BackendSelection,
    pub presentation: SurfacePresentation,
    pub decode_surface: DecodeSurfacePath,
    pub hdr_metadata: DisplayOutputStatus,
    pub hdr_output: DisplayOutputStatus,
}

impl RenderContract {
    pub const fn opengl_candidate() -> Self {
        Self {
            backend: BackendSelection::OpenGlLibMpv,
            presentation: SurfacePresentation::CallerOwnedOpenGlFbo,
            decode_surface: DecodeSurfacePath::NotValidated,
            hdr_metadata: DisplayOutputStatus::NotValidated,
            hdr_output: DisplayOutputStatus::NotValidated,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostPlatform {
    Windows,
    Linux,
    MacOs,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlImplementation {
    Angle,
    Wgl,
    Egl,
    Glx,
    Cgl,
    EguiFallbackEgl,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinuxGpuVendor {
    Intel,
    Nvidia,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeDisplayKind {
    X11,
    Wayland,
}

#[derive(Clone, Copy, Debug)]
pub enum NativeDisplay {
    X11(std::ptr::NonNull<c_void>),
    Wayland(std::ptr::NonNull<c_void>),
}

impl NativeDisplay {
    pub fn kind(self) -> NativeDisplayKind {
        match self {
            Self::X11(_) => NativeDisplayKind::X11,
            Self::Wayland(_) => NativeDisplayKind::Wayland,
        }
    }

    pub(crate) fn raw(self) -> *mut c_void {
        match self {
            Self::X11(display) | Self::Wayland(display) => display.as_ptr(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeInteropResult {
    Candidate,
    NotProven(&'static str),
}

/// Host-reported facts used to gate hardware interop claims. This is evidence
/// supplied by the platform integration; it is not a runtime GPU detector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeInteropEvidence {
    pub platform: HostPlatform,
    pub gl: GlImplementation,
    pub linux_vendor: Option<LinuxGpuVendor>,
    pub linux_display: Option<NativeDisplayKind>,
    pub gl_dx_interop_proven: bool,
    pub vdpau_enabled: bool,
    pub cgl_context_current: bool,
}

impl DecodeInteropEvidence {
    pub fn assess(self) -> DecodeInteropResult {
        match self.platform {
            HostPlatform::Windows if self.gl == GlImplementation::Angle => {
                DecodeInteropResult::Candidate
            }
            HostPlatform::Windows if self.gl_dx_interop_proven => DecodeInteropResult::Candidate,
            HostPlatform::Windows => DecodeInteropResult::NotProven(
                "Windows needs ANGLE or demonstrated GL/DX hardware-surface interop; generic EGL is not evidence",
            ),
            HostPlatform::Linux if self.linux_vendor == Some(LinuxGpuVendor::Intel) => {
                if self.gl != GlImplementation::Egl {
                    return DecodeInteropResult::NotProven(
                        "Intel/Linux hardware decode interop requires EGL",
                    );
                }
                if self.linux_display.is_none() {
                    return DecodeInteropResult::NotProven(
                        "Intel/Linux EGL interop requires the native X11 Display or Wayland wl_display handle",
                    );
                }
                DecodeInteropResult::Candidate
            }
            HostPlatform::Linux if self.linux_vendor == Some(LinuxGpuVendor::Nvidia) => {
                if self.vdpau_enabled && self.gl != GlImplementation::Glx {
                    return DecodeInteropResult::NotProven(
                        "NVIDIA VDPAU hardware decode requires the GLX path",
                    );
                }
                if !matches!(self.gl, GlImplementation::Glx | GlImplementation::Egl) {
                    return DecodeInteropResult::NotProven(
                        "NVIDIA/Linux hardware decode interop requires GLX or EGL",
                    );
                }
                DecodeInteropResult::Candidate
            }
            HostPlatform::Linux => DecodeInteropResult::NotProven(
                "Linux hardware interop must be validated for the actual GPU, GL API, and display system",
            ),
            HostPlatform::MacOs if self.gl == GlImplementation::Cgl && self.cgl_context_current => {
                DecodeInteropResult::Candidate
            }
            HostPlatform::MacOs => DecodeInteropResult::NotProven(
                "macOS hardware decode interop requires a current CGL context",
            ),
            HostPlatform::Other => DecodeInteropResult::NotProven(
                "hardware decode interop is not specified for this platform",
            ),
        }
    }
}
