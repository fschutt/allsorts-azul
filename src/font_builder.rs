//! Building a TrueType font from glyph outlines.
//!
//! The rest of this crate reads fonts, or writes fonts that are subsets of a font it has read.
//! This module writes a font that did not exist before: the caller supplies outlines, advance
//! widths and character mappings and gets the bytes of a `.ttf` file back. The file carries the
//! tables a font needs to be accepted by rasterizers, browsers and PDF viewers: `cmap`, `glyf`,
//! `head`, `hhea`, `hmtx`, `loca`, `maxp`, `name`, `OS/2` and `post`.
//!
//! Coordinates are in font units with the y axis pointing up and the baseline at `y = 0`.
//! Outlines may contain cubic Bézier curves; they are converted to the quadratic curves that
//! the `glyf` table stores.
//!
//! The output only depends on the input: there is no timestamp unless one is set, so the same
//! glyphs always give the same bytes.
//!
//! ```
//! use allsorts::font_builder::{Outline, TrueTypeBuilder};
//!
//! let mut square = Outline::new();
//! square.move_to(100.0, 0.0);
//! square.line_to(100.0, 700.0);
//! square.line_to(800.0, 700.0);
//! square.line_to(800.0, 0.0);
//! square.close();
//!
//! let mut builder = TrueTypeBuilder::new("Example", 1000);
//! builder.vertical_metrics(800, -200, 0);
//! builder.add_glyph(&['\u{E000}'], 900, square).unwrap();
//! let ttf: Vec<u8> = builder.build().unwrap();
//! assert_eq!(&ttf[0..4], &[0x00, 0x01, 0x00, 0x00]);
//! ```

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;

use pathfinder_geometry::line_segment::LineSegment2F;
use pathfinder_geometry::vector::Vector2F;

use crate::binary::read::ReadArrayCow;
use crate::error::{ParseError, ReadWriteError, WriteError};
use crate::outline::OutlineSink;
use crate::post::{Header as PostHeader, PostTable};
use crate::subset::{FontBuilder, TableFilter};
use crate::tables::cmap::owned::{
    Cmap, CmapSubtable, CmapSubtableFormat12, CmapSubtableFormat4, EncodingRecord,
};
use crate::tables::cmap::subset::{MappingsToKeep, NewIds};
use crate::tables::cmap::{EncodingId, PlatformId};
use crate::tables::glyf::{
    BoundingBox, GlyfRecord, GlyfTable, Glyph, Point, SimpleGlyph, SimpleGlyphFlag,
    SimpleGlyphFlags,
};
use crate::tables::os2::{FsSelectionFlag, Os2, Version0, Version1, Version2to4};
use crate::tables::owned::{NameRecord, NameTable};
use crate::tables::{
    Fixed, HeadTable, HheaTable, HmtxTable, IndexToLocFormat, LongHorMetric, MacStyle, MaxpTable,
    MaxpVersion1SubTable,
};
use crate::tag;

/// The largest coordinate an outline point may have, in font units.
///
/// The `glyf` table stores each point as a 16-bit difference to the point before it, so two
/// points must never be further apart than an `i16` can express. Keeping every coordinate in
/// half that range guarantees it.
pub const MAX_COORDINATE: i16 = 16383;

/// How far a quadratic curve may be from the cubic curve it replaces, in font units, unless
/// [`Outline::with_tolerance`] says otherwise.
pub const DEFAULT_CUBIC_TOLERANCE: f32 = 0.5;

/// A cubic curve is not split more often than this, whatever the tolerance.
const MAX_CUBIC_SPLITS: u32 = 8;

/// Seconds between 1904-01-01, where the `head` table counts from, and 1970-01-01.
const SECONDS_1904_TO_1970: i64 = 2_082_844_800;

/// Why a font could not be built.
#[derive(Debug)]
pub enum BuildError {
    /// More glyphs than a font can number (65535).
    TooManyGlyphs,
    /// The character is already mapped to an earlier glyph.
    DuplicateCharacter(char),
    /// A point of the glyph lies outside ±[`MAX_COORDINATE`].
    CoordinateOutOfRange {
        /// The id the glyph would have got.
        glyph_id: u16,
    },
    /// The glyph has more points or contours than a glyph can hold.
    TooManyPoints {
        /// The id the glyph would have got.
        glyph_id: u16,
    },
    /// A value did not fit the table it belongs to.
    Write(WriteError),
    /// A table could not be put together.
    Parse(ParseError),
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::TooManyGlyphs => write!(f, "a font holds at most 65535 glyphs"),
            BuildError::DuplicateCharacter(ch) => {
                write!(f, "U+{:04X} is mapped to more than one glyph", *ch as u32)
            }
            BuildError::CoordinateOutOfRange { glyph_id } => write!(
                f,
                "glyph {glyph_id} has a point outside ±{MAX_COORDINATE} font units"
            ),
            BuildError::TooManyPoints { glyph_id } => {
                write!(f, "glyph {glyph_id} has too many points or contours")
            }
            BuildError::Write(err) => write!(f, "write error: {err}"),
            BuildError::Parse(err) => write!(f, "table error: {err}"),
        }
    }
}

impl std::error::Error for BuildError {}

impl From<WriteError> for BuildError {
    fn from(err: WriteError) -> Self {
        BuildError::Write(err)
    }
}

impl From<ParseError> for BuildError {
    fn from(err: ParseError) -> Self {
        BuildError::Parse(err)
    }
}

impl From<ReadWriteError> for BuildError {
    fn from(err: ReadWriteError) -> Self {
        match err {
            ReadWriteError::Read(err) => BuildError::Parse(err),
            ReadWriteError::Write(err) => BuildError::Write(err),
        }
    }
}

/// One point of a contour: its position, and whether the outline passes through it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OutlinePoint {
    x: f32,
    y: f32,
    on_curve: bool,
}

/// The outline of one glyph: closed contours of lines and curves.
///
/// Fill follows the non-zero winding rule, as in every TrueType font: a hole must run in the
/// opposite direction to the contour it is cut out of. [`Outline::contour_area`] and
/// [`Outline::reverse_contour`] are there to put contours from another source in order.
///
/// `Outline` also implements [`OutlineSink`], so a glyph read from another font with this crate
/// can be drawn straight into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Outline {
    contours: Vec<Vec<OutlinePoint>>,
    current: Vec<OutlinePoint>,
    tolerance: f32,
}

impl Default for Outline {
    fn default() -> Self {
        Outline::new()
    }
}

impl Outline {
    /// An outline with no contours, which is what a space has.
    pub fn new() -> Self {
        Outline::with_tolerance(DEFAULT_CUBIC_TOLERANCE)
    }

    /// An outline whose cubic curves are replaced by quadratic ones no further than `tolerance`
    /// font units away. Values that are not positive fall back to the default.
    pub fn with_tolerance(tolerance: f32) -> Self {
        let tolerance = if tolerance.is_finite() && tolerance > 0.0 {
            tolerance
        } else {
            DEFAULT_CUBIC_TOLERANCE
        };
        Outline {
            contours: Vec::new(),
            current: Vec::new(),
            tolerance,
        }
    }

    /// Starts a new contour at a point. A contour that was not closed is closed first.
    pub fn move_to(&mut self, x: f32, y: f32) {
        self.finish_contour();
        self.current.push(OutlinePoint {
            x,
            y,
            on_curve: true,
        });
    }

    /// Draws a straight line to a point.
    pub fn line_to(&mut self, x: f32, y: f32) {
        self.ensure_started(x, y);
        self.current.push(OutlinePoint {
            x,
            y,
            on_curve: true,
        });
    }

    /// Draws a quadratic Bézier curve with one control point.
    pub fn quad_to(&mut self, ctrl_x: f32, ctrl_y: f32, x: f32, y: f32) {
        self.ensure_started(x, y);
        self.current.push(OutlinePoint {
            x: ctrl_x,
            y: ctrl_y,
            on_curve: false,
        });
        self.current.push(OutlinePoint {
            x,
            y,
            on_curve: true,
        });
    }

    /// Draws a cubic Bézier curve with two control points, as quadratic curves that stay within
    /// the outline's tolerance of it.
    pub fn cubic_to(
        &mut self,
        ctrl1_x: f32,
        ctrl1_y: f32,
        ctrl2_x: f32,
        ctrl2_y: f32,
        x: f32,
        y: f32,
    ) {
        self.ensure_started(x, y);
        // NOTE(unwrap): `ensure_started` leaves at least one point in the contour.
        let start = *self.current.last().unwrap();
        let mut quads = Vec::new();
        cubic_to_quadratics(
            (start.x, start.y),
            (ctrl1_x, ctrl1_y),
            (ctrl2_x, ctrl2_y),
            (x, y),
            self.tolerance,
            0,
            &mut quads,
        );
        for (ctrl, to) in quads {
            self.current.push(OutlinePoint {
                x: ctrl.0,
                y: ctrl.1,
                on_curve: false,
            });
            self.current.push(OutlinePoint {
                x: to.0,
                y: to.1,
                on_curve: true,
            });
        }
    }

    /// Closes the contour that is being drawn.
    pub fn close(&mut self) {
        self.finish_contour();
    }

    /// True if there is nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.contours.is_empty() && self.current.is_empty()
    }

    /// The number of contours, counting one that is still being drawn.
    pub fn contour_count(&self) -> usize {
        self.contours.len() + usize::from(!self.current.is_empty())
    }

    /// Twice-signed area of a contour, taking its control points as corners: positive if the
    /// contour runs counter-clockwise with y pointing up, negative if clockwise. `None` if there
    /// is no such contour.
    pub fn contour_area(&self, index: usize) -> Option<f32> {
        let contour = self.contour(index)?;
        let mut doubled = 0.0;
        for (i, a) in contour.iter().enumerate() {
            let b = contour[(i + 1) % contour.len()];
            doubled += a.x * b.y - b.x * a.y;
        }
        Some(doubled / 2.0)
    }

    /// Reverses the direction of a contour, which turns a filled shape into a hole and back.
    /// Does nothing if there is no such contour.
    pub fn reverse_contour(&mut self, index: usize) {
        let count = self.contours.len();
        let contour = if index < count {
            &mut self.contours[index]
        } else if index == count && !self.current.is_empty() {
            &mut self.current
        } else {
            return;
        };
        // The first point stays the first, so the contour still starts on the curve.
        if contour.len() > 2 {
            contour[1..].reverse();
        }
    }

    fn contour(&self, index: usize) -> Option<&Vec<OutlinePoint>> {
        let count = self.contours.len();
        if index < count {
            self.contours.get(index)
        } else if index == count && !self.current.is_empty() {
            Some(&self.current)
        } else {
            None
        }
    }

    fn ensure_started(&mut self, x: f32, y: f32) {
        if self.current.is_empty() {
            self.current.push(OutlinePoint {
                x,
                y,
                on_curve: true,
            });
        }
    }

    fn finish_contour(&mut self) {
        if !self.current.is_empty() {
            let contour = std::mem::take(&mut self.current);
            self.contours.push(contour);
        }
    }

    /// The contours as `glyf` points, or `None` for a point that does not fit.
    ///
    /// A contour is closed by the format itself, so a last point that repeats the first is
    /// dropped, as are points that repeat the one before them. What is left of a contour with
    /// fewer than three points encloses nothing and is dropped too.
    fn to_glyf_contours(&self) -> Option<Vec<Vec<(SimpleGlyphFlags, Point)>>> {
        let mut contours = Vec::new();
        let unfinished = (!self.current.is_empty()).then_some(&self.current);
        for contour in self.contours.iter().chain(unfinished) {
            let mut points: Vec<(bool, Point)> = Vec::with_capacity(contour.len());
            for point in contour {
                let rounded = Point(round_coordinate(point.x)?, round_coordinate(point.y)?);
                match points.last() {
                    Some(&(on_curve, last)) if last == rounded && on_curve && point.on_curve => {}
                    _ => points.push((point.on_curve, rounded)),
                }
            }
            while points.len() > 1 {
                let first = points[0];
                let last = points[points.len() - 1];
                if first.1 == last.1 && first.0 && last.0 {
                    points.pop();
                } else {
                    break;
                }
            }
            if points.len() < 3 {
                continue;
            }
            contours.push(
                points
                    .into_iter()
                    .map(|(on_curve, point)| {
                        let flags = if on_curve {
                            SimpleGlyphFlags::from(SimpleGlyphFlag::ON_CURVE_POINT)
                        } else {
                            SimpleGlyphFlags::empty()
                        };
                        (flags, point)
                    })
                    .collect(),
            );
        }
        Some(contours)
    }
}

impl OutlineSink for Outline {
    fn move_to(&mut self, to: Vector2F) {
        Outline::move_to(self, to.x(), to.y());
    }

    fn line_to(&mut self, to: Vector2F) {
        Outline::line_to(self, to.x(), to.y());
    }

    fn quadratic_curve_to(&mut self, ctrl: Vector2F, to: Vector2F) {
        Outline::quad_to(self, ctrl.x(), ctrl.y(), to.x(), to.y());
    }

    fn cubic_curve_to(&mut self, ctrl: LineSegment2F, to: Vector2F) {
        Outline::cubic_to(
            self,
            ctrl.from_x(),
            ctrl.from_y(),
            ctrl.to_x(),
            ctrl.to_y(),
            to.x(),
            to.y(),
        );
    }

    fn close(&mut self) {
        Outline::close(self);
    }
}

fn round_coordinate(value: f32) -> Option<i16> {
    let rounded = value.round();
    let limit = f32::from(MAX_COORDINATE);
    (rounded.is_finite() && rounded >= -limit && rounded <= limit).then_some(rounded as i16)
}

type Pt = (f32, f32);

fn midpoint(a: Pt, b: Pt) -> Pt {
    ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0)
}

/// Replaces one cubic curve by quadratic curves, each given as (control point, end point).
///
/// The quadratic curve with the control point `(3·(p1 + p2) − p0 − p3) / 4` differs from the
/// cubic by `(d / 2) · t · (1 − t) · (1 − 2t)` with `d = p3 − 3·p2 + 3·p1 − p0`, which is at
/// most `|d| · √3 / 36`. Halving the cubic divides `d` by eight, so a few splits are enough.
fn cubic_to_quadratics(
    p0: Pt,
    p1: Pt,
    p2: Pt,
    p3: Pt,
    tolerance: f32,
    depth: u32,
    out: &mut Vec<(Pt, Pt)>,
) {
    let dx = p3.0 - 3.0 * p2.0 + 3.0 * p1.0 - p0.0;
    let dy = p3.1 - 3.0 * p2.1 + 3.0 * p1.1 - p0.1;
    let error = (dx * dx + dy * dy).sqrt() * (3.0_f32.sqrt() / 36.0);
    if error <= tolerance || depth >= MAX_CUBIC_SPLITS || !error.is_finite() {
        let ctrl = (
            (3.0 * (p1.0 + p2.0) - p0.0 - p3.0) / 4.0,
            (3.0 * (p1.1 + p2.1) - p0.1 - p3.1) / 4.0,
        );
        out.push((ctrl, p3));
        return;
    }
    // de Casteljau at t = 1/2
    let p01 = midpoint(p0, p1);
    let p12 = midpoint(p1, p2);
    let p23 = midpoint(p2, p3);
    let p012 = midpoint(p01, p12);
    let p123 = midpoint(p12, p23);
    let mid = midpoint(p012, p123);
    cubic_to_quadratics(p0, p01, p012, mid, tolerance, depth + 1, out);
    cubic_to_quadratics(mid, p123, p23, p3, tolerance, depth + 1, out);
}

struct BuilderGlyph {
    advance_width: u16,
    contours: Vec<Vec<(SimpleGlyphFlags, Point)>>,
}

impl BuilderGlyph {
    fn bounding_box(&self) -> Option<BoundingBox> {
        let mut points = self.contours.iter().flatten().map(|(_, point)| *point);
        let Point(x, y) = points.next()?;
        let mut bbox = BoundingBox {
            x_min: x,
            x_max: x,
            y_min: y,
            y_max: y,
        };
        for Point(x, y) in points {
            bbox.x_min = bbox.x_min.min(x);
            bbox.x_max = bbox.x_max.max(x);
            bbox.y_min = bbox.y_min.min(y);
            bbox.y_max = bbox.y_max.max(y);
        }
        Some(bbox)
    }

    fn point_count(&self) -> usize {
        self.contours.iter().map(Vec::len).sum()
    }
}

/// Builds a TrueType font (`.ttf`, quadratic outlines in a `glyf` table) from scratch.
///
/// Glyph 0 is the glyph shown for characters the font does not have. The builder creates it
/// empty, half an em wide; [`TrueTypeBuilder::set_missing_glyph`] replaces it. Every glyph
/// added afterwards gets the next id.
pub struct TrueTypeBuilder {
    family_name: String,
    style_name: String,
    version: String,
    extra_names: BTreeMap<u16, String>,
    vendor_id: [u8; 4],
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    x_height: i16,
    cap_height: i16,
    created: i64,
    glyphs: Vec<BuilderGlyph>,
    mappings: BTreeMap<char, u16>,
}

impl TrueTypeBuilder {
    /// A font with the given family name and the given number of font units per em.
    ///
    /// The style is "Regular". Until [`TrueTypeBuilder::vertical_metrics`] is called the
    /// ascender is 80% of the em and the descender 20%.
    pub fn new(family_name: impl Into<String>, units_per_em: u16) -> Self {
        let units_per_em = units_per_em.clamp(16, 16384);
        let em = i32::from(units_per_em);
        TrueTypeBuilder {
            family_name: family_name.into(),
            style_name: String::from("Regular"),
            version: String::from("Version 1.000"),
            extra_names: BTreeMap::new(),
            vendor_id: *b"    ",
            units_per_em,
            ascender: (em * 4 / 5) as i16,
            descender: -(em / 5) as i16,
            line_gap: 0,
            x_height: 0,
            cap_height: 0,
            created: 0,
            glyphs: vec![BuilderGlyph {
                advance_width: units_per_em / 2,
                contours: Vec::new(),
            }],
            mappings: BTreeMap::new(),
        }
    }

    /// Sets the distance from the baseline to the top and to the bottom of a line (the
    /// descender is negative) and the extra space between lines. `hhea` and `OS/2` get the same
    /// values and `OS/2` tells applications to use them, so that a line is equally high
    /// everywhere.
    pub fn vertical_metrics(&mut self, ascender: i16, descender: i16, line_gap: i16) -> &mut Self {
        self.ascender = ascender;
        self.descender = descender;
        self.line_gap = line_gap;
        self
    }

    /// Sets the height of a lowercase x and of a capital letter, which `OS/2` records. Both are
    /// 0, meaning unknown, if this is not called.
    pub fn letter_heights(&mut self, x_height: i16, cap_height: i16) -> &mut Self {
        self.x_height = x_height;
        self.cap_height = cap_height;
        self
    }

    /// Sets the style name (name id 2). The default is "Regular".
    pub fn style_name(&mut self, style_name: impl Into<String>) -> &mut Self {
        self.style_name = style_name.into();
        self
    }

    /// Sets the version string (name id 5). The default is "Version 1.000".
    pub fn version(&mut self, version: impl Into<String>) -> &mut Self {
        self.version = version.into();
        self
    }

    /// Sets any other entry of the `name` table, for example the copyright notice (0), the
    /// licence text (13) or the licence URL (14). Ids 1 to 6 are written by the builder.
    pub fn name(&mut self, name_id: u16, value: impl Into<String>) -> &mut Self {
        self.extra_names.insert(name_id, value.into());
        self
    }

    /// Sets the four-character vendor id in `OS/2`. The default is four spaces.
    pub fn vendor_id(&mut self, vendor_id: [u8; 4]) -> &mut Self {
        self.vendor_id = vendor_id;
        self
    }

    /// Sets the creation and modification time in seconds since 1970-01-01. Without this call
    /// the font carries no time at all, and building it twice gives the same bytes.
    pub fn timestamp(&mut self, seconds_since_unix_epoch: i64) -> &mut Self {
        self.created = seconds_since_unix_epoch.saturating_add(SECONDS_1904_TO_1970);
        self
    }

    /// Replaces glyph 0, the glyph shown for characters the font does not have.
    pub fn set_missing_glyph(
        &mut self,
        advance_width: u16,
        outline: Outline,
    ) -> Result<(), BuildError> {
        let contours = Self::checked_contours(0, &outline)?;
        self.glyphs[0] = BuilderGlyph {
            advance_width,
            contours,
        };
        Ok(())
    }

    /// Adds a glyph and maps the given characters to it. Returns the glyph's id.
    ///
    /// `characters` may be empty for a glyph that is only reached by its id. An empty outline
    /// is a glyph that draws nothing, such as a space.
    pub fn add_glyph(
        &mut self,
        characters: &[char],
        advance_width: u16,
        outline: Outline,
    ) -> Result<u16, BuildError> {
        let glyph_id = u16::try_from(self.glyphs.len()).map_err(|_| BuildError::TooManyGlyphs)?;
        if glyph_id == u16::MAX {
            return Err(BuildError::TooManyGlyphs);
        }
        for (index, ch) in characters.iter().enumerate() {
            if self.mappings.contains_key(ch) || characters[..index].contains(ch) {
                return Err(BuildError::DuplicateCharacter(*ch));
            }
        }
        let contours = Self::checked_contours(glyph_id, &outline)?;
        self.glyphs.push(BuilderGlyph {
            advance_width,
            contours,
        });
        for ch in characters {
            self.mappings.insert(*ch, glyph_id);
        }
        Ok(glyph_id)
    }

    /// The number of glyphs so far, glyph 0 included.
    pub fn glyph_count(&self) -> u16 {
        // NOTE(cast): `add_glyph` refuses to go past `u16::MAX`.
        self.glyphs.len() as u16
    }

    fn checked_contours(
        glyph_id: u16,
        outline: &Outline,
    ) -> Result<Vec<Vec<(SimpleGlyphFlags, Point)>>, BuildError> {
        let contours = outline
            .to_glyf_contours()
            .ok_or(BuildError::CoordinateOutOfRange { glyph_id })?;
        let points: usize = contours.iter().map(Vec::len).sum();
        if points > usize::from(u16::MAX) || contours.len() > i16::MAX as usize {
            return Err(BuildError::TooManyPoints { glyph_id });
        }
        Ok(contours)
    }

    /// Writes the font.
    pub fn build(self) -> Result<Vec<u8>, BuildError> {
        let num_glyphs = u16::try_from(self.glyphs.len()).map_err(|_| BuildError::TooManyGlyphs)?;
        let em = i32::from(self.units_per_em);

        // Glyph outlines and the measurements the other tables need from them
        let mut records = Vec::with_capacity(self.glyphs.len());
        let mut h_metrics = Vec::with_capacity(self.glyphs.len());
        let mut font_bbox: Option<BoundingBox> = None;
        let mut advance_width_max = 0_u16;
        let mut min_left_side_bearing = i16::MAX;
        let mut min_right_side_bearing = i16::MAX;
        let mut x_max_extent = i16::MIN;
        let mut max_points = 0_u16;
        let mut max_contours = 0_u16;
        let mut advance_sum = 0_u64;
        let mut advance_count = 0_u64;

        for glyph in &self.glyphs {
            advance_width_max = advance_width_max.max(glyph.advance_width);
            if glyph.advance_width > 0 {
                advance_sum += u64::from(glyph.advance_width);
                advance_count += 1;
            }
            let Some(bbox) = glyph.bounding_box() else {
                records.push(GlyfRecord::empty());
                h_metrics.push(LongHorMetric {
                    advance_width: glyph.advance_width,
                    lsb: 0,
                });
                continue;
            };

            // NOTE(cast): `checked_contours` keeps both within these types.
            max_points = max_points.max(glyph.point_count() as u16);
            max_contours = max_contours.max(glyph.contours.len() as u16);
            let advance = i32::from(glyph.advance_width);
            let right_side_bearing = advance - i32::from(bbox.x_max);
            min_left_side_bearing = min_left_side_bearing.min(bbox.x_min);
            min_right_side_bearing = min_right_side_bearing.min(clamp_i16(right_side_bearing));
            x_max_extent = x_max_extent.max(bbox.x_max);
            font_bbox = Some(match font_bbox {
                None => bbox,
                Some(all) => BoundingBox {
                    x_min: all.x_min.min(bbox.x_min),
                    x_max: all.x_max.max(bbox.x_max),
                    y_min: all.y_min.min(bbox.y_min),
                    y_max: all.y_max.max(bbox.y_max),
                },
            });

            let mut end_pts_of_contours = Vec::with_capacity(glyph.contours.len());
            let mut coordinates = Vec::with_capacity(glyph.point_count());
            for contour in &glyph.contours {
                coordinates.extend_from_slice(contour);
                // NOTE(cast): at most `u16::MAX` points, and no contour is empty.
                end_pts_of_contours.push((coordinates.len() - 1) as u16);
            }
            records.push(GlyfRecord::Parsed(Glyph::Simple(SimpleGlyph {
                bounding_box: bbox,
                end_pts_of_contours,
                instructions: Box::new([]),
                coordinates,
                phantom_points: None,
            })));
            h_metrics.push(LongHorMetric {
                advance_width: glyph.advance_width,
                lsb: bbox.x_min,
            });
        }

        let font_bbox = font_bbox.unwrap_or_else(BoundingBox::empty);
        if min_left_side_bearing == i16::MAX {
            min_left_side_bearing = 0;
            min_right_side_bearing = 0;
            x_max_extent = 0;
        }

        let head = HeadTable {
            major_version: 1,
            minor_version: 0,
            font_revision: Fixed::from(1.0_f32),
            check_sum_adjustment: 0,
            magic_number: 0x5F0F3CF5,
            // Bit 0: the baseline is at y = 0. Bit 1: the left sidebearing point is at x = 0.
            flags: 0b11,
            units_per_em: self.units_per_em,
            created: self.created,
            modified: self.created,
            x_min: font_bbox.x_min,
            y_min: font_bbox.y_min,
            x_max: font_bbox.x_max,
            y_max: font_bbox.y_max,
            mac_style: MacStyle::empty(),
            lowest_rec_ppem: 6,
            font_direction_hint: 2,
            index_to_loc_format: IndexToLocFormat::Long,
            glyph_data_format: 0,
        };

        let hhea = HheaTable {
            ascender: self.ascender,
            descender: self.descender,
            line_gap: self.line_gap,
            advance_width_max,
            min_left_side_bearing,
            min_right_side_bearing,
            x_max_extent,
            caret_slope_rise: 1,
            caret_slope_run: 0,
            caret_offset: 0,
            num_h_metrics: num_glyphs,
        };

        let hmtx = HmtxTable {
            h_metrics: ReadArrayCow::Owned(h_metrics),
            left_side_bearings: ReadArrayCow::Owned(Vec::new()),
        };

        let maxp = MaxpTable {
            num_glyphs,
            version1_sub_table: Some(MaxpVersion1SubTable {
                max_points,
                max_contours,
                max_composite_points: 0,
                max_composite_contours: 0,
                // The glyphs carry no instructions, so only the glyph zone is used.
                max_zones: 1,
                max_twilight_points: 0,
                max_storage: 0,
                max_function_defs: 0,
                max_instruction_defs: 0,
                max_stack_elements: 0,
                max_size_of_instructions: 0,
                max_component_elements: 0,
                max_component_depth: 0,
            }),
        };

        let cmap = self.cmap_table()?;
        let name = self.name_table();
        let os2 = self.os2_table(&font_bbox, advance_sum, advance_count);

        let post = PostTable {
            header: PostHeader {
                // Version 3: no glyph names
                version: 0x00030000,
                italic_angle: 0,
                underline_position: clamp_i16(-em * 3 / 40),
                underline_thickness: clamp_i16(em / 20),
                is_fixed_pitch: 0,
                min_mem_type_42: 0,
                max_mem_type_42: 0,
                min_mem_type_1: 0,
                max_mem_type_1: 0,
            },
            opt_sub_table: None,
        };

        let glyf = GlyfTable::new(records)?;

        let mut builder = FontBuilder::new(0x00010000_u32, TableFilter::All);
        builder.add_table::<_, Cmap>(tag::CMAP, cmap, ())?;
        builder.add_table::<_, HheaTable>(tag::HHEA, &hhea, ())?;
        builder.add_table::<_, HmtxTable<'_>>(tag::HMTX, &hmtx, ())?;
        builder.add_table::<_, MaxpTable>(tag::MAXP, &maxp, ())?;
        builder.add_table::<_, NameTable<'_>>(tag::NAME, &name, ())?;
        builder.add_table::<_, Os2>(tag::OS_2, &os2, ())?;
        builder.add_table::<_, PostTable<'_>>(tag::POST, &post, ())?;
        let mut builder = builder.add_head_table(&head)?;
        builder.add_glyf_table(glyf)?;
        Ok(builder.data()?)
    }

    /// A Unicode `cmap`: format 4 for the Basic Multilingual Plane, listed for both the Unicode
    /// and the Windows platform, and format 12 as well if a character lies beyond it.
    fn cmap_table(&self) -> Result<Cmap, BuildError> {
        let bmp = MappingsToKeep::<NewIds>::from_new_ids(
            self.mappings
                .iter()
                .filter(|(ch, _)| **ch <= '\u{FFFF}')
                .map(|(ch, gid)| (*ch, *gid)),
        );
        let format4 = CmapSubtableFormat4::from_mappings(&bmp)?;
        let beyond_bmp = self.mappings.keys().any(|ch| *ch > '\u{FFFF}');
        let format12 = beyond_bmp.then(|| {
            let all = MappingsToKeep::<NewIds>::from_new_ids(
                self.mappings.iter().map(|(ch, gid)| (*ch, *gid)),
            );
            CmapSubtableFormat12::from_mappings(&all)
        });

        // Encoding records are ordered by platform id, then encoding id.
        let mut encoding_records = vec![EncodingRecord {
            platform_id: PlatformId::UNICODE,
            encoding_id: EncodingId::UNICODE_BMP,
            sub_table: CmapSubtable::Format4(format4.clone()),
        }];
        if let Some(format12) = &format12 {
            encoding_records.push(EncodingRecord {
                platform_id: PlatformId::UNICODE,
                encoding_id: EncodingId::UNICODE_FULL,
                sub_table: CmapSubtable::Format12(format12.clone()),
            });
        }
        encoding_records.push(EncodingRecord {
            platform_id: PlatformId::WINDOWS,
            encoding_id: EncodingId::WINDOWS_UNICODE_BMP_UCS2,
            sub_table: CmapSubtable::Format4(format4),
        });
        if let Some(format12) = format12 {
            encoding_records.push(EncodingRecord {
                platform_id: PlatformId::WINDOWS,
                encoding_id: EncodingId::WINDOWS_UNICODE_UCS4,
                sub_table: CmapSubtable::Format12(format12),
            });
        }
        Ok(Cmap { encoding_records })
    }

    /// The names every application looks for, as Windows Unicode records in US English.
    fn name_table(&self) -> NameTable<'static> {
        let full_name = if self.style_name == "Regular" {
            self.family_name.clone()
        } else {
            format!("{} {}", self.family_name, self.style_name)
        };
        let postscript_name: String = format!("{}-{}", self.family_name, self.style_name)
            .chars()
            .filter(|ch| ch.is_ascii_graphic() && !"[](){}<>/%".contains(*ch))
            .take(63)
            .collect();

        let mut names = self.extra_names.clone();
        names.insert(1, self.family_name.clone());
        names.insert(2, self.style_name.clone());
        names.insert(3, format!("{};{}", self.version, postscript_name));
        names.insert(4, full_name);
        names.insert(5, self.version.clone());
        names.insert(6, postscript_name);

        // `names` is ordered by name id, which is the order the records must be in.
        let name_records = names
            .into_iter()
            .map(|(name_id, value)| NameRecord {
                platform_id: 3,      // Windows
                encoding_id: 1,      // Unicode BMP
                language_id: 0x0409, // English, United States
                name_id,
                string: Cow::from(utf16be(&value)),
            })
            .collect();
        NameTable {
            name_records,
            langtag_records: Vec::new(),
        }
    }

    fn os2_table(&self, font_bbox: &BoundingBox, advance_sum: u64, advance_count: u64) -> Os2 {
        let em = i32::from(self.units_per_em);
        let first_char = self.mappings.keys().next().map_or(0, |ch| *ch as u32);
        let last_char = self.mappings.keys().next_back().map_or(0, |ch| *ch as u32);

        // Which blocks the font has characters in: only the ones a font made of symbols or of
        // Latin text is likely to need are worked out.
        let has = |from: u32, to: u32| {
            self.mappings
                .keys()
                .any(|ch| (from..=to).contains(&(*ch as u32)))
        };
        let mut unicode_range1 = 0_u32;
        let mut unicode_range2 = 0_u32;
        if has(0x0020, 0x007E) {
            unicode_range1 |= 1 << 0; // Basic Latin
        }
        if has(0x00A0, 0x00FF) {
            unicode_range1 |= 1 << 1; // Latin-1 Supplement
        }
        if has(0x1_0000, 0x10_FFFF) {
            unicode_range2 |= 1 << (57 - 32); // Non-Plane 0
        }
        if has(0xE000, 0xF8FF) {
            unicode_range2 |= 1 << (60 - 32); // Private Use Area (plane 0)
        }

        Os2 {
            version: 4,
            x_avg_char_width: clamp_i16(advance_sum.checked_div(advance_count).unwrap_or(0) as i32),
            us_weight_class: 400,
            us_width_class: 5,
            // 0: the font may be embedded and installed without restriction.
            fs_type: 0,
            y_subscript_x_size: clamp_i16(em * 13 / 20),
            y_subscript_y_size: clamp_i16(em * 3 / 5),
            y_subscript_x_offset: 0,
            y_subscript_y_offset: clamp_i16(em * 3 / 40),
            y_superscript_x_size: clamp_i16(em * 13 / 20),
            y_superscript_y_size: clamp_i16(em * 3 / 5),
            y_superscript_x_offset: 0,
            y_superscript_y_offset: clamp_i16(em * 7 / 20),
            y_strikeout_size: clamp_i16(em / 20),
            y_strikeout_position: clamp_i16(em * 13 / 50),
            s_family_class: 0,
            panose: [0; 10],
            ul_unicode_range1: unicode_range1,
            ul_unicode_range2: unicode_range2,
            ul_unicode_range3: 0,
            ul_unicode_range4: 0,
            ach_vend_id: u32::from_be_bytes(self.vendor_id),
            fs_selection: FsSelectionFlag::REGULAR | FsSelectionFlag::USE_TYPO_METRICS,
            us_first_char_index: first_char.min(0xFFFF) as u16,
            us_last_char_index: last_char.min(0xFFFF) as u16,
            version0: Some(Version0 {
                s_typo_ascender: self.ascender,
                s_typo_descender: self.descender,
                s_typo_line_gap: self.line_gap,
                // The Windows values clip, so they also cover what sticks out of the line.
                us_win_ascent: self.ascender.max(font_bbox.y_max).max(0) as u16,
                us_win_descent: i32::from(self.descender)
                    .min(i32::from(font_bbox.y_min))
                    .min(0)
                    .unsigned_abs() as u16,
            }),
            version1: Some(Version1 {
                ul_code_page_range1: 1, // Latin 1
                ul_code_page_range2: 0,
            }),
            version2to4: Some(Version2to4 {
                s_x_height: self.x_height,
                s_cap_height: self.cap_height,
                us_default_char: 0,
                us_break_char: 32,
                us_max_context: 0,
            }),
            version5: None,
        }
    }
}

fn clamp_i16(value: i32) -> i16 {
    value.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

fn utf16be(string: &str) -> Vec<u8> {
    string
        .encode_utf16()
        .flat_map(|unit| unit.to_be_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::read::ReadScope;
    use crate::font::{Font, MatchingPresentation};
    use crate::font_data::FontData;
    use crate::tables::glyf::GlyfTable;
    use crate::tables::loca::LocaTable;
    use crate::tables::{FontTableProvider, NameTable as ReadNameTable};

    fn square(left: f32, bottom: f32, size: f32) -> Outline {
        let mut outline = Outline::new();
        outline.move_to(left, bottom);
        outline.line_to(left, bottom + size);
        outline.line_to(left + size, bottom + size);
        outline.line_to(left + size, bottom);
        outline.close();
        outline
    }

    /// A circle of four cubic curves, the way a drawing program writes one.
    fn circle(cx: f32, cy: f32, r: f32) -> Outline {
        let k = 0.552_284_8 * r;
        let mut outline = Outline::new();
        outline.move_to(cx + r, cy);
        outline.cubic_to(cx + r, cy - k, cx + k, cy - r, cx, cy - r);
        outline.cubic_to(cx - k, cy - r, cx - r, cy - k, cx - r, cy);
        outline.cubic_to(cx - r, cy + k, cx - k, cy + r, cx, cy + r);
        outline.cubic_to(cx + k, cy + r, cx + r, cy + k, cx + r, cy);
        outline.close();
        outline
    }

    fn example_font() -> Vec<u8> {
        let mut builder = TrueTypeBuilder::new("Builder Test", 1000);
        builder.vertical_metrics(800, -200, 0);
        builder.name(0, "No rights reserved");
        builder.add_glyph(&[' '], 250, Outline::new()).unwrap();
        builder
            .add_glyph(&['A', '\u{E000}'], 900, square(100.0, 0.0, 700.0))
            .unwrap();
        builder
            .add_glyph(&['\u{E001}'], 600, circle(300.0, 250.0, 250.0))
            .unwrap();
        builder.build().unwrap()
    }

    #[test]
    fn cubic_is_split_until_it_is_within_tolerance() {
        // A quarter circle of radius 1000. Here d = (343, -343), so a single quadratic would be
        // 485 · √3 / 36 = 23 units off. Each halving divides that by eight: 2.9, then 0.36,
        // which is within the tolerance of 0.5. That makes four quadratics.
        let k = 552.284_8;
        let mut quads = Vec::new();
        cubic_to_quadratics(
            (1000.0, 0.0),
            (1000.0, k),
            (k, 1000.0),
            (0.0, 1000.0),
            0.5,
            0,
            &mut quads,
        );
        assert_eq!(quads.len(), 4);
        assert_eq!(quads.last().unwrap().1, (0.0, 1000.0));

        // Every on-curve point that the split produced lies on the circle.
        for (_, (x, y)) in &quads {
            let radius = (x * x + y * y).sqrt();
            assert!((radius - 1000.0).abs() < 1.0, "radius {radius}");
        }

        // A cubic that is a quadratic in disguise needs no split at all.
        let (p0, c, p3) = ((0.0, 0.0), (300.0, 600.0), (600.0, 0.0));
        let p1 = (
            p0.0 + 2.0 / 3.0 * (c.0 - p0.0),
            p0.1 + 2.0 / 3.0 * (c.1 - p0.1),
        );
        let p2 = (
            p3.0 + 2.0 / 3.0 * (c.0 - p3.0),
            p3.1 + 2.0 / 3.0 * (c.1 - p3.1),
        );
        let mut quads = Vec::new();
        cubic_to_quadratics(p0, p1, p2, p3, 0.5, 0, &mut quads);
        assert_eq!(quads.len(), 1);
        assert!((quads[0].0 .0 - 300.0).abs() < 0.01 && (quads[0].0 .1 - 600.0).abs() < 0.01);
    }

    #[test]
    fn closing_point_and_repeated_points_are_dropped() {
        let mut outline = Outline::new();
        outline.move_to(0.0, 0.0);
        outline.line_to(0.0, 100.0);
        outline.line_to(0.0, 100.2); // rounds to the point before it
        outline.line_to(100.0, 100.0);
        outline.line_to(0.0, 0.0); // back at the start
        outline.close();
        let contours = outline.to_glyf_contours().unwrap();
        assert_eq!(contours.len(), 1);
        assert_eq!(contours[0].len(), 3);
    }

    #[test]
    fn contour_direction_can_be_read_and_reversed() {
        // Up, right, down, left with y pointing up is clockwise: negative area.
        let mut outline = square(0.0, 0.0, 10.0);
        assert_eq!(outline.contour_area(0), Some(-100.0));
        outline.reverse_contour(0);
        assert_eq!(outline.contour_area(0), Some(100.0));
        assert_eq!(outline.contour_area(1), None);
    }

    #[test]
    fn out_of_range_and_duplicate_input_is_refused() {
        let mut builder = TrueTypeBuilder::new("Builder Test", 1000);
        let far = square(0.0, 0.0, 20000.0);
        assert!(matches!(
            builder.add_glyph(&['a'], 500, far),
            Err(BuildError::CoordinateOutOfRange { glyph_id: 1 })
        ));
        builder
            .add_glyph(&['a'], 500, square(0.0, 0.0, 500.0))
            .unwrap();
        assert!(matches!(
            builder.add_glyph(&['a'], 500, square(0.0, 0.0, 500.0)),
            Err(BuildError::DuplicateCharacter('a'))
        ));
        assert_eq!(builder.glyph_count(), 2);
    }

    #[test]
    fn building_twice_gives_the_same_bytes() {
        assert_eq!(example_font(), example_font());
    }

    #[test]
    fn built_font_reads_back() {
        let data = example_font();
        let font_file = ReadScope::new(&data).read::<FontData<'_>>().unwrap();
        let provider = font_file.table_provider(0).unwrap();

        // The checksum of the whole file is the constant the specification asks for.
        assert_eq!(
            crate::checksum::table_checksum(&data).unwrap().0,
            0xB1B0AFBA
        );

        let head_data = provider.read_table_data(tag::HEAD).unwrap();
        let head = ReadScope::new(&head_data).read::<HeadTable>().unwrap();
        assert_eq!(head.units_per_em, 1000);
        assert_eq!(
            (head.x_min, head.y_min, head.x_max, head.y_max),
            (50, 0, 800, 700)
        );

        let maxp_data = provider.read_table_data(tag::MAXP).unwrap();
        let maxp = ReadScope::new(&maxp_data).read::<MaxpTable>().unwrap();
        assert_eq!(maxp.num_glyphs, 4);

        let hhea_data = provider.read_table_data(tag::HHEA).unwrap();
        let hhea = ReadScope::new(&hhea_data).read::<HheaTable>().unwrap();
        assert_eq!((hhea.ascender, hhea.descender), (800, -200));
        assert_eq!(hhea.advance_width_max, 900);

        let hmtx_data = provider.read_table_data(tag::HMTX).unwrap();
        let hmtx = ReadScope::new(&hmtx_data)
            .read_dep::<HmtxTable<'_>>((
                usize::from(maxp.num_glyphs),
                usize::from(hhea.num_h_metrics),
            ))
            .unwrap();
        assert_eq!(hmtx.horizontal_advance(1).unwrap(), 250);
        assert_eq!(hmtx.horizontal_advance(2).unwrap(), 900);
        assert_eq!(hmtx.horizontal_advance(3).unwrap(), 600);

        let loca_data = provider.read_table_data(tag::LOCA).unwrap();
        let loca = ReadScope::new(&loca_data)
            .read_dep::<LocaTable<'_>>((maxp.num_glyphs, head.index_to_loc_format))
            .unwrap();
        let glyf_data = provider.read_table_data(tag::GLYF).unwrap();
        let mut glyf = ReadScope::new(&glyf_data)
            .read_dep::<GlyfTable<'_>>(&loca)
            .unwrap();
        assert_eq!(glyf.num_glyphs(), 4);
        match glyf.get_parsed_glyph(2).unwrap() {
            Glyph::Simple(glyph) => {
                assert_eq!(glyph.end_pts_of_contours, vec![3]);
                assert_eq!(glyph.coordinates.len(), 4);
                assert_eq!(glyph.bounding_box.x_max, 800);
            }
            other => panic!("glyph 2 is not a simple glyph: {other:?}"),
        }
        match glyf.get_parsed_glyph(3).unwrap() {
            Glyph::Simple(glyph) => {
                // Four cubic quarter circles, each split into quadratic curves
                assert!(
                    glyph.coordinates.len() >= 16,
                    "{} points",
                    glyph.coordinates.len()
                );
                assert_eq!(
                    (glyph.bounding_box.x_min, glyph.bounding_box.x_max),
                    (50, 550)
                );
            }
            other => panic!("glyph 3 is not a simple glyph: {other:?}"),
        }
        assert!(matches!(glyf.get_parsed_glyph(1).unwrap(), Glyph::Empty(_)));

        let name_data = provider.read_table_data(tag::NAME).unwrap();
        let name = ReadScope::new(&name_data)
            .read::<ReadNameTable<'_>>()
            .unwrap();
        assert_eq!(name.string_for_id(1).as_deref(), Some("Builder Test"));
        assert_eq!(name.string_for_id(0).as_deref(), Some("No rights reserved"));
        assert_eq!(
            name.string_for_id(6).as_deref(),
            Some("BuilderTest-Regular")
        );

        // The font is usable through the crate's own entry point, and maps its characters.
        let mut font = Font::new(provider).unwrap();
        let glyph_for = |font: &mut Font<_>, ch: char| {
            font.lookup_glyph_index(ch, MatchingPresentation::NotRequired, None)
                .0
        };
        assert_eq!(glyph_for(&mut font, ' '), 1);
        assert_eq!(glyph_for(&mut font, 'A'), 2);
        assert_eq!(glyph_for(&mut font, '\u{E000}'), 2);
        assert_eq!(glyph_for(&mut font, '\u{E001}'), 3);
        assert_eq!(glyph_for(&mut font, 'B'), 0);
    }

    #[test]
    fn characters_beyond_the_bmp_get_a_format_12_table() {
        let mut builder = TrueTypeBuilder::new("Builder Test", 1000);
        builder
            .add_glyph(&['\u{E000}'], 500, square(0.0, 0.0, 500.0))
            .unwrap();
        builder
            .add_glyph(&['\u{F0000}'], 500, square(0.0, 0.0, 400.0))
            .unwrap();
        let data = builder.build().unwrap();
        let font_file = ReadScope::new(&data).read::<FontData<'_>>().unwrap();
        let provider = font_file.table_provider(0).unwrap();
        let mut font = Font::new(provider).unwrap();
        let (glyph, _) =
            font.lookup_glyph_index('\u{F0000}', MatchingPresentation::NotRequired, None);
        assert_eq!(glyph, 2);
        let (glyph, _) =
            font.lookup_glyph_index('\u{E000}', MatchingPresentation::NotRequired, None);
        assert_eq!(glyph, 1);
    }
}
