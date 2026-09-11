//! Disposition des tuiles du multiview (rendu et détection des clics).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

#[derive(Debug, Clone)]
pub struct MultiviewLayout {
    pub program: Rect,
    pub preview: Rect,
    pub tiles: Vec<Rect>,
}

/// Ajuste un rectangle 16:9 dans une cellule, centré.
fn fit_16_9(cx: i32, cy: i32, cw: i32, ch: i32, margin: i32) -> Rect {
    let cw = (cw - 2 * margin).max(16);
    let ch = (ch - 2 * margin).max(9);
    let (w, h) = if cw * 9 <= ch * 16 {
        (cw, cw * 9 / 16)
    } else {
        (ch * 16 / 9, ch)
    };
    Rect {
        x: cx + margin + (cw - w) / 2,
        y: cy + margin + (ch - h) / 2,
        w,
        h,
    }
}

pub fn compute(canvas_w: i32, canvas_h: i32, scene_count: usize, columns: u32) -> MultiviewLayout {
    let margin = (canvas_w / 240).max(2);
    let top_h = canvas_h / 2;
    let preview = fit_16_9(0, 0, canvas_w / 2, top_h, margin);
    let program = fit_16_9(canvas_w / 2, 0, canvas_w / 2, top_h, margin);

    let cols = columns.max(1) as i32;
    let rows = ((scene_count as i32 + cols - 1) / cols).max(1);
    let cell_w = canvas_w / cols;
    let cell_h = ((canvas_h - top_h) / rows).min(cell_w * 9 / 16 + 2 * margin);
    let mut tiles = Vec::with_capacity(scene_count);
    for i in 0..scene_count as i32 {
        let r = i / cols;
        let c = i % cols;
        tiles.push(fit_16_9(
            c * cell_w,
            top_h + r * cell_h,
            cell_w,
            cell_h,
            margin,
        ));
    }
    MultiviewLayout {
        program,
        preview,
        tiles,
    }
}

impl MultiviewLayout {
    /// Index de la tuile de scène sous le point (coordonnées du canevas).
    pub fn tile_at(&self, x: i32, y: i32) -> Option<usize> {
        self.tiles.iter().position(|r| r.contains(x, y))
    }
}
