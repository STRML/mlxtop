// SPDX-License-Identifier: MIT
use super::*;
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Chart {
    #[default]
    Prompt,
    Generation,
    Prefill,
    Cache,
    Gpu,
    Memory,
    Paging,
    Footprint,
    Queue,
    Latency,
}

impl From<ChartMetric> for Chart {
    fn from(metric: ChartMetric) -> Self {
        match metric {
            ChartMetric::Generation => Self::Generation,
            ChartMetric::Prefill => Self::Prefill,
            ChartMetric::Cache => Self::Cache,
            ChartMetric::Gpu => Self::Gpu,
            ChartMetric::Memory => Self::Memory,
            ChartMetric::Swap => Self::Paging,
        }
    }
}

pub(super) struct Navigation {
    pub focused: Chart,
    pub expanded: bool,
    zoom: [u16; 10],
    pub regions: RefCell<Vec<(Chart, Rect)>>,
}

impl Default for Navigation {
    fn default() -> Self {
        Self {
            focused: Chart::Prompt,
            expanded: false,
            zoom: [1; 10],
            regions: RefCell::new(Vec::new()),
        }
    }
}

impl Navigation {
    pub fn register(&self, chart: Chart, area: Rect) {
        if area.width >= 4 && area.height >= 1 {
            let mut regions = self.regions.borrow_mut();
            if let Some(region) = regions.iter_mut().find(|region| region.0 == chart) {
                region.1 = area;
            } else {
                regions.push((chart, area));
            }
        }
    }

    pub fn at(&self, x: u16, y: u16) -> Option<Chart> {
        self.regions.borrow().iter().find_map(|(chart, area)| {
            (area.contains((x, y).into()) && (!self.expanded || *chart == self.focused))
                .then_some(*chart)
        })
    }

    pub fn zoom(&self, chart: Chart) -> u16 {
        self.zoom[chart as usize]
    }

    pub fn change_zoom(&mut self, inward: bool) {
        let zoom = &mut self.zoom[self.focused as usize];
        *zoom = if inward {
            (*zoom * 2).min(8)
        } else {
            (*zoom / 2).max(1)
        };
    }

    pub fn reset_zoom(&mut self) {
        self.zoom[self.focused as usize] = 1;
    }

    pub fn cycle(&mut self, backwards: bool) {
        let regions = self.regions.borrow();
        if regions.is_empty() {
            return;
        }
        let index = regions
            .iter()
            .position(|(chart, _)| *chart == self.focused)
            .unwrap_or(0);
        self.focused = regions[if backwards {
            (index + regions.len() - 1) % regions.len()
        } else {
            (index + 1) % regions.len()
        }]
        .0;
    }

    pub fn move_focus(&mut self, key: KeyCode) {
        let regions = self.regions.borrow();
        let Some((_, current)) = regions.iter().find(|(chart, _)| *chart == self.focused) else {
            if let Some((chart, _)) = regions.first() {
                self.focused = *chart;
            }
            return;
        };
        let center = |area: Rect| {
            (
                i32::from(area.x) * 2 + i32::from(area.width),
                i32::from(area.y) * 2 + i32::from(area.height),
            )
        };
        let (cx, cy) = center(*current);
        if let Some((chart, _)) = regions
            .iter()
            .filter(|(chart, area)| {
                let (x, y) = center(*area);
                *chart != self.focused
                    && match key {
                        KeyCode::Left => x < cx,
                        KeyCode::Right => x > cx,
                        KeyCode::Up => y < cy,
                        KeyCode::Down => y > cy,
                        _ => false,
                    }
            })
            .min_by_key(|(_, area)| {
                let (x, y) = center(*area);
                // Favor charts aligned with the movement axis before diagonals.
                match key {
                    KeyCode::Left | KeyCode::Right => (y - cy).abs() * 4 + (x - cx).abs(),
                    _ => (x - cx).abs() * 4 + (y - cy).abs(),
                }
            })
        {
            self.focused = *chart;
        }
    }

    pub fn decorate(&self, frame: &mut Frame) {
        let regions = self.regions.borrow();
        let Some((_, area)) = regions.iter().find(|(chart, _)| *chart == self.focused) else {
            return;
        };
        let buffer = frame.buffer_mut();
        for x in area.x..area.right() {
            for y in [area.y, area.bottom() - 1] {
                if matches!(buffer[(x, y)].symbol(), "─" | "┌" | "┐" | "└" | "┘") {
                    buffer[(x, y)].set_fg(CYAN);
                }
            }
        }
        for y in area.y..area.bottom() {
            buffer[(area.x, y)].set_fg(CYAN);
            buffer[(area.right() - 1, y)].set_fg(CYAN);
        }
        // Focus remains recognizable without relying on color.
        if area.height < 3 {
            buffer[(area.x, area.y)].set_symbol("▸");
            return;
        }
        buffer[(area.x, area.y)].set_symbol("╔");
        buffer[(area.right() - 1, area.y)].set_symbol("╗");
        buffer[(area.x, area.bottom() - 1)].set_symbol("╚");
        buffer[(area.right() - 1, area.bottom() - 1)].set_symbol("╝");
    }
}
