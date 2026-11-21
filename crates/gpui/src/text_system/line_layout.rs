use crate::{FontId, GlyphId, Pixels, PlatformTextSystem, Point, SharedString, Size, point, px};
use collections::{FxHashMap, FxHashSet};
use itertools::Itertools;
use smallvec::SmallVec;
use std::{
    borrow::Borrow,
    cell::RefCell,
    hash::{Hash, Hasher},
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use unicode_bidi::BidiInfo;

use super::LineWrapper;
use super::direction::TextDirection;

/// A laid out and styled line of text
#[derive(Default, Debug)]
pub struct LineLayout {
    /// The font size for this line
    pub font_size: Pixels,
    /// The width of the line
    pub width: Pixels,
    /// The ascent of the line
    pub ascent: Pixels,
    /// The descent of the line
    pub descent: Pixels,
    /// The shaped runs that make up this line
    pub runs: Vec<ShapedRun>,
    /// The length of the line in utf-8 bytes
    pub len: usize,
    /// Bidi-aware caret stops, sorted by logical UTF-8 byte index.
    pub index_positions: Vec<IndexPosition>,
    /// Bidi-aware caret stops, sorted by visual x position.
    pub visual_index_positions: Vec<IndexPosition>,
    /// The base text direction that was used to shape this line.
    ///
    /// This records the paragraph embedding level the platform text system
    /// actually applied, which can differ from a requested direction on
    /// platforms that do not support right-to-left paragraph layout.
    pub direction: TextDirection,
}

/// A caret stop for a logical UTF-8 byte index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IndexPosition {
    /// The logical UTF-8 byte index.
    pub index: usize,
    /// The visual x position for this logical boundary.
    pub x: Pixels,
}

/// A run of text that has been shaped .
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The font id for this run
    pub font_id: FontId,
    /// The glyphs that make up this run
    pub glyphs: Vec<ShapedGlyph>,
}

/// A single glyph, ready to paint.
#[derive(Clone, Debug)]
pub struct ShapedGlyph {
    /// The ID for this glyph, as determined by the text system.
    pub id: GlyphId,

    /// The position of this glyph in its containing line.
    pub position: Point<Pixels>,

    /// The index of this glyph in the original text.
    pub index: usize,

    /// Whether this glyph is an emoji
    pub is_emoji: bool,
}

impl LineLayout {
    /// The index for the character at the given x coordinate
    pub fn index_for_x(&self, x: Pixels) -> Option<usize> {
        if x >= self.width {
            None
        } else if !self.visual_index_positions.is_empty() {
            if let Some(group) = Self::exact_visual_group(&self.visual_index_positions, x) {
                return Some(Self::preferred_index_for_visual_group(
                    &self.visual_index_positions,
                    group,
                ));
            }

            let mut left = *self.visual_index_positions.first()?;
            for right in self.visual_index_positions.iter().copied().skip(1) {
                if right.x == left.x {
                    left = right;
                    continue;
                }

                if x < right.x {
                    return if left.index <= right.index {
                        Some(left.index)
                    } else {
                        Some(right.index)
                    };
                }

                left = right;
            }

            Some(left.index)
        } else {
            for run in self.runs.iter().rev() {
                for glyph in run.glyphs.iter().rev() {
                    if glyph.position.x <= x {
                        return Some(glyph.index);
                    }
                }
            }
            Some(0)
        }
    }

    /// closest_index_for_x returns the character boundary closest to the given x coordinate
    /// (e.g. to handle aligning up/down arrow keys)
    pub fn closest_index_for_x(&self, x: Pixels) -> usize {
        if !self.visual_index_positions.is_empty() {
            if let Some(group) = Self::exact_visual_group(&self.visual_index_positions, x) {
                return Self::preferred_index_for_visual_group(&self.visual_index_positions, group);
            }

            let mut left = *self.visual_index_positions.first().unwrap();
            if x < left.x {
                return left.index;
            }

            for right in self.visual_index_positions.iter().copied().skip(1) {
                if right.x == left.x {
                    left = right;
                    continue;
                }

                if x < right.x {
                    if right.x - x < x - left.x {
                        return right.index;
                    } else {
                        return left.index;
                    }
                }

                left = right;
            }

            return left.index;
        }

        let mut prev_index = 0;
        let mut prev_x = px(0.);

        for run in self.runs.iter() {
            for glyph in run.glyphs.iter() {
                if glyph.position.x >= x {
                    if glyph.position.x - x < x - prev_x {
                        return glyph.index;
                    } else {
                        return prev_index;
                    }
                }
                prev_index = glyph.index;
                prev_x = glyph.position.x;
            }
        }

        if self.len == 1 {
            if x > self.width / 2. {
                return 1;
            } else {
                return 0;
            }
        }

        self.len
    }

    /// The x position of the character at the given index
    pub fn x_for_index(&self, index: usize) -> Pixels {
        if let Some(position) = self
            .index_positions
            .iter()
            .filter(|position| position.index == index)
            .max_by(|a, b| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
        {
            return position.x;
        }

        if let Some(position) = self
            .index_positions
            .iter()
            .filter(|position| position.index > index)
            .min_by_key(|position| position.index)
        {
            return position.x;
        }

        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return glyph.position.x;
                }
            }
        }
        self.width
    }

    fn exact_visual_group(positions: &[IndexPosition], x: Pixels) -> Option<Range<usize>> {
        let start = positions.iter().position(|position| position.x == x)?;
        let end = positions[start..]
            .iter()
            .position(|position| position.x != x)
            .map_or(positions.len(), |offset| start + offset);
        Some(start..end)
    }

    fn preferred_index_for_visual_group(positions: &[IndexPosition], group: Range<usize>) -> usize {
        if group.len() == 1 {
            return positions[group.start].index;
        }

        let previous_interval = group
            .start
            .checked_sub(1)
            .map(|left_ix| (positions[left_ix], positions[group.start]));
        let next_interval = positions
            .get(group.end)
            .map(|right| (positions[group.end - 1], *right));

        if previous_interval.is_some_and(|(left, right)| left.index < right.index) {
            positions[group.start].index
        } else if next_interval.is_some_and(|(left, right)| left.index < right.index) {
            positions[group.end - 1].index
        } else {
            positions[group.start].index
        }
    }

    fn index_before_visual_boundary(&self, x: Pixels) -> usize {
        if let Some(group) = Self::exact_visual_group(&self.visual_index_positions, x) {
            self.visual_index_positions[group.start].index
        } else {
            self.closest_index_for_x(x)
        }
    }

    fn index_after_visual_boundary(&self, x: Pixels) -> usize {
        if let Some(group) = Self::exact_visual_group(&self.visual_index_positions, x) {
            self.visual_index_positions[group.end - 1].index
        } else {
            self.closest_index_for_x(x)
        }
    }

    /// The visual x ranges covered by a logical UTF-8 byte range.
    pub fn x_ranges_for_range(&self, range: Range<usize>) -> SmallVec<[Range<Pixels>; 2]> {
        let mut ranges = SmallVec::new();
        if range.start >= range.end {
            return ranges;
        }

        if self.visual_index_positions.is_empty() {
            Self::push_x_range(
                &mut ranges,
                self.x_for_index(range.start),
                self.x_for_index(range.end),
            );
            return ranges;
        }

        for adjacent_positions in self.visual_index_positions.windows(2) {
            let left = adjacent_positions[0];
            let right = adjacent_positions[1];
            if left.x == right.x {
                continue;
            }

            let logical_start = left.index.min(right.index);
            let logical_end = left.index.max(right.index);
            if logical_start < range.end && range.start < logical_end {
                Self::push_x_range(&mut ranges, left.x, right.x);
            }
        }

        ranges
    }

    fn push_x_range(ranges: &mut SmallVec<[Range<Pixels>; 2]>, start_x: Pixels, end_x: Pixels) {
        let (start_x, end_x) = if start_x <= end_x {
            (start_x, end_x)
        } else {
            (end_x, start_x)
        };

        if start_x == end_x {
            return;
        }

        if let Some(last_range) = ranges.last_mut()
            && last_range.end == start_x
        {
            last_range.end = end_x;
            return;
        }

        ranges.push(start_x..end_x);
    }

    pub(crate) fn compute_bidi_index_positions(&mut self, text: &str) {
        self.index_positions.clear();
        self.visual_index_positions.clear();
        if text.is_empty() {
            return;
        }

        // A right-to-left paragraph mirrors the whole line: the platform lays it
        // out against the right edge of the paragraph even when the text itself
        // contains no right-to-left characters. The caret stops have to follow
        // that mirroring, so they are computed for right-to-left paragraphs
        // regardless of the text they contain.
        let rtl_paragraph = self.direction.is_rtl();
        if !rtl_paragraph && !contains_rtl_or_bidi_control(text) {
            return;
        }

        // Resolve the Unicode bidi algorithm against the same paragraph base
        // direction the platform text system used for shaping (`self.direction`),
        // so that every caret stop below maps consistently with what was
        // rendered.
        let bidi_info = BidiInfo::new(text, Some(self.direction.bidi_level()));
        if !rtl_paragraph && !bidi_info.has_rtl() {
            return;
        }

        let mut glyphs = self
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter())
            .collect::<Vec<_>>();
        glyphs.sort_by(|a, b| {
            a.position
                .x
                .partial_cmp(&b.position.x)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut run_edges = Vec::with_capacity(glyphs.len());
        for (ix, glyph) in glyphs.iter().enumerate() {
            let next_x = glyphs
                .get(ix + 1)
                .map_or(self.width, |next_glyph| next_glyph.position.x);
            run_edges.push((glyph.index, glyph.position.x, next_x));
        }

        for paragraph in &bidi_info.paragraphs {
            let (_levels, visual_runs) = bidi_info.visual_runs(paragraph, paragraph.range.clone());
            if visual_runs.is_empty() {
                continue;
            }

            for visual_run in visual_runs {
                let Some(level) = bidi_info.levels.get(visual_run.start) else {
                    continue;
                };
                let run_glyphs = run_edges
                    .iter()
                    .filter(|(index, _, _)| visual_run.contains(index))
                    .copied()
                    .collect::<Vec<_>>();
                if run_glyphs.is_empty() {
                    continue;
                }

                let run_left = run_glyphs
                    .first()
                    .map_or(Pixels::ZERO, |(_, left, _)| *left);
                let run_right = run_glyphs.last().map_or(self.width, |(_, _, right)| *right);

                let mut run_glyphs_by_index = run_glyphs.clone();
                run_glyphs_by_index.sort_by_key(|(index, _, _)| *index);

                let mut boundaries = text[visual_run.clone()]
                    .char_indices()
                    .map(|(offset, _)| visual_run.start + offset)
                    .chain([visual_run.end])
                    .collect::<Vec<_>>();
                boundaries.sort_unstable();
                boundaries.dedup();

                let mut glyph_ix = 0;
                if level.is_rtl() {
                    for boundary in boundaries {
                        while glyph_ix < run_glyphs_by_index.len()
                            && run_glyphs_by_index[glyph_ix].0 < boundary
                        {
                            glyph_ix += 1;
                        }
                        let x = if boundary == visual_run.start {
                            run_right
                        } else if glyph_ix > 0 {
                            run_glyphs_by_index[glyph_ix - 1].1
                        } else {
                            run_left
                        };
                        self.index_positions
                            .push(IndexPosition { index: boundary, x });
                    }
                } else {
                    for boundary in boundaries {
                        while glyph_ix < run_glyphs_by_index.len()
                            && run_glyphs_by_index[glyph_ix].0 < boundary
                        {
                            glyph_ix += 1;
                        }
                        let x = if boundary == visual_run.end {
                            run_right
                        } else {
                            run_glyphs_by_index
                                .get(glyph_ix)
                                .map_or(run_left, |(_, left, _)| *left)
                        };
                        self.index_positions
                            .push(IndexPosition { index: boundary, x });
                    }
                }
            }
        }

        self.index_positions.sort_by(|a, b| {
            a.index
                .cmp(&b.index)
                .then_with(|| a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
        });
        self.index_positions
            .dedup_by(|a, b| a.index == b.index && a.x == b.x);
        self.visual_index_positions = self.index_positions.clone();
        self.visual_index_positions.sort_by(|a, b| {
            a.x.partial_cmp(&b.x)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.index.cmp(&b.index))
        });
    }

    /// The corresponding Font at the given index
    pub fn font_id_for_index(&self, index: usize) -> Option<FontId> {
        for run in &self.runs {
            for glyph in &run.glyphs {
                if glyph.index >= index {
                    return Some(run.font_id);
                }
            }
        }
        None
    }

    /// Split this layout at a byte index, returning `(prefix, suffix)`.
    ///
    /// Lines whose glyphs are stored in logical byte order are split by advancing
    /// a pen to the split point: the prefix keeps its original glyph positions
    /// and its width is the x-advance up to the split point.
    ///
    /// Lines with visually reordered glyphs (bidirectional text) are split by
    /// grouping glyphs into visual columns instead: the `prefix` contains glyphs
    /// for bytes `[0, byte_index)` normalized to the left edge of the prefix's
    /// visual bounds, and the `suffix` contains glyphs for bytes
    /// `[byte_index, len)` normalized to the left edge of the suffix's visual
    /// bounds, with byte indices rebased to 0.
    ///
    /// - `font_size`, `ascent`, and `descent` are copied to both halves.
    pub fn split_at(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let byte_ordered = self
            .runs
            .iter()
            .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index))
            .is_sorted();

        if byte_ordered {
            self.split_at_byte_ordered(byte_index)
        } else {
            self.split_at_visual(byte_index)
        }
    }

    fn split_at_byte_ordered(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let x_offset = self.x_for_index(byte_index);

        // Partition glyph runs. A single run may contribute glyphs to both halves.
        let mut left_runs = Vec::new();
        let mut right_runs = Vec::new();

        for run in &self.runs {
            let split_pos = run.glyphs.partition_point(|g| g.index < byte_index);

            if split_pos > 0 {
                left_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: run.glyphs[..split_pos].to_vec(),
                });
            }

            if split_pos < run.glyphs.len() {
                let right_glyphs = run.glyphs[split_pos..]
                    .iter()
                    .map(|g| ShapedGlyph {
                        id: g.id,
                        position: point(g.position.x - x_offset, g.position.y),
                        index: g.index - byte_index,
                        is_emoji: g.is_emoji,
                    })
                    .collect();
                right_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: right_glyphs,
                });
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: left_runs,
            len: byte_index,
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: self.direction,
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: self.width - x_offset,
            ascent: self.ascent,
            descent: self.descent,
            runs: right_runs,
            len: self.len - byte_index,
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: self.direction,
        };

        (left, right)
    }

    fn split_at_visual(&self, byte_index: usize) -> (LineLayout, LineLayout) {
        let mut glyph_edges = self
            .runs
            .iter()
            .enumerate()
            .flat_map(|(run_ix, run)| {
                run.glyphs
                    .iter()
                    .enumerate()
                    .map(move |(glyph_ix, glyph)| (run_ix, glyph_ix, glyph))
            })
            .collect::<Vec<_>>();
        glyph_edges.sort_by(|a, b| {
            a.2.position
                .x
                .partial_cmp(&b.2.position.x)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut left_positions = self
            .runs
            .iter()
            .map(|run| vec![None; run.glyphs.len()])
            .collect::<Vec<_>>();
        let mut right_positions = self
            .runs
            .iter()
            .map(|run| vec![None; run.glyphs.len()])
            .collect::<Vec<_>>();
        let mut left_right = Pixels::ZERO;
        let mut right_right = Pixels::ZERO;
        let mut ix = 0;
        while ix < glyph_edges.len() {
            let glyph_left = glyph_edges[ix].2.position.x;
            let mut next_ix = ix + 1;
            while next_ix < glyph_edges.len() && glyph_edges[next_ix].2.position.x == glyph_left {
                next_ix += 1;
            }
            let glyph_right = glyph_edges
                .get(next_ix)
                .map_or(self.width, |(_, _, next_glyph)| next_glyph.position.x);
            let glyph_width = glyph_right - glyph_left;
            let glyph_group = &glyph_edges[ix..next_ix];

            if glyph_group
                .iter()
                .any(|(_, _, glyph)| glyph.index < byte_index)
            {
                for (run_ix, glyph_ix, glyph) in glyph_group {
                    if glyph.index < byte_index {
                        left_positions[*run_ix][*glyph_ix] =
                            Some(left_right + (glyph.position.x - glyph_left));
                    }
                }
                left_right += glyph_width;
            }

            if glyph_group
                .iter()
                .any(|(_, _, glyph)| glyph.index >= byte_index)
            {
                for (run_ix, glyph_ix, glyph) in glyph_group {
                    if glyph.index >= byte_index {
                        right_positions[*run_ix][*glyph_ix] =
                            Some(right_right + (glyph.position.x - glyph_left));
                    }
                }
                right_right += glyph_width;
            }

            ix = next_ix;
        }

        let left_width = left_right;
        let right_width = right_right;

        // Partition glyph runs by logical byte index. Glyphs are not guaranteed
        // to be stored in logical order after RTL shaping, so this must not use
        // partition_point.
        let mut left_runs = Vec::new();
        let mut right_runs = Vec::new();

        for (run_ix, run) in self.runs.iter().enumerate() {
            let mut left_glyphs = Vec::new();
            let mut right_glyphs = Vec::new();

            for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                if let Some(x) = left_positions[run_ix][glyph_ix] {
                    left_glyphs.push(ShapedGlyph {
                        id: glyph.id,
                        position: point(x, glyph.position.y),
                        index: glyph.index,
                        is_emoji: glyph.is_emoji,
                    });
                } else if let Some(x) = right_positions[run_ix][glyph_ix] {
                    right_glyphs.push(ShapedGlyph {
                        id: glyph.id,
                        position: point(x, glyph.position.y),
                        index: glyph.index - byte_index,
                        is_emoji: glyph.is_emoji,
                    });
                }
            }

            if !left_glyphs.is_empty() {
                left_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: left_glyphs,
                });
            }

            if !right_glyphs.is_empty() {
                right_runs.push(ShapedRun {
                    font_id: run.font_id,
                    glyphs: right_glyphs,
                });
            }
        }

        let left = LineLayout {
            font_size: self.font_size,
            width: left_width,
            ascent: self.ascent,
            descent: self.descent,
            runs: left_runs,
            len: byte_index,
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: self.direction,
        };

        let right = LineLayout {
            font_size: self.font_size,
            width: right_width,
            ascent: self.ascent,
            descent: self.descent,
            runs: right_runs,
            len: self.len - byte_index,
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: self.direction,
        };

        (left, right)
    }

    fn compute_wrap_boundaries(
        &self,
        text: &str,
        wrap_width: Pixels,
        max_lines: Option<usize>,
        scratch: &mut WrapScratch,
    ) -> SmallVec<[WrapBoundary; 1]> {
        let mut boundaries = SmallVec::new();
        let mut current_segment: Option<Range<usize>> = None;
        let mut last_candidate_ix = None;
        let mut last_candidate_x = px(0.);
        let mut last_boundary = WrapBoundary {
            run_ix: 0,
            glyph_ix: 0,
        };
        let mut last_boundary_x = px(0.);
        // Shaping can combine characters across a line break opportunity, so we ignore those
        // break opportunities that would separate them.
        let WrapScratch {
            glyph_indices,
            break_indices,
        } = scratch;
        glyph_indices.clear();
        glyph_indices.extend(
            self.runs
                .iter()
                .flat_map(|run| run.glyphs.iter().map(|glyph| glyph.index)),
        );
        break_indices.clear();
        break_indices
            .extend(LineWrapper::break_indices(text).filter(|index| glyph_indices.contains(index)));
        let mut glyphs = self
            .runs
            .iter()
            .enumerate()
            .flat_map(move |(run_ix, run)| {
                run.glyphs.iter().enumerate().map(move |(glyph_ix, glyph)| {
                    (
                        WrapBoundary { run_ix, glyph_ix },
                        glyph.index,
                        glyph.position.x,
                    )
                })
            })
            .dedup_by(|(_, previous_index, ..), (_, index, ..)| previous_index == index)
            .peekable();

        while let Some((boundary, index, x)) = glyphs.next() {
            let byte = text.as_bytes()[index];
            if byte == b'\n' {
                continue;
            }

            if byte != b' '
                && current_segment
                    .as_ref()
                    .is_none_or(|segment| !segment.contains(&index))
            {
                // Detect transitions between line-break segments rather than matching glyph
                // indices directly, to accommodate RTL text. Workaround for the lack of complete
                // UAX #9 support.
                let segment = break_indices.partition_point(|break_index| *break_index <= index);
                if current_segment.is_some() {
                    last_candidate_ix = Some(boundary);
                    last_candidate_x = x;
                }
                let start = segment
                    .checked_sub(1)
                    .and_then(|previous| break_indices.get(previous))
                    .copied()
                    .unwrap_or(0);
                let end = break_indices.get(segment).copied().unwrap_or(text.len());
                current_segment = Some(start..end);
            }

            let next_x = glyphs.peek().map_or(self.width, |(_, _, x)| *x);
            let width = next_x - last_boundary_x;

            if width > wrap_width && boundary > last_boundary {
                // When used line_clamp, we should limit the number of lines.
                if let Some(max_lines) = max_lines
                    && boundaries.len() >= max_lines.saturating_sub(1)
                {
                    break;
                }

                if let Some(last_candidate_ix) = last_candidate_ix.take() {
                    last_boundary = last_candidate_ix;
                    last_boundary_x = last_candidate_x;
                } else {
                    last_boundary = boundary;
                    last_boundary_x = x;
                }
                boundaries.push(last_boundary);
            }
        }

        boundaries
    }
}

/// A line of text that has been wrapped to fit a given width
#[derive(Default, Debug)]
pub struct WrappedLineLayout {
    /// The line layout, pre-wrapping.
    pub unwrapped_layout: Arc<LineLayout>,

    /// The boundaries at which the line was wrapped
    pub wrap_boundaries: SmallVec<[WrapBoundary; 1]>,

    /// The width of the line, if it was wrapped
    pub wrap_width: Option<Pixels>,
}

/// A boundary at which a line was wrapped
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WrapBoundary {
    /// The index in the run just before the line was wrapped
    pub run_ix: usize,
    /// The index of the glyph just before the line was wrapped
    pub glyph_ix: usize,
}

impl WrappedLineLayout {
    /// The length of the underlying text, in utf8 bytes.
    pub fn len(&self) -> usize {
        self.unwrapped_layout.len
    }

    /// The width of this line, in pixels, whether or not it was wrapped.
    pub fn width(&self) -> Pixels {
        self.wrap_width
            .unwrap_or(Pixels::MAX)
            .min(self.unwrapped_layout.width)
    }

    /// The size of the whole wrapped text, for the given line_height.
    /// can span multiple lines if there are multiple wrap boundaries.
    pub fn size(&self, line_height: Pixels) -> Size<Pixels> {
        Size {
            width: self.width(),
            height: line_height * (self.wrap_boundaries.len() + 1),
        }
    }

    /// The ascent of a line in this layout
    pub fn ascent(&self) -> Pixels {
        self.unwrapped_layout.ascent
    }

    /// The descent of a line in this layout
    pub fn descent(&self) -> Pixels {
        self.unwrapped_layout.descent
    }

    /// The wrap boundaries in this layout
    pub fn wrap_boundaries(&self) -> &[WrapBoundary] {
        &self.wrap_boundaries
    }

    /// The font size of this layout
    pub fn font_size(&self) -> Pixels {
        self.unwrapped_layout.font_size
    }

    /// The runs in this layout, sans wrapping
    pub fn runs(&self) -> &[ShapedRun] {
        &self.unwrapped_layout.runs
    }

    /// The index corresponding to a given position in this layout for the given line height.
    ///
    /// See also [`Self::closest_index_for_position`].
    pub fn index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, false)
    }

    /// The closest index to a given position in this layout for the given line height.
    ///
    /// Closest means the character boundary closest to the given position.
    ///
    /// See also [`LineLayout::closest_index_for_x`].
    pub fn closest_index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
    ) -> Result<usize, usize> {
        self._index_for_position(position, line_height, true)
    }

    fn wrapped_line_x_range(&self, wrapped_line_ix: usize) -> Option<(Pixels, Pixels)> {
        if wrapped_line_ix > self.wrap_boundaries.len() {
            return None;
        }

        let start_x = if wrapped_line_ix > 0 {
            self.wrap_boundary_x(self.wrap_boundaries[wrapped_line_ix - 1])?
        } else {
            Pixels::ZERO
        };

        let end_x = if wrapped_line_ix < self.wrap_boundaries.len() {
            self.wrap_boundary_x(self.wrap_boundaries[wrapped_line_ix])?
        } else {
            self.unwrapped_layout.width
        };

        Some((start_x, end_x))
    }

    fn wrap_boundary_x(&self, boundary: WrapBoundary) -> Option<Pixels> {
        let run = self.unwrapped_layout.runs.get(boundary.run_ix)?;
        let glyph = run.glyphs.get(boundary.glyph_ix)?;
        Some(glyph.position.x)
    }

    fn _index_for_position(
        &self,
        position: Point<Pixels>,
        line_height: Pixels,
        closest: bool,
    ) -> Result<usize, usize> {
        let wrapped_line_ix = (position.y / line_height) as usize;

        let Some((wrapped_line_start_x, wrapped_line_end_x)) =
            self.wrapped_line_x_range(wrapped_line_ix)
        else {
            return Err(0);
        };
        let wrapped_line_start_index = self
            .unwrapped_layout
            .index_after_visual_boundary(wrapped_line_start_x);
        let wrapped_line_end_index = self
            .unwrapped_layout
            .index_before_visual_boundary(wrapped_line_end_x);

        let mut position_in_unwrapped_line = position;
        position_in_unwrapped_line.x += wrapped_line_start_x;
        if position_in_unwrapped_line.x < wrapped_line_start_x {
            Err(wrapped_line_start_index)
        } else if position_in_unwrapped_line.x >= wrapped_line_end_x {
            Err(wrapped_line_end_index)
        } else {
            if closest {
                Ok(self
                    .unwrapped_layout
                    .closest_index_for_x(position_in_unwrapped_line.x))
            } else {
                // The shaper can place a trailing zero-width wrap boundary glyph slightly past
                // the line's width, so the row can extend past where `index_for_x` has glyphs.
                self.unwrapped_layout
                    .index_for_x(position_in_unwrapped_line.x)
                    .ok_or(wrapped_line_end_index)
            }
        }
    }

    /// Returns the pixel position for the given byte index.
    pub fn position_for_index(&self, index: usize, line_height: Pixels) -> Option<Point<Pixels>> {
        let x = self.unwrapped_layout.x_for_index(index);

        for ix in 0..=self.wrap_boundaries.len() {
            let Some((line_start_x, line_end_x)) = self.wrapped_line_x_range(ix) else {
                break;
            };
            let line_y = ix as f32 * line_height;
            if x < line_start_x {
                break;
            } else if x > line_end_x {
                continue;
            } else {
                return Some(point(x - line_start_x, line_y));
            }
        }

        None
    }
}

pub(crate) struct LineLayoutCache {
    state: RefCell<LineLayoutCacheState>,
    wrap_scratch: RefCell<WrapScratch>,
    platform_text_system: Arc<dyn PlatformTextSystem>,
    /// Advances when [`TextSystem::add_fonts`] successfully changes the font database.
    font_generation: Arc<AtomicUsize>,
    /// Records the generation represented by both frame caches.
    cached_font_generation: AtomicUsize,
}

#[derive(Default)]
struct WrapScratch {
    glyph_indices: FxHashSet<usize>,
    break_indices: Vec<usize>,
}

#[derive(Default)]
struct LineLayoutCacheState {
    previous_frame: FrameCache,
    current_frame: FrameCache,
}

#[derive(Default)]
struct FrameCache {
    lines: FxHashMap<Arc<CacheKey>, Arc<LineLayout>>,
    wrapped_lines: FxHashMap<Arc<CacheKey>, Arc<WrappedLineLayout>>,
    used_lines: Vec<Arc<CacheKey>>,
    used_wrapped_lines: Vec<Arc<CacheKey>>,
}

#[derive(Clone, Default)]
pub(crate) struct LineLayoutIndex {
    font_generation: usize,
    lines_index: usize,
    wrapped_lines_index: usize,
}

impl LineLayoutCache {
    pub fn new(
        platform_text_system: Arc<dyn PlatformTextSystem>,
        font_generation: Arc<AtomicUsize>,
    ) -> Self {
        let cached_font_generation = font_generation.load(Ordering::Acquire);
        Self {
            state: RefCell::default(),
            wrap_scratch: RefCell::default(),
            platform_text_system,
            font_generation,
            cached_font_generation: AtomicUsize::new(cached_font_generation),
        }
    }

    pub fn layout_index(&self) -> LineLayoutIndex {
        let font_generation = self.clear_if_font_generation_changed();
        let state = self.state.borrow();
        let frame = &state.current_frame;
        LineLayoutIndex {
            font_generation,
            lines_index: frame.used_lines.len(),
            wrapped_lines_index: frame.used_wrapped_lines.len(),
        }
    }

    pub fn reuse_layouts(&self, range: Range<LineLayoutIndex>) {
        let font_generation = self.clear_if_font_generation_changed();
        if range.start.font_generation != font_generation
            || range.end.font_generation != font_generation
        {
            return;
        }
        let mut state = self.state.borrow_mut();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *state;

        for key in &previous_frame.used_lines[range.start.lines_index..range.end.lines_index] {
            if let Some((key, line)) = previous_frame.lines.remove_entry(key) {
                current_frame.lines.insert(key, line);
            }
            current_frame.used_lines.push(key.clone());
        }

        for key in &previous_frame.used_wrapped_lines
            [range.start.wrapped_lines_index..range.end.wrapped_lines_index]
        {
            if let Some((key, line)) = previous_frame.wrapped_lines.remove_entry(key) {
                current_frame.wrapped_lines.insert(key, line);
            }
            current_frame.used_wrapped_lines.push(key.clone());
        }
    }

    pub fn truncate_layouts(&self, index: LineLayoutIndex) {
        let font_generation = self.clear_if_font_generation_changed();
        if index.font_generation != font_generation {
            return;
        }
        let mut state = self.state.borrow_mut();
        let current_frame = &mut state.current_frame;
        current_frame.used_lines.truncate(index.lines_index);
        current_frame
            .used_wrapped_lines
            .truncate(index.wrapped_lines_index);
    }

    pub fn finish_frame(&self) {
        let _font_generation = self.clear_if_font_generation_changed();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *self.state.borrow_mut();
        std::mem::swap(previous_frame, current_frame);
        current_frame.lines.clear();
        current_frame.wrapped_lines.clear();
        current_frame.used_lines.clear();
        current_frame.used_wrapped_lines.clear();
    }

    pub fn layout_wrapped_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        direction: TextDirection,
        wrap_width: Option<Pixels>,
        max_lines: Option<usize>,
    ) -> Arc<WrappedLineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let _font_generation = self.clear_if_font_generation_changed();
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width,
            force_width: None,
            direction,
        } as &dyn AsCacheKeyRef;

        let state = self.state.borrow();
        if let Some(layout) = state.current_frame.wrapped_lines.get(key) {
            return layout.clone();
        }
        drop(state);

        let mut state = self.state.borrow_mut();
        let previous_frame_entry = state.previous_frame.wrapped_lines.remove_entry(key);
        if let Some((key, layout)) = previous_frame_entry {
            state
                .current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            state.current_frame.used_wrapped_lines.push(key);
            layout
        } else {
            drop(state);
            let text = SharedString::from(text);
            let unwrapped_layout =
                self.layout_line::<&SharedString>(&text, font_size, runs, direction, None);
            let wrap_boundaries = if let Some(wrap_width) = wrap_width {
                unwrapped_layout.compute_wrap_boundaries(
                    text.as_ref(),
                    wrap_width,
                    max_lines,
                    &mut self.wrap_scratch.borrow_mut(),
                )
            } else {
                SmallVec::new()
            };
            let layout = Arc::new(WrappedLineLayout {
                unwrapped_layout,
                wrap_boundaries,
                wrap_width,
            });
            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width,
                force_width: None,
                direction,
            });

            let mut state = self.state.borrow_mut();
            state
                .current_frame
                .wrapped_lines
                .insert(key.clone(), layout.clone());
            state.current_frame.used_wrapped_lines.push(key);

            layout
        }
    }

    pub fn layout_line<Text>(
        &self,
        text: Text,
        font_size: Pixels,
        runs: &[FontRun],
        direction: TextDirection,
        force_width: Option<Pixels>,
    ) -> Arc<LineLayout>
    where
        Text: AsRef<str>,
        SharedString: From<Text>,
    {
        let _font_generation = self.clear_if_font_generation_changed();
        let key = &CacheKeyRef {
            text: text.as_ref(),
            font_size,
            runs,
            wrap_width: None,
            force_width,
            direction,
        } as &dyn AsCacheKeyRef;

        let state = self.state.borrow();
        if let Some(layout) = state.current_frame.lines.get(key) {
            return layout.clone();
        }
        drop(state);

        let mut state = self.state.borrow_mut();
        let LineLayoutCacheState {
            previous_frame,
            current_frame,
        } = &mut *state;
        if let Some((key, layout)) = previous_frame.lines.remove_entry(key) {
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        } else {
            let text = SharedString::from(text);
            let mut layout = self
                .platform_text_system
                .layout_line(&text, font_size, runs, direction);

            if let Some(force_width) = force_width {
                apply_force_width_to_layout(&mut layout, force_width);
            }
            layout.compute_bidi_index_positions(&text);

            let key = Arc::new(CacheKey {
                text,
                font_size,
                runs: SmallVec::from(runs),
                wrap_width: None,
                force_width,
                direction,
            });
            let layout = Arc::new(layout);
            current_frame.lines.insert(key.clone(), layout.clone());
            current_frame.used_lines.push(key);
            layout
        }
    }

    fn clear_if_font_generation_changed(&self) -> usize {
        let font_generation = self.font_generation.load(Ordering::Acquire);
        if self.cached_font_generation.load(Ordering::Acquire) == font_generation {
            return font_generation;
        }

        let mut state = self.state.borrow_mut();
        if self.cached_font_generation.load(Ordering::Acquire) == font_generation {
            return font_generation;
        }

        *state = LineLayoutCacheState::default();
        self.cached_font_generation
            .store(font_generation, Ordering::Release);
        font_generation
    }
}

// Combining marks (e.g. Thai vowel signs, Arabic diacritics) are shaped by
// HarfBuzz at the same x position as their base character. The force-width
// loop must not advance the cell counter for these zero-advance glyphs,
// otherwise they get displaced into the next cell. We detect them by checking
// whether shaped x has advanced by at least half a cell beyond the last base.
fn apply_force_width_to_layout(layout: &mut LineLayout, force_width: Pixels) {
    let mut glyph_pos: usize = 0;
    // NEG_INFINITY ensures the first glyph is always classified as a base.
    let mut last_base_shaped_x = px(f32::NEG_INFINITY);
    let mut last_base_actual_x = px(0.);

    for run in layout.runs.iter_mut() {
        for glyph in run.glyphs.iter_mut() {
            let shaped_x = glyph.position.x;

            if shaped_x > last_base_shaped_x + force_width * 0.5 {
                let forced_x = glyph_pos * force_width;
                if (shaped_x - forced_x).abs() > px(1.) {
                    glyph.position.x = forced_x;
                }
                last_base_shaped_x = shaped_x;
                last_base_actual_x = glyph.position.x;
                glyph_pos += 1;
            } else {
                glyph.position.x = last_base_actual_x + (shaped_x - last_base_shaped_x);
            }
        }
    }
}

fn contains_rtl_or_bidi_control(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(
            character,
            '\u{0590}'..='\u{08ff}'
                | '\u{200e}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
                | '\u{fb1d}'..='\u{fdff}'
                | '\u{fe70}'..='\u{fefc}'
                | '\u{10800}'..='\u{10fff}'
                | '\u{1e800}'..='\u{1eFFF}'
        )
    })
}

/// A run of text with a single font.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
#[expect(missing_docs)]
pub struct FontRun {
    pub len: usize,
    pub font_id: FontId,
}

trait AsCacheKeyRef {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_>;
}

#[derive(Clone, Debug, Eq)]
struct CacheKey {
    text: SharedString,
    font_size: Pixels,
    runs: SmallVec<[FontRun; 1]>,
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    direction: TextDirection,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct CacheKeyRef<'a> {
    text: &'a str,
    font_size: Pixels,
    runs: &'a [FontRun],
    wrap_width: Option<Pixels>,
    force_width: Option<Pixels>,
    direction: TextDirection,
}

impl PartialEq for dyn AsCacheKeyRef + '_ {
    fn eq(&self, other: &dyn AsCacheKeyRef) -> bool {
        self.as_cache_key_ref() == other.as_cache_key_ref()
    }
}

impl Eq for dyn AsCacheKeyRef + '_ {}

impl Hash for dyn AsCacheKeyRef + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state)
    }
}

impl AsCacheKeyRef for CacheKey {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        CacheKeyRef {
            text: &self.text,
            font_size: self.font_size,
            runs: self.runs.as_slice(),
            wrap_width: self.wrap_width,
            force_width: self.force_width,
            direction: self.direction,
        }
    }
}

impl PartialEq for CacheKey {
    fn eq(&self, other: &Self) -> bool {
        self.as_cache_key_ref().eq(&other.as_cache_key_ref())
    }
}

impl Hash for CacheKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_cache_key_ref().hash(state);
    }
}

impl<'a> Borrow<dyn AsCacheKeyRef + 'a> for Arc<CacheKey> {
    fn borrow(&self) -> &(dyn AsCacheKeyRef + 'a) {
        self.as_ref() as &dyn AsCacheKeyRef
    }
}

impl AsCacheKeyRef for CacheKeyRef<'_> {
    fn as_cache_key_ref(&self) -> CacheKeyRef<'_> {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GlyphId, NoopTextSystem, TextRun, TextSystem, WindowTextSystem, font};

    #[test]
    fn shape_line_shares_its_text_with_the_layout_cache() {
        let text_system =
            WindowTextSystem::new(Arc::new(TextSystem::new(Arc::new(NoopTextSystem::new()))));
        // Long enough to be stored on the heap rather than inline.
        let text = SharedString::from("let value = compute_something(argument_one, argument_two);");
        let runs = [TextRun {
            len: text.len(),
            font: font(".ZedMono"),
            ..Default::default()
        }];

        text_system.shape_line(text.clone(), px(14.), &runs, None);
        let state = text_system.line_layout_cache.state.borrow();
        let (key, _) = state.current_frame.lines.iter().next().unwrap();
        assert_eq!(key.text.as_ptr(), text.as_ptr());
    }

    fn glyph_at(x: f32, index: usize) -> ShapedGlyph {
        ShapedGlyph {
            id: GlyphId(0),
            position: point(px(x), px(0.)),
            index,
            is_emoji: false,
        }
    }

    fn make_layout(glyphs: Vec<ShapedGlyph>) -> LineLayout {
        LineLayout {
            font_size: px(16.),
            width: px(100.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: 0,
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        }
    }

    fn glyph_x_positions(layout: &LineLayout) -> Vec<f32> {
        layout.runs[0]
            .glyphs
            .iter()
            .map(|g| f32::from(g.position.x))
            .collect()
    }

    #[test]
    fn test_wrap_preserves_shaped_clusters() {
        let mut layout = make_layout(vec![
            glyph_at(0., 0),
            glyph_at(5., 0),
            glyph_at(10., 3),
            glyph_at(15., 3),
        ]);
        layout.width = px(20.);
        layout.len = "你好".len();
        assert_eq!(
            layout
                .compute_wrap_boundaries("你好", px(15.), None, &mut WrapScratch::default())
                .as_slice(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 2
            }],
        );

        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(10., 0)]);
        layout.width = px(20.);
        layout.len = 1;
        assert!(
            layout
                .compute_wrap_boundaries("a", px(10.), None, &mut WrapScratch::default())
                .is_empty()
        );
    }

    #[test]
    fn test_wrap_right_to_left_text() {
        for (text, indices, expected_boundaries) in [
            ("אבג דהו", vec![11, 9, 7, 6, 4, 2, 0], vec![4]),
            ("a אבג b", vec![0, 1, 6, 4, 2, 8, 9], vec![2, 6]),
        ] {
            let mut layout = make_layout(
                indices
                    .iter()
                    .enumerate()
                    .map(|(position, index)| glyph_at(position as f32 * 10., *index))
                    .collect(),
            );
            layout.width = px(indices.len() as f32 * 10.);
            layout.len = text.len();
            assert_eq!(
                layout.compute_wrap_boundaries(text, px(40.), None, &mut WrapScratch::default()),
                expected_boundaries
                    .into_iter()
                    .map(|glyph_ix| WrapBoundary {
                        run_ix: 0,
                        glyph_ix
                    })
                    .collect::<SmallVec<[WrapBoundary; 1]>>(),
                "{text:?}",
            );
        }
    }

    #[test]
    fn test_wrap_ignores_breaks_inside_shaped_clusters() {
        let text = "x a-bcde";
        let mut layout = make_layout(
            [0, 1, 2, 5, 6, 7]
                .into_iter()
                .map(|index| glyph_at(index as f32 * 10., index))
                .collect(),
        );
        layout.width = px(80.);
        layout.len = text.len();
        assert_eq!(
            layout
                .compute_wrap_boundaries(text, px(60.), None, &mut WrapScratch::default())
                .as_slice(),
            &[WrapBoundary {
                run_ix: 0,
                glyph_ix: 2
            }],
        );
    }

    #[test]
    fn test_bidi_index_positions_rtl_base_mixed_line() {
        // "abc אבג" shaped with an RTL paragraph base direction: the Hebrew
        // run is placed at the left edge of the line (reading right-to-left)
        // and the Latin run ends up at the right edge. Glyph x positions
        // mirror what DirectWrite produces for such a line.
        let text = "abc אבג";
        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(70.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![
                    glyph_at(0., 8),  // ג
                    glyph_at(10., 6), // ב
                    glyph_at(20., 4), // א
                    glyph_at(30., 3), // space
                    glyph_at(40., 0), // a
                    glyph_at(50., 1), // b
                    glyph_at(60., 2), // c
                ],
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::RightToLeft,
        };

        layout.compute_bidi_index_positions(text);

        // The paragraph reads from the right: the first logical index (start
        // of the Latin run) sits at the right side of the line.
        assert_eq!(layout.x_for_index(0), px(40.));
        assert_eq!(layout.x_for_index(1), px(50.));
        assert_eq!(layout.x_for_index(2), px(60.));
        assert_eq!(layout.x_for_index(3), px(70.));
        // The Hebrew run reads right-to-left from x=30 down to the left edge.
        assert_eq!(layout.x_for_index(4), px(30.));
        assert_eq!(layout.x_for_index(6), px(20.));
        assert_eq!(layout.x_for_index(8), px(10.));
        assert_eq!(layout.x_for_index(10), px(0.));

        // A position between two caret stops resolves to the following caret stop.
        assert_eq!(layout.index_for_x(px(45.)), Some(1));
        assert_eq!(layout.index_for_x(px(15.)), Some(6));

        // The caret stop at x=40 is shared by the start of the Latin run and the
        // end of the Hebrew run, so the visual range of the Latin run starts at
        // the left edge of the Hebrew run.
        assert_eq!(
            layout.x_ranges_for_range(0..3).as_slice(),
            [px(30.)..px(70.)]
        );
        assert_eq!(
            layout.x_ranges_for_range(4..10).as_slice(),
            [px(0.)..px(30.)]
        );
    }

    #[test]
    fn test_force_width_latin_unchanged() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(8., 1), glyph_at(16., 2)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_marks_not_advanced() {
        let cell_width = px(8.);
        // Simulates Thai "กี" — base consonant at x=0, combining vowel also at x=0
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // ี (combining mark, same x)
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0.]);
    }

    #[test]
    fn test_force_width_base_after_combining_mark() {
        let cell_width = px(8.);
        let mut layout = make_layout(vec![glyph_at(0., 0), glyph_at(0., 3), glyph_at(8., 6)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 8.]);
    }

    #[test]
    fn test_force_width_multiple_combining_marks() {
        let cell_width = px(8.);
        // Simulates "ก้" — base + vowel + tone mark (two combining marks stacked)
        let mut layout = make_layout(vec![
            glyph_at(0., 0), // ก (base)
            glyph_at(0., 3), // vowel (combining)
            glyph_at(0., 6), // tone mark (combining)
            glyph_at(8., 9), // next base
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0., 0., 0., 8.]);
    }

    #[test]
    fn test_force_width_corrects_drifted_base_positions() {
        let cell_width = px(8.);
        // Font metrics don't perfectly match cell grid — glyphs drift >1px from cell boundary
        let mut layout = make_layout(vec![
            glyph_at(0.5, 0),  // within 1px tolerance, kept as-is
            glyph_at(10.2, 1), // >1px off from 8.0, corrected
            glyph_at(19.8, 2), // >1px off from 16.0, corrected
        ]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 8., 16.]);
    }

    #[test]
    fn test_force_width_combining_mark_after_within_tolerance_base() {
        let cell_width = px(8.);
        // Base glyph is within 1px of grid so it keeps its shaped position.
        // The combining mark must align to the base's actual position, not the grid slot.
        let mut layout = make_layout(vec![glyph_at(0.5, 0), glyph_at(0.5, 3)]);

        apply_force_width_to_layout(&mut layout, cell_width);

        let positions = glyph_x_positions(&layout);
        assert_eq!(positions, vec![0.5, 0.5]);
    }

    #[test]
    fn test_bidi_index_positions_for_hebrew_line() {
        let text = "אבג";
        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(30.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![glyph_at(0., 4), glyph_at(10., 2), glyph_at(20., 0)],
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        };

        layout.compute_bidi_index_positions(text);

        assert_eq!(layout.x_for_index(0), px(30.));
        assert_eq!(layout.x_for_index("א".len()), px(20.));
        assert_eq!(layout.x_for_index("אב".len()), px(10.));
        assert_eq!(layout.x_for_index(text.len()), px(0.));
        assert_eq!(layout.closest_index_for_x(px(29.)), 0);
        assert_eq!(layout.closest_index_for_x(px(1.)), text.len());
        assert_eq!(layout.index_for_x(px(29.)), Some(0));
        assert_eq!(layout.index_for_x(px(19.)), Some("א".len()));
        assert_eq!(layout.index_for_x(px(9.)), Some("אב".len()));
        assert_eq!(
            layout.x_ranges_for_range(0.."א".len()).as_slice(),
            [px(20.)..px(30.)]
        );
        assert_eq!(
            layout.x_ranges_for_range(0..text.len()).as_slice(),
            [px(0.)..px(30.)]
        );
    }

    #[test]
    fn test_bidi_index_positions_for_multiple_paragraphs() {
        let text = "abc\nאבג";
        let Some(alef) = text.find('א') else {
            panic!("expected alef in test text");
        };
        let Some(bet) = text.find('ב') else {
            panic!("expected bet in test text");
        };
        let Some(gimel) = text.find('ג') else {
            panic!("expected gimel in test text");
        };
        let after_gimel = gimel + 'ג'.len_utf8();
        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(60.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![
                    glyph_at(0., 0),
                    glyph_at(10., 1),
                    glyph_at(20., 2),
                    glyph_at(30., gimel),
                    glyph_at(40., bet),
                    glyph_at(50., alef),
                ],
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        };

        layout.compute_bidi_index_positions(text);

        assert_eq!(layout.x_for_index(alef), px(60.));
        assert_eq!(layout.x_for_index(bet), px(50.));
        assert_eq!(layout.x_for_index(gimel), px(40.));
        assert_eq!(layout.x_for_index(after_gimel), px(30.));
        assert_eq!(layout.index_for_x(px(59.)), Some(alef));
        assert_eq!(layout.index_for_x(px(39.)), Some(gimel));
    }

    #[test]
    fn test_wrapped_rtl_positioning_uses_visual_x_ranges() {
        let text = "אבגד";
        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(40.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![
                    glyph_at(0., "אבג".len()),
                    glyph_at(10., "אב".len()),
                    glyph_at(20., "א".len()),
                    glyph_at(30., 0),
                ],
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        };
        layout.compute_bidi_index_positions(text);

        let mut wrap_boundaries = SmallVec::new();
        wrap_boundaries.push(WrapBoundary {
            run_ix: 0,
            glyph_ix: 2,
        });
        let wrapped = WrappedLineLayout {
            unwrapped_layout: Arc::new(layout),
            wrap_boundaries,
            wrap_width: Some(px(20.)),
        };
        let line_height = px(18.);

        assert_eq!(
            wrapped.position_for_index(0, line_height),
            Some(point(px(20.), line_height))
        );
        assert_eq!(
            wrapped.position_for_index("אב".len(), line_height),
            Some(point(px(20.), px(0.)))
        );
        assert_eq!(
            wrapped.position_for_index(text.len(), line_height),
            Some(point(px(0.), px(0.)))
        );
        assert_eq!(
            wrapped.index_for_position(point(px(19.), line_height), line_height),
            Ok(0)
        );
        assert_eq!(
            wrapped.index_for_position(point(px(-1.), px(0.)), line_height),
            Err(text.len())
        );
        assert_eq!(
            wrapped.index_for_position(point(px(21.), line_height), line_height),
            Err(0)
        );
    }

    #[test]
    fn test_bidi_index_positions_for_arabic_and_persian_zwnj_lines() {
        for text in ["مرحبا", "می\u{200c}روم"] {
            let character_indices = text
                .char_indices()
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let mut glyphs = character_indices
                .iter()
                .rev()
                .enumerate()
                .map(|(position, index)| glyph_at(position as f32 * 10., *index))
                .collect::<Vec<_>>();
            let width = glyphs.len() as f32 * 10.;
            let mut layout = LineLayout {
                font_size: px(16.),
                width: px(width),
                ascent: px(12.),
                descent: px(4.),
                runs: vec![ShapedRun {
                    font_id: FontId(0),
                    glyphs: std::mem::take(&mut glyphs),
                }],
                len: text.len(),
                index_positions: Vec::new(),
                visual_index_positions: Vec::new(),
                direction: TextDirection::LeftToRight,
            };

            layout.compute_bidi_index_positions(text);

            assert_eq!(layout.x_for_index(0), px(width));
            assert_eq!(layout.x_for_index(text.len()), px(0.));
            assert_eq!(layout.closest_index_for_x(px(width - 1.)), 0);
            assert_eq!(layout.closest_index_for_x(px(1.)), text.len());
            assert_eq!(layout.index_for_x(px(width - 1.)), Some(0));
            let last_character_index = text.char_indices().last().map(|(index, _)| index).unwrap();
            assert_eq!(layout.index_for_x(px(1.)), Some(last_character_index));
        }
    }

    #[test]
    fn test_bidi_index_positions_for_mixed_ltr_and_hebrew_line() {
        let text = "abc אבג def";
        let Some(alef) = text.find('א') else {
            panic!("expected alef in test text");
        };
        let Some(bet) = text.find('ב') else {
            panic!("expected bet in test text");
        };
        let Some(gimel) = text.find('ג') else {
            panic!("expected gimel in test text");
        };
        let after_gimel = gimel + 'ג'.len_utf8();
        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(110.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs: vec![
                    glyph_at(0., 0),
                    glyph_at(10., 1),
                    glyph_at(20., 2),
                    glyph_at(30., 3),
                    glyph_at(40., gimel),
                    glyph_at(50., bet),
                    glyph_at(60., alef),
                    glyph_at(70., after_gimel),
                    glyph_at(80., after_gimel + 1),
                    glyph_at(90., after_gimel + 2),
                    glyph_at(100., after_gimel + 3),
                ],
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        };

        layout.compute_bidi_index_positions(text);

        assert_eq!(layout.x_for_index(bet), px(60.));
        assert_eq!(layout.x_for_index(gimel), px(50.));
        assert_eq!(layout.index_for_x(px(40.)), Some(alef));
        assert_eq!(layout.index_for_x(px(70.)), Some(after_gimel));
        assert_eq!(layout.closest_index_for_x(px(40.)), alef);
        assert_eq!(layout.closest_index_for_x(px(70.)), after_gimel);
        assert_eq!(layout.index_before_visual_boundary(px(40.)), alef);
        assert_eq!(layout.index_after_visual_boundary(px(40.)), after_gimel);
        assert_eq!(layout.index_before_visual_boundary(px(70.)), alef);
        assert_eq!(layout.index_after_visual_boundary(px(70.)), after_gimel);
        assert_eq!(layout.closest_index_for_x(px(59.)), bet);
        assert_eq!(layout.closest_index_for_x(px(51.)), gimel);
        assert_eq!(layout.index_for_x(px(55.)), Some(bet));
        assert_eq!(layout.index_for_x(px(45.)), Some(gimel));
        assert_eq!(
            layout.x_ranges_for_range(0..bet).as_slice(),
            [px(0.)..px(40.), px(60.)..px(70.)]
        );
        assert_eq!(
            layout.x_ranges_for_range(alef..after_gimel).as_slice(),
            [px(40.)..px(70.)]
        );
        assert_eq!(
            layout.x_ranges_for_range(bet..gimel).as_slice(),
            [px(50.)..px(60.)]
        );
    }

    #[test]
    fn test_index_for_x_for_mixed_english_and_arabic_punctuation() {
        let text = "x (مرحبا) y";
        let Some(arabic_start) = text.find('م') else {
            panic!("expected arabic text in test");
        };
        let Some(arabic_second) = text.find('ر') else {
            panic!("expected arabic text in test");
        };
        let Some(arabic_end) = text.find('ا') else {
            panic!("expected arabic text in test");
        };
        let after_arabic = arabic_end + 'ا'.len_utf8();
        let arabic_indices = text[arabic_start..after_arabic]
            .char_indices()
            .map(|(offset, _)| arabic_start + offset)
            .collect::<Vec<_>>();
        let mut glyphs = vec![glyph_at(0., 0), glyph_at(10., 1), glyph_at(20., 2)];
        glyphs.push(glyph_at(30., after_arabic));
        glyphs.extend(
            arabic_indices
                .iter()
                .rev()
                .enumerate()
                .map(|(position, index)| glyph_at(40. + position as f32 * 10., *index)),
        );
        glyphs.extend([
            glyph_at(90., after_arabic + 1),
            glyph_at(100., after_arabic + 2),
        ]);

        let mut layout = LineLayout {
            font_size: px(16.),
            width: px(110.),
            ascent: px(12.),
            descent: px(4.),
            runs: vec![ShapedRun {
                font_id: FontId(0),
                glyphs,
            }],
            len: text.len(),
            index_positions: Vec::new(),
            visual_index_positions: Vec::new(),
            direction: TextDirection::LeftToRight,
        };

        layout.compute_bidi_index_positions(text);

        assert_eq!(layout.index_for_x(px(89.)), Some(arabic_start));
        assert_eq!(layout.index_for_x(px(79.)), Some(arabic_second));
        assert_eq!(layout.index_for_x(px(49.)), Some(arabic_end));
    }
}
