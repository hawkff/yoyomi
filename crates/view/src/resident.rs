//! The kitty images this process keeps in the terminal.

use std::collections::{HashMap, VecDeque};

use parking_lot::Mutex;
use yoyomi_core::{ImageId, ImageIds};

/// A claim on a kitty image id. It holds until the registry gives the id to
/// someone else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lease {
    pub id: ImageId,
    generation: u64,
}

/// Which kitty image ids are in use, least recently shown first. When every
/// id is taken, the least recently shown one goes to the next caller. Reusing
/// an id needs no delete, because every kitty upload deletes its id first.
pub struct Resident {
    ids: ImageIds,
    resend: bool,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    leases: HashMap<ImageId, u64>,
    order: VecDeque<ImageId>,
    generation: u64,
}

impl Resident {
    /// A registry over `ids`. With `resend`, as under tmux, an upload goes out
    /// only with a frame that draws its image, and again whenever a frame
    /// draws the image anew, because tmux drops passthrough that arrives while
    /// it redraws.
    pub fn new(ids: ImageIds, resend: bool) -> Self {
        Resident { ids, resend, state: Mutex::new(State::default()) }
    }

    /// Whether uploads go out with the frames that draw their images.
    pub fn resend(&self) -> bool {
        self.resend
    }

    /// Claims an id: a free one, or else the least recently shown.
    pub fn lease(&self) -> Lease {
        let mut state = self.state.lock();
        let id = match self.ids.take() {
            Some(id) => id,
            None => state.order.pop_front().expect("every id the pool lacks has a lease"),
        };
        state.generation += 1;
        let generation = state.generation;
        state.leases.insert(id, generation);
        state.order.retain(|&held| held != id);
        state.order.push_back(id);
        Lease { id, generation }
    }

    /// Whether `lease` still holds its id.
    pub fn holds(&self, lease: Lease) -> bool {
        self.state.lock().leases.get(&lease.id) == Some(&lease.generation)
    }

    /// Marks the image of `lease` as shown now, so it goes last.
    pub fn touch(&self, lease: Lease) {
        let mut state = self.state.lock();
        if state.leases.get(&lease.id) == Some(&lease.generation) {
            state.order.retain(|&held| held != lease.id);
            state.order.push_back(lease.id);
        }
    }

    /// Gives the id of `lease` back to the pool.
    pub fn release(&self, lease: Lease) {
        let mut state = self.state.lock();
        if state.leases.get(&lease.id) == Some(&lease.generation) {
            state.leases.remove(&lease.id);
            state.order.retain(|&held| held != lease.id);
            self.ids.give_back(lease.id);
        }
    }

    /// Every id this process may use, for freeing them all at exit.
    pub fn ids(&self) -> impl Iterator<Item = ImageId> {
        self.ids.all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_recently_shown_image_goes_first() {
        let resident = Resident::new(ImageIds::with_base(0x100), false);
        let leases: Vec<Lease> = (0..ImageIds::PER_PROCESS).map(|_| resident.lease()).collect();
        resident.touch(leases[0]);
        let next = resident.lease();
        assert_eq!(next.id, leases[1].id, "the second lease was the least recently shown");
        assert!(!resident.holds(leases[1]));
        assert!(resident.holds(leases[0]) && resident.holds(next));
    }

    #[test]
    fn released_ids_return_to_the_pool() {
        let resident = Resident::new(ImageIds::with_base(0x100), true);
        let lease = resident.lease();
        resident.release(lease);
        assert!(!resident.holds(lease));
        let again = resident.lease();
        assert_eq!(again.id, lease.id);
        assert_ne!(again, lease, "a new lease on the same id is a different claim");
        assert!(resident.resend());
    }
}
