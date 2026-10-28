#![allow(clippy::arithmetic_side_effects)]

use ironrdp::pdu::geometry::{InclusiveRectangle, Rectangle as _};
use ironrdp::session::image::DecodedImage;
use ironrdp_core::WriteBuf;

/// Copies the dirty `region` into `buffer` from its current cursor (clear it between regions).
/// The returned rect may be wider than `region`: the whole-rows path widens to full image width.
pub(crate) fn extract_partial_image(
    image: &DecodedImage,
    region: InclusiveRectangle,
    buffer: &mut WriteBuf,
) -> InclusiveRectangle {
    // PERF: needs actual benchmark to find a better heuristic
    if region.height() > 64 || region.width() > 512 {
        extract_whole_rows(image, region, buffer)
    } else {
        extract_smallest_rectangle(image, region, buffer)
    }
}

// Faster for low-height and smaller images
fn extract_smallest_rectangle(
    image: &DecodedImage,
    region: InclusiveRectangle,
    buffer: &mut WriteBuf,
) -> InclusiveRectangle {
    let pixel_size = usize::from(image.pixel_format().bytes_per_pixel());

    let image_width = usize::from(image.width());
    let image_stride = image_width * pixel_size;

    let region_top = usize::from(region.top);
    let region_left = usize::from(region.left);
    let region_width = usize::from(region.width());
    let region_height = usize::from(region.height());
    let region_stride = region_width * pixel_size;

    let dst_buf_size = region_width * region_height * pixel_size;
    let dst = buffer.unfilled_to(dst_buf_size);

    let src = image.data();

    for row in 0..region_height {
        let src_begin = image_stride * (region_top + row) + region_left * pixel_size;
        let src_end = src_begin + region_stride;
        let src_slice = &src[src_begin..src_end];

        let target_begin = region_stride * row;
        let target_end = target_begin + region_stride;
        let target_slice = &mut dst[target_begin..target_end];

        target_slice.copy_from_slice(src_slice);
    }

    buffer.advance(dst_buf_size);

    region
}

// Faster for high-height and bigger images
fn extract_whole_rows(image: &DecodedImage, region: InclusiveRectangle, buffer: &mut WriteBuf) -> InclusiveRectangle {
    let pixel_size = usize::from(image.pixel_format().bytes_per_pixel());

    let image_width = usize::from(image.width());
    let image_stride = image_width * pixel_size;

    let region_top = usize::from(region.top);
    let region_bottom = usize::from(region.bottom);

    let src = image.data();

    let src_begin = region_top * image_stride;
    let src_end = (region_bottom + 1) * image_stride;
    let len = src_end - src_begin;

    buffer.unfilled_to(len).copy_from_slice(&src[src_begin..src_end]);
    buffer.advance(len);

    InclusiveRectangle {
        left: 0,
        top: region.top,
        right: image.width() - 1,
        bottom: region.bottom,
    }
}

/// Clips `region` to a `width` x `height` surface; `None` when nothing of it is inside.
pub(crate) fn clip_region(region: &InclusiveRectangle, width: u16, height: u16) -> Option<InclusiveRectangle> {
    if width == 0
        || height == 0
        || region.left > region.right
        || region.top > region.bottom
        || region.left >= width
        || region.top >= height
    {
        return None;
    }

    Some(InclusiveRectangle {
        left: region.left,
        top: region.top,
        right: region.right.min(width - 1),
        bottom: region.bottom.min(height - 1),
    })
}

/// Copies exactly `region` of the framebuffer into `dst` (tightly packed rows, `dst.len()` must be
/// `region.width() * region.height() * 4`), forcing alpha opaque in the same pass. The framebuffer
/// isn't guaranteed opaque (zero-init columns, QOI-RGBA) and `put_image_data` stores alpha
/// verbatim. `region` must lie inside the framebuffer (see [`clip_region`]).
pub(crate) fn copy_region_opaque(image: &DecodedImage, region: &InclusiveRectangle, dst: &mut [u8]) {
    debug_assert_eq!(image.bytes_per_pixel(), 4);

    let stride = image.stride();
    let row_begin = usize::from(region.left) * 4;
    let row_len = usize::from(region.width()) * 4;
    debug_assert_eq!(dst.len(), row_len * usize::from(region.height()));

    let src = image.data();
    let src_rows = src[usize::from(region.top) * stride..].chunks(stride);

    for (dst_row, src_row) in dst.chunks_exact_mut(row_len).zip(src_rows) {
        dst_row.copy_from_slice(&src_row[row_begin..row_begin + row_len]);
        for pixel in dst_row.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }
    }
}

#[cfg(test)]
mod tests {
    use ironrdp::graphics::image_processing::PixelFormat;

    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> InclusiveRectangle {
        InclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn clip_region_bounds() {
        assert_eq!(clip_region(&rect(0, 0, 9, 9), 10, 10), Some(rect(0, 0, 9, 9)));
        assert_eq!(clip_region(&rect(5, 5, 20, 30), 10, 10), Some(rect(5, 5, 9, 9)));
        assert_eq!(clip_region(&rect(10, 0, 12, 3), 10, 10), None);
        assert_eq!(clip_region(&rect(0, 10, 3, 12), 10, 10), None);
        assert_eq!(clip_region(&rect(0, 0, 3, 3), 0, 10), None);
    }

    #[test]
    fn copy_region_opaque_packs_the_region_and_forces_alpha() {
        // Zero-initialized framebuffer: alpha is 0 everywhere.
        let image = DecodedImage::new(PixelFormat::RgbA32, 4, 3);
        let region = rect(1, 1, 2, 2);
        let mut dst = vec![0x11u8; 2 * 2 * 4];

        copy_region_opaque(&image, &region, &mut dst);

        for pixel in dst.chunks_exact(4) {
            assert_eq!(pixel, [0, 0, 0, 0xFF]);
        }
    }

    #[test]
    fn copy_region_opaque_matches_extract_smallest_rectangle() {
        let image = DecodedImage::new(PixelFormat::RgbA32, 64, 32);
        let region = rect(5, 7, 40, 30);
        let mut extracted = WriteBuf::new();
        extract_smallest_rectangle(&image, region.clone(), &mut extracted);
        let mut expected = extracted.filled().to_vec();
        for pixel in expected.chunks_exact_mut(4) {
            pixel[3] = 0xFF;
        }

        let mut dst = vec![0u8; expected.len()];
        copy_region_opaque(&image, &region, &mut dst);

        assert_eq!(dst, expected);
    }
}
