use glow::HasContext;
use parking_lot::{Mutex, MutexGuard};
use raw_window_handle::{HasRawWindowHandle, RawWindowHandle};

use std::{ffi::CStr, os::raw, ptr, sync::Arc, time::Duration};

/// The amount of time to wait while trying to obtain a lock to the adapter context
const CONTEXT_LOCK_TIMEOUT_SECS: u64 = 1;

const EGL_CONTEXT_FLAGS_KHR: i32 = 0x30FC;
const EGL_CONTEXT_OPENGL_DEBUG_BIT_KHR: i32 = 0x0001;
const EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT: i32 = 0x30BF;
const EGL_PLATFORM_WAYLAND_KHR: u32 = 0x31D8;
const EGL_PLATFORM_X11_KHR: u32 = 0x31D5;
const EGL_GL_COLORSPACE_KHR: u32 = 0x309D;
const EGL_GL_COLORSPACE_SRGB_KHR: u32 = 0x3089;

type XOpenDisplayFun =
    unsafe extern "system" fn(display_name: *const raw::c_char) -> *mut raw::c_void;

type WlDisplayConnectFun =
    unsafe extern "system" fn(display_name: *const raw::c_char) -> *mut raw::c_void;

type WlDisplayDisconnectFun = unsafe extern "system" fn(display: *const raw::c_void);

type WlEglWindowCreateFun = unsafe extern "system" fn(
    surface: *const raw::c_void,
    width: raw::c_int,
    height: raw::c_int,
) -> *mut raw::c_void;

type WlEglWindowResizeFun = unsafe extern "system" fn(
    window: *const raw::c_void,
    width: raw::c_int,
    height: raw::c_int,
    dx: raw::c_int,
    dy: raw::c_int,
);

type WlEglWindowDestroyFun = unsafe extern "system" fn(window: *const raw::c_void);

#[cfg(target_os = "android")]
extern "C" {
    pub fn ANativeWindow_setBuffersGeometry(
        window: *mut raw::c_void,
        width: i32,
        height: i32,
        format: i32,
    ) -> i32;
}

type EglLabel = *const raw::c_void;

#[allow(clippy::upper_case_acronyms)]
type EGLDEBUGPROCKHR = Option<
    unsafe extern "system" fn(
        error: egl::Enum,
        command: *const raw::c_char,
        message_type: u32,
        thread_label: EglLabel,
        object_label: EglLabel,
        message: *const raw::c_char,
    ),
>;

const EGL_DEBUG_MSG_CRITICAL_KHR: u32 = 0x33B9;
const EGL_DEBUG_MSG_ERROR_KHR: u32 = 0x33BA;
const EGL_DEBUG_MSG_WARN_KHR: u32 = 0x33BB;
const EGL_DEBUG_MSG_INFO_KHR: u32 = 0x33BC;

type EglDebugMessageControlFun =
    unsafe extern "system" fn(proc: EGLDEBUGPROCKHR, attrib_list: *const egl::Attrib) -> raw::c_int;

unsafe extern "system" fn egl_debug_proc(
    error: egl::Enum,
    command_raw: *const raw::c_char,
    message_type: u32,
    _thread_label: EglLabel,
    _object_label: EglLabel,
    message_raw: *const raw::c_char,
) {
    let log_severity = match message_type {
        EGL_DEBUG_MSG_CRITICAL_KHR | EGL_DEBUG_MSG_ERROR_KHR => log::Level::Error,
        EGL_DEBUG_MSG_WARN_KHR => log::Level::Warn,
        EGL_DEBUG_MSG_INFO_KHR => log::Level::Info,
        _ => log::Level::Debug,
    };
    let command = CStr::from_ptr(command_raw).to_string_lossy();
    let message = if message_raw.is_null() {
        "".into()
    } else {
        CStr::from_ptr(message_raw).to_string_lossy()
    };

    log::log!(
        log_severity,
        "EGL '{}' code 0x{:x}: {}",
        command,
        error,
        message,
    );
}

fn open_x_display() -> Option<(ptr::NonNull<raw::c_void>, libloading::Library)> {
    log::info!("Loading X11 library to get the current display");
    unsafe {
        let library = libloading::Library::new("libX11.so").ok()?;
        let func: libloading::Symbol<XOpenDisplayFun> = library.get(b"XOpenDisplay").unwrap();
        let result = func(ptr::null());
        ptr::NonNull::new(result).map(|ptr| (ptr, library))
    }
}

fn test_wayland_display() -> Option<libloading::Library> {
    /* We try to connect and disconnect here to simply ensure there
     * is an active wayland display available.
     */
    log::info!("Loading Wayland library to get the current display");
    let library = unsafe {
        let client_library = libloading::Library::new("libwayland-client.so").ok()?;
        let wl_display_connect: libloading::Symbol<WlDisplayConnectFun> =
            client_library.get(b"wl_display_connect").unwrap();
        let wl_display_disconnect: libloading::Symbol<WlDisplayDisconnectFun> =
            client_library.get(b"wl_display_disconnect").unwrap();
        let display = ptr::NonNull::new(wl_display_connect(ptr::null()))?;
        wl_display_disconnect(display.as_ptr());
        libloading::Library::new("libwayland-egl.so").ok()?
    };
    Some(library)
}

#[derive(Clone, Copy, Debug)]
enum SrgbFrameBufferKind {
    /// No support for SRGB surface
    None,
    /// Using EGL 1.5's support for colorspaces
    Core,
    /// Using EGL_KHR_gl_colorspace
    Khr,
}

/// Choose GLES framebuffer configuration.
fn choose_config(
    egl: &egl::DynamicInstance<egl::EGL1_4>,
    display: egl::Display,
    srgb_kind: SrgbFrameBufferKind,
) -> Result<(egl::Config, bool), crate::InstanceError> {
    //TODO: EGL_SLOW_CONFIG
    let tiers = [
        (
            "off-screen",
            &[egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT][..],
        ),
        ("presentation", &[egl::SURFACE_TYPE, egl::WINDOW_BIT]),
        #[cfg(not(target_os = "android"))]
        ("native-render", &[egl::NATIVE_RENDERABLE, egl::TRUE as _]),
    ];

    let mut attributes = Vec::with_capacity(9);
    for tier_max in (0..tiers.len()).rev() {
        let name = tiers[tier_max].0;
        log::info!("\tTrying {}", name);

        attributes.clear();
        for &(_, tier_attr) in tiers[..=tier_max].iter() {
            attributes.extend_from_slice(tier_attr);
        }
        // make sure the Alpha is enough to support sRGB
        match srgb_kind {
            SrgbFrameBufferKind::None => {}
            _ => {
                attributes.push(egl::ALPHA_SIZE);
                attributes.push(8);
            }
        }
        attributes.push(egl::NONE);

        match egl.choose_first_config(display, &attributes) {
            Ok(Some(config)) => {
                if tier_max == 1 {
                    log::warn!(
                        "EGL says it can present to the window but not natively. {}.",
                        "This has been confirmed to malfunction on Intel+NV laptops",
                    );
                }
                // Android emulator can't natively present either.
                let tier_threshold = if cfg!(target_os = "android") { 1 } else { 2 };
                return Ok((config, tier_max >= tier_threshold));
            }
            Ok(None) => {
                log::warn!("No config found!");
            }
            Err(e) => {
                log::error!("error in choose_first_config: {:?}", e);
            }
        }
    }

    Err(crate::InstanceError)
}

fn gl_debug_message_callback(source: u32, gltype: u32, id: u32, severity: u32, message: &str) {
    let source_str = match source {
        glow::DEBUG_SOURCE_API => "API",
        glow::DEBUG_SOURCE_WINDOW_SYSTEM => "Window System",
        glow::DEBUG_SOURCE_SHADER_COMPILER => "ShaderCompiler",
        glow::DEBUG_SOURCE_THIRD_PARTY => "Third Party",
        glow::DEBUG_SOURCE_APPLICATION => "Application",
        glow::DEBUG_SOURCE_OTHER => "Other",
        _ => unreachable!(),
    };

    let log_severity = match severity {
        glow::DEBUG_SEVERITY_HIGH => log::Level::Error,
        glow::DEBUG_SEVERITY_MEDIUM => log::Level::Warn,
        glow::DEBUG_SEVERITY_LOW => log::Level::Info,
        glow::DEBUG_SEVERITY_NOTIFICATION => log::Level::Trace,
        _ => unreachable!(),
    };

    let type_str = match gltype {
        glow::DEBUG_TYPE_DEPRECATED_BEHAVIOR => "Deprecated Behavior",
        glow::DEBUG_TYPE_ERROR => "Error",
        glow::DEBUG_TYPE_MARKER => "Marker",
        glow::DEBUG_TYPE_OTHER => "Other",
        glow::DEBUG_TYPE_PERFORMANCE => "Performance",
        glow::DEBUG_TYPE_POP_GROUP => "Pop Group",
        glow::DEBUG_TYPE_PORTABILITY => "Portability",
        glow::DEBUG_TYPE_PUSH_GROUP => "Push Group",
        glow::DEBUG_TYPE_UNDEFINED_BEHAVIOR => "Undefined Behavior",
        _ => unreachable!(),
    };

    log::log!(
        log_severity,
        "GLES: [{}/{}] ID {} : {}",
        source_str,
        type_str,
        id,
        message
    );

    if cfg!(debug_assertions) && log_severity == log::Level::Error {
        std::process::exit(1);
    }
}

/// A wrapper around a [`glow::Context`] and the required EGL context that uses locking to guarantee
/// exclusive access when shared with multiple threads.
pub struct AdapterContext {
    glow_context: Mutex<glow::Context>,
    egl: Arc<egl::DynamicInstance<egl::EGL1_4>>,
    egl_display: egl::Display,
    pub(super) egl_context: egl::Context,
    egl_pbuffer: Option<egl::Surface>,
}

unsafe impl Sync for AdapterContext {}
unsafe impl Send for AdapterContext {}

/// A guard containing a lock to an [`AdapterContext`]
pub struct AdapterContextLock<'a> {
    glow_context: MutexGuard<'a, glow::Context>,
    egl: &'a Arc<egl::DynamicInstance<egl::EGL1_4>>,
    egl_display: egl::Display,
}

impl<'a> std::ops::Deref for AdapterContextLock<'a> {
    type Target = glow::Context;

    fn deref(&self) -> &Self::Target {
        &self.glow_context
    }
}

impl<'a> Drop for AdapterContextLock<'a> {
    fn drop(&mut self) {
        // Make the EGL context *not* current on this thread
        self.egl
            .make_current(self.egl_display, None, None, None)
            .expect("Cannot make EGL context not current");
    }
}

impl AdapterContext {
    /// Get's the [`glow::Context`] without waiting for a lock
    ///
    /// # Safety
    ///
    /// This should only be called when you have manually made sure that the current thread has made
    /// the EGL context current and that no other thread also has the EGL context current.
    /// Additionally, you must manually make the EGL context **not** current after you are done with
    /// it, so that future calls to `lock()` will not fail.
    ///
    /// > **Note:** Calling this function **will** still lock the [`glow::Context`] which adds an
    /// > extra safe-guard against accidental concurrent access to the context.
    pub unsafe fn get_without_egl_lock(&self) -> MutexGuard<glow::Context> {
        self.glow_context
            .try_lock_for(Duration::from_secs(CONTEXT_LOCK_TIMEOUT_SECS))
            .expect("Could not lock adapter context. This is most-likely a deadlcok.")
    }

    /// Obtain a lock to the EGL context and get handle to the [`glow::Context`] that can be used to
    /// do rendering.
    #[track_caller]
    pub fn lock<'a>(&'a self) -> AdapterContextLock<'a> {
        let glow_context = self
            .glow_context
            // Don't lock forever. If it takes longer than 1 second to get the lock we've got a
            // deadlock and should panic to show where we got stuck
            .try_lock_for(Duration::from_secs(CONTEXT_LOCK_TIMEOUT_SECS))
            .expect("Could not lock adapter context. This is most-likely a deadlcok.");

        // Make the EGL context current on this thread
        self.egl
            .make_current(
                self.egl_display,
                self.egl_pbuffer,
                self.egl_pbuffer,
                Some(self.egl_context),
            )
            .expect("Cannot make EGL context current");

        AdapterContextLock {
            glow_context,
            egl: &self.egl,
            egl_display: self.egl_display,
        }
    }
}

#[derive(Debug)]
struct Inner {
    egl: Arc<egl::DynamicInstance<egl::EGL1_4>>,
    #[allow(unused)]
    version: (i32, i32),
    supports_native_window: bool,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    /// Dummy pbuffer (1x1).
    /// Required for `eglMakeCurrent` on platforms that doesn't supports `EGL_KHR_surfaceless_context`.
    pbuffer: Option<egl::Surface>,
    wl_display: Option<*mut raw::c_void>,
    /// Method by which the framebuffer should support srgb
    srgb_kind: SrgbFrameBufferKind,
}

impl Inner {
    fn create(
        flags: crate::InstanceFlags,
        egl: Arc<egl::DynamicInstance<egl::EGL1_4>>,
        display: egl::Display,
    ) -> Result<Self, crate::InstanceError> {
        let version = egl.initialize(display).map_err(|_| crate::InstanceError)?;
        let vendor = egl.query_string(Some(display), egl::VENDOR).unwrap();
        let display_extensions = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .unwrap()
            .to_string_lossy();
        log::info!("Display vendor {:?}, version {:?}", vendor, version,);
        log::debug!(
            "Display extensions: {:#?}",
            display_extensions.split_whitespace().collect::<Vec<_>>()
        );

        let srgb_kind = if version >= (1, 5) {
            log::info!("\tEGL surface: +srgb");
            SrgbFrameBufferKind::Core
        } else if display_extensions.contains("EGL_KHR_gl_colorspace") {
            log::info!("\tEGL surface: +srgb khr");
            SrgbFrameBufferKind::Khr
        } else {
            log::warn!("\tEGL surface: -srgb");
            SrgbFrameBufferKind::None
        };

        if log::max_level() >= log::LevelFilter::Trace {
            log::trace!("Configurations:");
            let config_count = egl.get_config_count(display).unwrap();
            let mut configurations = Vec::with_capacity(config_count);
            egl.get_configs(display, &mut configurations).unwrap();
            for &config in configurations.iter() {
                log::trace!("\tCONFORMANT=0x{:X}, RENDERABLE=0x{:X}, NATIVE_RENDERABLE=0x{:X}, SURFACE_TYPE=0x{:X}, ALPHA_SIZE={}",
                    egl.get_config_attrib(display, config, egl::CONFORMANT).unwrap(),
                    egl.get_config_attrib(display, config, egl::RENDERABLE_TYPE).unwrap(),
                    egl.get_config_attrib(display, config, egl::NATIVE_RENDERABLE).unwrap(),
                    egl.get_config_attrib(display, config, egl::SURFACE_TYPE).unwrap(),
                    egl.get_config_attrib(display, config, egl::ALPHA_SIZE).unwrap(),
                );
            }
        }

        let (config, supports_native_window) = choose_config(&egl, display, srgb_kind)?;
        egl.bind_api(egl::OPENGL_ES_API).unwrap();

        let needs_robustness = true;
        let mut khr_context_flags = 0;
        let supports_khr_context = display_extensions.contains("EGL_KHR_create_context");

        //TODO: make it so `Device` == EGL Context
        let mut context_attributes = vec![
            egl::CONTEXT_CLIENT_VERSION,
            3, // Request GLES 3.0 or higher
        ];
        if flags.contains(crate::InstanceFlags::DEBUG) {
            if version >= (1, 5) {
                log::info!("\tEGL context: +debug");
                context_attributes.push(egl::CONTEXT_OPENGL_DEBUG);
                context_attributes.push(egl::TRUE as _);
            } else if supports_khr_context {
                log::info!("\tEGL context: +debug KHR");
                khr_context_flags |= EGL_CONTEXT_OPENGL_DEBUG_BIT_KHR;
            } else {
                log::info!("\tEGL context: -debug");
            }
        }
        if needs_robustness {
            //Note: we specifically check the extension first, and then the core.
            // This is because the core version can fail if robustness is not supported
            // (regardless of whether the extension is supported!).
            // In fact, Angle does precisely that awful behavior.
            if display_extensions.contains("EGL_EXT_create_context_robustness") {
                log::info!("\tEGL context: +robust access EXT");
                context_attributes.push(EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT);
                context_attributes.push(egl::TRUE as _);
            } else if version >= (1, 5) {
                log::info!("\tEGL context: +robust access");
                context_attributes.push(egl::CONTEXT_OPENGL_ROBUST_ACCESS);
                context_attributes.push(egl::TRUE as _);
            } else {
                //Note: we aren't trying `EGL_CONTEXT_OPENGL_ROBUST_ACCESS_BIT_KHR`
                // because it's for desktop GL only, not GLES.
                log::warn!("\tEGL context: -robust access");
            }

            //TODO do we need `egl::CONTEXT_OPENGL_NOTIFICATION_STRATEGY_EXT`?
        }
        if khr_context_flags != 0 {
            context_attributes.push(EGL_CONTEXT_FLAGS_KHR);
            context_attributes.push(khr_context_flags);
        }
        context_attributes.push(egl::NONE);
        let context = match egl.create_context(display, config, None, &context_attributes) {
            Ok(context) => context,
            Err(e) => {
                log::warn!("unable to create GLES 3.x context: {:?}", e);
                return Err(crate::InstanceError);
            }
        };

        // Testing if context can be binded without surface
        // and creating dummy pbuffer surface if not.
        let pbuffer =
            if version >= (1, 5) || display_extensions.contains("EGL_KHR_surfaceless_context") {
                log::info!("\tEGL context: +surfaceless");
                None
            } else {
                let attributes = [egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE];
                egl.create_pbuffer_surface(display, config, &attributes)
                    .map(Some)
                    .map_err(|e| {
                        log::warn!("Error in create_pbuffer_surface: {:?}", e);
                        crate::InstanceError
                    })?
            };

        Ok(Self {
            egl,
            display,
            version,
            supports_native_window,
            config,
            context,
            pbuffer,
            wl_display: None,
            srgb_kind,
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Err(e) = self.egl.destroy_context(self.display, self.context) {
            log::warn!("Error in destroy_context: {:?}", e);
        }
        if let Err(e) = self.egl.terminate(self.display) {
            log::warn!("Error in terminate: {:?}", e);
        }
    }
}

pub struct Instance {
    wsi_library: Option<Arc<libloading::Library>>,
    flags: crate::InstanceFlags,
    inner: Mutex<Inner>,
}

unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

impl crate::Instance<super::Api> for Instance {
    unsafe fn init(desc: &crate::InstanceDescriptor) -> Result<Self, crate::InstanceError> {
        let egl = match egl::DynamicInstance::<egl::EGL1_4>::load_required() {
            Ok(egl) => Arc::new(egl),
            Err(e) => {
                log::warn!("Unable to open libEGL.so: {:?}", e);
                return Err(crate::InstanceError);
            }
        };

        let client_extensions = egl.query_string(None, egl::EXTENSIONS);

        let client_ext_str = match client_extensions {
            Ok(ext) => ext.to_string_lossy().into_owned(),
            Err(_) => String::new(),
        };
        log::debug!(
            "Client extensions: {:#?}",
            client_ext_str.split_whitespace().collect::<Vec<_>>()
        );

        let mut wsi_library = None;

        let wayland_library = if client_ext_str.contains(&"EGL_EXT_platform_wayland") {
            test_wayland_display()
        } else {
            None
        };

        let x11_display_library = if client_ext_str.contains(&"EGL_EXT_platform_x11") {
            open_x_display()
        } else {
            None
        };

        let display = if let (Some(library), Some(egl)) =
            (wayland_library, egl.upcast::<egl::EGL1_5>())
        {
            log::info!("Using Wayland platform");
            let display_attributes = [egl::ATTRIB_NONE];
            wsi_library = Some(Arc::new(library));
            egl.get_platform_display(
                EGL_PLATFORM_WAYLAND_KHR,
                egl::DEFAULT_DISPLAY,
                &display_attributes,
            )
            .unwrap()
        } else if let (Some((display, library)), Some(egl)) =
            (x11_display_library, egl.upcast::<egl::EGL1_5>())
        {
            log::info!("Using X11 platform");
            let display_attributes = [egl::ATTRIB_NONE];
            wsi_library = Some(Arc::new(library));
            egl.get_platform_display(EGL_PLATFORM_X11_KHR, display.as_ptr(), &display_attributes)
                .unwrap()
        } else {
            log::info!("Using default platform");
            egl.get_display(egl::DEFAULT_DISPLAY).unwrap()
        };

        if desc.flags.contains(crate::InstanceFlags::VALIDATION)
            && client_ext_str.contains(&"EGL_KHR_debug")
        {
            log::info!("Enabling EGL debug output");
            let function: EglDebugMessageControlFun =
                std::mem::transmute(egl.get_proc_address("eglDebugMessageControlKHR").unwrap());
            let attributes = [
                EGL_DEBUG_MSG_CRITICAL_KHR as egl::Attrib,
                1,
                EGL_DEBUG_MSG_ERROR_KHR as egl::Attrib,
                1,
                EGL_DEBUG_MSG_WARN_KHR as egl::Attrib,
                1,
                EGL_DEBUG_MSG_INFO_KHR as egl::Attrib,
                1,
                egl::ATTRIB_NONE,
            ];
            (function)(Some(egl_debug_proc), attributes.as_ptr());
        }

        let inner = Inner::create(desc.flags, egl, display)?;

        Ok(Instance {
            wsi_library,
            flags: desc.flags,
            inner: Mutex::new(inner),
        })
    }

    #[cfg_attr(target_os = "macos", allow(unused, unused_mut, unreachable_code))]
    unsafe fn create_surface(
        &self,
        has_handle: &impl HasRawWindowHandle,
    ) -> Result<Surface, crate::InstanceError> {
        use raw_window_handle::RawWindowHandle as Rwh;

        let raw_window_handle = has_handle.raw_window_handle();

        #[cfg_attr(target_os = "android", allow(unused_mut))]
        let mut inner = self.inner.lock();

        match raw_window_handle {
            Rwh::Xlib(_) => {}
            Rwh::Xcb(_) => {}
            #[cfg(target_os = "android")]
            Rwh::AndroidNdk(handle) => {
                let format = inner
                    .egl
                    .get_config_attrib(inner.display, inner.config, egl::NATIVE_VISUAL_ID)
                    .unwrap();

                let ret = ANativeWindow_setBuffersGeometry(handle.a_native_window, 0, 0, format);

                if ret != 0 {
                    log::error!("Error returned from ANativeWindow_setBuffersGeometry");
                    return Err(crate::InstanceError);
                }
            }
            Rwh::Wayland(handle) => {
                /* Wayland displays are not sharable between surfaces so if the
                 * surface we receive from this handle is from a different
                 * display, we must re-initialize the context.
                 *
                 * See gfx-rs/gfx#3545
                 */
                log::warn!("Re-initializing Gles context due to Wayland window");
                if inner
                    .wl_display
                    .map(|ptr| ptr != handle.display)
                    .unwrap_or(true)
                {
                    use std::ops::DerefMut;
                    let display_attributes = [egl::ATTRIB_NONE];
                    let display = inner
                        .egl
                        .upcast::<egl::EGL1_5>()
                        .unwrap()
                        .get_platform_display(
                            EGL_PLATFORM_WAYLAND_KHR,
                            handle.display,
                            &display_attributes,
                        )
                        .unwrap();

                    let new_inner = Inner::create(self.flags, inner.egl.clone(), display)
                        .map_err(|_| crate::InstanceError)?;

                    let old_inner = std::mem::replace(inner.deref_mut(), new_inner);
                    inner.wl_display = Some(handle.display);
                    drop(old_inner);
                }
            }
            other => {
                log::error!("Unsupported window: {:?}", other);
                return Err(crate::InstanceError);
            }
        };

        inner
            .egl
            .make_current(inner.display, None, None, None)
            .unwrap();

        Ok(Surface {
            egl: Arc::clone(&inner.egl),
            wsi_library: self.wsi_library.clone(),
            config: inner.config,
            display: inner.display,
            context: inner.context,
            presentable: inner.supports_native_window,
            pbuffer: inner.pbuffer,
            raw_window_handle,
            swapchain: None,
            srgb_kind: inner.srgb_kind,
        })
    }
    unsafe fn destroy_surface(&self, _surface: Surface) {}

    unsafe fn enumerate_adapters(&self) -> Vec<crate::ExposedAdapter<super::Api>> {
        let inner = self.inner.lock();
        inner
            .egl
            .make_current(
                inner.display,
                inner.pbuffer,
                inner.pbuffer,
                Some(inner.context),
            )
            .unwrap();

        let gl = glow::Context::from_loader_function(|name| {
            inner
                .egl
                .get_proc_address(name)
                .map_or(ptr::null(), |p| p as *const _)
        });

        if self.flags.contains(crate::InstanceFlags::DEBUG) && gl.supports_debug() {
            log::info!(
                "Max label length: {}",
                gl.get_parameter_i32(glow::MAX_LABEL_LENGTH)
            );
        }

        if self.flags.contains(crate::InstanceFlags::VALIDATION) && gl.supports_debug() {
            log::info!("Enabling GLES debug output");
            gl.enable(glow::DEBUG_OUTPUT);
            gl.debug_message_callback(gl_debug_message_callback);
        }

        inner
            .egl
            .make_current(inner.display, None, None, None)
            .unwrap();

        super::Adapter::expose(AdapterContext {
            glow_context: Mutex::new(gl),
            egl: inner.egl.clone(),
            egl_display: inner.display,
            egl_context: inner.context,
            egl_pbuffer: inner.pbuffer,
        })
        .into_iter()
        .collect()
    }
}

#[derive(Debug)]
pub struct Swapchain {
    surface: egl::Surface,
    wl_window: Option<*mut raw::c_void>,
    framebuffer: glow::Framebuffer,
    renderbuffer: glow::Renderbuffer,
    /// Extent because the window lies
    extent: wgt::Extent3d,
    format: wgt::TextureFormat,
    format_desc: super::TextureFormatDesc,
    #[allow(unused)]
    sample_type: wgt::TextureSampleType,
}

#[derive(Debug)]
pub struct Surface {
    egl: Arc<egl::DynamicInstance<egl::EGL1_4>>,
    wsi_library: Option<Arc<libloading::Library>>,
    config: egl::Config,
    display: egl::Display,
    context: egl::Context,
    #[allow(unused)]
    pbuffer: Option<egl::Surface>,
    pub(super) presentable: bool,
    raw_window_handle: RawWindowHandle,
    swapchain: Option<Swapchain>,
    srgb_kind: SrgbFrameBufferKind,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

impl Surface {
    pub(super) unsafe fn present(
        &mut self,
        _suf_texture: super::Texture,
        gl: &glow::Context,
    ) -> Result<(), crate::SurfaceError> {
        let sc = self.swapchain.as_ref().unwrap();

        self.egl
            .make_current(
                self.display,
                Some(sc.surface),
                Some(sc.surface),
                Some(self.context),
            )
            .map_err(|e| {
                log::error!("make_current(surface) failed: {}", e);
                crate::SurfaceError::Lost
            })?;

        gl.disable(glow::SCISSOR_TEST);
        gl.color_mask(true, true, true, true);

        gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(sc.framebuffer));
        // Note the Y-flipping here. GL's presentation is not flipped,
        // but main rendering is. Therefore, we Y-flip the output positions
        // in the shader, and also this blit.
        gl.blit_framebuffer(
            0,
            sc.extent.height as i32,
            sc.extent.width as i32,
            0,
            0,
            0,
            sc.extent.width as i32,
            sc.extent.height as i32,
            glow::COLOR_BUFFER_BIT,
            glow::NEAREST,
        );
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);

        self.egl
            .swap_buffers(self.display, sc.surface)
            .map_err(|e| {
                log::error!("swap_buffers failed: {}", e);
                crate::SurfaceError::Lost
            })?;
        self.egl
            .make_current(self.display, None, None, None)
            .map_err(|e| {
                log::error!("make_current(null) failed: {}", e);
                crate::SurfaceError::Lost
            })?;

        Ok(())
    }

    unsafe fn unconfigure_impl(
        &mut self,
        device: &super::Device,
    ) -> Option<(egl::Surface, Option<*mut raw::c_void>)> {
        let gl = &device.shared.context.lock();
        match self.swapchain.take() {
            Some(sc) => {
                gl.delete_renderbuffer(sc.renderbuffer);
                gl.delete_framebuffer(sc.framebuffer);
                Some((sc.surface, sc.wl_window))
            }
            None => None,
        }
    }

    pub fn supports_srgb(&self) -> bool {
        match self.srgb_kind {
            SrgbFrameBufferKind::None => false,
            _ => true,
        }
    }
}

impl crate::Surface<super::Api> for Surface {
    unsafe fn configure(
        &mut self,
        device: &super::Device,
        config: &crate::SurfaceConfiguration,
    ) -> Result<(), crate::SurfaceError> {
        use raw_window_handle::RawWindowHandle as Rwh;

        let (surface, wl_window) = match self.unconfigure_impl(device) {
            Some(pair) => pair,
            None => {
                let mut wl_window = None;
                let (mut temp_xlib_handle, mut temp_xcb_handle);
                #[allow(trivial_casts)]
                let native_window_ptr = match self.raw_window_handle {
                    Rwh::Xlib(handle) => {
                        temp_xlib_handle = handle.window;
                        &mut temp_xlib_handle as *mut _ as *mut std::ffi::c_void
                    }
                    Rwh::Xcb(handle) => {
                        temp_xcb_handle = handle.window;
                        &mut temp_xcb_handle as *mut _ as *mut std::ffi::c_void
                    }
                    Rwh::AndroidNdk(handle) => handle.a_native_window,
                    Rwh::Wayland(handle) => {
                        let library = self.wsi_library.as_ref().expect("unsupported window");
                        let wl_egl_window_create: libloading::Symbol<WlEglWindowCreateFun> =
                            library.get(b"wl_egl_window_create").unwrap();
                        let window = wl_egl_window_create(handle.surface, 640, 480) as *mut _
                            as *mut std::ffi::c_void;
                        let wl_egl_window_resize: libloading::Symbol<WlEglWindowResizeFun> =
                            library.get(b"wl_egl_window_resize").unwrap();
                        wl_egl_window_resize(
                            window,
                            config.extent.width as i32,
                            config.extent.height as i32,
                            0,
                            0,
                        );
                        wl_window = Some(window);
                        window
                    }
                    _ => unreachable!(),
                };

                let mut attributes = vec![
                    egl::RENDER_BUFFER,
                    if cfg!(target_os = "android") {
                        egl::BACK_BUFFER
                    } else {
                        egl::SINGLE_BUFFER
                    },
                ];
                match self.srgb_kind {
                    SrgbFrameBufferKind::None => {}
                    SrgbFrameBufferKind::Core => {
                        attributes.push(egl::GL_COLORSPACE);
                        attributes.push(egl::GL_COLORSPACE_SRGB);
                    }
                    SrgbFrameBufferKind::Khr => {
                        attributes.push(EGL_GL_COLORSPACE_KHR as i32);
                        attributes.push(EGL_GL_COLORSPACE_SRGB_KHR as i32);
                    }
                }
                attributes.push(egl::ATTRIB_NONE as i32);

                // Careful, we can still be in 1.4 version even if `upcast` succeeds
                let raw_result = if let Some(egl) = self.egl.upcast::<egl::EGL1_5>() {
                    let attributes_usize = attributes
                        .into_iter()
                        .map(|v| v as usize)
                        .collect::<Vec<_>>();
                    egl.create_platform_window_surface(
                        self.display,
                        self.config,
                        native_window_ptr,
                        &attributes_usize,
                    )
                } else {
                    self.egl.create_window_surface(
                        self.display,
                        self.config,
                        native_window_ptr,
                        Some(&attributes),
                    )
                };

                match raw_result {
                    Ok(raw) => (raw, wl_window),
                    Err(e) => {
                        log::warn!("Error in create_platform_window_surface: {:?}", e);
                        return Err(crate::SurfaceError::Lost);
                    }
                }
            }
        };

        let format_desc = device.shared.describe_texture_format(config.format);
        let gl = &device.shared.context.lock();
        let renderbuffer = gl.create_renderbuffer().unwrap();
        gl.bind_renderbuffer(glow::RENDERBUFFER, Some(renderbuffer));
        gl.renderbuffer_storage(
            glow::RENDERBUFFER,
            format_desc.internal,
            config.extent.width as _,
            config.extent.height as _,
        );
        let framebuffer = gl.create_framebuffer().unwrap();
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(framebuffer));
        gl.framebuffer_renderbuffer(
            glow::READ_FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::RENDERBUFFER,
            Some(renderbuffer),
        );
        gl.bind_renderbuffer(glow::RENDERBUFFER, None);
        gl.bind_framebuffer(glow::READ_FRAMEBUFFER, None);

        self.swapchain = Some(Swapchain {
            surface,
            wl_window,
            renderbuffer,
            framebuffer,
            extent: config.extent,
            format: config.format,
            format_desc,
            sample_type: wgt::TextureSampleType::Float { filterable: false },
        });

        Ok(())
    }

    unsafe fn unconfigure(&mut self, device: &super::Device) {
        if let Some((surface, wl_window)) = self.unconfigure_impl(device) {
            self.egl.destroy_surface(self.display, surface).unwrap();
            if let Some(window) = wl_window {
                let wl_egl_window_destroy: libloading::Symbol<WlEglWindowDestroyFun> = self
                    .wsi_library
                    .as_ref()
                    .expect("unsupported window")
                    .get(b"wl_egl_window_destroy")
                    .unwrap();
                wl_egl_window_destroy(window);
            }
        }
    }

    unsafe fn acquire_texture(
        &mut self,
        _timeout_ms: u32, //TODO
    ) -> Result<Option<crate::AcquiredSurfaceTexture<super::Api>>, crate::SurfaceError> {
        let sc = self.swapchain.as_ref().unwrap();
        let texture = super::Texture {
            inner: super::TextureInner::Renderbuffer {
                raw: sc.renderbuffer,
            },
            array_layer_count: 1,
            mip_level_count: 1,
            format: sc.format,
            format_desc: sc.format_desc.clone(),
            copy_size: crate::CopyExtent {
                width: sc.extent.width,
                height: sc.extent.height,
                depth: 1,
            },
        };
        Ok(Some(crate::AcquiredSurfaceTexture {
            texture,
            suboptimal: false,
        }))
    }
    unsafe fn discard_texture(&mut self, _texture: super::Texture) {}
}
