//! Disposition des tuiles du multiview (partagée entre GStreamer et la fenêtre).

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

    /// Met à l'échelle un rectangle du canevas vers une autre taille.
    pub fn scaled(&self, from: (i32, i32), to: (i32, i32)) -> Rect {
        let sx = to.0 as f64 / from.0.max(1) as f64;
        let sy = to.1 as f64 / from.1.max(1) as f64;
        Rect {
            x: (self.x as f64 * sx).round() as i32,
            y: (self.y as f64 * sy).round() as i32,
            w: (self.w as f64 * sx).round() as i32,
            h: (self.h as f64 * sy).round() as i32,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MultiviewLayout {
    pub canvas: (i32, i32),
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
        canvas: (canvas_w, canvas_h),
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
