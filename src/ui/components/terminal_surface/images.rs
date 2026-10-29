//! Bounded owner-window image textures and source-rectangle clipping.
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::Arc,
};

use crate::{
    core::frame::{ImageLayer, ImagePlacement, TerminalFrame, TerminalImage},
    ui::theme::TerminalMetrics,
};
use gpui::{Bounds, ContentMask, Pixels, Point, RenderImage, Window, point, px, size};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_PLACEMENTS: usize = 4096;

struct Texture {
    pixels: Arc<TerminalImage>,
    render: Arc<RenderImage>,
}

#[derive(Default)]
pub(super) struct ImageCache {
    textures: BTreeMap<(u32, u64), Texture>,
}

pub(super) struct PreparedImage {
    placement: ImagePlacement,
    texture: Arc<RenderImage>,
}

impl ImageCache {
    pub(super) fn clear(&mut self, window: &mut Window) {
        for (_, texture) in std::mem::take(&mut self.textures) {
            let _ = window.drop_image(texture.render);
        }
    }

    pub(super) fn prepare(
        &mut self,
        frame: &TerminalFrame,
        window: &mut Window,
        metrics: TerminalMetrics,
    ) -> Vec<PreparedImage> {
        let mut old = std::mem::take(&mut self.textures);
        let mut prepared = Vec::new();
        let mut bytes = 0_usize;
        let viewport = Bounds::new(
            point(px(0.), px(0.)),
            size(
                px(f32::from(frame.size.columns()) * metrics.cell_width),
                px(f32::from(frame.size.rows()) * metrics.cell_height),
            ),
        );
        for placement in frame.images.iter().take(MAX_PLACEMENTS) {
            let Some((_, clip)) = geometry(placement, viewport.origin, metrics) else {
                continue;
            };
            if !clip.intersects(&viewport) {
                continue;
            }
            let pixels = &placement.image;
            let key = (pixels.id, pixels.generation);
            if let Entry::Vacant(entry) = self.textures.entry(key) {
                let Some(length) = pixel_bytes(pixels.width, pixels.height) else {
                    continue;
                };
                if pixels.rgba.len() != length || bytes.saturating_add(length) > MAX_BYTES {
                    continue;
                }
                let cached = old.remove(&key);
                let texture = match cached {
                    Some(texture) if Arc::ptr_eq(&texture.pixels, pixels) => texture,
                    stale => {
                        if let Some(texture) = stale {
                            let _ = window.drop_image(texture.render);
                        }
                        let mut bgra = pixels.rgba.as_ref().clone();
                        for pixel in bgra.as_chunks_mut::<4>().0 {
                            pixel.swap(0, 2);
                        }
                        let Some(buffer) =
                            image::RgbaImage::from_raw(pixels.width, pixels.height, bgra)
                        else {
                            continue;
                        };
                        Texture {
                            pixels: Arc::clone(pixels),
                            render: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
                        }
                    }
                };
                bytes += length;
                entry.insert(texture);
            }
            if let Some(texture) = self.textures.get(&key) {
                prepared.push(PreparedImage {
                    placement: placement.clone(),
                    texture: Arc::clone(&texture.render),
                });
            }
        }
        for (_, texture) in old {
            let _ = window.drop_image(texture.render);
        }
        prepared.sort_by_key(|image| (image.placement.layer, image.placement.z));
        prepared
    }
}

fn pixel_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return None;
    }
    usize::try_from(width)
        .ok()?
        .checked_mul(usize::try_from(height).ok()?)?
        .checked_mul(4)
        .filter(|size| *size <= MAX_BYTES)
}

/// Placement clipping gives GPUI a source rectangle without duplicating pixels.
#[allow(
    clippy::cast_precision_loss,
    reason = "GPU geometry uses f32; integer pixel bounds are validated before conversion and offscreen placements are clipped."
)]
fn geometry(
    placement: &ImagePlacement,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
    let [source_x, source_y, source_width, source_height] = placement.source;
    if source_width == 0
        || source_height == 0
        || placement.cell_size.contains(&0)
        || placement.pixel_size.contains(&0)
        || source_x.checked_add(source_width)? > placement.image.width
        || source_y.checked_add(source_height)? > placement.image.height
    {
        return None;
    }
    let scale_x = metrics.cell_width / placement.cell_size[0] as f32;
    let scale_y = metrics.cell_height / placement.cell_size[1] as f32;
    let width = placement.pixel_size[0] as f32 * scale_x;
    let height = placement.pixel_size[1] as f32 * scale_y;
    let x =
        placement.column as f32 * metrics.cell_width + placement.cell_offset[0] as f32 * scale_x;
    let y = placement.row as f32 * metrics.cell_height + placement.cell_offset[1] as f32 * scale_y;
    let sample_x = width / source_width as f32;
    let sample_y = height / source_height as f32;
    let clip = Bounds::new(origin + point(px(x), px(y)), size(px(width), px(height)));
    let texture = Bounds::new(
        clip.origin
            - point(
                px(source_x as f32 * sample_x),
                px(source_y as f32 * sample_y),
            ),
        size(
            px(placement.image.width as f32 * sample_x),
            px(placement.image.height as f32 * sample_y),
        ),
    );
    Some((texture, clip))
}

pub(super) fn paint(
    images: &[PreparedImage],
    layer: ImageLayer,
    origin: Point<Pixels>,
    metrics: TerminalMetrics,
    window: &mut Window,
) {
    for image in images.iter().filter(|image| image.placement.layer == layer) {
        let Some((bounds, clip)) = geometry(&image.placement, origin, metrics) else {
            continue;
        };
        if !clip.intersects(&window.content_mask().bounds) {
            continue;
        }
        window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
            let _ = window.paint_image(
                bounds,
                gpui::Corners::default(),
                Arc::clone(&image.texture),
                0,
                false,
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;

    #[test]
    fn crop_uses_full_texture_with_scaled_clip_and_negative_rows() -> anyhow::Result<()> {
        let metrics = Theme::default().terminal;
        let placement = ImagePlacement {
            image: Arc::new(TerminalImage {
                id: 1,
                generation: 1,
                width: 40,
                height: 20,
                rgba: Arc::default(),
            }),
            layer: ImageLayer::AboveText,
            z: 0,
            column: 2,
            row: -1,
            cell_offset: [4, 2],
            pixel_size: [20, 10],
            source: [10, 5, 20, 10],
            cell_size: [10, 20],
        };
        let (texture, clip) = geometry(&placement, point(px(0.), px(0.)), metrics)
            .ok_or_else(|| anyhow::anyhow!("valid crop missing"))?;
        assert!((f32::from(clip.origin.x) - 2.4 * metrics.cell_width).abs() < 0.001);
        assert!((f32::from(clip.origin.y) + 0.9 * metrics.cell_height).abs() < 0.001);
        assert_eq!(texture.size.width, px(4. * metrics.cell_width));
        assert_eq!(texture.size.height, px(metrics.cell_height));
        assert!(texture.origin.x < clip.origin.x);
        assert!(texture.origin.y < clip.origin.y);
        let mut invalid = placement;
        invalid.source[0] = u32::MAX;
        assert!(geometry(&invalid, point(px(0.), px(0.)), metrics).is_none());
        invalid.source[0] = 10;
        invalid.cell_size[0] = 0;
        assert!(geometry(&invalid, point(px(0.), px(0.)), metrics).is_none());
        Ok(())
    }

    #[test]
    fn texture_allocations_are_bounded_before_copying() {
        assert_eq!(pixel_bytes(4096, 1024), Some(MAX_BYTES));
        assert_eq!(pixel_bytes(4096, 4096), None);
        assert_eq!(pixel_bytes(u32::MAX, u32::MAX), None);
        assert_eq!(pixel_bytes(0, 1), None);
    }
}
