//! The trail too long for its room, decided without Slint: the root stays,
//! then the last steps that fit, an ellipsis standing for those between.
//! The page measures each step (its gap before it included) and the room;
//! a step not measured yet takes none.

/// What the page last measured of the trail.
#[derive(Debug, Default)]
pub(crate) struct Trail {
    widths: Vec<f32>,
    room: f32,
    ellipsis: f32,
}

impl Trail {
    /// A trail of `steps`: those beyond are forgotten, new ones not
    /// measured yet.
    pub(crate) fn resize(&mut self, steps: usize) {
        self.widths.resize(steps, 0.0);
    }

    /// What step `index` takes; one beyond the trail is ignored (the page
    /// measures a step before its removal is known).
    pub(crate) fn measured(&mut self, index: usize, width: f32) {
        if let Some(step) = self.widths.get_mut(index) {
            *step = width;
        }
    }

    pub(crate) fn room(&mut self, room: f32, ellipsis: f32) {
        self.room = room;
        self.ellipsis = ellipsis;
    }

    /// The first step kept after the root: 1 when the whole trail fits;
    /// else the earliest whose steps up to the last fit beside the root
    /// and the ellipsis; the last alone when none do.
    #[must_use]
    pub(crate) fn first_kept(&self) -> usize {
        let Some((root, steps)) = self.widths.split_first() else {
            return 1;
        };
        let left = self.room - root;
        if steps.iter().sum::<f32>() <= left || steps.len() < 2 {
            return 1;
        }
        let mut kept = 0.0;
        let mut first = steps.len();
        for (index, width) in steps.iter().enumerate().rev() {
            kept += width;
            if index + 1 < steps.len() && kept + self.ellipsis > left {
                break;
            }
            first = index + 1;
        }
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trail(widths: &[f32], room: f32) -> Trail {
        let mut trail = Trail::default();
        trail.resize(widths.len());
        for (index, width) in widths.iter().enumerate() {
            trail.measured(index, *width);
        }
        trail.room(room, 10.0);
        trail
    }

    #[test]
    fn a_trail_that_fits_is_kept_whole() {
        assert_eq!(trail(&[50.0, 40.0, 40.0], 130.0).first_kept(), 1);
        assert_eq!(trail(&[50.0], 10.0).first_kept(), 1);
        assert_eq!(Trail::default().first_kept(), 1);
    }

    #[test]
    fn the_last_steps_that_fit_beside_the_root_and_the_ellipsis_are_kept() {
        // The root leaves 80; with the ellipsis, the last two take 70, the
        // last three 110.
        assert_eq!(
            trail(&[50.0, 40.0, 40.0, 30.0, 30.0], 130.0).first_kept(),
            3
        );
    }

    #[test]
    fn steps_that_fill_the_room_exactly_fit() {
        assert_eq!(
            trail(&[50.0, 40.0, 40.0, 30.0, 30.0], 120.0).first_kept(),
            3
        );
    }

    #[test]
    fn a_root_wider_than_the_room_keeps_the_last_step_alone() {
        assert_eq!(trail(&[150.0, 40.0, 40.0, 30.0], 100.0).first_kept(), 3);
    }

    #[test]
    fn the_ellipsis_is_counted_once_the_trail_is_cut() {
        // Whole, it takes 160 of 125; cut, the last two and the ellipsis
        // take 70 of the 75 the root leaves, the last three 80.
        assert_eq!(
            trail(&[50.0, 40.0, 10.0, 30.0, 30.0], 125.0).first_kept(),
            3
        );
    }

    #[test]
    fn the_last_step_stays_even_when_it_does_not_fit() {
        assert_eq!(trail(&[50.0, 40.0, 40.0, 200.0], 130.0).first_kept(), 3);
    }

    #[test]
    fn a_trail_of_two_is_never_cut() {
        assert_eq!(trail(&[50.0, 200.0], 100.0).first_kept(), 1);
    }

    #[test]
    fn a_step_measured_beyond_the_trail_is_ignored_and_new_ones_take_none() {
        let mut trail = trail(&[50.0, 40.0, 40.0], 100.0);
        assert_eq!(trail.first_kept(), 2);
        trail.resize(2);
        trail.measured(2, 500.0);
        assert_eq!(trail.first_kept(), 1);
        trail.resize(4);
        assert_eq!(trail.first_kept(), 1, "the new steps not measured yet");
    }
}
