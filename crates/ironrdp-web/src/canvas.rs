use core::num::NonZeroU32;

#[cfg(target_arch = "wasm32")]
use anyhow::anyhow;
use ironrdp::pdu::geometry::{InclusiveRectangle, Rectangle as _};
use ironrdp::session::image::DecodedImage;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{Clamped, JsCast as _};
#[cfg(target_arch = "wasm32")]
use web_sys::ImageData;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, OffscreenCanvas, OffscreenCanvasRenderingContext2d};

use crate::image::{clip_region, copy_region_opaque};

/// What the session renders into: a main-thread `<canvas>` element, or an `OffscreenCanvas`
/// (typically transferred into a Web Worker with `canvas.transferControlToOffscreen()`).
#[derive(Clone)]
pub(crate) enum RenderTarget {
    Html(HtmlCanvasElement),
    Offscreen(OffscreenCanvas),
}

/// A render target paired with its matching 2D context.
enum Surface {
    Html {
        canvas: HtmlCanvasElement,
        ctx: CanvasRenderingContext2d,
    },
    Offscreen {
        canvas: OffscreenCanvas,
        ctx: OffscreenCanvasRenderingContext2d,
    },
}

/// Web render surface: blits each dirty region to the canvas with `put_image_data`.
pub(crate) struct Canvas {
    surface: Surface,
    /// Staging buffer of [`Canvas::paint`] (the batched `OffscreenCanvas` path), reused across
    /// frames. It grows to the largest region painted so far, at most the framebuffer size
    /// (4 bytes per pixel: about 59 MB at 5120x2880), and is never shrunk or reallocated otherwise.
    staging: Vec<u8>,
}

impl Canvas {
    pub(crate) fn new(target: RenderTarget, width: NonZeroU32, height: NonZeroU32) -> anyhow::Result<Self> {
        let surface = match target {
            RenderTarget::Html(render_canvas) => {
                render_canvas.set_width(width.get());
                render_canvas.set_height(height.get());
                let ctx = context_2d(&render_canvas)?;

                Surface::Html {
                    canvas: render_canvas,
                    ctx,
                }
            }
            RenderTarget::Offscreen(render_canvas) => {
                render_canvas.set_width(width.get());
                render_canvas.set_height(height.get());
                let ctx = offscreen_context_2d(&render_canvas)?;

                Surface::Offscreen {
                    canvas: render_canvas,
                    ctx,
                }
            }
        };

        Ok(Self {
            surface,
            staging: Vec::new(),
        })
    }

    /// Current backing-store size in pixels.
    pub(crate) fn size(&self) -> (u32, u32) {
        match &self.surface {
            Surface::Html { canvas, .. } => (canvas.width(), canvas.height()),
            Surface::Offscreen { canvas, .. } => (canvas.width(), canvas.height()),
        }
    }

    /// Resizes the backing store. Note: this also clears the canvas and resets 2D context state;
    /// the cached `ctx` stays valid.
    pub(crate) fn resize(&mut self, width: NonZeroU32, height: NonZeroU32) {
        match &self.surface {
            Surface::Html { canvas, .. } => {
                canvas.set_width(width.get());
                canvas.set_height(height.get());
            }
            Surface::Offscreen { canvas, .. } => {
                canvas.set_width(width.get());
                canvas.set_height(height.get());
            }
        }
    }

    /// Blits a dirty region with `put_image_data`. Forces alpha opaque first: the framebuffer isn't
    /// guaranteed opaque (zero-init columns, QOI-RGBA) and `put_image_data` stores alpha verbatim.
    pub(crate) fn draw(&self, buffer: &mut [u8], region: InclusiveRectangle) -> anyhow::Result<()> {
        for pixel in buffer.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }

        #[cfg(target_arch = "wasm32")]
        {
            let image = ImageData::new_with_u8_clamped_array_and_sh(
                Clamped(&*buffer),
                u32::from(region.width()),
                u32::from(region.height()),
            )
            .map_err(|err| anyhow!("ImageData::new failed: {err:?}"))?;
            let (left, top) = (f64::from(region.left), f64::from(region.top));
            match &self.surface {
                Surface::Html { ctx, .. } => ctx.put_image_data(&image, left, top),
                Surface::Offscreen { ctx, .. } => ctx.put_image_data(&image, left, top),
            }
            .map_err(|err| anyhow!("put_image_data failed: {err:?}"))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (&self.surface, buffer, region);
            unimplemented!("web canvas is only available on wasm32")
        }
    }

    /// Paints the dirty `regions` of the framebuffer `image`: each region is copied exactly into the
    /// reused staging buffer (alpha forced opaque in the same pass) and blitted with
    /// `put_image_data`. The `ImageData` is a view of the staging buffer in wasm memory, not a copy,
    /// so painting allocates nothing proportional to the frame or region size once the staging
    /// buffer has grown to the largest region.
    ///
    /// (A persistent full-frame `ImageData` blitted with the dirty-rectangle form of
    /// `putImageData` was measured slower and heavier in Chrome: about 3x the time per frame for
    /// scattered tiles, and about twice the renderer and GPU process memory at 5120x2880.)
    pub(crate) fn paint(&mut self, image: &DecodedImage, regions: &[InclusiveRectangle]) -> anyhow::Result<()> {
        anyhow::ensure!(
            image.bytes_per_pixel() == 4,
            "unsupported framebuffer pixel size: {}",
            image.bytes_per_pixel()
        );

        let (width, height) = (image.width(), image.height());

        for region in regions {
            let Some(region) = clip_region(region, width, height) else {
                continue;
            };

            let len = usize::from(region.width()) * usize::from(region.height()) * 4;
            if self.staging.len() < len {
                self.staging.resize(len, 0);
            }
            let pixels = &mut self.staging[..len];
            copy_region_opaque(image, &region, pixels);

            #[cfg(target_arch = "wasm32")]
            {
                let image_data = ImageData::new_with_u8_clamped_array_and_sh(
                    Clamped(&*pixels),
                    u32::from(region.width()),
                    u32::from(region.height()),
                )
                .map_err(|err| anyhow!("ImageData::new failed: {err:?}"))?;
                let (left, top) = (f64::from(region.left), f64::from(region.top));
                match &self.surface {
                    Surface::Html { ctx, .. } => ctx.put_image_data(&image_data, left, top),
                    Surface::Offscreen { ctx, .. } => ctx.put_image_data(&image_data, left, top),
                }
                .map_err(|err| anyhow!("put_image_data failed: {err:?}"))?;
            }
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = &self.surface;
            unimplemented!("web canvas is only available on wasm32")
        }
        #[cfg(target_arch = "wasm32")]
        Ok(())
    }
}

/// Acquires the canvas 2D context (wasm only; panics on other targets).
fn context_2d(canvas: &HtmlCanvasElement) -> anyhow::Result<CanvasRenderingContext2d> {
    #[cfg(target_arch = "wasm32")]
    {
        canvas
            .get_context("2d")
            .map_err(|err| anyhow!("get_context(\"2d\") failed: {err:?}"))?
            .ok_or_else(|| anyhow!("canvas has no 2d context"))?
            .dyn_into::<CanvasRenderingContext2d>()
            .map_err(|_| anyhow!("2d context is not a CanvasRenderingContext2d"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = canvas;
        unimplemented!("web canvas is only available on wasm32")
    }
}

/// Acquires the offscreen canvas 2D context (wasm only; panics on other targets).
///
/// An `OffscreenCanvas` hands out an `OffscreenCanvasRenderingContext2D`, a distinct interface from
/// `CanvasRenderingContext2D`, and the only 2D context type that exists in a Web Worker.
fn offscreen_context_2d(canvas: &OffscreenCanvas) -> anyhow::Result<OffscreenCanvasRenderingContext2d> {
    #[cfg(target_arch = "wasm32")]
    {
        canvas
            .get_context("2d")
            .map_err(|err| anyhow!("get_context(\"2d\") failed: {err:?}"))?
            .ok_or_else(|| anyhow!("offscreen canvas has no 2d context"))?
            .dyn_into::<OffscreenCanvasRenderingContext2d>()
            .map_err(|_| anyhow!("2d context is not an OffscreenCanvasRenderingContext2D"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = canvas;
        unimplemented!("web canvas is only available on wasm32")
    }
}
