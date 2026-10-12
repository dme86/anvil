//! Pure logical-output direction selection for keyboard-controlled monitor navigation.
//! Geometry is re-evaluated on every action, so hotplug, rotation and position reloads
//! cannot leave the compositor with cached/stale output neighbors.
use anvil::layout::Rect;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

fn axis_bounds(r: Rect, horizontal: bool) -> (i64, i64) {
    if horizontal {
        (i64::from(r.x), i64::from(r.x) + i64::from(r.width))
    } else {
        (i64::from(r.y), i64::from(r.y) + i64::from(r.height))
    }
}

fn separation(a: (i64, i64), b: (i64, i64)) -> i64 {
    (a.0 - b.1).max(b.0 - a.1).max(0)
}

/// Return the adjacent output index for a direction. Only candidates whose logical
/// center lies in the requested half-plane qualify. Prefer outputs sharing rows
/// (left/right) or columns (up/down); then closest along that axis, then offsets.
///
/// No cyclic fallback or wrapping: at a physical edge the result is None.
/// The final index is the deterministic tie-breaker for equivalent geometry.
pub fn neighbor(outputs: &[(&str, Rect)], current: usize, direction: Direction) -> Option<usize> {
    let source = outputs.get(current)?.1;
    if source.width <= 0 || source.height <= 0 {
        return None;
    }
    let horizontal = matches!(direction, Direction::Left | Direction::Right);
    let forward = matches!(direction, Direction::Right | Direction::Down);
    let main = axis_bounds(source, horizontal);
    let cross = axis_bounds(source, !horizontal);
    let center = main.0 + main.1;
    outputs
        .iter()
        .enumerate()
        .filter(|(i, (_, rect))| *i != current && rect.width > 0 && rect.height > 0)
        .filter_map(|(index, (_, rect))| {
            let candidate = axis_bounds(*rect, horizontal);
            let candidate_center = candidate.0 + candidate.1;
            let distance = if forward {
                candidate_center - center
            } else {
                center - candidate_center
            };
            if distance <= 0 {
                return None;
            }
            let perpendicular = separation(cross, axis_bounds(*rect, !horizontal));
            Some(((perpendicular != 0, distance, perpendicular, index), index))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, index)| index)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }
    #[test]
    fn horizontal_and_vertical_no_wrap() {
        let outputs = [
            ("left", r(0, 0, 800, 600)),
            ("center", r(800, 0, 1000, 600)),
            ("right", r(1800, 0, 800, 600)),
            ("above", r(800, -900, 900, 900)),
            ("below", r(800, 600, 900, 800)),
        ];
        assert_eq!(neighbor(&outputs, 1, Direction::Left), Some(0));
        assert_eq!(neighbor(&outputs, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&outputs, 1, Direction::Up), Some(3));
        assert_eq!(neighbor(&outputs, 1, Direction::Down), Some(4));
        assert_eq!(neighbor(&outputs, 0, Direction::Left), None);
    }
    #[test]
    fn overlap_beats_diagonally_closer_display_and_ties_are_stable() {
        let outputs = [
            ("source", r(0, 0, 900, 700)),
            ("diagonal", r(900, 720, 500, 300)),
            ("aligned", r(1500, 100, 500, 500)),
            ("duplicate", r(1500, 100, 500, 500)),
        ];
        assert_eq!(neighbor(&outputs, 0, Direction::Right), Some(2));
        assert_eq!(neighbor(&outputs, 0, Direction::Up), None);
    }
    #[test]
    fn positions_and_sizes_determine_direction_not_connector_name() {
        let outputs = [
            ("HDMI-A-1", r(1000, 900, 600, 400)),
            ("DP-3", r(-400, 700, 1000, 800)),
            ("DP-1", r(800, -500, 1200, 900)),
        ];
        assert_eq!(neighbor(&outputs, 0, Direction::Left), Some(1));
        assert_eq!(neighbor(&outputs, 0, Direction::Up), Some(2));
    }
    #[test]
    fn disconnected_or_one_monitor_does_nothing() {
        assert_eq!(
            neighbor(&[("only", r(0, 0, 800, 600))], 0, Direction::Right),
            None
        );
        assert_eq!(neighbor(&[], 0, Direction::Right), None);
    }
}
