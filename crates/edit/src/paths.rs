//! Painted paths already on a page: rectangles and general paths in the page content stream.
//!
//! A clip that is not painted (`re W* n`, including the full-page clip Word puts around a text
//! run) is not an object. A path that also clips is deleted or moved only when nothing else is
//! painted under that clip; otherwise the edit is refused and the stream stays as it was.
//! Forms are not walked: a Form is still one object until Ungroup.

use std::collections::BTreeMap;
use std::sync::Arc;

use kurbo::{BezPath, Point, Shape};
use pdfcraft_content::{Matrix, Op};
use pdfcraft_cos::{Document, Object};

use crate::EditError;
use crate::text::{Read, content_streams, joined, rewritten_contents, splice};

/// Painted paths kept from one page. Further paths are left out of the inventory so a ruled
/// table cannot by itself make the page's text unselectable.
const MAX_PATHS: usize = 10_000;
const MAX_SAVES: usize = 1024;
const MAX_ELEMENTS: usize = 16_384;
const MAX_CLIPS: usize = 256;
/// A stroke thinner than this is still clickable: half a hairline is smaller than a point.
const MIN_STROKE: f64 = 1.0;

/// What [`edit_paths`] does to one or more painted paths.
#[derive(Clone, Debug, PartialEq)]
pub enum PathEdit {
    /// Drop the paint. A path that also clips later content is refused.
    Delete,
    /// DeviceRGB fill and/or stroke, written in a `q`/`Q` around the paint so neighbours keep
    /// their colours. A pattern, shading or other non-device colour is refused.
    Recolor { fill: Option<[f64; 3]>, stroke: Option<[f64; 3]> },
}

/// One painted path, in content order. `index` is its place in [`page_paths`].
#[derive(Clone, Debug)]
pub struct PagePath {
    pub index: usize,
    /// User-space bounds, inflated by the stroke and clipped to the crop box.
    pub rect: [f64; 4],
    /// Fill as RGB when the path fills in a device colour.
    pub fill: Option<[f64; 3]>,
    /// Stroke as RGB when the path strokes in a device colour.
    pub stroke: Option<[f64; 3]>,
    /// The paint operator's index among the page's joined operators. Later paints are on top.
    pub paint: usize,
    pub(crate) first: usize,
    pub(crate) clip_ops: Vec<usize>,
    pub(crate) clips_content: bool,
    pub(crate) exclusive: bool,
    pub(crate) fills: bool,
    pub(crate) strokes: bool,
    pub(crate) fill_device: bool,
    pub(crate) stroke_device: bool,
    even_odd: bool,
    width: f64,
    path: BezPath,
    clips: Vec<(BezPath, bool)>,
}

impl PagePath {
    /// Whether `point` (user space) is inside the fill or on the stroke, and inside every clip.
    pub fn contains(&self, point: [f64; 2]) -> bool {
        if !point.iter().all(|v| v.is_finite()) {
            return false;
        }
        let [x0, y0, x1, y1] = self.rect;
        if point[0] < x0 || point[0] > x1 || point[1] < y0 || point[1] > y1 {
            return false;
        }
        let pt = Point::new(point[0], point[1]);
        if self.clips.iter().any(|(path, even)| !filled(path, *even, pt)) {
            return false;
        }
        let filled = self.fills && filled(&self.path, self.even_odd, pt);
        let stroked = self.strokes && on_stroke(&self.path, self.width, pt);
        filled || stroked
    }

    pub fn paints_fill(&self) -> bool {
        self.fills
    }

    pub fn paints_stroke(&self) -> bool {
        self.strokes
    }
}

/// Painted paths on `page` (0-based), in content order. Clip-only paths are omitted. A stream
/// that did not decode whole refuses the page: the graphics state after it is not reliable.
pub fn page_paths(doc: &Document, page: usize) -> Result<Vec<PagePath>, EditError> {
    let pages = pdfcraft_model::pages(doc);
    let p = pages.get(page).ok_or(EditError::NoSuchPage(page))?;
    let streams = content_streams(doc, &p.dict)?;
    if streams.iter().any(|s| s.read != Read::Whole) {
        return Err(EditError::Invalid("page content could not be completely decoded; its paths cannot be edited safely".into()));
    }
    let ops = joined(&streams).parse().ops;
    Ok(collect(&ops, p.crop(doc)))
}

/// Delete or recolor `indexes` (from [`page_paths`]) as one change. A refusal leaves the
/// document unchanged.
pub fn edit_paths(doc: &mut Document, page: usize, indexes: &[usize], change: &PathEdit) -> Result<(), EditError> {
    if indexes.is_empty() || indexes.len() > crate::objects::MAX_MOVE_OBJECTS {
        return Err(EditError::Invalid("select between 1 and 1000 paths".into()));
    }
    let change = Change::from_edit(change)?;
    let mut work = doc.clone();
    apply(&mut work, page, indexes, &change)?;
    *doc = work;
    Ok(())
}

/// The paint operator of each path `indexes`, for Copy.
pub(crate) fn path_ops(doc: &Document, page: usize, indexes: &[usize]) -> Result<Vec<usize>, EditError> {
    if indexes.is_empty() {
        return Ok(Vec::new());
    }
    let paths = page_paths(doc, page)?;
    indexes
        .iter()
        .map(|i| {
            paths
                .get(*i)
                .map(|p| p.paint)
                .ok_or_else(|| EditError::Invalid("a selected path no longer exists; select the objects again".into()))
        })
        .collect()
}

/// Move paths by a user-space vector. The caller discards `doc` if this returns an error.
pub(crate) fn translate_paths(doc: &mut Document, page: usize, indexes: &[usize], offset: [f64; 2]) -> Result<(), EditError> {
    if indexes.is_empty() {
        return Ok(());
    }
    apply(doc, page, indexes, &Change::Move(offset))
}

enum Change {
    Delete,
    Recolor { fill: Option<[f64; 3]>, stroke: Option<[f64; 3]> },
    Move([f64; 2]),
}

impl Change {
    fn from_edit(edit: &PathEdit) -> Result<Self, EditError> {
        match edit {
            PathEdit::Delete => Ok(Self::Delete),
            PathEdit::Recolor { fill, stroke } => {
                for rgb in [fill, stroke].into_iter().flatten() {
                    if !rgb.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) {
                        return Err(EditError::Invalid("the colour must be RGB components from 0 to 1".into()));
                    }
                }
                if fill.is_none() && stroke.is_none() {
                    return Err(EditError::Invalid("choose a fill or a stroke colour".into()));
                }
                Ok(Self::Recolor { fill: *fill, stroke: *stroke })
            }
        }
    }
}

fn apply(doc: &mut Document, page: usize, indexes: &[usize], change: &Change) -> Result<(), EditError> {
    let mut indexes = indexes.to_vec();
    indexes.sort_unstable();
    indexes.dedup();
    indexes.reverse();
    for index in indexes {
        edit_one(doc, page, index, change)?;
    }
    Ok(())
}

fn edit_one(doc: &mut Document, page: usize, index: usize, change: &Change) -> Result<(), EditError> {
    let paths = page_paths(doc, page)?;
    let path = paths.get(index).ok_or_else(|| EditError::Invalid("a selected path no longer exists; select the objects again".into()))?;
    let plan = plan_of(path);
    let pages = pdfcraft_model::pages(doc);
    let p = pages.get(page).ok_or(EditError::NoSuchPage(page))?;
    let streams = content_streams(doc, &p.dict)?;
    let pieces = joined(&streams);
    let ops = pieces.parse().ops;
    let edits = changes(&ops, &plan, change)?;
    let changed = splice(&pieces, &ops, |i| edits.get(&i).cloned().unwrap_or_else(|| (Vec::new(), true)));
    let after = pdfcraft_content::parse(&changed.concat()).ops;
    if !same_balance(&ops, &after) {
        return Err(EditError::Invalid("rewriting this path would unbalance the content stream".into()));
    }
    let contents = rewritten_contents(doc, &streams, changed)?;
    doc.update_dict(p.obj, |d| d.set(b"Contents".to_vec(), contents))?;
    Ok(())
}

struct Plan {
    paint: usize,
    first: usize,
    clip_ops: Vec<usize>,
    clips_content: bool,
    exclusive: bool,
    fills: bool,
    strokes: bool,
    fill_device: bool,
    stroke_device: bool,
}

fn plan_of(path: &PagePath) -> Plan {
    Plan {
        paint: path.paint,
        first: path.first,
        clip_ops: path.clip_ops.clone(),
        clips_content: path.clips_content,
        exclusive: path.exclusive,
        fills: path.fills,
        strokes: path.strokes,
        fill_device: path.fill_device,
        stroke_device: path.stroke_device,
    }
}

fn changes(ops: &[Op], path: &Plan, change: &Change) -> Result<BTreeMap<usize, (Vec<Op>, bool)>, EditError> {
    let stale = || EditError::Invalid("a selected path no longer exists; select the objects again".into());
    let mut map = BTreeMap::new();
    match change {
        Change::Delete => {
            if path.clips_content && !path.exclusive {
                return Err(EditError::Invalid("this path also clips other content, so it cannot be deleted".into()));
            }
            if path.exclusive {
                for i in &path.clip_ops {
                    map.insert(*i, (Vec::new(), false));
                }
            }
            if !ops.get(path.paint).is_some_and(is_path_paint) {
                return Err(stale());
            }
            map.insert(path.paint, (vec![Op::new("n", Vec::new())], false));
        }
        Change::Recolor { fill, stroke } => {
            if fill.is_some() && !path.fills {
                return Err(EditError::Invalid("this path has no fill to recolour".into()));
            }
            if stroke.is_some() && !path.strokes {
                return Err(EditError::Invalid("this path has no stroke to recolour".into()));
            }
            if fill.is_some() && !path.fill_device || stroke.is_some() && !path.stroke_device {
                return Err(EditError::Invalid("this path uses a colour that cannot be changed yet; it can still be deleted".into()));
            }
            let paint = ops.get(path.paint).cloned().filter(|op| is_path_paint(op)).ok_or_else(stale)?;
            let mut seq = vec![Op::new("q", Vec::new())];
            if let Some(rgb) = fill {
                seq.push(Op::new("rg", rgb.map(pdfcraft_content::num).to_vec()));
            }
            if let Some(rgb) = stroke {
                seq.push(Op::new("RG", rgb.map(pdfcraft_content::num).to_vec()));
            }
            seq.push(paint);
            seq.push(Op::new("Q", Vec::new()));
            map.insert(path.paint, (seq, false));
        }
        Change::Move(offset) => {
            if path.clips_content && !path.exclusive {
                return Err(EditError::Invalid("this path also clips other content, so it cannot be moved".into()));
            }
            if path.first > path.paint || ops.get(path.paint).is_none() {
                return Err(stale());
            }
            for i in path.first..path.paint {
                let op = ops.get(i).ok_or_else(stale)?;
                if matches!(op.op.as_slice(), b"q" | b"Q" | b"BT" | b"ET" | b"BDC" | b"BMC" | b"EMC") {
                    return Err(EditError::Invalid(
                        "this path is interrupted by a graphics-state or text boundary, so it cannot be moved".into(),
                    ));
                }
            }
            let mut seq = vec![
                Op::new("q", Vec::new()),
                Op::new("cm", [1.0, 0.0, 0.0, 1.0, offset[0], offset[1]].map(pdfcraft_content::num).to_vec()),
            ];
            for i in path.first..=path.paint {
                seq.push(ops.get(i).cloned().ok_or_else(stale)?);
            }
            seq.push(Op::new("Q", Vec::new()));
            map.insert(path.first, (seq, false));
            for i in path.first + 1..=path.paint {
                map.insert(i, (Vec::new(), false));
            }
        }
    }
    Ok(map)
}

fn is_path_paint(op: &Op) -> bool {
    matches!(op.op.as_slice(), b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*")
}

fn same_balance(before: &[Op], after: &[Op]) -> bool {
    tally(before) == tally(after)
}

/// `(q depth, text-object depth, marked-content depth)` when none goes negative and text objects
/// do not nest; `None` when the stream was already unbalanced in that way.
fn tally(ops: &[Op]) -> Option<(i32, i32, i32)> {
    let (mut q, mut text, mut marked) = (0i32, 0i32, 0i32);
    for op in ops {
        match op.op.as_slice() {
            b"q" => q += 1,
            b"Q" => {
                q -= 1;
                if q < 0 {
                    return None;
                }
            }
            b"BT" => {
                text += 1;
                if text > 1 {
                    return None;
                }
            }
            b"ET" => {
                text -= 1;
                if text < 0 {
                    return None;
                }
            }
            b"BDC" | b"BMC" => marked += 1,
            b"EMC" => {
                marked -= 1;
                if marked < 0 {
                    return None;
                }
            }
            _ => {}
        }
    }
    Some((q, text, marked))
}

fn collect(ops: &[Op], crop: [f64; 4]) -> Vec<PagePath> {
    let mut walk = Walk::new();
    let mut out = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        if out.len() >= MAX_PATHS {
            break;
        }
        if let Some(mut path) = walk.step(index, op, ops, crop) {
            path.index = out.len();
            out.push(path);
        }
    }
    out
}

#[derive(Clone)]
struct Clip {
    path: Arc<BezPath>,
    even_odd: bool,
}

#[derive(Clone)]
enum Ink {
    Device(Vec<f64>),
    Other,
}

#[derive(Clone, Copy)]
enum Space {
    Device(usize),
    Other,
}

#[derive(Clone)]
struct Gfx {
    ctm: Matrix,
    fill: Ink,
    stroke: Ink,
    fill_space: Space,
    stroke_space: Space,
    width: f64,
    clips: Vec<Clip>,
}

impl Gfx {
    fn initial() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
            fill: Ink::Device(vec![0.0]),
            stroke: Ink::Device(vec![0.0]),
            fill_space: Space::Device(1),
            stroke_space: Space::Device(1),
            width: 1.0,
            clips: Vec::new(),
        }
    }
}

struct Walk {
    gfx: Gfx,
    stack: Vec<Gfx>,
    /// `q` past [`MAX_SAVES`]: the saved state is no longer tracked, so a path there is skipped.
    lost: usize,
    path: BezPath,
    sub_start: Option<Point>,
    first: Option<usize>,
    pending_clip: Option<bool>,
    pending_clip_ops: Vec<usize>,
    in_text: bool,
}

impl Walk {
    fn new() -> Self {
        Self {
            gfx: Gfx::initial(),
            stack: Vec::new(),
            lost: 0,
            path: BezPath::new(),
            sub_start: None,
            first: None,
            pending_clip: None,
            pending_clip_ops: Vec::new(),
            in_text: false,
        }
    }

    fn step(&mut self, index: usize, op: &Op, ops: &[Op], crop: [f64; 4]) -> Option<PagePath> {
        if self.path.elements().len() > MAX_ELEMENTS || self.gfx.clips.len() > MAX_CLIPS {
            self.clear_path();
        }
        match op.op.as_slice() {
            b"q" => self.push(),
            b"Q" => self.pop(),
            b"cm" => self.concat(op),
            b"BT" => self.in_text = true,
            b"ET" => self.in_text = false,
            b"w" => {
                if let Some([w]) = op.nums::<1>()
                    && w.is_finite()
                    && w >= 0.0
                {
                    self.gfx.width = w;
                }
            }
            b"g" => self.set_fill(op, Space::Device(1)),
            b"G" => self.set_stroke(op, Space::Device(1)),
            b"rg" => self.set_fill(op, Space::Device(3)),
            b"RG" => self.set_stroke(op, Space::Device(3)),
            b"k" => self.set_fill(op, Space::Device(4)),
            b"K" => self.set_stroke(op, Space::Device(4)),
            b"cs" => self.gfx.fill_space = space_of(op),
            b"CS" => self.gfx.stroke_space = space_of(op),
            b"sc" | b"scn" => self.gfx.fill = components(self.gfx.fill_space, op),
            b"SC" | b"SCN" => self.gfx.stroke = components(self.gfx.stroke_space, op),
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" if !self.in_text => self.construct(index, op),
            b"W" | b"W*" if !self.in_text => {
                self.pending_clip = Some(op.op.as_slice() == b"W*");
                self.pending_clip_ops.push(index);
            }
            b"n" | b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" if !self.in_text => {
                return self.finish(index, op, ops, crop);
            }
            _ => {}
        }
        None
    }

    fn push(&mut self) {
        if self.stack.len() >= MAX_SAVES {
            self.lost = self.lost.saturating_add(1);
        } else {
            self.stack.push(self.gfx.clone());
        }
    }

    fn pop(&mut self) {
        if self.lost > 0 {
            self.lost -= 1;
        } else if let Some(saved) = self.stack.pop() {
            self.gfx = saved;
        }
    }

    fn concat(&mut self, op: &Op) {
        if let Some(m) = Matrix::from_operands(&op.operands) {
            let next = m.then(&self.gfx.ctm);
            if next.0.iter().all(|v| v.is_finite()) {
                self.gfx.ctm = next;
            }
        }
    }

    fn set_fill(&mut self, op: &Op, space: Space) {
        self.gfx.fill_space = space;
        self.gfx.fill = components(space, op);
    }

    fn set_stroke(&mut self, op: &Op, space: Space) {
        self.gfx.stroke_space = space;
        self.gfx.stroke = components(space, op);
    }

    fn construct(&mut self, index: usize, op: &Op) {
        if self.lost > 0 {
            return;
        }
        let before = self.path.elements().len();
        match op.op.as_slice() {
            b"m" => self.move_to(op),
            b"l" => self.line_to(op),
            b"c" => self.curve(op),
            b"v" => self.curve_v(op),
            b"y" => self.curve_y(op),
            b"h" => {
                if self.has_point() {
                    self.path.close_path();
                }
            }
            b"re" => self.rect_path(op),
            _ => {}
        }
        if self.path.elements().len() > before && self.first.is_none() {
            self.first = Some(index);
        }
    }

    fn finish(&mut self, index: usize, op: &Op, ops: &[Op], crop: [f64; 4]) -> Option<PagePath> {
        let kind = paint_kind(op.op.as_slice());
        if kind.close {
            self.close();
        }
        let clipping = self.pending_clip.take();
        let clip_ops = std::mem::take(&mut self.pending_clip_ops);
        let first = self.first.take();
        if self.path.elements().is_empty() || self.lost > 0 {
            self.clear_path();
            return None;
        }
        let path = std::mem::take(&mut self.path);
        self.sub_start = None;
        if let Some(even) = clipping {
            // The clip is the path as painted, including a close the paint operator implies.
            self.gfx.clips.push(Clip { path: Arc::new(path.clone()), even_odd: even });
        }
        if !kind.fills && !kind.strokes {
            return None;
        }
        let Some(first) = first else { return None };
        let (fills, strokes, even_odd) = (kind.fills, kind.strokes, kind.even_odd);
        let pad = if strokes { (self.gfx.width.max(MIN_STROKE)) / 2.0 } else { 0.0 };
        let bounds = path.bounding_box();
        let mut rect = [bounds.x0 - pad, bounds.y0 - pad, bounds.x1 + pad, bounds.y1 + pad];
        for clip in &self.gfx.clips {
            let b = clip.path.bounding_box();
            rect = intersect(rect, [b.x0, b.y0, b.x1, b.y1])?;
        }
        rect = intersect(rect, crop)?;
        let clips_content = clipping.is_some();
        let exclusive = clips_content && exclusive_clip(ops, index, self.stack.len());
        let item = PagePath {
            index: 0,
            rect,
            fill: fills.then(|| rgb_of(&self.gfx.fill)).flatten(),
            stroke: strokes.then(|| rgb_of(&self.gfx.stroke)).flatten(),
            paint: index,
            first,
            clip_ops: if clips_content { clip_ops } else { Vec::new() },
            clips_content,
            exclusive,
            fills,
            strokes,
            fill_device: matches!(self.gfx.fill, Ink::Device(_)),
            stroke_device: matches!(self.gfx.stroke, Ink::Device(_)),
            even_odd,
            width: self.gfx.width,
            path,
            clips: self.gfx.clips.iter().map(|c| ((*c.path).clone(), c.even_odd)).collect(),
        };
        Some(item)
    }

    fn clear_path(&mut self) {
        self.path = BezPath::new();
        self.sub_start = None;
        self.first = None;
        self.pending_clip = None;
        self.pending_clip_ops.clear();
    }

    fn close(&mut self) {
        if self.has_point() {
            self.path.close_path();
        }
    }

    fn has_point(&self) -> bool {
        match self.path.elements().last() {
            Some(kurbo::PathEl::MoveTo(_) | kurbo::PathEl::LineTo(_) | kurbo::PathEl::QuadTo(_, _) | kurbo::PathEl::CurveTo(_, _, _)) => true,
            Some(kurbo::PathEl::ClosePath) => self.sub_start.is_some(),
            None => false,
        }
    }

    fn current(&self) -> Option<Point> {
        match self.path.elements().last() {
            Some(kurbo::PathEl::MoveTo(p) | kurbo::PathEl::LineTo(p)) => Some(*p),
            Some(kurbo::PathEl::QuadTo(_, p) | kurbo::PathEl::CurveTo(_, _, p)) => Some(*p),
            Some(kurbo::PathEl::ClosePath) => self.sub_start,
            None => None,
        }
    }

    fn point(&self, x: f64, y: f64) -> Option<Point> {
        if !x.is_finite() || !y.is_finite() || x.abs() > 1e9 || y.abs() > 1e9 {
            return None;
        }
        let (x, y) = self.gfx.ctm.apply(x, y);
        (x.is_finite() && y.is_finite()).then_some(Point::new(x, y))
    }

    fn move_to(&mut self, op: &Op) {
        if let Some([x, y]) = op.nums::<2>()
            && let Some(p) = self.point(x, y)
        {
            self.sub_start = Some(p);
            self.path.move_to(p);
        }
    }

    fn line_to(&mut self, op: &Op) {
        if let Some([x, y]) = op.nums::<2>()
            && self.has_point()
            && let Some(p) = self.point(x, y)
        {
            self.path.line_to(p);
        }
    }

    fn curve(&mut self, op: &Op) {
        if let Some([x1, y1, x2, y2, x3, y3]) = op.nums::<6>()
            && self.has_point()
            && let (Some(p1), Some(p2), Some(p3)) = (self.point(x1, y1), self.point(x2, y2), self.point(x3, y3))
        {
            self.path.curve_to(p1, p2, p3);
        }
    }

    fn curve_v(&mut self, op: &Op) {
        if let Some([x2, y2, x3, y3]) = op.nums::<4>()
            && self.has_point()
            && let Some(current) = self.current()
            && let (Some(p2), Some(p3)) = (self.point(x2, y2), self.point(x3, y3))
        {
            self.path.curve_to(current, p2, p3);
        }
    }

    fn curve_y(&mut self, op: &Op) {
        if let Some([x1, y1, x3, y3]) = op.nums::<4>()
            && self.has_point()
            && let (Some(p1), Some(p3)) = (self.point(x1, y1), self.point(x3, y3))
        {
            self.path.curve_to(p1, p3, p3);
        }
    }

    fn rect_path(&mut self, op: &Op) {
        let Some([x, y, w, h]) = op.nums::<4>() else { return };
        let Some(p0) = self.point(x, y) else { return };
        let Some(p1) = self.point(x + w, y) else { return };
        let Some(p2) = self.point(x + w, y + h) else { return };
        let Some(p3) = self.point(x, y + h) else { return };
        self.sub_start = Some(p0);
        self.path.move_to(p0);
        self.path.line_to(p1);
        self.path.line_to(p2);
        self.path.line_to(p3);
        self.path.close_path();
    }
}

struct PaintKind {
    fills: bool,
    strokes: bool,
    even_odd: bool,
    close: bool,
}

fn paint_kind(op: &[u8]) -> PaintKind {
    let close = matches!(op, b"s" | b"b" | b"b*");
    let even_odd = matches!(op, b"f*" | b"B*" | b"b*");
    let fills = matches!(op, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*");
    let strokes = matches!(op, b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*");
    PaintKind { fills, strokes, even_odd, close }
}

fn space_of(op: &Op) -> Space {
    match op.name(0) {
        Some(b"DeviceGray") => Space::Device(1),
        Some(b"DeviceRGB") => Space::Device(3),
        Some(b"DeviceCMYK") => Space::Device(4),
        _ => Space::Other,
    }
}

fn components(space: Space, op: &Op) -> Ink {
    let comps: Option<Vec<f64>> = op.operands.iter().map(Object::as_f64).collect();
    match (space, comps) {
        (Space::Device(n), Some(c)) if c.len() == n && c.iter().all(|v| v.is_finite() && v.abs() <= 1e6) => Ink::Device(c),
        _ => Ink::Other,
    }
}

fn rgb_of(ink: &Ink) -> Option<[f64; 3]> {
    let Ink::Device(c) = ink else { return None };
    let rgb = match c.as_slice() {
        [g] => [*g, *g, *g],
        [r, g, b] => [*r, *g, *b],
        [c, m, y, k] => {
            let ch = |a: f64| (1.0 - (a + k).min(1.0)).clamp(0.0, 1.0);
            [ch(*c), ch(*m), ch(*y)]
        }
        _ => return None,
    };
    rgb.iter().all(|v| v.is_finite()).then_some(rgb)
}

fn intersect(a: [f64; 4], b: [f64; 4]) -> Option<[f64; 4]> {
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    (r.iter().all(|v| v.is_finite()) && r[2] > r[0] && r[3] > r[1]).then_some(r)
}

/// Whether anything painted after `paint` still sees the clip set at graphics-state depth `depth`.
fn exclusive_clip(ops: &[Op], paint: usize, depth: usize) -> bool {
    let mut depth_now = depth;
    for op in ops.iter().skip(paint.saturating_add(1)) {
        match op.op.as_slice() {
            b"q" => depth_now = depth_now.saturating_add(1),
            b"Q" => {
                if depth_now == 0 {
                    continue;
                }
                depth_now -= 1;
                if depth_now < depth {
                    return true;
                }
            }
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"Tj" | b"TJ" | b"'" | b"\"" | b"Do" | b"sh" | b"BI" => {
                return false;
            }
            _ => {}
        }
    }
    true
}

fn filled(path: &BezPath, even_odd: bool, pt: Point) -> bool {
    let w = path.winding(pt);
    if even_odd { w % 2 != 0 } else { w != 0 }
}

fn on_stroke(path: &BezPath, width: f64, pt: Point) -> bool {
    let band = width.max(MIN_STROKE);
    let bounds = path.bounding_box();
    let pad = band / 2.0;
    if pt.x < bounds.x0 - pad || pt.x > bounds.x1 + pad || pt.y < bounds.y0 - pad || pt.y > bounds.y1 + pad {
        return false;
    }
    let style = kurbo::Stroke::new(band);
    let outlined = kurbo::stroke(path.elements().iter().copied(), &style, &kurbo::StrokeOpts::default(), 0.25);
    outlined.winding(pt) != 0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use pdfcraft_cos::{Document, SaveOptions, write_incremental};

    use super::*;
    use crate::objects::{ObjectKind, ObjectTarget, copy_objects, editable_objects, move_objects, paste_objects};
    use crate::text::{content_streams, text_blocks};

    fn page(w: i32, h: i32, content: &str) -> Document {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 {w} {h}] >>"),
            "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offs = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offs.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let x = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offs {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
        Document::open(Arc::new(out)).unwrap()
    }

    fn stream(doc: &Document) -> String {
        let page = &pdfcraft_model::pages(doc)[0];
        let streams = content_streams(doc, &page.dict).unwrap();
        String::from_utf8_lossy(&streams.into_iter().flat_map(|s| s.data).collect::<Vec<_>>()).into_owned()
    }

    fn paths_of(doc: &Document) -> Vec<PagePath> {
        page_paths(doc, 0).unwrap()
    }

    fn pixel(doc: &Document, x: u32, y: u32) -> [u8; 4] {
        let bytes = write_incremental(doc, &SaveOptions::default()).unwrap();
        let mut renderer = pdfcraft_render::PageRenderer::new(Arc::new(bytes), pdfcraft_render::RenderConfig::default());
        let page = renderer.render(pdfcraft_render::RenderRequest {
            page: 0,
            kind: pdfcraft_render::RequestKind::Pixels,
            scale: 1.0,
            ..Default::default()
        });
        let idx = ((y * page.width + x) * 4) as usize;
        page.rgba[idx..idx + 4].try_into().unwrap()
    }

    #[test]
    fn painted_rects_delete_the_cell_and_recolor_only_the_header() {
        let content = "\
q 0 0 60 40 re W* n Q
0.5 g
0 30 60 10 re
f
0 0 1 rg
10 10 20 10 re
f
BT /F1 12 Tf 32 12 Td (Label) Tj ET
1 0 0 RG
1 w
40 5 15 10 re
S
";
        let doc = page(60, 40, content);
        let objects = editable_objects(&doc, 0).unwrap();
        let paths: Vec<_> = objects.iter().filter(|o| o.target.kind == ObjectKind::Path).collect();
        assert_eq!(paths.len(), 3, "the full-page clip is not an object");
        assert!(paths.iter().all(|o| o.rect[2] - o.rect[0] < 60.0 || o.rect[3] - o.rect[1] < 40.0));
        assert_eq!(text_blocks(&doc, 0).unwrap().iter().map(|b| b.text.clone()).collect::<Vec<_>>(), vec!["Label".to_string()]);

        let mut deleted = doc.clone();
        edit_paths(&mut deleted, 0, &[1], &PathEdit::Delete).unwrap();
        let left = stream(&deleted);
        assert!(left.contains("(Label)") && left.contains("Tj"), "{left}");
        assert!(left.contains("0.5 g"), "the header paint stays");
        assert_eq!(paths_of(&deleted).len(), 2);

        let mut recolored = doc.clone();
        edit_paths(&mut recolored, 0, &[0], &PathEdit::Recolor { fill: Some([1.0, 0.0, 0.0]), stroke: None }).unwrap();
        let header = pixel(&recolored, 30, 5);
        let cell = pixel(&recolored, 15, 25);
        let outside = pixel(&recolored, 5, 35);
        assert!(header[0] > 200 && header[1] < 30 && header[2] < 30, "header {header:?}");
        assert!(cell[2] > 200 && cell[0] < 30, "cell {cell:?}");
        assert!(outside.iter().take(3).all(|c| *c > 240), "outside {outside:?}");
    }

    #[test]
    fn a_clip_only_rectangle_is_not_a_path_and_its_text_stays() {
        let content = "\
q
0 0 200 200 re
W*
n
BT /F1 12 Tf 20 100 Td (Hello) Tj ET
Q
0 90 200 0.4 re
f
";
        let mut doc = page(200, 200, content);
        assert_eq!(paths_of(&doc).len(), 1);
        assert_eq!(text_blocks(&doc, 0).unwrap()[0].text, "Hello");
        edit_paths(&mut doc, 0, &[0], &PathEdit::Delete).unwrap();
        assert!(paths_of(&doc).is_empty());
        assert!(stream(&doc).contains("(Hello)"));
    }

    #[test]
    fn an_outline_path_is_selectable_and_deletable() {
        let mut doc = page(50, 50, "10 10 m 40 10 l 25 40 l h f\n");
        assert!(text_blocks(&doc, 0).unwrap().is_empty());
        let path = &paths_of(&doc)[0];
        assert!(path.contains([25.0, 20.0]));
        assert!(!path.contains([5.0, 5.0]));
        edit_paths(&mut doc, 0, &[0], &PathEdit::Delete).unwrap();
        assert!(paths_of(&doc).is_empty());
    }

    #[test]
    fn a_shared_clip_refuses_delete_and_an_exclusive_clip_does_not() {
        let shared = "\
q
0 0 40 40 re
W
f
BT /F1 12 Tf 10 10 Td (Keep) Tj ET
Q
";
        let mut doc = page(80, 80, shared);
        let before = stream(&doc);
        let err = edit_paths(&mut doc, 0, &[0], &PathEdit::Delete).unwrap_err();
        assert!(err.to_string().contains("also clips"), "{err}");
        assert_eq!(stream(&doc), before);

        let exclusive = "\
q
0 0 40 40 re
W
f
Q
BT /F1 12 Tf 10 10 Td (Keep) Tj ET
";
        let mut doc = page(80, 80, exclusive);
        edit_paths(&mut doc, 0, &[0], &PathEdit::Delete).unwrap();
        assert!(paths_of(&doc).is_empty());
        assert!(stream(&doc).contains("(Keep)"));
    }

    #[test]
    fn moving_a_path_shifts_its_bounds() {
        let mut doc = page(80, 80, "0 0 10 10 re f\n");
        let before = paths_of(&doc)[0].rect;
        move_objects(&mut doc, 0, &[ObjectTarget { kind: ObjectKind::Path, index: 0 }], [10.0, 5.0]).unwrap();
        let after = paths_of(&doc)[0].rect;
        for i in 0..4 {
            let delta = if i % 2 == 0 { 10.0 } else { 5.0 };
            assert!((after[i] - before[i] - delta).abs() < 0.01, "{before:?} {after:?}");
        }
    }

    #[test]
    fn copying_a_path_pastes_another() {
        let mut doc = page(80, 80, "0.2 0.4 0.6 rg 0 0 10 10 re f\n");
        let target = ObjectTarget { kind: ObjectKind::Path, index: 0 };
        let copied = copy_objects(&doc, 0, &[target]).unwrap();
        paste_objects(&mut doc, 0, &copied, [0.0, 0.0]).unwrap();
        assert_eq!(paths_of(&doc).len(), 2);
    }

    #[test]
    fn a_pattern_fill_can_be_deleted_but_not_recolored() {
        let mut doc = page(40, 40, "/Pattern cs /P scn 0 0 10 10 re f\n");
        let err = edit_paths(&mut doc, 0, &[0], &PathEdit::Recolor { fill: Some([1.0, 0.0, 0.0]), stroke: None }).unwrap_err();
        assert!(err.to_string().contains("cannot be changed"), "{err}");
        assert_eq!(paths_of(&doc).len(), 1);
        edit_paths(&mut doc, 0, &[0], &PathEdit::Delete).unwrap();
        assert!(paths_of(&doc).is_empty());
    }
}
