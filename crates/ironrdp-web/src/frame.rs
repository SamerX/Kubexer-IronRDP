//! Per-animation-frame paint batching for the `OffscreenCanvas` render target.
//!
//! Graphics updates arrive many times per display frame (each fast-path PDU yields one). Instead of
//! blitting each one immediately, [`FrameScheduler`] records the dirty rectangles and paints them
//! once per `requestAnimationFrame` tick, reading the pixels from the decoded framebuffer at that
//! point (the framebuffer is the source of truth, so painting later is still correct).

use futures_channel::mpsc;
use ironrdp::pdu::geometry::{InclusiveRectangle, Rectangle as _};
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::closure::Closure;

/// Above this many disjoint rectangles the pending set collapses into its bounding rectangle,
/// which keeps both memory and the per-flush call count bounded (e.g. while a hidden page
/// throttles animation frames). Painting the bounding rectangle allocates nothing: it is copied
/// into the canvas's persistent backbuffer like any other dirty rectangle.
const MAX_PENDING_RECTS: usize = 256;

fn area(rect: &InclusiveRectangle) -> u64 {
    u64::from(rect.width()) * u64::from(rect.height())
}

/// Two rectangles merge when their bounding rectangle paints no pixel that neither covers:
/// duplicates, containment and edge-adjacent strips or tiles merge, while a rectangle never
/// grows to swallow nearby ones (painting the wasted pixels costs more than a second call).
fn should_merge(a: &InclusiveRectangle, b: &InclusiveRectangle) -> bool {
    let overlap = a.intersect(b).as_ref().map_or(0, area);
    let covered = area(a).saturating_add(area(b)).saturating_sub(overlap);

    area(&a.union(b)) <= covered
}

/// Dirty rectangles accumulated since the last paint.
///
/// At most `MAX_PENDING_RECTS + 1` rectangles are held, and the storage is reused across frames.
#[derive(Debug, Default)]
pub(crate) struct DirtyRegions {
    rects: Vec<InclusiveRectangle>,
}

impl DirtyRegions {
    /// Records `rect`, merging it with pending rectangles it duplicates, overlaps or adjoins.
    pub(crate) fn add(&mut self, mut rect: InclusiveRectangle) {
        while let Some(index) = self.rects.iter().position(|pending| should_merge(pending, &rect)) {
            rect = self.rects.swap_remove(index).union(&rect);
        }

        self.rects.push(rect);

        if self.rects.len() > MAX_PENDING_RECTS {
            let bounds = InclusiveRectangle::union_all(&self.rects);
            self.rects.clear();
            self.rects.push(bounds);
        }
    }

    /// The rectangles to paint. They are painted as they are: a bounding rectangle would paint
    /// pixels that did not change.
    pub(crate) fn as_slice(&self) -> &[InclusiveRectangle] {
        &self.rects
    }

    /// Forgets the pending rectangles, keeping the storage for the next frame.
    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }
}

/// Coalesces graphics updates and wakes the session once per animation frame to paint them.
///
/// Dropping it cancels the pending animation frame callback.
pub(crate) struct FrameScheduler {
    dirty: DirtyRegions,
    /// The pending `requestAnimationFrame` handle.
    scheduled: Option<JsValue>,
    tick_tx: mpsc::UnboundedSender<()>,
    #[cfg(target_arch = "wasm32")]
    raf: Raf,
}

#[cfg(target_arch = "wasm32")]
struct Raf {
    global: JsValue,
    request: js_sys::Function,
    cancel: js_sys::Function,
    callback: Closure<dyn FnMut(f64)>,
}

impl FrameScheduler {
    /// Returns `None` when the global scope has no `requestAnimationFrame` (callers then paint
    /// every update immediately), along with the receiver that yields one item per animation frame.
    pub(crate) fn new() -> Option<(Self, mpsc::UnboundedReceiver<()>)> {
        #[cfg(target_arch = "wasm32")]
        {
            let global = JsValue::from(js_sys::global());
            let function = |name: &str| {
                js_sys::Reflect::get(&global, &JsValue::from_str(name))
                    .ok()?
                    .dyn_into::<js_sys::Function>()
                    .ok()
            };
            let request = function("requestAnimationFrame")?;
            let cancel = function("cancelAnimationFrame")?;

            let (tick_tx, tick_rx) = mpsc::unbounded();
            let callback_tx = tick_tx.clone();
            let callback = Closure::<dyn FnMut(f64)>::new(move |_timestamp: f64| {
                let _ = callback_tx.unbounded_send(());
            });

            Some((
                Self {
                    dirty: DirtyRegions::default(),
                    scheduled: None,
                    tick_tx,
                    raf: Raf {
                        global,
                        request,
                        cancel,
                        callback,
                    },
                },
                tick_rx,
            ))
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            None
        }
    }

    /// Records a dirty rectangle and makes sure an animation frame is requested.
    pub(crate) fn add(&mut self, rect: InclusiveRectangle) {
        self.dirty.add(rect);

        if self.scheduled.is_none() {
            self.scheduled = self.request_frame();
        }
    }

    /// Called when the animation frame fired: returns the rectangles to paint now (clear them
    /// once painted).
    pub(crate) fn on_frame(&mut self) -> &mut DirtyRegions {
        self.scheduled = None;
        &mut self.dirty
    }

    /// The pending rectangles, for an out-of-band paint (e.g. before a resize). The animation
    /// frame stays requested and finds nothing left to paint once they are cleared.
    pub(crate) fn pending(&mut self) -> &mut DirtyRegions {
        &mut self.dirty
    }

    fn request_frame(&self) -> Option<JsValue> {
        #[cfg(target_arch = "wasm32")]
        if let Ok(handle) = self.raf.request.call1(&self.raf.global, self.raf.callback.as_ref()) {
            return Some(handle);
        }

        // Without a frame callback nothing would ever paint: wake the session right away.
        let _ = self.tick_tx.unbounded_send(());
        Some(JsValue::UNDEFINED)
    }
}

impl Drop for FrameScheduler {
    fn drop(&mut self) {
        self.dirty.clear();

        #[cfg(target_arch = "wasm32")]
        if let Some(handle) = self.scheduled.take() {
            let _ = self.raf.cancel.call1(&self.raf.global, &handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> InclusiveRectangle {
        InclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    fn take(dirty: &mut DirtyRegions) -> Vec<InclusiveRectangle> {
        let rects = dirty.as_slice().to_vec();
        dirty.clear();
        rects
    }

    #[test]
    fn repeated_region_is_kept_once() {
        let mut dirty = DirtyRegions::default();
        for _ in 0..10 {
            dirty.add(rect(100, 100, 739, 459));
        }
        assert_eq!(take(&mut dirty), vec![rect(100, 100, 739, 459)]);
        assert!(take(&mut dirty).is_empty());
    }

    #[test]
    fn contained_region_is_absorbed() {
        let mut dirty = DirtyRegions::default();
        dirty.add(rect(0, 0, 99, 99));
        dirty.add(rect(10, 10, 19, 19));
        assert_eq!(take(&mut dirty), vec![rect(0, 0, 99, 99)]);
    }

    #[test]
    fn adjacent_tiles_merge() {
        let mut dirty = DirtyRegions::default();
        dirty.add(rect(0, 0, 63, 63));
        dirty.add(rect(64, 0, 127, 63));
        dirty.add(rect(0, 64, 127, 127));
        assert_eq!(take(&mut dirty), vec![rect(0, 0, 127, 127)]);
    }

    #[test]
    fn distant_tiles_stay_separate() {
        let mut dirty = DirtyRegions::default();
        dirty.add(rect(0, 0, 63, 63));
        dirty.add(rect(1000, 1000, 1063, 1063));
        let mut taken = take(&mut dirty);
        taken.sort_by_key(|r| r.left);
        assert_eq!(taken, vec![rect(0, 0, 63, 63), rect(1000, 1000, 1063, 1063)]);
    }

    #[test]
    fn large_region_does_not_swallow_nearby_tile() {
        let mut dirty = DirtyRegions::default();
        dirty.add(rect(0, 0, 999, 999));
        dirty.add(rect(1100, 0, 1163, 63));
        // Nor at paint time: the bounding rectangle would repaint unchanged pixels.
        assert_eq!(take(&mut dirty), vec![rect(0, 0, 999, 999), rect(1100, 0, 1163, 63)]);
    }

    #[test]
    fn diagonal_tiles_stay_separate() {
        let mut dirty = DirtyRegions::default();
        dirty.add(rect(0, 0, 63, 63));
        dirty.add(rect(64, 64, 127, 127));
        // Their bounding square is 2x their area.
        assert_eq!(take(&mut dirty).len(), 2);
    }

    #[test]
    fn storage_stays_bounded_and_is_reused() {
        let mut dirty = DirtyRegions::default();
        for frame in 0..4u16 {
            // Many more scattered tiles than the cap, as while frames are throttled.
            for i in 0..2000u16 {
                let x = (i % 200) * 128 + frame;
                let y = (i / 200) * 128;
                dirty.add(rect(x, y, x + 63, y + 63));
                assert!(dirty.as_slice().len() <= MAX_PENDING_RECTS);
            }
            let capacity = dirty.rects.capacity();
            dirty.clear();
            assert!(dirty.as_slice().is_empty());
            assert_eq!(dirty.rects.capacity(), capacity);
        }
        assert!(dirty.rects.capacity() <= 2 * (MAX_PENDING_RECTS + 1));
    }

    #[test]
    fn too_many_rects_collapse_to_bounds() {
        let mut dirty = DirtyRegions::default();
        for i in 0..=u16::try_from(MAX_PENDING_RECTS).unwrap() {
            let x = i * 128;
            dirty.add(rect(x, 0, x + 63, 63));
        }
        let taken = take(&mut dirty);
        assert_eq!(taken.len(), 1);
        assert_eq!(
            taken[0],
            rect(0, 0, 128 * u16::try_from(MAX_PENDING_RECTS).unwrap() + 63, 63)
        );
    }

    #[test]
    fn scheduler_requests_one_frame_until_it_fires() {
        // Native build: no requestAnimationFrame, so construction reports it unavailable.
        assert!(FrameScheduler::new().is_none());
    }
}
