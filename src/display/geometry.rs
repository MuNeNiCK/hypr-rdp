#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Size {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Size {
    pub(crate) fn new(width: u32, height: u32) -> Option<Self> {
        (width > 0 && height > 0).then_some(Self { width, height })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rect {
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl Rect {
    pub(crate) fn right(self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub(crate) fn bottom(self) -> u32 {
        self.y.saturating_add(self.height)
    }

    fn as_damage_tuple(self) -> Option<(i32, i32, i32, i32)> {
        Some((
            i32::try_from(self.x).ok()?,
            i32::try_from(self.y).ok()?,
            i32::try_from(self.width).ok()?,
            i32::try_from(self.height).ok()?,
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PresentationGeometry {
    source: Size,
    presentation: Size,
    visible: Rect,
}

impl PresentationGeometry {
    pub(crate) fn new(source: Size, presentation: Size) -> Self {
        let visible = Rect {
            x: 0,
            y: 0,
            width: presentation.width,
            height: presentation.height,
        };

        Self {
            source,
            presentation,
            visible,
        }
    }

    pub(crate) fn source(self) -> Size {
        self.source
    }

    pub(crate) fn presentation(self) -> Size {
        self.presentation
    }

    pub(crate) fn visible_rect(self) -> Rect {
        self.visible
    }

    pub(crate) fn is_identity(self) -> bool {
        self.source == self.presentation
            && self.visible.x == 0
            && self.visible.y == 0
            && self.visible.width == self.source.width
            && self.visible.height == self.source.height
    }

    pub(crate) fn map_source_damage(
        self,
        damage_regions: &[(i32, i32, i32, i32)],
    ) -> Vec<(i32, i32, i32, i32)> {
        damage_regions
            .iter()
            .filter_map(|&(x, y, w, h)| self.map_source_rect(x, y, w, h))
            .filter_map(Rect::as_damage_tuple)
            .collect()
    }

    pub(crate) fn map_source_rect(self, x: i32, y: i32, w: i32, h: i32) -> Option<Rect> {
        if w <= 0 || h <= 0 {
            return None;
        }

        let source_w = i64::from(self.source.width);
        let source_h = i64::from(self.source.height);
        let left = i64::from(x).clamp(0, source_w);
        let top = i64::from(y).clamp(0, source_h);
        let right = i64::from(x).saturating_add(i64::from(w)).clamp(0, source_w);
        let bottom = i64::from(y).saturating_add(i64::from(h)).clamp(0, source_h);
        if right <= left || bottom <= top {
            return None;
        }

        let visible = self.visible;
        let mapped_left = visible.x + scale_floor(left as u64, visible.width, self.source.width);
        let mapped_top = visible.y + scale_floor(top as u64, visible.height, self.source.height);
        let mapped_right = visible.x + scale_ceil(right as u64, visible.width, self.source.width);
        let mapped_bottom =
            visible.y + scale_ceil(bottom as u64, visible.height, self.source.height);

        let clamped_left = mapped_left.clamp(visible.x, visible.right());
        let clamped_top = mapped_top.clamp(visible.y, visible.bottom());
        let clamped_right = mapped_right.clamp(visible.x, visible.right());
        let clamped_bottom = mapped_bottom.clamp(visible.y, visible.bottom());

        if clamped_right <= clamped_left || clamped_bottom <= clamped_top {
            return None;
        }

        Some(Rect {
            x: clamped_left,
            y: clamped_top,
            width: clamped_right - clamped_left,
            height: clamped_bottom - clamped_top,
        })
    }

    pub(crate) fn map_presentation_point_to_source(self, x: u32, y: u32) -> (u32, u32) {
        let visible = self.visible;
        let visible_right = visible.right().saturating_sub(1);
        let visible_bottom = visible.bottom().saturating_sub(1);
        let clamped_x = x.clamp(visible.x, visible_right);
        let clamped_y = y.clamp(visible.y, visible_bottom);
        let rel_x = clamped_x - visible.x;
        let rel_y = clamped_y - visible.y;

        let source_x = if clamped_x == visible_right {
            self.source.width.saturating_sub(1)
        } else {
            scale_floor(rel_x.into(), self.source.width, visible.width)
                .min(self.source.width.saturating_sub(1))
        };
        let source_y = if clamped_y == visible_bottom {
            self.source.height.saturating_sub(1)
        } else {
            scale_floor(rel_y.into(), self.source.height, visible.height)
                .min(self.source.height.saturating_sub(1))
        };

        (source_x, source_y)
    }
}

fn scale_floor(value: u64, numerator: u32, denominator: u32) -> u32 {
    ((value * u64::from(numerator)) / u64::from(denominator)) as u32
}

fn scale_ceil(value: u64, numerator: u32, denominator: u32) -> u32 {
    value
        .saturating_mul(u64::from(numerator))
        .saturating_add(u64::from(denominator).saturating_sub(1))
        .saturating_div(u64::from(denominator)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn geometry(source: (u32, u32), presentation: (u32, u32)) -> PresentationGeometry {
        PresentationGeometry::new(
            Size::new(source.0, source.1).unwrap(),
            Size::new(presentation.0, presentation.1).unwrap(),
        )
    }

    #[test]
    fn presentation_geometry_uses_full_frame_for_equal_aspect_ratio() {
        let geom = geometry((3840, 2160), (1280, 720));

        assert_eq!(
            geom.visible_rect(),
            Rect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720
            }
        );
    }

    #[test]
    fn presentation_geometry_uses_full_frame_for_taller_presentation() {
        let geom = geometry((3840, 2160), (1024, 768));

        assert_eq!(
            geom.visible_rect(),
            Rect {
                x: 0,
                y: 0,
                width: 1024,
                height: 768
            }
        );
    }

    #[test]
    fn presentation_geometry_uses_full_frame_for_wider_presentation() {
        let geom = geometry((1080, 1920), (1280, 720));

        assert_eq!(
            geom.visible_rect(),
            Rect {
                x: 0,
                y: 0,
                width: 1280,
                height: 720
            }
        );
    }

    #[test]
    fn source_damage_uses_floor_left_top_and_ceil_right_bottom() {
        let geom = geometry((3, 3), (2, 2));

        assert_eq!(geom.map_source_damage(&[(1, 1, 1, 1)]), vec![(0, 0, 2, 2)]);
    }

    #[test]
    fn source_damage_clamps_to_visible_rect() {
        let geom = geometry((3840, 2160), (1024, 768));

        assert_eq!(
            geom.map_source_damage(&[(-10, -10, 20, 20)]),
            vec![(0, 0, 3, 4)]
        );
    }

    #[test]
    fn presentation_points_map_full_surface_to_source_edges() {
        let geom = geometry((1920, 1080), (1024, 768));

        assert_eq!(geom.map_presentation_point_to_source(0, 0), (0, 0));
        assert_eq!(
            geom.map_presentation_point_to_source(1023, 767),
            (1919, 1079)
        );
        assert_eq!(geom.map_presentation_point_to_source(512, 384), (960, 540));
    }

    proptest! {
        #[test]
        fn generated_pointer_mapping_stays_in_source_bounds(
            source_w in 1u32..=8192,
            source_h in 1u32..=8192,
            presentation_w in 1u32..=8192,
            presentation_h in 1u32..=8192,
            x in 0u32..=9000,
            y in 0u32..=9000,
        ) {
            let geom = geometry((source_w, source_h), (presentation_w, presentation_h));
            let (source_x, source_y) = geom.map_presentation_point_to_source(x, y);

            prop_assert!(source_x < source_w);
            prop_assert!(source_y < source_h);
        }

        #[test]
        fn generated_damage_mapping_stays_inside_presentation_visible_rect(
            source_w in 1u32..=8192,
            source_h in 1u32..=8192,
            presentation_w in 1u32..=8192,
            presentation_h in 1u32..=8192,
            x in -9000i32..=9000,
            y in -9000i32..=9000,
            w in -100i32..=9000,
            h in -100i32..=9000,
        ) {
            let geom = geometry((source_w, source_h), (presentation_w, presentation_h));
            let visible = geom.visible_rect();

            for (left, top, width, height) in geom.map_source_damage(&[(x, y, w, h)]) {
                prop_assert!(left >= visible.x as i32);
                prop_assert!(top >= visible.y as i32);
                prop_assert!(width > 0);
                prop_assert!(height > 0);
                prop_assert!((left as u32 + width as u32) <= visible.right());
                prop_assert!((top as u32 + height as u32) <= visible.bottom());
            }
        }
    }
}
