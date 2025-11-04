// surfman/surfman/src/platform/egl/redox_surface.rs
//
//! Surface management for Redox using the `GraphicBuffer` class and EGL.

use super::super::context::Context;
use super::super::device::Device;
use super::{Surface, SurfaceTexture};
use crate::egl;
use crate::egl::types::{EGLSurface, EGLint};
use crate::gl;
use crate::gl_utils;
use crate::platform::generic;
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::ffi::EGL_EXTENSION_FUNCTIONS;
use crate::platform::generic::egl::ffi::EGL_IMAGE_PRESERVED_KHR;
use crate::platform::generic::egl::ffi::EGL_NO_IMAGE_KHR;
use crate::platform::generic::egl::ffi::{EGLImageKHR, EGL_NATIVE_BUFFER_REDOX};
use crate::renderbuffers::Renderbuffers;
use crate::{Error, SurfaceAccess, SurfaceID, SurfaceInfo, SurfaceType};

use euclid::default::Size2D;
use glow::{HasContext, Texture};
use orbclient::{Color, Renderer};
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::ptr::{self, addr_of};

const SURFACE_GL_TEXTURE_TARGET: u32 = crate::gl::TEXTURE_2D;

pub(crate) enum SurfaceObjects {
    HardwareBuffer {
        hardware_buffer: orbclient::Surface,
        egl_image: EGLImageKHR,
        framebuffer_object: Option<glow::Framebuffer>,
        texture_object: Option<Texture>,
        renderbuffers: Renderbuffers,
    },
    Window {
        egl_surface: EGLSurface,
    },
}

/// An Redox native window.
pub struct NativeWidget {
    pub(crate) native_window: *mut orbclient::Window,
}

impl Device {
    /// Creates either a generic or a widget surface, depending on the supplied surface type.
    ///
    /// Only the given context may ever render to the surface, but generic surfaces can be wrapped
    /// up in a `SurfaceTexture` for reading by other contexts.
    pub fn create_surface(
        &mut self,
        context: &Context,
        _: SurfaceAccess,
        surface_type: SurfaceType<NativeWidget>,
    ) -> Result<Surface, Error> {
        match surface_type {
            SurfaceType::Generic { size } => self.create_generic_surface(context, &size),
            SurfaceType::Widget { native_widget } => unsafe {
                self.create_window_surface(context, native_widget.native_window)
            },
        }
    }

    fn create_generic_surface(
        &mut self,
        context: &Context,
        size: &Size2D<i32>,
    ) -> Result<Surface, Error> {
        let _guard = self.temporarily_make_context_current(context)?;
        let gl = &context.gl;
        unsafe {
            // Create a native hardware buffer.
            let mut hardware_buffer =
                orbclient::Surface::new(size.width as u32, size.height as u32).unwrap();
            let hardware_buffer_ref: *mut [Color] = hardware_buffer.data_mut();
            // Create an EGL image, and bind it to a texture.
            let egl_image = self.create_egl_image(context, hardware_buffer_ref);

            // Initialize and bind the image to the texture.
            let texture_object = generic::egl::surface::bind_egl_image_to_gl_texture(gl, egl_image);

            // Create the framebuffer, and bind the texture to it.
            let framebuffer_object = gl_utils::create_and_bind_framebuffer(
                gl,
                SURFACE_GL_TEXTURE_TARGET,
                Some(texture_object),
            );

            // Bind renderbuffers as appropriate.
            let context_descriptor = self.context_descriptor(context);
            let context_attributes = self.context_descriptor_attributes(&context_descriptor);
            let renderbuffers = Renderbuffers::new(gl, size, &context_attributes);
            renderbuffers.bind_to_current_framebuffer(gl);

            debug_assert_eq!(
                gl.check_framebuffer_status(gl::FRAMEBUFFER),
                gl::FRAMEBUFFER_COMPLETE
            );

            Ok(Surface {
                size: *size,
                context_id: context.id,
                objects: SurfaceObjects::HardwareBuffer {
                    hardware_buffer,
                    egl_image,
                    framebuffer_object: Some(framebuffer_object),
                    texture_object: Some(texture_object),
                    renderbuffers,
                },
                destroyed: false,
            })
        }
    }

    unsafe fn create_window_surface(
        &mut self,
        context: &Context,
        native_window: *mut orbclient::Window,
    ) -> Result<Surface, Error> {
        let width = (*native_window).width() as i32;
        let height = (*native_window).height() as i32;

        EGL_FUNCTIONS.with(|egl| {
            let egl_surface = egl.CreateWindowSurface(
                self.egl_display,
                self.context_to_egl_config(context),
                addr_of!(native_window) as *const c_void,
                ptr::null(),
            );
            assert_ne!(egl_surface, egl::NO_SURFACE);

            Ok(Surface {
                context_id: context.id,
                size: Size2D::new(width, height),
                objects: SurfaceObjects::Window { egl_surface },
                destroyed: false,
            })
        })
    }

    /// Creates a surface texture from an existing generic surface for use with the given context.
    ///
    /// The surface texture is local to the supplied context and takes ownership of the surface.
    /// Destroying the surface texture allows you to retrieve the surface again.
    ///
    /// *The supplied context does not have to be the same context that the surface is associated
    /// with.* This allows you to render to a surface in one context and sample from that surface
    /// in another context.
    ///
    /// Calling this method on a widget surface returns a `WidgetAttached` error.
    pub fn create_surface_texture(
        &self,
        context: &mut Context,
        mut surface: Surface,
    ) -> Result<SurfaceTexture, (Error, Surface)> {
        unsafe {
            match &mut surface.objects {
                SurfaceObjects::Window { .. } => return Err((Error::WidgetAttached, surface)),
                SurfaceObjects::HardwareBuffer {
                    hardware_buffer, ..
                } => {
                    let _guard = match self.temporarily_make_context_current(context) {
                        Ok(guard) => guard,
                        Err(err) => return Err((err, surface)),
                    };
                    let gl = &context.gl;

                    let s = hardware_buffer.data_mut();
                    let local_egl_image = self.create_egl_image(context, s);
                    let texture_object =
                        generic::egl::surface::bind_egl_image_to_gl_texture(gl, local_egl_image);
                    Ok(SurfaceTexture {
                        surface,
                        local_egl_image,
                        texture_object: Some(texture_object),
                        phantom: PhantomData,
                    })
                }
            }
        }
    }

    /// Displays the contents of a widget surface on screen.
    ///
    /// Widget surfaces are internally double-buffered, so changes to them don't show up in their
    /// associated widgets until this method is called.
    ///
    /// The supplied context must match the context the surface was created with, or an
    /// `IncompatibleSurface` error is returned.
    pub fn present_surface(&self, context: &Context, surface: &mut Surface) -> Result<(), Error> {
        if context.id != surface.context_id {
            return Err(Error::IncompatibleSurface);
        }

        EGL_FUNCTIONS.with(|egl| unsafe {
            match surface.objects {
                SurfaceObjects::Window { egl_surface } => {
                    egl.SwapBuffers(self.egl_display, egl_surface);
                    Ok(())
                }
                SurfaceObjects::HardwareBuffer { .. } => Err(Error::NoWidgetAttached),
            }
        })
    }

    /// Resizes a widget surface.
    pub fn resize_surface(
        &self,
        _context: &Context,
        surface: &mut Surface,
        size: Size2D<i32>,
    ) -> Result<(), Error> {
        surface.size = size;
        Ok(())
    }

    #[allow(non_snake_case)]
    unsafe fn create_egl_image(&self, _: &Context, hardware_buffer: *mut [Color]) -> EGLImageKHR {
        // Get the native client buffer.
        let eglGetNativeClientBufferREDOX: extern "C" fn(*const std::ffi::c_void) -> *mut generic::egl::ffi::EGLClientBufferOpaque =
            EGL_EXTENSION_FUNCTIONS.GetNativeClientBufferREDOX.expect(
                "Where's the `EGL_REDOX_get_native_client_buffer` \
                                            extension?",
            );
        let client_buffer =
            eglGetNativeClientBufferREDOX(hardware_buffer as *const [Color] as *const _);
        assert!(!client_buffer.is_null());

        // Create the EGL image.
        let egl_image_attributes = [
            EGL_IMAGE_PRESERVED_KHR as EGLint,
            egl::TRUE as EGLint,
            egl::NONE as EGLint,
            0,
        ];
        let egl_image = (EGL_EXTENSION_FUNCTIONS.CreateImageKHR)(
            self.egl_display,
            egl::NO_CONTEXT,
            EGL_NATIVE_BUFFER_REDOX,
            client_buffer,
            egl_image_attributes.as_ptr(),
        );
        assert_ne!(egl_image, EGL_NO_IMAGE_KHR);
        egl_image
    }

    /// Destroys a surface.
    ///
    /// The supplied context must be the context the surface is associated with, or this returns
    /// an `IncompatibleSurface` error.
    ///
    /// You must explicitly call this method to dispose of a surface. Otherwise, a panic occurs in
    /// the `drop` method.
    pub fn destroy_surface(
        &self,
        context: &mut Context,
        surface: &mut Surface,
    ) -> Result<(), Error> {
        if context.id != surface.context_id {
            return Err(Error::IncompatibleSurface);
        }

        unsafe {
            match surface.objects {
                SurfaceObjects::HardwareBuffer {
                    ref mut hardware_buffer,
                    ref mut egl_image,
                    ref mut framebuffer_object,
                    ref mut texture_object,
                    ref mut renderbuffers,
                } => {
                    let gl = &context.gl;
                    gl.bind_framebuffer(gl::FRAMEBUFFER, None);
                    if let Some(framebuffer) = framebuffer_object.take() {
                        gl.delete_framebuffer(framebuffer);
                    }

                    renderbuffers.destroy(gl);

                    if let Some(texture) = texture_object.take() {
                        gl.delete_texture(texture);
                    }

                    let egl_display = self.egl_display;
                    let result = (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(egl_display, *egl_image);
                    assert_ne!(result, egl::FALSE);
                    *egl_image = EGL_NO_IMAGE_KHR;

                    drop(hardware_buffer);
                }
                SurfaceObjects::Window {
                    ref mut egl_surface,
                } => EGL_FUNCTIONS.with(|egl| {
                    egl.DestroySurface(self.egl_display, *egl_surface);
                    *egl_surface = egl::NO_SURFACE;
                }),
            }
        }

        surface.destroyed = true;
        Ok(())
    }

    /// Destroys a surface texture and returns the underlying surface.
    ///
    /// The supplied context must be the same context the surface texture was created with, or an
    /// `IncompatibleSurfaceTexture` error is returned.
    ///
    /// All surface textures must be explicitly destroyed with this function, or a panic will
    /// occur.
    pub fn destroy_surface_texture(
        &self,
        context: &mut Context,
        mut surface_texture: SurfaceTexture,
    ) -> Result<Surface, (Error, SurfaceTexture)> {
        let _guard = self.temporarily_make_context_current(context);
        let gl = &context.gl;
        unsafe {
            if let Some(texture) = surface_texture.texture_object.take() {
                gl.delete_texture(texture);
            }

            let egl_display = self.egl_display;
            let result = (EGL_EXTENSION_FUNCTIONS.DestroyImageKHR)(
                egl_display,
                surface_texture.local_egl_image,
            );
            assert_ne!(result, egl::FALSE);
            surface_texture.local_egl_image = EGL_NO_IMAGE_KHR;
        }

        Ok(surface_texture.surface)
    }

    /// Returns a pointer to the underlying surface data for reading or writing by the CPU.
    #[inline]
    pub fn lock_surface_data<'s>(&self, _: &'s mut Surface) -> Result<SurfaceDataGuard<'s>, Error> {
        // TODO(pcwalton)
        Err(Error::Unimplemented)
    }

    /// Returns the OpenGL texture target needed to read from this surface texture.
    ///
    /// This will be `GL_TEXTURE_2D` or `GL_TEXTURE_RECTANGLE`, depending on platform.
    #[inline]
    pub fn surface_gl_texture_target(&self) -> u32 {
        SURFACE_GL_TEXTURE_TARGET
    }

    /// Returns various information about the surface, including the framebuffer object needed to
    /// render to this surface.
    ///
    /// Before rendering to a surface attached to a context, you must call `glBindFramebuffer()`
    /// on the framebuffer object returned by this function. This framebuffer object may or not be
    /// 0, the default framebuffer, depending on platform.
    pub fn surface_info(&self, surface: &Surface) -> SurfaceInfo {
        SurfaceInfo {
            size: surface.size,
            id: surface.id(),
            context_id: surface.context_id,
            framebuffer_object: match surface.objects {
                SurfaceObjects::HardwareBuffer {
                    framebuffer_object, ..
                } => framebuffer_object,
                SurfaceObjects::Window { .. } => None,
            },
        }
    }

    /// Returns the OpenGL texture object containing the contents of this surface.
    ///
    /// It is only legal to read from, not write to, this texture object.
    #[inline]
    pub fn surface_texture_object(&self, surface_texture: &SurfaceTexture) -> Option<Texture> {
        surface_texture.texture_object
    }
}

impl NativeWidget {
    /// Creates a native widget type from an Redox `NativeWindow`.
    #[inline]
    pub unsafe fn from_native_window(native_window: *mut orbclient::Window) -> NativeWidget {
        NativeWidget { native_window }
    }
}

impl Surface {
    pub(super) fn id(&self) -> SurfaceID {
        match self.objects {
            SurfaceObjects::HardwareBuffer { egl_image, .. } => SurfaceID(egl_image as usize),
            SurfaceObjects::Window { egl_surface } => SurfaceID(egl_surface as usize),
        }
    }
}

/// Represents the CPU view of the pixel data of this surface.
pub struct SurfaceDataGuard<'a> {
    phantom: PhantomData<&'a ()>,
}
